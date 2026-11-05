//! Discoverable command-line interface for persistent named agents.
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, CommandFactory, Parser, Subcommand};
use serde::Serialize;
use serde_json::{json, Value};

use crate::agent::{AgentError, DrainOptions, QueueOutcome};
use crate::client::HerdrClient;
use crate::subagents::{
    environment_entries, stop_reason, AdoptOptions, CloudLaunch, CloudTools, ManagedAgents,
    RecoveryAction, SlotLaunch, StartOptions, StopContext, StopFailure, StopOptions,
};

const MAX_CHAT_PUBLISH_BYTES: usize = 30_000;

#[derive(Parser)]
#[command(
    name = "agentctl",
    version,
    about = "Manage persistent coding agents that you can message and inspect",
    long_about = "Manage named coding-agent sessions with durable prompt delivery and direct human access.\nThe Rust implementation controls interactive Codex, Claude, and Muse sessions through Herdr and can run a durable event-driven chat bridge through installed subscription plugins.",
    after_help = "Examples:\n  agentctl quickstart\n  agentctl start reviewer --cwd .\n  agentctl adopt reviewer --pane w1:p2 --workspace project --cwd /work/project --harness codex\n  agentctl send reviewer 'Review the current changes'\n  agentctl chat status --bridge-state ~/.local/state/agentctl/chat\n  agentctl pause reviewer\n  agentctl attach reviewer\n  agentctl resume reviewer\n  agentctl stop reviewer"
)]
struct Cli {
    /// Registry directory containing agent records and durable queues
    #[arg(
        long,
        visible_alias = "state",
        global = true,
        default_value = ".agentctl",
        value_name = "DIR"
    )]
    registry: PathBuf,
    /// Herdr executable name on PATH or an explicit path (interactive adapter only)
    #[arg(long, global = true, default_value = "herdr", value_name = "PATH")]
    herdr_bin: PathBuf,
    /// agentcloudctl executable name on PATH or an explicit path (agentcloud agents only)
    #[arg(
        long,
        global = true,
        default_value = "agentcloudctl",
        value_name = "PATH"
    )]
    agentcloudctl_bin: PathBuf,
    /// agentterm executable run in an agentcloud agent's Herdr tab; name on PATH or explicit path
    #[arg(long, global = true, default_value = "agentterm", value_name = "PATH")]
    agentterm_bin: PathBuf,
    /// Agentcloud orchestrator endpoint recorded for NEW agentcloud agents and passed as --ws-url to both agentcloudctl and agentterm; default: $AGENTCLOUD_ORCHESTRATOR_URL when set and nonempty, else agentcloudctl's documented production endpoint wss://mm.internalmeta.com/ws/chat. Later commands use the endpoint recorded for the agent and refuse a different explicit value; a record from before endpoints were recorded gets this resolved endpoint saved on its first send or stop
    #[arg(long, global = true, value_name = "URL")]
    agentcloud_url: Option<String>,
    /// Agentcloud session this caller runs in; input to another agentcloud agent is then an attributed `agentcloudctl send-message` from it. An empty value selects human attribution (`agentcloudctl send`), which is refused when this process runs inside a different agentcloud session than the recipient's. Default: $AGENTCLOUD_SESSION_ID when set and nonempty
    #[arg(long, global = true, value_name = "ID")]
    from_session: Option<String>,
    /// Print the installed operator reference and exit
    #[arg(long)]
    userguide: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Show installed runtime adapters and optional services as JSON
    #[command(after_help = "Example: agentctl capabilities")]
    Capabilities,
    /// Launch an interactive agent, or an agentcloud session viewed through agentterm, in its own Herdr tab
    #[command(
        after_help = "Examples:\n  agentctl start reviewer --cwd . --harness codex --brief 'Review the changes'\n  agentctl start sub-cloud-worker --harness agentcloud --cloud-harness claude-code \\\n    --provision --envspec ENVSPEC --brief 'Run hostname, then reply DONE.'\n  agentctl start sub-cloud-worker --cwd . --profile cloud-worker --brief 'Investigate the failure'\n\nWith --harness agentcloud, agentctl runs `agentcloudctl create` (title = NAME, the brief is its\ndurable --prompt), records the printed session ID, then runs `agentterm -s SESSION_ID` in the\nnew tab. Both receive the same --ws-url: --agentcloud-url, else $AGENTCLOUD_ORCHESTRATOR_URL, else\nagentcloudctl's documented production endpoint; it is recorded for every later command. --model and --reasoning-effort map to create --model and --effort; --harness-arg adds\nliteral long create options written as --option=value. `agentcloudctl create` is bounded at\n300 seconds; --startup-timeout bounds the wait for agentterm to become the tab's foreground\nprocess. --env applies to the tab only, never to agentcloudctl."
    )]
    Start(Box<Start>),
    /// List safe metadata for ignored private launch profiles
    #[command(after_help = "Example: agentctl profiles --cwd /work/project")]
    Profiles(Profiles),
    /// Install the bundled agentctl harness skill
    Skill(Skill),
    /// Register an existing Herdr agent without taking ownership of its runtime; Muse requires a pinned foreground process
    #[command(
        after_help = "Example: agentctl adopt reviewer --pane w1:p2 --workspace project --cwd /work/project --harness codex"
    )]
    Adopt(Adopt),
    /// Move a running owned native Herdr agent into an existing configured workspace; adopted, custom-pane, and multi-pane tabs are refused
    #[command(after_help = "Example: agentctl move reviewer")]
    Move(Named),
    /// Rename a running native or adopted agent: registry entry, Herdr agent name, and tab label together. Use it when an agent's warm context is still useful but its purpose changed; prefer a new agent otherwise. An interrupted rename refuses both names until the same command is rerun
    #[command(after_help = "Example: agentctl rename reviewer release-reviewer")]
    Rename(Rename),
    /// Pin the terminal and foreground harness process of an agent whose record pins no harness process, after checking that its pane runs the intended agent. Input to an agent with no pinned harness process or observed native session is refused
    #[command(after_help = "Example: agentctl anchor reviewer")]
    Anchor(Anchor),
    /// Compare every registry record with live Herdr state and report each mismatch. Read-only unless --repair-labels. Exit 0 when clean, 1 with findings
    #[command(after_help = "Example: agentctl doctor")]
    Doctor(Doctor),
    /// Stop an owned runtime, or safely unregister an adopted one, and archive state
    #[command(
        after_help = "Examples:\n  agentctl stop reviewer\n  agentctl stop sub-cloud-worker\n  agentctl stop reviewer --retire-dead-adoption --expected-token TOKEN --expected-record-sha256 SHA256\n\nFor an agentcloud agent, stop runs `agentcloudctl halt` (the open run is interrupted and no new\nrun starts), closes the recorded agentterm pane, runs `agentcloudctl archive`, and archives the\nregistry record. A halt failure changes nothing locally. stop does not release the node\nreservation; the orchestrator releases it on its own schedule after the halted session goes\nidle (check `agentcloudctl inspect -s SESSION_ID`).\n\nA headless turn-runner worker is retired only when every process its runtime recorded has\nprovably exited: stop --expected-token then archives the record with its runtime home and leaves\nthe tmux window or Herdr tab in place. A live or unprovable runner exits 69 and changes nothing."
    )]
    Stop(Stop),
    /// Relaunch a dead owned local agent from its recorded conversation and launch policy, preserving its old queue in the archive
    #[command(
        after_help = "Examples:\n  agentctl revive reviewer\n  agentctl revive --all --dry-run\n  agentctl revive reviewer --expected-token TOKEN\n\nA live or unverifiable harness is never relaunched. Recovery opens a fresh tab, verifies the\nresumed runtime, archives the complete old generation, then closes only its proved stale pane.\nAn interrupted revive reserves the name until the same command finishes publication or cleanup.\nAdopted agents, headless workers, and remote agentcloud sessions are refused."
    )]
    Revive(Revive),
    /// List registered agents with live status or an explicit probe error
    #[command(after_help = "Example: agentctl list --registry .agentctl")]
    List,
    /// Show durable metadata, queue state, and the live runtime probe
    #[command(after_help = "Example: agentctl status reviewer")]
    Status(Named),
    /// Persist and deliver a prompt when the agent is ready
    #[command(
        after_help = "Examples:\n  agentctl send reviewer 'Review the diff'\n  agentctl send reviewer --file task.txt --message-id review-1\n\nFor an agentcloud agent the prompt is journaled by `agentcloudctl send --end-of-turn` (a leading\n/goal is sent without --end-of-turn), up to 102400 bytes. When --from-session (default:\n$AGENTCLOUD_SESSION_ID) names a different agentcloud session, agentcloudctl refuses that send,\nso the prompt goes as an attributed `agentcloudctl send-message` from that session instead; it\narrives as an ordinary input and a /goal is not interpreted. An explicit\nhuman send (--from-session '') from inside a different agentcloud session is refused, never\ndisguised. --message-id becomes the idempotency\nkey, so repeating it journals nothing new; without it agentctl generates and prints one. The\ndelivery timeouts and --max-attempts do not apply."
    )]
    Send(Send),
    /// Deliver queued prompts that are known not to have been submitted
    #[command(after_help = "Example: agentctl drain reviewer --ready-timeout 60")]
    Drain(Drain),
    /// Read visible terminal output and save a bounded snapshot
    #[command(
        after_help = "Examples:\n  agentctl read reviewer --lines 100\n  agentctl read sub-cloud-worker --output last\n\nFor an agentcloud agent, --output last prints the last settled run's final reply from\n`agentcloudctl output` (unbounded by --lines); tail and all read the agentterm tab."
    )]
    Read(Read),
    /// Wait until the agent is ready for input; this does not prove goal completion
    #[command(
        after_help = "Example: agentctl wait reviewer --timeout 60\n\nFor an agentcloud agent this is `agentcloudctl wait --until settle`: success means the current or\nlast run completed (or none has run); a failed or interrupted run exits 75 with a redirect."
    )]
    Wait(Wait),
    /// Inspect a goal or set an ongoing objective (native /goal for Codex)
    #[command(
        after_help = "Examples:\n  agentctl goal reviewer\n  agentctl goal reviewer 'Complete the review and report blockers'"
    )]
    Goal(Goal),
    /// Bind an explicitly known native conversation ID to a verified agent pane
    #[command(after_help = "Example: agentctl bind-session reviewer THREAD_ID")]
    BindSession(BindSession),
    /// Read, set, or clear a native Codex goal through the app-server protocol, without resuming or prompting its thread
    #[command(after_help = crate::codex_goal::command::AFTER_HELP)]
    CodexGoal(crate::codex_goal::command::CodexGoalArgs),
    /// Verify and focus the agent pane for human interaction; does not pause automation
    #[command(after_help = "Example: agentctl pause reviewer && agentctl attach reviewer")]
    Attach(Named),
    /// Show the agentcloud session this process runs in through its viewer in a Herdr tab, and register it so it is listed beside the agents it starts
    #[command(
        after_help = "Examples:\n  agentctl attach-self coordinator\n  agentctl attach-self coordinator --cwd . --workspace-id w2\n\nThe session is $AGENTCLOUD_SESSION_ID; without it attach-self is refused, because a session\nin a local terminal cannot be moved into Herdr. Nothing changes in agentcloud: the tab runs\n`agentterm -s SESSION_ID` with the same endpoint resolution as start. The tab goes to\n--workspace-id, else the configured project workspace, else HERDR_WORKSPACE_ID, else the\nworkspace labelled with the --cwd directory's base name, which is created when absent; agents this session starts later\nwithout an explicit workspace open in the same workspace. stop NAME closes the tab and archives\nthe record but never halts or archives the session, which is the caller itself."
    )]
    AttachSelf(AttachSelf),
    /// Pause automation input without suspending the harness or its running work
    #[command(after_help = "Example: agentctl pause reviewer")]
    Pause(Named),
    /// Re-enable automation input; queued messages still require drain or send
    #[command(after_help = "Example: agentctl resume reviewer && agentctl drain reviewer")]
    Resume(Named),
    /// Initialize, inspect, recover, or run a durable chat bridge
    #[command(
        after_help = "Example: agentctl chat status --bridge-state ~/.local/state/agentctl/chat"
    )]
    Chat(Chat),
    /// Queue notices about workers for a coordinator and deliver them as one prioritized batch
    #[command(
        after_help = "Examples:\n  agentctl inbox quickstart\n  agentctl inbox post --to coord --from reviewer --kind idle --text 'Review done'\n  agentctl inbox deliver --to coord --via print"
    )]
    Inbox(crate::inbox::InboxArgs),
    /// Print a short, runnable introduction to starting and controlling a worker
    Quickstart,
    /// Print the operator reference, including dependencies and delivery guarantees
    Userguide,
}

#[derive(Args)]
struct Named {
    /// Registered agent name (1-32 lowercase letters, digits, and hyphens)
    name: String,
}

#[derive(Args)]
struct Revive {
    /// Registered name to recover; required unless --all
    #[arg(
        value_name = "NAME",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    name: Option<String>,
    /// Inspect every registered generation and continue through independent recovery refusals
    #[arg(long, conflicts_with_all = ["name", "expected_token"])]
    all: bool,
    /// Print safe recovery plans without changing records, tabs, queues, or lock files
    #[arg(long)]
    dry_run: bool,
    /// Require this exact old generation token; applies only to a named revive
    #[arg(long, requires = "name", value_name = "TOKEN")]
    expected_token: Option<String>,
    /// Startup-readiness deadline in seconds, greater than 0 and at most 300 (default: 30)
    #[arg(long, default_value = "30", value_parser = startup_seconds, value_name = "SECONDS")]
    startup_timeout: f64,
}

#[derive(Args)]
struct Rename {
    #[command(flatten)]
    agent: Named,
    /// New registered name (lowercase letters, digits, hyphens)
    #[arg(value_name = "NEW")]
    new_name: String,
}

#[derive(Args)]
struct Anchor {
    #[command(flatten)]
    agent: Named,
    /// Replace anchors that no longer match (the pane may hold another program; inspect it first)
    #[arg(long)]
    replace: bool,
}

#[derive(Args)]
struct Doctor {
    /// Restore the tab label and Herdr agent name of owned agents whose every other anchor still matches
    #[arg(long)]
    repair_labels: bool,
}

#[derive(Args)]
struct Stop {
    #[command(flatten)]
    agent: Named,
    /// Require this exact current registry generation; mandatory for dead managed, adopted, or headless turn-runner recovery
    #[arg(long, value_name = "TOKEN")]
    expected_token: Option<String>,
    /// Recover an identity-less dead adopted row without changing its pane; requires --expected-token and --expected-record-sha256
    #[arg(long)]
    recover_legacy_adoption: bool,
    /// Archive a running adopted record whose pinned harness is dead, without querying Herdr; requires --expected-token and --expected-record-sha256
    #[arg(long, conflicts_with = "recover_legacy_adoption")]
    retire_dead_adoption: bool,
    /// Exact lowercase 64-hex SHA-256 of raw agent.json bytes; valid only with an adopted-record recovery flag
    #[arg(long, value_name = "SHA256")]
    expected_record_sha256: Option<String>,
    /// Agentcloud agents only: close the tab and archive the record WITHOUT halting or archiving
    /// the agentcloud session (use when the session no longer exists or was retired elsewhere)
    #[arg(long)]
    skip_cloud_halt: bool,
}

#[derive(Args)]
struct Delivery {
    /// Seconds to wait for a submit-safe state (0 probes immediately)
    #[arg(long, default_value = "900", value_parser = seconds)]
    ready_timeout: f64,
    /// Positive seconds to wait for evidence that a submitted prompt started work
    #[arg(long, default_value = "30", value_parser = positive_seconds)]
    working_timeout: f64,
    /// Maximum known-safe submission attempts per message
    #[arg(long, default_value = "3", value_parser = clap::value_parser!(u64).range(1..=1_000_000))]
    max_attempts: u64,
}
impl Delivery {
    fn options(&self) -> DrainOptions {
        DrainOptions {
            ready_timeout: Duration::from_secs_f64(self.ready_timeout),
            working_timeout: Duration::from_secs_f64(self.working_timeout),
            max_attempts: self.max_attempts,
            deliver_while_working: false,
        }
    }
}

#[derive(Args)]
struct AttachSelf {
    #[command(flatten)]
    agent: Named,
    /// Directory recorded for the session; with no workspace configured, its base name labels the Herdr workspace
    #[arg(long, default_value = ".", value_name = "DIR")]
    cwd: PathBuf,
    /// Existing Herdr workspace ID; default: configured project workspace, else HERDR_WORKSPACE_ID, else the workspace labelled with the --cwd base name; an explicit ID must match configured policy
    #[arg(long, value_name = "ID")]
    workspace_id: Option<String>,
    /// Seconds to wait for the viewer to become the tab's foreground process, greater than zero and at most 300
    #[arg(long, default_value = "30", value_parser = startup_seconds)]
    startup_timeout: f64,
}

#[derive(Args)]
struct Start {
    #[command(flatten)]
    agent: Named,
    /// Working directory for the new harness
    #[arg(long, default_value = ".", value_name = "DIR")]
    cwd: PathBuf,
    /// Owner-configured profile from CWD/.agentctl/profiles.json, or, when CWD has none, from the
    /// profiles.json beside a registry named .agentctl (so a worktree outside the project uses the
    /// project's profiles). Can combine with --resume for local interactive Claude, Codex, or
    /// Muse; other explicit launch settings conflict
    #[arg(long)]
    profile: Option<String>,
    /// Execution mode; headless workers require an installation with the worker extension
    #[arg(long, value_parser = ["interactive", "headless"])]
    mode: Option<String>,
    /// Terminal host; interactive Rust agents require Herdr
    #[arg(long, default_value = "herdr", value_parser = ["herdr", "tmux"])]
    backend: String,
    /// Herdr harness kind; Codex, Claude, and Muse have model presets; agentcloud creates an agentcloud session viewed through agentterm
    #[arg(long)]
    harness: Option<String>,
    /// agentcloud only: session driver passed to agentcloudctl create --harness (default: server default)
    #[arg(long, value_name = "KIND", value_parser = ["native", "claude-code", "codex", "muse-code"])]
    cloud_harness: Option<String>,
    /// agentcloud only: provision a fresh node for the session; the orchestrator holds its lease while the session is live and releases it once idle
    #[arg(long)]
    provision: bool,
    /// agentcloud only: envspec the provisioned node is reserved from, which decides its checkout; requires --provision
    #[arg(long, value_name = "NAME", requires = "provision")]
    envspec: Option<String>,
    /// agentcloud only: roster label for the provisioned node; requires --provision
    #[arg(long, value_name = "TEXT", requires = "provision")]
    purpose: Option<String>,
    /// agentcloud only: absolute session working directory on the node (default: server default); may contain {cwd}, {name}, {host}, or {fqdn}
    #[arg(long, value_name = "PATH")]
    cloud_workspace: Option<String>,
    /// agentcloud only: bind this existing node at creation; conflicts with --provision; may contain {host} or {fqdn} (example: --node-id '{fqdn}')
    #[arg(long, value_name = "ID", conflicts_with = "provision")]
    node_id: Option<String>,
    /// agentcloud only: session title (default: NAME); {name} is the agent name, {host} the local host's first label, {fqdn} its full name, {cwd} the agent's directory (example: --cloud-title '{name}-{host}')
    #[arg(long, value_name = "TEMPLATE")]
    cloud_title: Option<String>,
    /// Model identifier passed unchanged to the Codex or Claude harness
    #[arg(long)]
    model: Option<String>,
    /// Structured harness reasoning effort
    #[arg(long)]
    reasoning_effort: Option<String>,
    /// Existing native conversation ID for local interactive Claude, Codex, or Muse; can combine
    /// with --profile (example: --profile reviewer --resume CONVERSATION_ID)
    #[arg(long, value_name = "SESSION")]
    resume: Option<String>,
    /// Extra literal harness argument; repeat or use --harness-arg=--flag
    #[arg(long = "harness-arg", value_name = "ARG")]
    harness_args: Vec<String>,
    /// Set a literal KEY=VALUE variable in the created interactive Herdr tab; repeatable and omitted from status
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = environment_value)]
    environment: Vec<String>,
    /// Initial prompt to deliver after startup; conflicts with --file
    #[arg(long, conflicts_with = "file", value_name = "TEXT")]
    brief: Option<String>,
    /// UTF-8 file containing the initial prompt; conflicts with --brief
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
    /// Existing Herdr workspace ID; default: configured project workspace, else HERDR_WORKSPACE_ID, else the workspace of the caller's own agentcloud session registered by attach-self, else shared subagents; an explicit ID must match configured policy
    #[arg(long, value_name = "ID")]
    workspace_id: Option<String>,
    /// Interactive only: box the agent to this wrkslots slot (plain-worktree or image-backed); the pane shell is replaced by the line `wrkslots shell-command SLOT` prints (per-slot slice limits and, except with cgroup isolation, a confined file-system view) before the harness starts, and the agent's cwd is the slot directory. Needs wrkslots on PATH or AGENTCTL_WRKSLOTS_BIN
    #[arg(long, value_name = "SLOT")]
    slot: Option<String>,
    /// With --slot: userns = limits plus the file-system view in a user namespace; root = the same view built through sudo -n, for harness launchers that need a setuid step (claude and codex only: the boxed harness runs behind sudo's terminal relay, so agentctl launches it itself, reads its state from Herdr's screen rules, and types prompts with screen verification; goals are unavailable); cgroup = limits only (default: the project's configuration.sandbox.isolation, else userns)
    #[arg(long, value_name = "MODE", value_parser = ["userns", "cgroup", "root"], requires = "slot")]
    slot_isolation: Option<String>,
    /// With --slot: wrkslots project root (default: --cwd, searched upward for .wrkslots.yml)
    #[arg(long, value_name = "DIR", requires = "slot")]
    slot_project: Option<PathBuf>,
    /// Interactive only: run the agent as a (sub)coordinator in a wrkslots coordinator box (`wrkslots shell-command --box`): the same limits and file-system view as --slot, but with no slot, starting in --cwd, with every slot, the wrkslots registry, the slots' Git directories, and the agentctl registry in --cwd writable (or the whole project root with --box-writable project), so it can create slots and launch its own subagents into them. Subagents it launches start outside its box and box themselves into their own slot. Conflicts with --slot. Needs wrkslots on PATH or AGENTCTL_WRKSLOTS_BIN
    #[arg(long, conflicts_with = "slot")]
    project_box: bool,
    /// With --project-box: worktrees = the managed worktrees directory and the slots' Git directories; project = the whole project root (default: the project's configuration.sandbox.coordinator_writable, else worktrees)
    #[arg(long, value_name = "SCOPE", value_parser = ["worktrees", "project"], requires = "project_box")]
    box_writable: Option<String>,
    /// With --project-box: userns, root, or cgroup, as for --slot-isolation; root supports claude and codex only (default: the project's configuration.sandbox.isolation, else userns)
    #[arg(long, value_name = "MODE", value_parser = ["userns", "cgroup", "root"], requires = "project_box")]
    box_isolation: Option<String>,
    /// With --project-box: wrkslots project root (default: --cwd, searched upward for .wrkslots.yml)
    #[arg(long, value_name = "DIR", requires = "project_box")]
    box_project: Option<PathBuf>,
    /// With --project-box: box name, which names its slice and its persistent private $HOME layer (default: the agent name)
    #[arg(long, value_name = "NAME", requires = "project_box")]
    box_name: Option<String>,
    /// Seconds to wait for harness startup, greater than zero and at most 300
    #[arg(long, default_value = "30", value_parser = startup_seconds)]
    startup_timeout: f64,
    #[command(flatten)]
    delivery: Delivery,
}

#[derive(Args)]
struct Profiles {
    /// Project directory containing .agentctl/profiles.json; when it has none, the profiles.json
    /// beside a registry named .agentctl is listed
    #[arg(long, default_value = ".", value_name = "DIR")]
    cwd: PathBuf,
}

#[derive(Args)]
struct Skill {
    #[command(subcommand)]
    command: SkillCommand,
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Install for Codex, Claude, and Muse
    Install(SkillInstall),
}

#[derive(Args)]
struct SkillInstall {
    /// Target harness; repeat; omission installs all supported harnesses
    #[arg(long, value_parser = ["codex", "claude", "muse"])]
    harness: Vec<String>,
    /// Replace divergent regular content; symlinks are always refused
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct Adopt {
    #[command(flatten)]
    agent: Named,
    /// Exact live Herdr pane containing the agent (required)
    #[arg(long, value_name = "ID")]
    pane: String,
    /// Expected live Herdr workspace label; a mismatch is refused (required)
    #[arg(long, value_name = "LABEL")]
    workspace: String,
    /// Expected live agent working directory; compared canonically (required)
    #[arg(long, value_name = "DIR")]
    cwd: PathBuf,
    /// Expected live Herdr harness kind, such as codex, claude, or muse (required)
    #[arg(long, value_name = "KIND")]
    harness: String,
    /// Stable native conversation ID already reported by this exact pane
    #[arg(long, value_name = "ID")]
    session: Option<String>,
}

#[derive(Args)]
struct Prompt {
    /// Prompt text; use --file for a multiline or large prompt
    #[arg(conflicts_with = "file")]
    text: Option<String>,
    /// UTF-8 file containing prompt text
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,
}
impl Prompt {
    fn read(&self) -> Result<Option<String>, String> {
        let text = self.file.as_ref().map_or_else(
            || Ok(self.text.clone()),
            |path| {
                fs::read_to_string(path)
                    .map(Some)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))
            },
        )?;
        if text.as_deref().is_some_and(|text| text.trim().is_empty()) {
            return Err("instruction must not be empty".to_owned());
        }
        Ok(text)
    }

    fn read_bounded(&self, maximum: usize, label: &str) -> Result<Option<String>, String> {
        let text = match self.file.as_ref() {
            Some(path) => {
                let file = fs::File::open(path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let limit = u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1);
                let mut bytes = Vec::with_capacity(maximum.saturating_add(1));
                file.take(limit)
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                if bytes.len() > maximum {
                    return Err(format!("{label} exceeds {maximum} UTF-8 bytes"));
                }
                Some(
                    String::from_utf8(bytes)
                        .map_err(|_| format!("{} does not contain valid UTF-8", path.display()))?,
                )
            }
            None => self.text.clone(),
        };
        if text.as_deref().is_some_and(|text| text.trim().is_empty()) {
            return Err(format!("{label} must not be empty"));
        }
        if text.as_ref().is_some_and(|text| text.len() > maximum) {
            return Err(format!("{label} exceeds {maximum} UTF-8 bytes"));
        }
        Ok(text)
    }
}

#[derive(Args)]
struct Send {
    #[command(flatten)]
    agent: Named,
    #[command(flatten)]
    prompt: Prompt,
    /// Stable caller-supplied ID; an ID already present in any delivery state is refused
    #[arg(long, value_name = "ID")]
    message_id: Option<String>,
    /// Model override for a headless turn (requires an installation with the worker extension)
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
    #[command(flatten)]
    delivery: Delivery,
}
#[derive(Args)]
struct Drain {
    #[command(flatten)]
    agent: Named,
    #[command(flatten)]
    delivery: Delivery,
}
#[derive(Args)]
struct Read {
    #[command(flatten)]
    agent: Named,
    /// Maximum terminal lines to read
    #[arg(long, default_value = "500", value_parser = clap::value_parser!(u32).range(1..=1_000_000))]
    lines: u32,
    /// Output boundary; last reads an agentcloud agent's final reply or a headless transcript; since_turn requires headless transcripts
    #[arg(long, default_value = "tail", value_parser = ["tail", "all", "last", "since_turn"])]
    output: String,
    /// First included headless turn; requires the worker extension and --output since_turn
    #[arg(long, value_name = "NUMBER")]
    since_turn: Option<u64>,
}
#[derive(Args)]
struct Wait {
    #[command(flatten)]
    agent: Named,
    /// Seconds to wait for readiness (0 probes immediately)
    #[arg(long, default_value = "900", value_parser = seconds)]
    timeout: f64,
}
#[derive(Args)]
struct Goal {
    #[command(flatten)]
    agent: Named,
    #[command(flatten)]
    prompt: Prompt,
    /// JSON argv array for a native Codex goal-reader command
    #[arg(long, value_name = "JSON", value_parser = command_json)]
    goal_command_json: Option<GoalCommand>,
    #[command(flatten)]
    delivery: Delivery,
}
#[derive(Args)]
struct BindSession {
    #[command(flatten)]
    agent: Named,
    /// Native conversation ID verified by the operator
    session: String,
    /// JSON argv array for a native Codex goal-reader command
    #[arg(long, value_name = "JSON", value_parser = command_json)]
    goal_command_json: Option<GoalCommand>,
}

#[derive(Args)]
struct Chat {
    #[command(subcommand)]
    command: ChatCommand,
}

#[derive(Subcommand)]
enum ChatCommand {
    /// Print the shortest safe setup for the event-driven chat bridge
    Quickstart,
    /// Print the chat bridge configuration and operations guide
    Userguide,
    /// Validate plugin, target, helper, and config authority, then create state
    Init(ChatInit),
    /// Inspect durable bridge state without contacting Herdr or a provider
    Status(ChatState),
    /// Approve one unchanged checkpoint-only gap retry while the service is stopped
    RetryCheckpointGap(ChatRetryGap),
    /// Approve an exact committed-boundary replay, including messages, with the runner stopped
    #[command(
        after_help = "Example: agentctl chat retry-boundary-gap --bridge-state /home/me/chat-state --expected-gap-sha256 <SHA256> --expected-checkpoint-sha256 <SHA256> --expected-configuration-sha256 <SHA256> --keep-cursor <CURSOR> --evidence-file /home/me/private/evidence.json --evidence-sha256 <SHA256>\n\nThis preserves the cursor and unresolved gap. Recovery requires a newer exact provider commit; changed message payloads or a replacement checkpoint are refused."
    )]
    RetryBoundaryGap(ChatRetryGap),
    /// Inspect one exact active retained request without writes or external access
    Inspect(ChatInspect),
    /// Print one thread's retained messages as quoted text, oldest first, without writes
    #[command(
        after_help = "Example:\n  agentctl chat thread --bridge-state ~/.local/state/agentctl/chat \\\n    --thread spaces/example/threads/one --last 20\n\nA request prompt for a reply in an existing thread prints this command with the service's own\nexecutable, its exact state directory, and the thread. The output lists the thread's retained requests and the replies captured\nfor them, each with its UTC time and age. It drops the line breaks at the end of each request or\nreply and prefixes every line of message text with `> `, or an empty line with `>` alone.\nThe bridge retains only requests it admitted from allowed senders that have not been retired, so\nthe provider's own thread remains the complete record. It reads under the shared state lock\nand never writes state or contacts Herdr, a helper, or a provider."
    )]
    Thread(ChatThread),
    /// List the replies the bridge holds, newest first: what was sent, when, and as which message
    #[command(
        after_help = "Examples:\n  agentctl chat sent --bridge-state ~/.local/state/agentctl/chat\n  agentctl chat sent --bridge-state ~/.local/state/agentctl/chat --request <KEY> --json\n\nEach reply shows when it was sent (or captured, while unsent, or for a record kept before send\ntimes were recorded, which it says) in UTC and how long ago, its\nnumber within its request, the request key, the chat message and thread it answers, and its\noutcome: `sent as <provider message ID>` once the chat provider accepted it, `captured, not yet\nsent`, or `send in progress or outcome unknown`, which the service retries with the same\noperation ID so it is never posted twice. The first 300 characters of each reply follow, every\nline prefixed with `> `. --json prints one document (schema agentctl-chat-sent/v1) with the\nsame fields, times as Unix milliseconds, and each reply's whole text.\n\nThe bridge holds only the replies of requests that are not retired, so the chat thread itself\nis the complete record. It reads under the shared state lock and never writes state or\ncontacts Herdr, a helper, or a provider."
    )]
    Sent(ChatSent),
    /// Publish one explicit operator root message through the configured helper
    Publish(ChatPublish),
    /// Run one bounded delivery, terminal-capture, and outbound recovery pass
    Tick(ChatOperate),
    /// Run event-driven provider and Herdr subscriptions until SIGINT or SIGTERM
    Run(ChatRun),
    /// Store one reply for an open request from a file, and wake a running service to send it.
    /// Normally an agent replies between the prompt's two marker lines instead
    #[command(
        after_help = "Example:\n  agentctl chat reply --bridge-state ~/.local/state/agentctl/chat \\\n    --request <KEY> --reply-id 001 --file reply.md\n\nUse this command only when the two marker lines cannot do the job. A reply printed between\nthe request's two marker lines is the normal way: it reaches the user in the chat and also stays\nin the agent's terminal, where the owner may be reading. Use the command to send a reply before\nthe agent's turn ends, such as a progress update during long work, or to send again a reply that\nwas written between the lines but did not reach the chat: one reported as not sent, or one that\n`agentctl chat sent` does not list, as when the screen was redrawn before the service read it.\nSend each reply one way only. A copy printed in the terminal without the marker lines sends\nnothing, so it is a safe way to show a reply sent by the command.\n\nWhen `chat run` has --offer-reply-command and outbound replies are enabled, each request prompt\nprints this command with the service's own executable, the absolute path of its state\ndirectory, the request key, and the reply ID, unless no file is left at that executable's path\nor one of those words cannot be printed safely; then the prompt gives only the two marker\nlines.\n\nThe command stores the file's text as a reply of that request, as if the service had read it\nbetween the reply ID's two marker lines on the agent's screen, and the service sends it like\nany reply it reads. Line breaks at the end of the file are dropped. A text the request already\nholds is not stored again, so running the command twice sends it once. It then asks a service\nrunning on the same state directory with --offer-reply-command to send the reply at once;\notherwise the reply is sent at the service's next reconciliation, when the agent goes idle, or\nwhen the service starts.\n\nOn success it prints one JSON object: request, reply_id, outcome (stored or already_stored),\nordinal, phase, and service_woken, which says whether the wake was sent to a socket of the\ncurrent user in the state directory; nothing confirms that a service received it. Exit\nstatus: 0 when the request holds the reply; 1 when the reply is refused, with the reason on\nstandard error and nothing stored, as for an empty or blank text or one over 30,000 bytes;\n1 also when the state cannot be read or written, or when the result cannot be printed after\nthe reply was stored; 75 when nothing was stored but the same command can succeed later; 2\nfor a usage error, which includes a file that cannot be read or is not UTF-8. It never\ncontacts Herdr, a helper, or a provider."
    )]
    Reply(ChatReply),
    /// Stop accepting fenced replies for one exact retained request
    Close(ChatClose),
    /// Internal systemd ExecStop helper for one inode-bound main-process pidfd
    #[command(hide = true)]
    GracefulStopMain(ChatGracefulStopMain),
}

#[derive(Args)]
struct ChatState {
    /// Private durable bridge state directory
    #[arg(long, value_name = "DIR")]
    bridge_state: PathBuf,
}

#[derive(Args)]
struct ChatSent {
    #[command(flatten)]
    state: ChatState,
    /// Only this request's replies: its exact 64-character lowercase hexadecimal key
    #[arg(long, value_name = "KEY")]
    request: Option<String>,
    /// Print this many of the most recent replies, newest first (1-100)
    #[arg(
        long,
        value_name = "N",
        default_value_t = crate::chat_runtime::DEFAULT_THREAD_HISTORY_MESSAGES,
        value_parser = clap::value_parser!(u32)
            .range(1..=i64::from(crate::chat_runtime::MAX_THREAD_HISTORY_MESSAGES))
    )]
    last: u32,
    /// Print one JSON document with each reply's whole text instead of the text listing
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ChatInit {
    #[command(flatten)]
    state: ChatState,
    /// Private owner-only JSON bridge configuration (maximum 1 MiB)
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
}

#[derive(Args)]
struct ChatInspect {
    #[command(flatten)]
    state: ChatState,
    /// Exact 64-character lowercase hexadecimal request key
    #[arg(long)]
    request: String,
}

#[derive(Args)]
struct ChatThread {
    #[command(flatten)]
    state: ChatState,
    /// Exact provider thread ID; the command a request prompt prints already quotes it for the shell
    #[arg(long, value_name = "THREAD", allow_hyphen_values = true)]
    thread: String,
    /// Print this many of the thread's most recent retained messages, oldest first (1-100)
    #[arg(
        long,
        value_name = "N",
        default_value_t = crate::chat_runtime::DEFAULT_THREAD_HISTORY_MESSAGES,
        value_parser = clap::value_parser!(u32)
            .range(1..=i64::from(crate::chat_runtime::MAX_THREAD_HISTORY_MESSAGES))
    )]
    last: u32,
}

#[derive(Args)]
struct ChatRetryGap {
    #[command(flatten)]
    state: ChatState,
    /// SHA256 of the reviewed unresolved gap.json bytes
    #[arg(long, value_name = "SHA256")]
    expected_gap_sha256: String,
    /// SHA256 of the reviewed checkpoint.json bytes
    #[arg(long, value_name = "SHA256")]
    expected_checkpoint_sha256: String,
    /// SHA256 of the reviewed bridge.json configuration bytes
    #[arg(long, value_name = "SHA256")]
    expected_configuration_sha256: String,
    /// Exact existing opaque cursor to preserve; this command cannot advance it
    #[arg(long, value_name = "CURSOR")]
    keep_cursor: String,
    /// Private JSON object of operator-reviewed provider evidence (maximum 64 KiB)
    #[arg(long, value_name = "FILE")]
    evidence_file: PathBuf,
    /// SHA256 of the exact reviewed evidence file bytes
    #[arg(long, value_name = "SHA256")]
    evidence_sha256: String,
}

#[derive(Args)]
struct ChatPublish {
    #[command(flatten)]
    state: ChatState,
    /// Exact configured provider channel or space authority
    #[arg(long, value_name = "CHANNEL")]
    channel_id: String,
    /// Caller-owned lowercase RFC 4122 version-4 idempotency UUID
    #[arg(long, value_name = "UUID", value_parser = operation_uuid)]
    request_id: String,
    /// Message text; use --file for multiline input
    #[arg(required_unless_present = "file", conflicts_with = "file")]
    text: Option<String>,
    /// UTF-8 file containing the message, read with a 30,000-byte bound
    #[arg(long, value_name = "PATH", required_unless_present = "text")]
    file: Option<PathBuf>,
}

impl ChatPublish {
    fn read_message(&self) -> Result<String, String> {
        Prompt {
            text: self.text.clone(),
            file: self.file.clone(),
        }
        .read_bounded(MAX_CHAT_PUBLISH_BYTES, "chat publish message")?
        .ok_or_else(|| "chat publish requires message text or --file".to_owned())
    }
}

#[derive(Args)]
struct ChatReply {
    #[command(flatten)]
    state: ChatState,
    /// Exact 64-character lowercase hexadecimal request key
    #[arg(long)]
    request: String,
    /// Reply ID that the request's prompt gives in its two marker lines
    #[arg(long, value_name = "ID", allow_hyphen_values = true)]
    reply_id: String,
    /// UTF-8 file containing the reply, read with a 30,000-byte bound
    #[arg(long, value_name = "PATH")]
    file: PathBuf,
}

impl ChatReply {
    /// The reply file's text. A file that cannot be read or is not UTF-8 is a usage error. A text
    /// over the bound is refused like any reply the service refuses, and so is an empty or blank
    /// one, which the store refuses.
    fn read_reply(&self) -> Result<String, Failure> {
        let maximum = crate::chat_runtime::MAX_REPLY_BYTES;
        let unreadable = |error: io::Error| {
            Failure::Usage(format!("cannot read {}: {error}", self.file.display()))
        };
        let file = fs::File::open(&self.file).map_err(unreadable)?;
        let limit = u64::try_from(maximum).unwrap_or(u64::MAX).saturating_add(1);
        let mut bytes = Vec::with_capacity(maximum.saturating_add(1));
        file.take(limit)
            .read_to_end(&mut bytes)
            .map_err(unreadable)?;
        if bytes.len() > maximum {
            return Err(Failure::Chat(
                crate::chat_service::ChatServiceError::Runtime(
                    crate::chat_runtime::ChatRuntimeError::Invalid(format!(
                        "chat reply exceeds {maximum} UTF-8 bytes"
                    )),
                ),
            ));
        }
        String::from_utf8(bytes).map_err(|_| {
            Failure::Usage(format!(
                "{} does not contain valid UTF-8",
                self.file.display()
            ))
        })
    }
}

#[derive(Args)]
struct ChatRun {
    #[command(flatten)]
    operate: ChatOperate,
    /// Ignore new messages starting with PREFIX after leading whitespace (case-sensitive; repeat up to 32 times; default: none). Prefixes must be nonempty, at most 256 UTF-8 bytes, without control characters; applies only to this run.
    #[arg(long = "ignore-text-prefix", value_name = "PREFIX", value_parser = chat_ignored_text_prefix)]
    ignored_text_prefixes: Vec<String>,
    /// Print an `agentctl chat reply` command in each request prompt that gives the two reply marker lines, as a way to send a reply at once, and listen for its wakes on the state directory's .wake.sock (default: off). A prompt leaves the command out when no file is left at this executable's path or a word of the command cannot be printed safely, and the service does not listen when the socket's absolute path exceeds 107 bytes. The agent must be able to run this executable and write the state directory; applies only to this run.
    #[arg(long)]
    offer_reply_command: bool,
    /// Also type a request into the coordinator's pane while Herdr reports it `working`, when the pane runs Claude Code, and let Claude Code queue it behind the current turn (default: off, which waits for `idle` or `done`). The composer must be empty first, and the submission must be verified: the prompt leaves the composer, with Claude Code's queued-message hint or the prompt shown, and an Enter it drops is pressed again with a doubling wait. A prompt it never accepts is taken back out of the composer and stays pending. Slash commands and other harnesses still wait; applies only to this run.
    #[arg(long)]
    deliver_while_working: bool,
    /// Clear text left unsubmitted in the coordinator's composer once it has blocked a request and stayed unchanged for DURATION, such as 30m (default: off). Units: s, m or h; a bare number is seconds; at least 1m. The text is first saved with mode 0600 under the state directory's composer-backups/, then deleted with editing keys and the composer proved empty, and then posted once to the first configured channel through the outbound helper with a "[chat-bridge WARNING]" notice; delivery then resumes. Any change to the text restarts the wait, nothing is cleared while the agent works, and a failed clear leaves the text and posts nothing. Requires an outbound helper; applies only to this run.
    #[arg(long, value_name = "DURATION", value_parser = stale_composer_duration)]
    stale_composer_clear_after: Option<Duration>,
}

#[derive(Args)]
struct ChatOperate {
    #[command(flatten)]
    state: ChatState,
    /// Seconds to wait for coordinator readiness; zero keeps daemon passes nonblocking
    #[arg(long, default_value = "0", value_parser = chat_delivery_seconds)]
    ready_timeout: f64,
    /// Positive seconds to wait for evidence a delivered prompt started work
    #[arg(long, default_value = "5", value_parser = positive_chat_delivery_seconds)]
    working_timeout: f64,
    /// Maximum safe terminal-injection attempts for one request
    #[arg(long, default_value = "1", value_parser = clap::value_parser!(u64).range(1..=1_000_000))]
    max_attempts: u64,
    /// Seconds between disk-backed recovery snapshots; provider intake remains event-driven
    #[arg(long, default_value = "300", value_parser = positive_seconds)]
    reconcile_interval: f64,
}

impl ChatOperate {
    fn options(&self) -> crate::chat_service::ServiceOptions {
        crate::chat_service::ServiceOptions {
            delivery: DrainOptions {
                ready_timeout: Duration::from_secs_f64(self.ready_timeout),
                working_timeout: Duration::from_secs_f64(self.working_timeout),
                max_attempts: self.max_attempts,
                // `chat run` sets this from its own flag.
                deliver_while_working: false,
            },
            reconciliation_interval: Duration::from_secs_f64(self.reconcile_interval),
            timing: crate::chat_service::DeliveryTiming::default(),
        }
    }
}

#[derive(Args)]
struct ChatClose {
    #[command(flatten)]
    state: ChatState,
    /// Exact 64-character lowercase hexadecimal request key
    #[arg(long)]
    request: String,
}

#[derive(Args)]
struct ChatGracefulStopMain {
    /// Exact systemd MAINPID value
    #[arg(long)]
    main_pid: u32,
    /// Exact systemd MAINPIDFDID inode value
    #[arg(long)]
    main_pidfd_id: u64,
}

fn seconds(value: &str) -> Result<f64, String> {
    let value: f64 = value.parse().map_err(|_| "expected a number of seconds")?;
    if !value.is_finite() || !(0.0..=31_536_000.0).contains(&value) {
        return Err("seconds must be finite and between 0 and 31536000".to_owned());
    }
    Ok(value)
}
fn positive_seconds(value: &str) -> Result<f64, String> {
    let value = seconds(value)?;
    if value == 0.0 {
        return Err("seconds must be greater than zero".to_owned());
    }
    Ok(value)
}
fn startup_seconds(value: &str) -> Result<f64, String> {
    let value = seconds(value)?;
    if value <= 0.0 || value > 300.0 {
        return Err("startup seconds must be greater than zero and at most 300".to_owned());
    }
    Ok(value)
}
fn chat_delivery_seconds(value: &str) -> Result<f64, String> {
    let value = seconds(value)?;
    if value > 30.0 {
        return Err("chat delivery seconds must be at most 30".to_owned());
    }
    Ok(value)
}
fn positive_chat_delivery_seconds(value: &str) -> Result<f64, String> {
    let value = chat_delivery_seconds(value)?;
    if value == 0.0 {
        return Err("chat delivery seconds must be greater than zero".to_owned());
    }
    Ok(value)
}
fn environment_value(value: &str) -> Result<String, String> {
    environment_entries(&[value.to_owned()])
        .map(|entries| entries.into_iter().next().expect("one validated entry"))
        .map_err(|error| error.to_string())
}
fn stale_composer_duration(value: &str) -> Result<Duration, String> {
    crate::stale_composer::parse_duration(value)
}
fn chat_ignored_text_prefix(value: &str) -> Result<String, String> {
    crate::chat_runtime::validate_ignored_text_prefix(value)
        .map(|()| value.to_owned())
        .map_err(|error| error.to_string())
}
fn operation_uuid(value: &str) -> Result<String, String> {
    crate::chat_runtime::validate_operation_uuid(value)
        .map(|()| value.to_owned())
        .map_err(|error| error.to_string())
}
#[derive(Clone, Debug)]
struct GoalCommand(Vec<String>);
fn command_json(value: &str) -> Result<GoalCommand, String> {
    let command: Vec<String> = serde_json::from_str(value)
        .map_err(|error| format!("expected a JSON argv array: {error}"))?;
    if command.is_empty()
        || command
            .iter()
            .any(|value| value.is_empty() || value.contains('\0'))
    {
        return Err("command must contain nonempty arguments without NUL".to_owned());
    }
    Ok(GoalCommand(command))
}

/// Run the canonical CLI with arguments excluding the executable name.
pub fn main<I: IntoIterator<Item = OsString>>(arguments: I) -> i32 {
    main_with_environment(arguments, &|name| std::env::var(name).ok())
}

/// Run the CLI reading the agentcloud context variables through `environment`, so tests can
/// model running inside or outside an agentcloud session without mutating process state.
pub(crate) fn main_with_environment<I: IntoIterator<Item = OsString>>(
    arguments: I,
    environment: &dyn Fn(&str) -> Option<String>,
) -> i32 {
    let arguments: Vec<OsString> = std::iter::once(OsString::from("agentctl"))
        .chain(arguments)
        .collect();
    let args = match Cli::try_parse_from(arguments.iter().cloned()) {
        Ok(args) => args,
        Err(error) => {
            let code = error.exit_code();
            let _ = error.print();
            if code != 0 {
                if let Some(context) = partial_stop_context(&arguments) {
                    print_stop_recovery(&context, &RecoveryAction::Doctor);
                }
            }
            return code;
        }
    };
    let stop_context = matches!(&args.command, Some(Commands::Stop(_)))
        .then(|| StopContext::new(&args.registry, &args.herdr_bin));
    let service_log = args.writes_service_log();
    match run(args, environment) {
        Ok(code) => code,
        Err(Failure::Agent(error)) => emit_agent_failure(
            service_log,
            &error,
            stop_context.as_ref(),
            &RecoveryAction::Doctor,
        ),
        Err(Failure::Stop(failure)) => emit_agent_failure(
            service_log,
            &failure.error,
            stop_context.as_ref(),
            &failure.recovery,
        ),
        Err(Failure::Usage(error)) => {
            if let Some(context) = &stop_context {
                print_failure(service_log, format_args!("{}", stop_reason(&error)));
                print_stop_recovery(context, &RecoveryAction::Doctor);
            } else {
                print_failure(service_log, format_args!("{error}"));
            }
            2
        }
        Err(Failure::Output(error)) => {
            print_failure(service_log, format_args!("cannot write output: {error}"));
            if let Some(context) = &stop_context {
                print_stop_recovery(context, &RecoveryAction::Doctor);
            }
            1
        }
        Err(Failure::Chat(error)) => {
            print_failure(service_log, format_args!("{error}"));
            if let Some(context) = &stop_context {
                print_stop_recovery(context, &RecoveryAction::Doctor);
            }
            1
        }
        Err(Failure::Inbox(error)) => {
            print_failure(service_log, format_args!("{error}"));
            if let Some(context) = &stop_context {
                print_stop_recovery(context, &RecoveryAction::Doctor);
            }
            error.exit_code()
        }
    }
}

fn partial_stop_context(arguments: &[OsString]) -> Option<StopContext> {
    let matches = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(arguments.iter().cloned())
        .ok()?;
    if matches.subcommand_name() != Some("stop") {
        return None;
    }
    Some(StopContext::new(
        matches
            .get_one::<PathBuf>("registry")
            .map_or(Path::new(".agentctl"), PathBuf::as_path),
        matches
            .get_one::<PathBuf>("herdr_bin")
            .map_or(Path::new("herdr"), PathBuf::as_path),
    ))
}

fn print_stop_recovery(context: &StopContext, recovery: &RecoveryAction) {
    eprintln!("Recovery command: {}", context.command(recovery));
}

fn emit_agent_failure(
    service_log: bool,
    error: &AgentError,
    stop_context: Option<&StopContext>,
    recovery: &RecoveryAction,
) -> i32 {
    if let Some(message) = error.undelivered() {
        if let Err(output) = write_json(
            &json!({"outcome": error.outcome().map(QueueOutcome::as_str), "message_id": message.message_id, "artifact": message.artifact, "error": error.to_string(), "safe_to_retry": error.safe_to_retry()}),
        ) {
            print_failure(service_log, format_args!("{output}"));
            if let Some(context) = stop_context {
                print_stop_recovery(context, recovery);
            }
            return 1;
        }
    }
    if let Some(context) = stop_context {
        print_failure(service_log, format_args!("{}", stop_reason(error)));
        print_stop_recovery(context, recovery);
    } else if error.undelivered().is_none() {
        print_failure(service_log, format_args!("{error}"));
    }
    error.exit_code()
}

impl Cli {
    /// Whether standard error is the chat bridge service log: `chat run` and its stop helper.
    fn writes_service_log(&self) -> bool {
        matches!(
            &self.command,
            Some(Commands::Chat(Chat {
                command: ChatCommand::Run(_) | ChatCommand::GracefulStopMain(_),
            }))
        )
    }
}

/// Print the final error line. In the service log it starts with the UTC time, like every other
/// service log line, so the log shows when the service stopped as well as why.
fn print_failure(service_log: bool, message: std::fmt::Arguments<'_>) {
    if service_log {
        crate::chat_service::service_log(format_args!("agentctl: {message}"));
    } else {
        eprintln!("agentctl: {message}");
    }
}

enum Failure {
    Agent(AgentError),
    Stop(StopFailure),
    Usage(String),
    Output(io::Error),
    Chat(crate::chat_service::ChatServiceError),
    Inbox(crate::inbox::InboxError),
}
impl From<AgentError> for Failure {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}
impl From<StopFailure> for Failure {
    fn from(error: StopFailure) -> Self {
        Self::Stop(error)
    }
}
fn write_json(value: &impl Serialize) -> io::Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    output.write_all(b"\n")
}

fn write_text(text: &str) -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(text.as_bytes())?;
    output.flush()
}

fn plugin_discovery_required(command: &Commands) -> bool {
    matches!(command, Commands::Capabilities)
}

fn run(args: Cli, environment: &dyn Fn(&str) -> Option<String>) -> Result<i32, Failure> {
    if args.userguide {
        print!("{}", crate::USER_GUIDE);
        return Ok(0);
    }
    let Some(command) = args.command else {
        Cli::command().print_help().map_err(Failure::Output)?;
        println!();
        return Ok(0);
    };
    // Directory and manifest I/O is lazy. Session-control commands never inspect the user plugin
    // home; capability inspection and an actual plugin runtime startup own discovery.
    let plugin_inventory = plugin_discovery_required(&command).then(crate::plugins::discover);
    match command {
        Commands::Capabilities => {
            write_json(&capabilities_document(
                args.registry,
                plugin_inventory.expect("capabilities requires plugin discovery"),
            ))
            .map_err(Failure::Output)?;
            return Ok(0);
        }
        Commands::Quickstart => {
            print!("{}", crate::QUICKSTART);
            return Ok(0);
        }
        Commands::Userguide => {
            print!("{}", crate::USER_GUIDE);
            return Ok(0);
        }
        Commands::Chat(value) => {
            return run_chat(args.registry, args.herdr_bin, value);
        }
        Commands::Inbox(value) => {
            return crate::inbox::run(
                &args.registry,
                &args.herdr_bin,
                &args.agentcloudctl_bin,
                value,
            )
            .map_err(Failure::Inbox);
        }
        Commands::Profiles(value) => {
            let root = crate::profiles::configuration_root(&value.cwd, &args.registry, true)?;
            let (path, profiles, workspace) = crate::profiles::load_configuration(&root, true)?;
            write_json(&json!({
                "path": path,
                "workspace": workspace,
                "profiles": profiles.values().map(crate::profiles::LaunchProfile::public).collect::<Vec<_>>()
            }))
            .map_err(Failure::Output)?;
            return Ok(0);
        }
        Commands::Skill(value) => {
            if args.registry != Path::new(".agentctl")
                || args.herdr_bin != Path::new("herdr")
                || args.agentcloudctl_bin != Path::new("agentcloudctl")
                || args.agentterm_bin != Path::new("agentterm")
                || args.from_session.is_some()
                || args.agentcloud_url.is_some()
            {
                return Err(Failure::Usage(
                    "--registry, --herdr-bin, --agentcloudctl-bin, --agentterm-bin, --agentcloud-url, and --from-session do not apply to skill install".to_owned(),
                ));
            }
            let SkillCommand::Install(install) = value.command;
            write_json(&crate::skill_install::install(
                &install.harness,
                install.force,
            )?)
            .map_err(Failure::Output)?;
            return Ok(0);
        }
        Commands::CodexGoal(value) => {
            if args.registry != Path::new(".agentctl")
                || args.herdr_bin != Path::new("herdr")
                || args.agentcloudctl_bin != Path::new("agentcloudctl")
                || args.agentterm_bin != Path::new("agentterm")
                || args.from_session.is_some()
                || args.agentcloud_url.is_some()
            {
                return Err(Failure::Usage(
                    "--registry, --herdr-bin, --agentcloudctl-bin, --agentterm-bin, --agentcloud-url, and --from-session do not apply to codex-goal".to_owned(),
                ));
            }
            return crate::codex_goal::command::run(&value).map_err(Failure::Output);
        }
        _ => {}
    }
    let client =
        HerdrClient::with_executable("direct", &args.herdr_bin).map_err(AgentError::from)?;
    let from_environment = CloudTools::from_environment(environment);
    let manager = ManagedAgents::new(&client, &args.registry)?.with_cloud_tools(CloudTools {
        agentcloudctl: args.agentcloudctl_bin,
        agentterm: args.agentterm_bin,
        caller_session: caller_session(args.from_session, from_environment.caller_session),
        endpoint_explicit: args.agentcloud_url.is_some(),
        endpoint: args.agentcloud_url.or(from_environment.endpoint),
        ambient_session: from_environment.ambient_session,
        hostname: from_environment.hostname,
    });
    let mut result = match command {
        Commands::Start(value) => {
            let cloud_flags = [
                (value.cloud_harness.is_some(), "--cloud-harness"),
                (value.provision, "--provision"),
                (value.envspec.is_some(), "--envspec"),
                (value.purpose.is_some(), "--purpose"),
                (value.cloud_workspace.is_some(), "--cloud-workspace"),
                (value.node_id.is_some(), "--node-id"),
                (value.cloud_title.is_some(), "--cloud-title"),
            ]
            .into_iter()
            .filter_map(|(present, flag)| present.then_some(flag))
            .collect::<Vec<_>>();
            let (harness, mode, model, reasoning_effort, harness_args, environment, cloud) =
                if let Some(profile_name) = value.profile.as_deref() {
                    let mut overlaps = Vec::new();
                    if value.mode.is_some() {
                        overlaps.push("--mode");
                    }
                    if value.harness.is_some() {
                        overlaps.push("--harness");
                    }
                    if value.model.is_some() {
                        overlaps.push("--model");
                    }
                    if value.reasoning_effort.is_some() {
                        overlaps.push("--reasoning-effort");
                    }
                    if !value.harness_args.is_empty() {
                        overlaps.push("--harness-arg");
                    }
                    if !value.environment.is_empty() {
                        overlaps.push("--env");
                    }
                    overlaps.extend_from_slice(&cloud_flags);
                    if !overlaps.is_empty() {
                        return Err(Failure::Usage(format!(
                            "--profile conflicts with explicit launch settings: {}",
                            overlaps.join(", ")
                        )));
                    }
                    let root =
                        crate::profiles::configuration_root(&value.cwd, &args.registry, false)?;
                    let (_, profiles) = crate::profiles::load_profiles(&root, false)?;
                    let profile = profiles.get(profile_name).ok_or_else(|| {
                        Failure::Usage(format!(
                            "unknown profile {profile_name:?}; run agentctl profiles --cwd {}",
                            value.cwd.display()
                        ))
                    })?;
                    (
                        profile.harness.clone(),
                        profile.mode.clone(),
                        profile.model.clone(),
                        profile.reasoning_effort.clone(),
                        profile.argv.clone(),
                        profile.environment.clone(),
                        profile.cloud.clone(),
                    )
                } else {
                    let harness = value.harness.unwrap_or_else(|| "codex".to_owned());
                    let cloud = if harness == "agentcloud" {
                        Some(CloudLaunch {
                            harness: value.cloud_harness,
                            provision: value.provision,
                            envspec: value.envspec,
                            purpose: value.purpose,
                            workspace: value.cloud_workspace,
                            node_id: value.node_id,
                            title: value.cloud_title,
                        })
                    } else {
                        if !cloud_flags.is_empty() {
                            return Err(Failure::Usage(format!(
                                "{} apply only with --harness agentcloud",
                                cloud_flags.join(", ")
                            )));
                        }
                        crate::profiles::validate_raw_harness_arguments(
                            "launch",
                            &harness,
                            &value.harness_args,
                            value.model.is_some(),
                            value.reasoning_effort.is_some(),
                            value.resume.is_some(),
                        )?;
                        None
                    };
                    (
                        harness,
                        value.mode.unwrap_or_else(|| "interactive".to_owned()),
                        value.model,
                        value.reasoning_effort,
                        value.harness_args,
                        value.environment,
                        cloud,
                    )
                };
            if mode != "interactive" || value.backend != "herdr" {
                return Err(Failure::Usage("Rust agentctl supports interactive Herdr sessions; use agentctl with the worker extension for headless workers".to_owned()));
            }
            let brief = match value.file {
                Some(path) => Some(
                    fs::read_to_string(path).map_err(|error| Failure::Usage(error.to_string()))?,
                ),
                None => value.brief,
            };
            let slot = if value.project_box {
                Some(SlotLaunch {
                    slot: value
                        .box_name
                        .clone()
                        .unwrap_or_else(|| value.agent.name.clone()),
                    isolation: value.box_isolation.clone(),
                    project: value.box_project.clone(),
                    project_box: true,
                    box_writable: value.box_writable.clone(),
                    ..SlotLaunch::default()
                })
            } else {
                value.slot.clone().map(|slot| SlotLaunch {
                    slot,
                    isolation: value.slot_isolation.clone(),
                    project: value.slot_project.clone(),
                    ..SlotLaunch::default()
                })
            };
            let options = StartOptions {
                workspace_id: value.workspace_id,
                profile: value.profile,
                harness,
                model,
                resume: value.resume,
                harness_args,
                environment,
                brief,
                startup_timeout: Duration::from_secs_f64(value.startup_timeout),
                delivery: value.delivery.options(),
                cloud,
                slot,
            };
            if let Some(effort) = reasoning_effort.as_deref() {
                manager.start_with_reasoning_effort(
                    &value.agent.name,
                    &value.cwd,
                    effort,
                    options,
                )?
            } else {
                manager.start(&value.agent.name, &value.cwd, options)?
            }
        }
        Commands::Adopt(value) => manager.adopt(
            &value.agent.name,
            AdoptOptions {
                pane_id: value.pane,
                expected_workspace: value.workspace,
                cwd: value.cwd,
                harness: value.harness,
                session: value.session,
            },
        )?,
        Commands::Move(value) => manager.move_to_project_workspace(&value.name)?,
        Commands::Rename(value) => manager.rename(&value.agent.name, &value.new_name)?,
        Commands::Anchor(value) => manager.anchor(&value.agent.name, value.replace)?,
        Commands::Doctor(value) => {
            let report = manager.doctor(value.repair_labels)?;
            write_json(&report).map_err(Failure::Output)?;
            return Ok(if report["clean"] == true { 0 } else { 1 });
        }
        Commands::Stop(value) => {
            let recovery_selector = if value.retire_dead_adoption {
                Some("--retire-dead-adoption")
            } else if value.recover_legacy_adoption {
                Some("--recover-legacy-adoption")
            } else {
                None
            };
            if let Some(selector) = recovery_selector.filter(|_| {
                value.expected_token.is_none() || value.expected_record_sha256.is_none()
            }) {
                return Err(Failure::Usage(format!(
                    "{selector} requires --expected-token and --expected-record-sha256"
                )));
            }
            if recovery_selector.is_none() && value.expected_record_sha256.is_some() {
                return Err(Failure::Usage(
                    "--expected-record-sha256 requires --recover-legacy-adoption or --retire-dead-adoption".to_owned(),
                ));
            }
            manager.advised_stop_with_options(
                &value.agent.name,
                StopOptions {
                    expected_token: value.expected_token,
                    recover_legacy_adoption: value.recover_legacy_adoption,
                    retire_dead_adoption: value.retire_dead_adoption,
                    expected_record_sha256: value.expected_record_sha256,
                    skip_cloud_halt: value.skip_cloud_halt,
                },
            )?
        }
        Commands::Revive(value) => {
            let timeout = Duration::from_secs_f64(value.startup_timeout);
            let mut result = if value.all {
                manager.revive_all(value.dry_run, timeout)?
            } else {
                manager.revive(
                    value.name.as_deref().expect("clap requires NAME or --all"),
                    value.dry_run,
                    value.expected_token.as_deref(),
                    timeout,
                )?
            };
            add_capabilities(&mut result);
            if let Some(agents) = result.get_mut("agents") {
                add_capabilities(agents);
            }
            let code =
                if !value.dry_run && result["blocked"].as_u64().is_some_and(|count| count > 0) {
                    75
                } else {
                    0
                };
            write_json(&result).map_err(Failure::Output)?;
            return Ok(code);
        }
        Commands::List => json!(manager.list()?),
        Commands::Status(value) => manager.status(&value.name)?,
        Commands::Send(value) => {
            if value.model.is_some() {
                return Err(Failure::Usage(
                    "--model on send requires a headless worker and an installation with the worker extension"
                        .to_owned(),
                ));
            }
            let text = value
                .prompt
                .read()
                .map_err(Failure::Usage)?
                .ok_or_else(|| Failure::Usage("send requires prompt text or --file".to_owned()))?;
            if manager.is_cloud(&value.agent.name)? {
                manager.send_cloud(&value.agent.name, &text, value.message_id.as_deref())?
            } else {
                json!(manager.send_identified(
                    &value.agent.name,
                    &text,
                    value.delivery.options(),
                    value.message_id.as_deref()
                )?)
            }
        }
        Commands::Drain(value) => {
            let result = manager.drain(&value.agent.name, value.delivery.options())?;
            write_json(&result).map_err(Failure::Output)?;
            return Ok(if result.blocked.is_some() {
                75
            } else if result.quarantined.is_empty() {
                0
            } else {
                76
            });
        }
        Commands::Read(value) => {
            if manager.is_cloud(&value.agent.name)? {
                if value.since_turn.is_some() || value.output == "since_turn" {
                    return Err(Failure::Usage("agentcloud agents support --output last, tail, or all; since_turn requires a headless transcript".to_owned()));
                }
                print!(
                    "{}",
                    manager.read_cloud(&value.agent.name, value.lines as usize, &value.output)?
                );
                return Ok(0);
            }
            if !matches!(value.output.as_str(), "tail" | "all") || value.since_turn.is_some() {
                return Err(Failure::Usage("interactive agents expose terminal snapshots; last/since_turn require headless transcripts".to_owned()));
            }
            print!("{}", manager.read(&value.agent.name, value.lines as usize)?);
            return Ok(0);
        }
        Commands::Wait(value) => {
            manager.wait(&value.agent.name, Duration::from_secs_f64(value.timeout))?
        }
        Commands::Goal(value) => manager.goal(
            &value.agent.name,
            value.prompt.read().map_err(Failure::Usage)?.as_deref(),
            value.delivery.options(),
            value
                .goal_command_json
                .as_ref()
                .map(|command| command.0.as_slice()),
        )?,
        Commands::BindSession(value) => manager.bind_session(
            &value.agent.name,
            &value.session,
            value
                .goal_command_json
                .as_ref()
                .map(|command| command.0.as_slice()),
        )?,
        Commands::Attach(value) => manager.attach(&value.name)?,
        Commands::AttachSelf(value) => manager.attach_self(
            &value.agent.name,
            &value.cwd,
            StartOptions {
                workspace_id: value.workspace_id,
                harness: "agentcloud".to_owned(),
                startup_timeout: Duration::from_secs_f64(value.startup_timeout),
                ..StartOptions::default()
            },
        )?,
        Commands::Pause(value) => manager.pause(&value.name, true)?,
        Commands::Resume(value) => manager.pause(&value.name, false)?,
        Commands::Capabilities
        | Commands::Quickstart
        | Commands::Userguide
        | Commands::Chat(_)
        | Commands::Inbox(_)
        | Commands::Profiles(_)
        | Commands::Skill(_)
        | Commands::CodexGoal(_) => {
            unreachable!()
        }
    };
    add_capabilities(&mut result);
    write_json(&result).map_err(Failure::Output)?;
    // `list` prints every row it could read; an unreadable row makes the listing incomplete.
    let incomplete = result.as_array().is_some_and(|rows| {
        rows.iter()
            .any(|row| row.get("record_error").and_then(Value::as_bool) == Some(true))
    });
    Ok(i32::from(incomplete))
}

/// The agentcloud session input is sent from: `--from-session`, else `$AGENTCLOUD_SESSION_ID`.
/// An explicit empty value selects human attribution; the send path refuses it when the process
/// runs inside a different agentcloud session than the recipient's.
fn caller_session(flag: Option<String>, environment: Option<String>) -> Option<String> {
    match flag {
        Some(session) if session.is_empty() => None,
        Some(session) => Some(session),
        None => environment.filter(|session| !session.is_empty()),
    }
}

fn run_chat(registry: PathBuf, herdr_bin: PathBuf, chat: Chat) -> Result<i32, Failure> {
    match chat.command {
        ChatCommand::Quickstart => {
            print!("{}", crate::CHAT_QUICKSTART);
            Ok(0)
        }
        ChatCommand::Userguide => {
            print!("{}", crate::CHAT_USER_GUIDE);
            Ok(0)
        }
        ChatCommand::Status(value) => {
            let result = crate::chat_service::status(&value.bridge_state).map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::RetryCheckpointGap(value) => {
            let result = crate::chat_service::retry_checkpoint_gap(
                &value.state.bridge_state,
                &crate::chat_runtime::CheckpointGapRetryApproval {
                    expected_gap_sha256: &value.expected_gap_sha256,
                    expected_checkpoint_sha256: &value.expected_checkpoint_sha256,
                    expected_configuration_sha256: &value.expected_configuration_sha256,
                    keep_cursor: &value.keep_cursor,
                    evidence_path: &value.evidence_file,
                    evidence_sha256: &value.evidence_sha256,
                },
            )
            .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::RetryBoundaryGap(value) => {
            let result = crate::chat_service::retry_boundary_gap(
                &value.state.bridge_state,
                &crate::chat_runtime::GapRetryApproval {
                    expected_gap_sha256: &value.expected_gap_sha256,
                    expected_checkpoint_sha256: &value.expected_checkpoint_sha256,
                    expected_configuration_sha256: &value.expected_configuration_sha256,
                    keep_cursor: &value.keep_cursor,
                    evidence_path: &value.evidence_file,
                    evidence_sha256: &value.evidence_sha256,
                },
            )
            .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Inspect(value) => {
            let result =
                crate::chat_service::inspect_request(&value.state.bridge_state, &value.request)
                    .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Thread(value) => {
            let history = crate::chat_service::thread_history(
                &value.state.bridge_state,
                &value.thread,
                value.last,
            )
            .map_err(Failure::Chat)?;
            write_text(&history).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Sent(value) => {
            let listing = crate::chat_service::sent_replies(
                &value.state.bridge_state,
                value.request.as_deref(),
                value.last,
                value.json,
            )
            .map_err(Failure::Chat)?;
            write_text(&listing).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Publish(value) => {
            let body = value.read_message().map_err(Failure::Usage)?;
            let result = crate::chat_service::publish(
                &value.state.bridge_state,
                &value.channel_id,
                &value.request_id,
                &body,
            )
            .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Reply(value) => {
            let body = value.read_reply()?;
            match crate::chat_service::reply(
                &value.state.bridge_state,
                &value.request,
                &value.reply_id,
                &body,
            )
            .map_err(Failure::Chat)?
            {
                crate::chat_service::ReplyCommandOutcome::Stored(result) => {
                    write_json(&result).map_err(Failure::Output)?;
                    Ok(0)
                }
                crate::chat_service::ReplyCommandOutcome::TryAgain(reason) => {
                    eprintln!("agentctl: {reason}");
                    Ok(75)
                }
            }
        }
        ChatCommand::Close(value) => {
            let result = crate::chat_service::close(&value.state.bridge_state, &value.request)
                .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Init(value) => {
            let client =
                HerdrClient::with_executable("direct", &herdr_bin).map_err(AgentError::from)?;
            let manager = ManagedAgents::new(&client, &registry)?;
            let result =
                crate::chat_service::initialize(&value.state.bridge_state, &value.config, &manager)
                    .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::Tick(value) => {
            let options = value.options();
            let client =
                HerdrClient::with_executable("direct", &herdr_bin).map_err(AgentError::from)?;
            let manager = ManagedAgents::new(&client, &registry)?;
            let result = crate::chat_service::tick(&value.state.bridge_state, &manager, options)
                .map_err(Failure::Chat)?;
            let code = if result.has_errors() { 75 } else { 0 };
            write_json(&result).map_err(Failure::Output)?;
            Ok(code)
        }
        ChatCommand::Run(value) => {
            let mut options = value.operate.options();
            options.delivery.deliver_while_working = value.deliver_while_working;
            let client =
                HerdrClient::with_executable("direct", &herdr_bin).map_err(AgentError::from)?;
            let manager = ManagedAgents::new(&client, &registry)?;
            let result = crate::chat_service::run_with_settings(
                &value.operate.state.bridge_state,
                &client,
                &manager,
                options,
                crate::chat_service::RunSettings {
                    ignored_text_prefixes: value.ignored_text_prefixes,
                    offer_reply_command: value.offer_reply_command,
                    stale_composer_clear_after: value.stale_composer_clear_after,
                },
            )
            .map_err(Failure::Chat)?;
            write_json(&result).map_err(Failure::Output)?;
            Ok(0)
        }
        ChatCommand::GracefulStopMain(value) => {
            crate::chat_service::graceful_stop_main(value.main_pid, value.main_pidfd_id)
                .map_err(Failure::Chat)?;
            Ok(0)
        }
    }
}

fn capabilities_document(
    registry: PathBuf,
    plugins: crate::plugins::PluginInventory,
) -> serde_json::Value {
    let not_live = crate::plugins::MaturityStatus::discovered(false);
    json!({
        "interactive": {
            "backends": ["herdr"],
            "harnesses": ["codex", "claude", "muse"]
        },
        "agentcloud": {
            "drivers": crate::subagents::CLOUD_DRIVERS,
            "requires": ["herdr", "agentcloudctl", "agentterm"],
            "capabilities": CLOUD_CAPABILITIES
        },
        "headless": null,
        "build": crate::build_info::document(),
        "services": [{
            "name": "chat-bridge",
            "origin": "built_in",
            "status": crate::plugins::MaturityStatus::discovered(true)
        }],
        "registry": registry,
        "chat_subscriptions": {
            "core": {
                "origin": "built_in",
                "protocol_name": chat_subscription_plugin::PROTOCOL_NAME,
                "protocol_version": chat_subscription_plugin::PROTOCOL_VERSION,
                "max_frame_bytes": chat_subscription_plugin::MAX_FRAME_BYTES,
                "status": crate::plugins::MaturityStatus::discovered(true)
            },
            "reference_backends": [
                {
                    "name": "google-workspace-events",
                    "origin": "reference",
                    "status": not_live,
                    "available": ["design"],
                    "missing": [
                        "rust-provider",
                        "cloud-topic",
                        "pull-subscription",
                        "application-default-credentials",
                        "live-end-to-end-verification"
                    ]
                },
                {
                    "name": "discord-gateway",
                    "origin": "reference",
                    "status": crate::plugins::MaturityStatus::discovered(false),
                    "available": ["request-response-client"],
                    "missing": ["gateway-subscription", "live-end-to-end-verification"]
                }
            ],
            "plugins": plugins
        }
    })
}

/// Named operations an agentcloud-backed agent supports.
const CLOUD_CAPABILITIES: [&str; 9] = [
    "send",
    "status",
    "read",
    "wait",
    "stop",
    "attach",
    "pause",
    "resume",
    "final-output",
];

fn add_capabilities(value: &mut serde_json::Value) {
    if let Some(values) = value.as_array_mut() {
        for value in values {
            add_capabilities(value);
        }
    } else if value.get("name").is_some()
        && (value.get("adapter").is_some() || value.get("launch").is_some_and(Value::is_object))
    {
        // A nested agentctl-session/v2 or v3 row keeps its launch fields under `launch`.
        let launch = if value.get("adapter").is_some() {
            &*value
        } else {
            &value["launch"]
        };
        let interactive = launch["mode"] == "interactive" && launch["backend"] == "herdr";
        let adapter = launch["adapter"].as_str().map(str::to_owned);
        let adapter = adapter.as_deref();
        value["capabilities"] = if adapter == Some("agentcloud") {
            json!(CLOUD_CAPABILITIES)
        } else if matches!(
            adapter,
            Some("herdr" | "herdr-pane" | "herdr-foreign" | "herdr-relay")
        ) && interactive
        {
            let mut capabilities = vec![
                "send",
                "status",
                "read",
                "wait",
                "stop",
                "attach",
                "pause",
                "resume",
                "terminal-snapshot",
                "drain",
                "goal",
                "bind-session",
            ];
            if adapter == Some("herdr-relay") {
                // Goals are slash commands, which a relayed pane cannot take.
                capabilities.retain(|capability| *capability != "goal");
            }
            if adapter == Some("herdr") {
                capabilities.push("move");
            }
            if matches!(adapter, Some("herdr" | "herdr-foreign")) {
                capabilities.extend(["anchor", "rename"]);
            }
            if matches!(adapter, Some("herdr" | "herdr-pane" | "herdr-relay")) {
                capabilities.push("revive");
            }
            json!(capabilities)
        } else {
            json!(["status"])
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent;
    use clap::CommandFactory;

    fn profile_resume_arguments(root: &Path) -> Vec<OsString> {
        vec![
            "agentctl".into(),
            "--registry".into(),
            root.join("registry").into_os_string(),
            "--herdr-bin=/definitely/missing/herdr".into(),
            "start".into(),
            "worker".into(),
            "--cwd".into(),
            root.as_os_str().to_owned(),
            "--profile=reviewer".into(),
            "--resume=saved-conversation".into(),
        ]
    }

    #[test]
    fn profile_resume_cli_accepts_the_combination_and_documents_its_local_scope() {
        let fixture = crate::subagents::tests::Fixture::new();
        let parsed = Cli::try_parse_from(profile_resume_arguments(&fixture.root)).unwrap();
        let Some(Commands::Start(start)) = parsed.command else {
            panic!("start command");
        };
        assert_eq!(start.profile.as_deref(), Some("reviewer"));
        assert_eq!(start.resume.as_deref(), Some("saved-conversation"));
        let help = Cli::try_parse_from(["agentctl", "start", "--help"])
            .err()
            .unwrap()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(help.contains("local interactive Claude, Codex, or Muse"));
        assert!(help.contains("--profile reviewer --resume CONVERSATION_ID"));
        assert!(help.contains("other explicit launch settings conflict"));
    }

    #[test]
    fn profile_resume_cli_retains_every_other_explicit_profile_override_conflict() {
        let fixture = crate::subagents::tests::Fixture::new();
        for (flag, arguments) in [
            ("--mode", vec!["--mode=interactive"]),
            ("--harness", vec!["--harness=codex"]),
            ("--model", vec!["--model=other-model"]),
            ("--reasoning-effort", vec!["--reasoning-effort=high"]),
            ("--harness-arg", vec!["--harness-arg=--other-policy"]),
            ("--env", vec!["--env=REVIEW_MODE=other"]),
            ("--cloud-harness", vec!["--cloud-harness=native"]),
            ("--provision", vec!["--provision"]),
            ("--envspec", vec!["--provision", "--envspec=example"]),
            ("--purpose", vec!["--provision", "--purpose=example"]),
            ("--cloud-workspace", vec!["--cloud-workspace=/work/example"]),
            ("--node-id", vec!["--node-id=example"]),
        ] {
            let mut argv = profile_resume_arguments(&fixture.root);
            argv.extend(arguments.iter().map(OsString::from));
            let error = run(Cli::try_parse_from(argv).unwrap(), &|_| None).unwrap_err();
            let Failure::Usage(message) = error else {
                panic!("expected usage refusal for {flag}");
            };
            assert!(message.contains("--profile conflicts"), "{flag}: {message}");
            assert!(message.contains(flag), "{flag}: {message}");
            assert!(!message.contains("--resume"), "{flag}: {message}");
        }
        assert!(!fixture.root.join("registry").exists());
    }

    #[test]
    fn profile_resume_cli_keeps_headless_and_agentcloud_out_of_native_resume_scope() {
        for (harness, mode) in [("codex", "headless"), ("agentcloud", "interactive")] {
            let fixture = crate::subagents::tests::Fixture::new();
            assert!(std::process::Command::new("/usr/bin/git")
                .args(["init", "-q"])
                .arg(&fixture.root)
                .status()
                .unwrap()
                .success());
            fs::write(fixture.root.join(".gitignore"), ".agentctl/\n").unwrap();
            let directory = fixture.root.join(".agentctl");
            agent::create_private_directory(&directory, "test launch profiles", false, false)
                .unwrap();
            agent::atomic_json(
                &directory.join("profiles.json"),
                &json!({
                    "schema": "agentctl-profiles/v1",
                    "profiles": {"reviewer": {"harness": harness, "mode": mode}},
                }),
            )
            .unwrap();
            let error = run(
                Cli::try_parse_from(profile_resume_arguments(&fixture.root)).unwrap(),
                &|_| None,
            )
            .unwrap_err();
            if mode == "headless" {
                let Failure::Usage(message) = error else {
                    panic!("expected headless usage refusal");
                };
                assert!(message.contains("headless"));
            } else {
                let Failure::Agent(error) = error else {
                    panic!("expected agentcloud resume refusal");
                };
                assert_eq!(error.exit_code(), 75);
                assert!(error
                    .to_string()
                    .contains("--resume does not apply to agentcloud"));
            }
            assert!(!fixture.root.join("registry").exists());
        }
    }

    #[test]
    fn stop_advice_parser_uses_recognized_grammar_and_successfully_parsed_globals() {
        let prefix = [
            "agentctl",
            "--registry=/work/record dir/'registry'",
            "--herdr-bin=-literal herdr $argument",
        ];
        let expected = StopContext::new(
            Path::new("/work/record dir/'registry'"),
            Path::new("-literal herdr $argument"),
        )
        .command(&RecoveryAction::Doctor);
        for suffix in [
            &["stop"][..],
            &["stop", "worker", "--unknown-option"][..],
            &["stop", "worker", "--expected-token"][..],
        ] {
            for arguments in [
                [&prefix[..], suffix].concat(),
                [&prefix[..1], &suffix[..1], &prefix[1..], &suffix[1..]].concat(),
                [
                    &[
                        "agentctl",
                        "--registry=/work/earlier",
                        "--herdr-bin=earlier",
                    ][..],
                    &suffix[..1],
                    &prefix[1..],
                    &suffix[1..],
                ]
                .concat(),
            ] {
                let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
                assert_eq!(
                    Cli::try_parse_from(arguments.iter().cloned())
                        .err()
                        .unwrap()
                        .exit_code(),
                    2
                );
                assert_eq!(
                    partial_stop_context(&arguments)
                        .expect("grammar identified stop before the error")
                        .command(&RecoveryAction::Doctor),
                    expected
                );
            }
        }
        for arguments in [
            vec!["agentctl", "--registry", "stop"],
            vec!["agentctl", "--herdr-bin=stop", "status"],
            vec!["agentctl", "send", "worker", "stop", "--unknown-option"],
        ] {
            let arguments: Vec<OsString> = arguments.iter().map(OsString::from).collect();
            assert!(partial_stop_context(&arguments).is_none());
        }
    }

    #[test]
    fn stop_advice_bound_flags_accept_leading_hyphen_tokens_as_one_argument() {
        let parsed = Cli::try_parse_from([
            "agentctl",
            "stop",
            "worker",
            "--expected-token=-original-generation",
            "--skip-cloud-halt",
        ])
        .unwrap();
        let Some(Commands::Stop(options)) = parsed.command else {
            panic!("stop command");
        };
        assert_eq!(
            options.expected_token.as_deref(),
            Some("-original-generation")
        );
        assert!(options.skip_cloud_halt);
    }

    #[test]
    fn revive_requires_one_scope_and_limits_startup_deadlines() {
        for arguments in [
            vec!["agentctl", "revive"],
            vec!["agentctl", "revive", "worker", "--all"],
            vec![
                "agentctl",
                "revive",
                "--all",
                "--expected-token",
                "generation",
            ],
            vec!["agentctl", "revive", "worker", "--startup-timeout", "0"],
            vec!["agentctl", "revive", "worker", "--startup-timeout", "301"],
            vec!["agentctl", "revive", "worker", "--startup-timeout", "NaN"],
        ] {
            assert_eq!(Cli::try_parse_from(arguments).err().unwrap().exit_code(), 2);
        }
        let parsed = Cli::try_parse_from([
            "agentctl",
            "revive",
            "worker",
            "--expected-token",
            "generation",
            "--dry-run",
        ])
        .unwrap();
        let Some(Commands::Revive(value)) = parsed.command else {
            panic!("revive command");
        };
        assert_eq!(value.name.as_deref(), Some("worker"));
        assert_eq!(value.expected_token.as_deref(), Some("generation"));
        assert!(value.dry_run);
        assert!(!value.all);
        assert_eq!(value.startup_timeout, 30.0);
        assert!(Cli::try_parse_from([
            "agentctl",
            "revive",
            "--all",
            "--dry-run",
            "--startup-timeout",
            "300"
        ])
        .is_ok());
        let help = Cli::try_parse_from(["agentctl", "revive", "--help"])
            .err()
            .unwrap()
            .to_string();
        for option in [
            "NAME",
            "--all",
            "--dry-run",
            "--expected-token",
            "--startup-timeout",
        ] {
            assert!(help.contains(option));
        }
    }

    #[test]
    fn revive_capability_requires_an_owned_interactive_adapter() {
        for (adapter, mode, expected) in [
            ("herdr", "interactive", true),
            ("herdr-pane", "interactive", true),
            ("herdr-relay", "interactive", true),
            ("herdr-foreign", "interactive", false),
            ("codex-app-server", "headless", false),
        ] {
            let mut record =
                json!({"name":"worker", "adapter":adapter, "mode":mode, "backend":"herdr"});
            add_capabilities(&mut record);
            assert_eq!(
                record["capabilities"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("revive")),
                expected,
                "{adapter}"
            );
        }
    }

    #[test]
    fn subcommands_have_local_help_and_registry_is_global() {
        Cli::command().debug_assert();
        for arguments in [
            ["agentctl", "--registry", "custom", "status", "worker"],
            ["agentctl", "status", "worker", "--state", "custom"],
        ] {
            assert_eq!(
                Cli::try_parse_from(arguments).unwrap().registry,
                PathBuf::from("custom")
            );
        }
        let error = Cli::try_parse_from(["agentctl", "send", "--help"])
            .err()
            .unwrap();
        let help = error.to_string();
        assert!(help.contains("--message-id"));
        assert!(help.contains("Seconds to wait"));
        assert!(!help.contains("--startup-timeout"));
        let error = Cli::try_parse_from(["agentctl", "adopt", "--help"])
            .err()
            .unwrap();
        let help = error.to_string();
        for required in ["--pane", "--workspace", "--cwd", "--harness", "--session"] {
            assert!(help.contains(required));
        }
        assert!(help.contains("without taking ownership"));
        assert!(help.contains("Muse requires a pinned foreground process"));
        let error = Cli::try_parse_from(["agentctl", "stop", "--help"])
            .err()
            .unwrap();
        let help = error.to_string();
        for required in [
            "--recover-legacy-adoption",
            "--retire-dead-adoption",
            "--expected-token",
            "--expected-record-sha256",
        ] {
            assert!(help.contains(required));
        }
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "status",
            "--bridge-state",
            "/tmp/chat-state",
            "--registry",
            "/tmp/registry",
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["agentctl", "chat", "quickstart"]).is_ok());
        let error = Cli::try_parse_from(["agentctl", "chat", "run", "--help"])
            .err()
            .expect("chat run help");
        let help = error.to_string();
        for required in [
            "--bridge-state",
            "--ready-timeout",
            "--working-timeout",
            "--reconcile-interval",
        ] {
            assert!(help.contains(required));
        }
        let error = Cli::try_parse_from(["agentctl", "chat", "publish", "--help"])
            .err()
            .expect("chat publish help");
        let help = error.to_string();
        for required in ["--bridge-state", "--channel-id", "--request-id", "--file"] {
            assert!(help.contains(required));
        }
        let retry = Cli::try_parse_from([
            "agentctl",
            "chat",
            "retry-checkpoint-gap",
            "--bridge-state",
            "/tmp/chat-state",
            "--expected-gap-sha256",
            &"a".repeat(64),
            "--expected-checkpoint-sha256",
            &"b".repeat(64),
            "--expected-configuration-sha256",
            &"c".repeat(64),
            "--keep-cursor",
            "cursor-safe",
            "--evidence-file",
            "/tmp/evidence.json",
            "--evidence-sha256",
            &"d".repeat(64),
        ])
        .expect("parse explicit checkpoint retry");
        assert!(matches!(
            retry.command,
            Some(Commands::Chat(Chat {
                command: ChatCommand::RetryCheckpointGap(_),
            }))
        ));
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "retry-checkpoint-gap",
            "--bridge-state",
            "/tmp/chat-state",
        ])
        .is_err());
        let exact_retry = Cli::try_parse_from([
            "agentctl",
            "chat",
            "retry-boundary-gap",
            "--bridge-state",
            "/tmp/chat-state",
            "--expected-gap-sha256",
            &"a".repeat(64),
            "--expected-checkpoint-sha256",
            &"b".repeat(64),
            "--expected-configuration-sha256",
            &"c".repeat(64),
            "--keep-cursor",
            "cursor-safe",
            "--evidence-file",
            "/tmp/evidence.json",
            "--evidence-sha256",
            &"d".repeat(64),
        ])
        .expect("parse explicit exact-boundary retry");
        assert!(matches!(
            exact_retry.command,
            Some(Commands::Chat(Chat {
                command: ChatCommand::RetryBoundaryGap(_),
            }))
        ));
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "retry-boundary-gap",
            "--bridge-state",
            "/tmp/chat-state",
        ])
        .is_err());
        let help = Cli::try_parse_from(["agentctl", "chat", "retry-boundary-gap", "--help"])
            .err()
            .expect("exact-boundary help")
            .to_string();
        for option in [
            "--bridge-state",
            "--expected-gap-sha256",
            "--expected-checkpoint-sha256",
            "--expected-configuration-sha256",
            "--keep-cursor",
            "--evidence-file",
            "--evidence-sha256",
        ] {
            assert!(help.contains(option), "{option}");
        }
        assert!(help.contains("newer exact provider commit"));
        let inspect = Cli::try_parse_from([
            "agentctl",
            "chat",
            "inspect",
            "--bridge-state",
            "/tmp/chat-state",
            "--request",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ])
        .expect("parse read-only request inspection");
        assert!(matches!(
            inspect.command,
            Some(Commands::Chat(Chat {
                command: ChatCommand::Inspect(ChatInspect { .. })
            }))
        ));
        let thread_last = |arguments: &[&str]| {
            let mut argv = vec![
                "agentctl",
                "chat",
                "thread",
                "--bridge-state",
                "/tmp/chat-state",
            ];
            argv.extend_from_slice(arguments);
            match Cli::try_parse_from(argv).map(|parsed| parsed.command) {
                Ok(Some(Commands::Chat(Chat {
                    command: ChatCommand::Thread(thread),
                }))) => Ok((thread.thread, thread.last)),
                Ok(_) => panic!("chat thread parsed as another command"),
                Err(error) => Err(error.to_string()),
            }
        };
        assert_eq!(
            thread_last(&["--thread", "spaces/example/threads/one"]),
            Ok(("spaces/example/threads/one".to_owned(), 10))
        );
        assert_eq!(
            thread_last(&["--thread", "-leading-hyphen", "--last", "100"]),
            Ok(("-leading-hyphen".to_owned(), 100))
        );
        assert!(thread_last(&["--thread", "t", "--last", "0"]).is_err());
        assert!(thread_last(&["--thread", "t", "--last", "101"]).is_err());
        assert!(thread_last(&["--last", "5"]).is_err());
        let error = Cli::try_parse_from(["agentctl", "chat", "thread", "--help"])
            .err()
            .expect("chat thread help");
        let help = error.to_string();
        for required in [
            "--bridge-state",
            "--thread",
            "--last",
            "(1-100)",
            "[default: 10]",
            "Example:",
        ] {
            assert!(help.contains(required), "{required} missing from:\n{help}");
        }
        let parsed = Cli::try_parse_from([
            "agentctl",
            "chat",
            "graceful-stop-main",
            "--main-pid",
            "123",
            "--main-pidfd-id",
            "456",
        ])
        .expect("parse internal systemd stop helper");
        assert!(matches!(
            parsed.command,
            Some(Commands::Chat(Chat {
                command: ChatCommand::GracefulStopMain(ChatGracefulStopMain {
                    main_pid: 123,
                    main_pidfd_id: 456,
                })
            }))
        ));
    }

    #[test]
    fn only_the_chat_service_and_its_stop_helper_timestamp_their_final_error() {
        let writes_service_log = |arguments: &[&str]| {
            Cli::try_parse_from(std::iter::once("agentctl").chain(arguments.iter().copied()))
                .expect("parse")
                .writes_service_log()
        };
        assert!(writes_service_log(&[
            "chat",
            "run",
            "--bridge-state",
            "/tmp/s"
        ]));
        assert!(writes_service_log(&[
            "chat",
            "graceful-stop-main",
            "--main-pid",
            "123",
            "--main-pidfd-id",
            "456",
        ]));
        assert!(!writes_service_log(&[
            "chat",
            "tick",
            "--bridge-state",
            "/tmp/s"
        ]));
        assert!(!writes_service_log(&[
            "chat",
            "status",
            "--bridge-state",
            "/tmp/s"
        ]));
        assert!(!writes_service_log(&["list"]));
        assert!(!writes_service_log(&[]));
    }

    #[test]
    fn chat_run_parses_repeated_runtime_only_ignored_text_prefixes() {
        let parsed = Cli::try_parse_from([
            "agentctl",
            "chat",
            "run",
            "--bridge-state",
            "/tmp/chat-state",
            "--ignore-text-prefix",
            "[assistant",
            "--ignore-text-prefix",
            "[notice]",
        ])
        .unwrap();
        let Some(Commands::Chat(Chat {
            command: ChatCommand::Run(run),
        })) = parsed.command
        else {
            panic!("expected chat run");
        };
        assert_eq!(run.ignored_text_prefixes, ["[assistant", "[notice]"]);
        for prefix in ["", "  ", "invalid\n"] {
            assert!(Cli::try_parse_from([
                "agentctl",
                "chat",
                "run",
                "--bridge-state",
                "/tmp/chat-state",
                "--ignore-text-prefix",
                prefix,
            ])
            .is_err());
        }
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "tick",
            "--bridge-state",
            "/tmp/chat-state",
            "--ignore-text-prefix",
            "[assistant",
        ])
        .is_err());
    }

    #[test]
    fn chat_reply_takes_a_reply_id_that_starts_with_a_hyphen_and_needs_every_argument() {
        let key = "0123456789abcdef".repeat(4);
        let arguments = [
            "--bridge-state",
            "/tmp/chat-state",
            "--request",
            key.as_str(),
            "--reply-id",
            "-AbC_1",
            "--file",
            "reply.md",
        ];
        let parsed =
            Cli::try_parse_from(["agentctl", "chat", "reply"].into_iter().chain(arguments))
                .unwrap();
        let Some(Commands::Chat(Chat {
            command: ChatCommand::Reply(reply),
        })) = parsed.command
        else {
            panic!("expected chat reply");
        };
        assert_eq!(
            reply.state.bridge_state,
            std::path::Path::new("/tmp/chat-state")
        );
        assert_eq!(reply.request, key);
        assert_eq!(reply.reply_id, "-AbC_1");
        assert_eq!(reply.file, std::path::Path::new("reply.md"));
        for missing in 0..4 {
            let rest = arguments
                .chunks(2)
                .enumerate()
                .filter(|(index, _)| *index != missing)
                .flat_map(|(_, pair)| pair.iter().copied());
            assert!(
                Cli::try_parse_from(["agentctl", "chat", "reply"].into_iter().chain(rest)).is_err(),
                "parsed without {}",
                arguments[2 * missing]
            );
        }
        // Only `chat run` takes the flag that offers the command, and it is off by default.
        for (flag, offered) in [(None, false), (Some("--offer-reply-command"), true)] {
            let parsed = Cli::try_parse_from(
                [
                    "agentctl",
                    "chat",
                    "run",
                    "--bridge-state",
                    "/tmp/chat-state",
                ]
                .into_iter()
                .chain(flag),
            )
            .unwrap();
            let Some(Commands::Chat(Chat {
                command: ChatCommand::Run(run),
            })) = parsed.command
            else {
                panic!("expected chat run");
            };
            assert_eq!(run.offer_reply_command, offered);
        }
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "tick",
            "--bridge-state",
            "/tmp/chat-state",
            "--offer-reply-command",
        ])
        .is_err());
    }

    #[test]
    fn chat_run_parses_the_opt_in_delivery_flags_and_both_default_off() {
        let run = |extra: &[&str]| {
            let parsed = Cli::try_parse_from(
                [
                    "agentctl",
                    "chat",
                    "run",
                    "--bridge-state",
                    "/tmp/chat-state",
                ]
                .iter()
                .chain(extra),
            )?;
            let Some(Commands::Chat(Chat {
                command: ChatCommand::Run(run),
            })) = parsed.command
            else {
                panic!("expected chat run");
            };
            Ok::<_, clap::Error>(run)
        };
        let defaults = run(&[]).unwrap();
        assert!(!defaults.deliver_while_working);
        assert_eq!(defaults.stale_composer_clear_after, None);
        let set = run(&[
            "--deliver-while-working",
            "--stale-composer-clear-after",
            "30m",
        ])
        .unwrap();
        assert!(set.deliver_while_working);
        assert_eq!(
            set.stale_composer_clear_after,
            Some(Duration::from_secs(1_800))
        );
        for bad in ["10s", "30", "1d", "soon"] {
            assert!(
                run(&["--stale-composer-clear-after", bad]).is_err(),
                "{bad}"
            );
        }
        for flag in [
            "--deliver-while-working",
            "--stale-composer-clear-after=30m",
        ] {
            assert!(Cli::try_parse_from([
                "agentctl",
                "chat",
                "tick",
                "--bridge-state",
                "/tmp/chat-state",
                flag,
            ])
            .is_err());
        }
    }

    #[test]
    fn start_slot_options_parse_and_need_slot() {
        let parsed = Cli::try_parse_from([
            "agentctl",
            "start",
            "worker",
            "--slot",
            "s1",
            "--slot-isolation",
            "root",
            "--slot-project",
            "/p",
        ])
        .unwrap();
        let Some(Commands::Start(start)) = parsed.command else {
            panic!("expected start");
        };
        assert_eq!(start.slot.as_deref(), Some("s1"));
        assert_eq!(start.slot_isolation.as_deref(), Some("root"));
        assert_eq!(start.slot_project, Some(PathBuf::from("/p")));
        for arguments in [
            vec!["agentctl", "start", "worker", "--slot-isolation", "userns"],
            vec!["agentctl", "start", "worker", "--slot-project", "/p"],
            vec![
                "agentctl",
                "start",
                "worker",
                "--slot",
                "s1",
                "--slot-isolation",
                "namespace",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let parsed = Cli::try_parse_from([
            "agentctl",
            "start",
            "planner",
            "--project-box",
            "--box-writable",
            "project",
            "--box-isolation",
            "root",
            "--box-project",
            "/p",
            "--box-name",
            "lead",
        ])
        .unwrap();
        let Some(Commands::Start(start)) = parsed.command else {
            panic!("expected start");
        };
        assert!(start.project_box);
        assert_eq!(start.box_writable.as_deref(), Some("project"));
        assert_eq!(start.box_isolation.as_deref(), Some("root"));
        assert_eq!(start.box_project, Some(PathBuf::from("/p")));
        assert_eq!(start.box_name.as_deref(), Some("lead"));
        for arguments in [
            vec!["agentctl", "start", "w", "--box-writable", "project"],
            vec!["agentctl", "start", "w", "--box-isolation", "root"],
            vec!["agentctl", "start", "w", "--box-name", "lead"],
            vec!["agentctl", "start", "w", "--project-box", "--slot", "s1"],
            vec![
                "agentctl",
                "start",
                "w",
                "--project-box",
                "--box-writable",
                "all",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
        let help = Cli::try_parse_from(["agentctl", "start", "--help"])
            .err()
            .unwrap()
            .to_string();
        for flag in [
            "--slot <SLOT>",
            "--slot-isolation",
            "--slot-project",
            "--project-box",
            "--box-writable",
            "--box-isolation",
            "--box-project",
            "--box-name",
            "AGENTCTL_WRKSLOTS_BIN",
        ] {
            assert!(help.contains(flag), "{flag} missing from start help");
        }
    }

    #[test]
    fn stop_parser_preserves_all_legacy_recovery_assertions() {
        let parsed = Cli::try_parse_from([
            "agentctl",
            "stop",
            "legacy",
            "--recover-legacy-adoption",
            "--expected-token",
            "generation-1",
            "--expected-record-sha256",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ])
        .unwrap();
        let Some(Commands::Stop(stop)) = parsed.command else {
            panic!("expected stop command");
        };
        assert_eq!(stop.agent.name, "legacy");
        assert!(stop.recover_legacy_adoption);
        assert!(!stop.retire_dead_adoption);
        assert_eq!(stop.expected_token.as_deref(), Some("generation-1"));
        assert_eq!(
            stop.expected_record_sha256.as_deref(),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn stop_parser_preserves_dead_adoption_assertions_and_refuses_both_selectors() {
        let digest = "a".repeat(64);
        let parsed = Cli::try_parse_from([
            "agentctl",
            "stop",
            "adopted",
            "--retire-dead-adoption",
            "--expected-token",
            "generation-1",
            "--expected-record-sha256",
            &digest,
        ])
        .unwrap();
        let Some(Commands::Stop(stop)) = parsed.command else {
            panic!("expected stop command");
        };
        assert!(stop.retire_dead_adoption);
        assert!(!stop.recover_legacy_adoption);
        assert_eq!(stop.expected_token.as_deref(), Some("generation-1"));
        assert_eq!(
            stop.expected_record_sha256.as_deref(),
            Some(digest.as_str())
        );
        let error = Cli::try_parse_from([
            "agentctl",
            "stop",
            "adopted",
            "--recover-legacy-adoption",
            "--retire-dead-adoption",
        ])
        .err()
        .unwrap();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn embedded_chat_guide_requires_derived_stop_and_measured_resource_bounds() {
        for required in [
            "hello_seconds + close_seconds + shutdown_grace_seconds + 7",
            "outbound_command.timeout_millis +",
            "maximum of the applicable provider window",
            "complete selected provider timeout tuple",
            "agentctl-chat-request-inspection/v1",
            "ack_completed_at_millis",
            "outcome-unknown operations",
            "systemd 258 or newer",
            "ExecStop=/absolute/path/agentctl chat graceful-stop-main",
            "TimeoutStopSec=<DERIVED_SECONDS_WITH_MARGIN>",
            "TimeoutStopFailureMode=kill",
            "TasksMax=<MEASURED_TASKS_WITH_HEADROOM>",
            "MemoryMax=<MEASURED_HARD_LIMIT>",
        ] {
            assert!(
                crate::CHAT_USER_GUIDE.contains(required),
                "embedded Chat guide omitted {required:?}"
            );
        }
        for stale in [
            "TimeoutStopSec=60",
            "TimeoutStopSec=75",
            "TimeoutStopSec=85",
            "TasksMax=2048",
            "MemoryMax=4G",
        ] {
            assert!(
                !crate::CHAT_USER_GUIDE.contains(stale),
                "embedded Chat guide retained stale prescription {stale:?}"
            );
        }
    }

    #[test]
    fn chat_publish_parses_exactly_one_bounded_input_shape_and_lowercase_uuid() {
        let uuid = "123e4567-e89b-42d3-a456-426614174000";
        let parsed = Cli::try_parse_from([
            "agentctl",
            "chat",
            "publish",
            "--bridge-state",
            "/tmp/chat-state",
            "--channel-id",
            "spaces/example",
            "--request-id",
            uuid,
            "hello",
        ])
        .expect("positional publish text");
        let Some(Commands::Chat(Chat {
            command: ChatCommand::Publish(publish),
        })) = parsed.command
        else {
            panic!("expected publish command");
        };
        assert_eq!(publish.channel_id, "spaces/example");
        assert_eq!(publish.request_id, uuid);
        assert_eq!(publish.text.as_deref(), Some("hello"));
        assert!(publish.file.is_none());

        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "publish",
            "--bridge-state",
            "/tmp/chat-state",
            "--channel-id",
            "spaces/example",
            "--request-id",
            uuid,
            "--file",
            "message.txt",
        ])
        .is_ok());
        for invalid in [
            "123E4567-e89b-42d3-a456-426614174000",
            "123e4567-e89b-12d3-a456-426614174000",
            "not-a-uuid",
        ] {
            assert!(Cli::try_parse_from([
                "agentctl",
                "chat",
                "publish",
                "--bridge-state",
                "/tmp/chat-state",
                "--channel-id",
                "spaces/example",
                "--request-id",
                invalid,
                "hello",
            ])
            .is_err());
        }
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "publish",
            "--bridge-state",
            "/tmp/chat-state",
            "--channel-id",
            "spaces/example",
            "--request-id",
            uuid,
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "agentctl",
            "chat",
            "publish",
            "--bridge-state",
            "/tmp/chat-state",
            "--channel-id",
            "spaces/example",
            "--request-id",
            uuid,
            "hello",
            "--file",
            "message.txt",
        ])
        .is_err());
    }
    #[test]
    fn invalid_timeouts_and_ambiguous_prompts_are_rejected() {
        for value in ["NaN", "inf", "-1", "31536001"] {
            assert!(seconds(value).is_err());
        }
        assert!(startup_seconds("0").is_err());
        assert!(positive_seconds("0").is_err());
        for option in ["--ready-timeout", "--working-timeout"] {
            assert!(Cli::try_parse_from([
                "agentctl",
                "chat",
                "run",
                "--bridge-state",
                "/tmp/chat-state",
                option,
                "30.000001",
            ])
            .is_err());
        }
        assert!(Cli::try_parse_from([
            "agentctl",
            "goal",
            "worker",
            "--goal-command-json",
            "[\"codex\",\"app-server\"]"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "agentctl", "send", "worker", "prompt", "--file", "task.txt"
        ])
        .is_err());
    }

    #[test]
    fn chat_capabilities_separate_code_installation_from_runtime_evidence() {
        let plugins = crate::plugins::PluginInventory {
            home: Some(PathBuf::from("/tmp/fixture-home")),
            discovered: vec![crate::plugins::DiscoveredPlugin {
                name: "fixture-chat".to_owned(),
                capability: "chat-subscription.fixture".to_owned(),
                executable: "backend".to_owned(),
                protocol_name: chat_subscription_plugin::PROTOCOL_NAME,
                protocol_min: 1,
                protocol_max: 1,
                origin: "discovered",
                status: crate::plugins::MaturityStatus::discovered(true),
                process_phase_timeouts:
                    chat_subscription_plugin::process::ProcessPhaseTimeouts::new(
                        std::time::Duration::from_secs(10),
                        std::time::Duration::from_secs(30),
                        std::time::Duration::from_secs(30),
                        std::time::Duration::from_secs(10),
                        std::time::Duration::from_secs(2),
                    )
                    .expect("fixture process phase timeouts"),
                executable_identity: None,
            }],
            refused: vec![crate::plugins::RefusedPlugin {
                entry: "old-chat".to_owned(),
                code: "incompatible_protocol",
                detail: "fixture refusal".to_owned(),
            }],
        };
        let document = capabilities_document(PathBuf::from("registry"), plugins);
        assert_eq!(document["chat_subscriptions"]["core"]["origin"], "built_in");
        assert_eq!(
            document["chat_subscriptions"]["core"]["protocol_name"],
            "agentctl-chat-subscription"
        );
        let discovered = &document["chat_subscriptions"]["plugins"]["discovered"][0];
        assert_eq!(discovered["origin"], "discovered");
        assert_eq!(discovered["status"]["implemented"], true);
        assert_eq!(discovered["status"]["configured"], false);
        assert_eq!(discovered["status"]["connected"], false);
        assert_eq!(discovered["status"]["live_verified"], false);
        assert_eq!(
            document["chat_subscriptions"]["plugins"]["refused"][0]["code"],
            "incompatible_protocol"
        );
        assert_eq!(
            document["chat_subscriptions"]["reference_backends"][0]["status"]["implemented"],
            false
        );
        assert_eq!(
            document["chat_subscriptions"]["reference_backends"][0]["available"],
            json!(["design"])
        );
        assert!(
            document["chat_subscriptions"]["reference_backends"][0]["missing"]
                .as_array()
                .expect("missing capability list")
                .contains(&json!("rust-provider"))
        );
    }

    #[test]
    fn agentcloud_start_flags_parse_document_and_refuse_misuse() {
        let parsed = Cli::try_parse_from([
            "agentctl",
            "--agentcloudctl-bin",
            "/opt/bin/agentcloudctl",
            "start",
            "sub-cloud",
            "--harness",
            "agentcloud",
            "--cloud-harness",
            "claude-code",
            "--provision",
            "--envspec",
            "monorepo",
            "--purpose",
            "sub-cloud checkout",
            "--agentterm-bin",
            "/opt/bin/agentterm",
        ])
        .expect("agentcloud start parses");
        assert_eq!(
            parsed.agentcloudctl_bin,
            PathBuf::from("/opt/bin/agentcloudctl")
        );
        assert_eq!(parsed.agentterm_bin, PathBuf::from("/opt/bin/agentterm"));
        let Some(Commands::Start(start)) = parsed.command else {
            panic!("expected start");
        };
        assert!(start.provision);
        assert_eq!(start.cloud_harness.as_deref(), Some("claude-code"));
        for invalid in [
            vec![
                "start",
                "sub-cloud",
                "--harness",
                "agentcloud",
                "--envspec",
                "monorepo",
            ],
            vec![
                "start",
                "sub-cloud",
                "--harness",
                "agentcloud",
                "--purpose",
                "label",
            ],
            vec![
                "start",
                "sub-cloud",
                "--harness",
                "agentcloud",
                "--provision",
                "--node-id",
                "n1",
            ],
            vec![
                "start",
                "sub-cloud",
                "--harness",
                "agentcloud",
                "--cloud-harness",
                "claude",
            ],
        ] {
            assert!(
                Cli::try_parse_from(std::iter::once("agentctl").chain(invalid.iter().copied()))
                    .is_err(),
                "accepted {invalid:?}"
            );
        }
        let help = Cli::try_parse_from(["agentctl", "start", "--help"])
            .err()
            .expect("start help")
            .to_string();
        for required in [
            "--cloud-harness",
            "--provision",
            "--envspec",
            "--purpose",
            "--cloud-workspace",
            "--node-id",
            "--agentcloudctl-bin",
            "--agentterm-bin",
            "agentterm -s SESSION_ID",
            "300",
        ] {
            assert!(help.contains(required), "start help omitted {required:?}");
        }
        for (command, required) in [
            ("stop", "--skip-cloud-halt"),
            ("send", "idempotency"),
            ("read", "--output last"),
            ("wait", "--until settle"),
        ] {
            let help = Cli::try_parse_from(["agentctl", command, "-h"])
                .err()
                .expect("short help")
                .to_string();
            assert!(help.contains(required), "{command} -h omitted {required:?}");
        }
    }

    #[test]
    fn agentcloud_flags_are_usage_errors_outside_agentcloud_launches() {
        let registry = std::env::temp_dir().join(format!(
            "agentctl-cli-cloud-{}-{}",
            std::process::id(),
            line!()
        ));
        let registry = registry.display().to_string();
        for arguments in [
            vec![
                "--registry",
                registry.as_str(),
                "--herdr-bin",
                "/nonexistent/herdr",
                "start",
                "sub-local",
                "--harness",
                "codex",
                "--provision",
            ],
            vec![
                "--registry",
                registry.as_str(),
                "--herdr-bin",
                "/nonexistent/herdr",
                "start",
                "sub-local",
                "--profile",
                "cloud",
                "--cloud-workspace",
                "/w",
            ],
        ] {
            let code = main(arguments.iter().map(OsString::from));
            assert_eq!(code, 2, "{arguments:?}");
        }
        assert!(!Path::new(&registry).exists());
        assert_eq!(
            main(["skill", "install", "--agentterm-bin", "/x"].map(OsString::from)),
            2
        );
    }

    #[test]
    fn an_empty_from_session_pins_a_human_send_even_inside_a_session() {
        let inside = Some("c0ffee00-0000-4000-8000-000000000001".to_owned());
        assert_eq!(caller_session(Some(String::new()), inside.clone()), None);
        assert_eq!(caller_session(None, inside.clone()), inside);
        assert_eq!(
            caller_session(Some("a".to_owned()), inside),
            Some("a".to_owned())
        );
        assert_eq!(caller_session(None, Some(String::new())), None);
        assert_eq!(caller_session(None, None), None);
    }

    #[test]
    fn agentcloud_records_advertise_only_the_cloud_interface() {
        let mut record = json!({
            "name": "sub-cloud",
            "adapter": "agentcloud",
            "mode": "interactive",
            "backend": "herdr",
        });
        add_capabilities(&mut record);
        assert_eq!(record["capabilities"], json!(CLOUD_CAPABILITIES));
        assert!(!record["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("drain")));
    }

    #[test]
    fn adopted_interactive_records_advertise_the_full_named_interface() {
        let mut record = json!({
            "name": "foreign",
            "adapter": "herdr-foreign",
            "mode": "interactive",
            "backend": "herdr",
        });
        add_capabilities(&mut record);
        assert_eq!(
            record["capabilities"],
            json!([
                "send",
                "status",
                "read",
                "wait",
                "stop",
                "attach",
                "pause",
                "resume",
                "terminal-snapshot",
                "drain",
                "goal",
                "bind-session",
                "anchor",
                "rename"
            ])
        );
    }
}
#[test]
fn ordinary_session_control_does_not_discover_plugins() {
    assert!(plugin_discovery_required(&Commands::Capabilities));
    assert!(!plugin_discovery_required(&Commands::List));
    assert!(!plugin_discovery_required(&Commands::Status(Named {
        name: "worker".to_owned(),
    })));
}
