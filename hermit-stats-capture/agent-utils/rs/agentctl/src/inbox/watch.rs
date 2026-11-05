//! `agentctl inbox watch`: observe workers and post inbox notices when their state changes.
//!
//! One sample lists Herdr panes, asks Claude Code for the busy or idle state of each Claude
//! session, and joins the two by process id through `HERDR_PANE_ID` in `/proc/<pid>/environ`.
//! Claude's own state is used for Claude panes because Herdr's screen rules can classify a working
//! Claude pane as idle; when Claude cannot be asked, a Claude pane's state is unknown for that
//! sample rather than taken from the screen. Other panes use Herdr's state, and an idle edge must
//! hold for `--idle-samples` consecutive samples before it is announced.
//!
//! The pure core is [`step`]: previous watch state plus one sample gives the notices to post. The
//! caller adds text (the last assistant message of a Claude transcript since the last notice, or
//! the tail of the terminal) and posts through the inbox, so all coalescing rules apply.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Read as _, Seek as _, SeekFrom};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use clap::Args;
use serde::{Deserialize, Serialize};

use super::{
    check_name, cut, now_ms, print_json, Cursor, Inbox, InboxError, NoticeKind, PostRequest,
    Result, Target, DEFAULT_MAX_LIVE, DEFAULT_STALE_SECONDS, EXIT_BUSY, MAX_TEXT_BYTES,
};

/// Default seconds between samples.
pub(crate) const DEFAULT_INTERVAL_SECONDS: u64 = 30;
/// Default consecutive idle samples required before a non-Claude pane is announced idle.
pub(crate) const DEFAULT_IDLE_SAMPLES: u32 = 2;
/// Default consecutive samples a worker must be missing before it is announced as exited.
pub(crate) const DEFAULT_EXIT_SAMPLES: u32 = 3;
/// Default minutes before the first still-idle reminder; each later one waits twice as long.
pub(crate) const DEFAULT_REMIND_MINUTES: u64 = 30;
/// Most still-idle reminders for one idle period.
pub(crate) const MAX_REMINDERS: u32 = 3;
/// How long an exited worker is remembered, so its return withdraws the exited notice.
const EXITED_MEMORY_MS: u64 = 86_400_000;
/// Most transcript bytes read to find the last assistant message.
const TRANSCRIPT_TAIL_BYTES: u64 = 8 * 1024 * 1024;
/// Seconds one `herdr` or `claude` query may run.
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a query's output may stay open after its process exits, and after a kill.
const DESCENDANT_GRACE: Duration = Duration::from_secs(2);

/// `agentctl inbox watch` arguments.
#[derive(Args)]
pub(crate) struct WatchArgs {
    #[command(flatten)]
    target: Target,
    /// Take one sample, post its notices, save state, and exit (for a cron job or loop)
    #[arg(long)]
    once: bool,
    /// Seconds between samples when running continuously
    #[arg(long, default_value_t = DEFAULT_INTERVAL_SECONDS, value_name = "SECONDS")]
    interval: u64,
    /// Consecutive idle samples before a pane without Claude state is announced idle
    #[arg(long, default_value_t = DEFAULT_IDLE_SAMPLES, value_name = "N")]
    idle_samples: u32,
    /// Consecutive samples a worker must be missing from Herdr before it is announced as exited
    #[arg(long, default_value_t = DEFAULT_EXIT_SAMPLES, value_name = "N")]
    exit_samples: u32,
    /// Minutes an announced idle worker waits before the first still-idle reminder (then 2x, 4x; at most three)
    #[arg(long, default_value_t = DEFAULT_REMIND_MINUTES, value_name = "MINUTES")]
    remind_minutes: u64,
    /// Worker name to ignore; repeatable. The coordinator's own pane (named like --to) is always ignored
    #[arg(long, value_name = "NAME")]
    exclude: Vec<String>,
    /// Claude Code executable used for `claude agents --json`
    #[arg(long, default_value = "claude", value_name = "PATH")]
    claude_bin: PathBuf,
    /// Directory holding Claude Code transcripts as <project>/<session-id>.jsonl (default: ~/.claude/projects)
    #[arg(long, value_name = "DIR")]
    claude_projects: Option<PathBuf>,
    /// Process filesystem used to map Claude processes to Herdr panes
    #[arg(long, default_value = "/proc", value_name = "DIR")]
    proc_root: PathBuf,
}

/// What a worker is doing, as far as the watcher can tell.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WorkerState {
    /// Mid-turn.
    Working,
    /// Finished a turn and waiting for input.
    Idle,
    /// Waiting on a permission dialog, a question, or other outside input.
    Blocked,
}

/// One worker in one sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Observation {
    agent: String,
    pane: String,
    /// `None` when this sample cannot tell (for example, Claude could not be asked).
    state: Option<WorkerState>,
    /// True when the state came from Claude Code itself rather than from screen rules.
    authoritative: bool,
    transcript: Option<PathBuf>,
}

/// Persistent memory of one worker between samples.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorkerRecord {
    pane: String,
    state: WorkerState,
    since_unix_ms: u64,
    idle_samples: u32,
    /// The notice kind that currently stands for this worker in the inbox: `idle`, `blocked`, or
    /// `exited`; `None` after `working` withdrew it.
    announced: Option<NoticeKind>,
    reminders: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript: Option<PathBuf>,
    /// Transcript bytes already covered by an earlier notice.
    #[serde(default)]
    offset: u64,
    /// Consecutive samples in which the worker was missing.
    #[serde(default)]
    missing: u32,
    /// Set once the worker was announced as exited; kept so that its return is announced too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exited_unix_ms: Option<u64>,
    /// True while the worker is in the idle period it was first seen in, which was never
    /// posted: its end needs no `working` notice.
    #[serde(default)]
    quiet: bool,
    /// When the worker was last listed by Herdr (0 in records written before this was kept).
    #[serde(default)]
    last_seen_unix_ms: u64,
}

impl WorkerRecord {
    /// Record a confirmed idle edge and say whether to post it. A parked worker's idle is not
    /// posted, and its idle period is then treated like a quiet one, unless a blocked or exited
    /// notice still stands for it: that notice must be replaced, not left standing.
    fn idle_edge(&mut self, parked: bool) -> bool {
        let standing = matches!(
            self.announced,
            Some(NoticeKind::Blocked | NoticeKind::Exited)
        );
        self.announced = Some(NoticeKind::Idle);
        if parked && !standing {
            self.quiet = true;
            self.reminders = MAX_REMINDERS;
            return false;
        }
        true
    }
}

/// Everything the watcher remembers for one coordinator.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WatchState {
    schema: u32,
    workers: BTreeMap<String, WorkerRecord>,
    /// Workers a coordinator marked parked (`agentctl inbox park`), loaded from `parked.json`
    /// for each sample and not saved with the state.
    #[serde(skip)]
    parked: std::collections::BTreeSet<String>,
}

/// A notice the core decided to post; the caller supplies the text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Decision {
    agent: String,
    kind: NoticeKind,
}

/// Tunables for [`step`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Policy {
    idle_samples: u32,
    exit_samples: u32,
    remind_after_ms: u64,
}

fn file_length(path: Option<&Path>) -> u64 {
    path.and_then(|path| fs::metadata(path).ok())
        .map_or(0, |metadata| metadata.len())
}

/// Advance the watch state by one sample and decide which notices to post.
///
/// A worker seen for the first time is recorded without a notice unless it is blocked, so starting
/// the watcher does not announce every idle worker at once; its current idle period earns no
/// reminders either. A worker missing for `exit_samples` samples in a row is announced as exited,
/// and remembered for a day so that its return withdraws or replaces that notice. A sample in
/// which Herdr listed no pane at all (`listed_anything` false, judged before the coordinator and
/// excluded panes are filtered out) is treated as unknown, not as every worker leaving.
pub(crate) fn step(
    state: &mut WatchState,
    sample: &[Observation],
    listed_anything: bool,
    now: u64,
    policy: Policy,
) -> Vec<Decision> {
    let mut decisions = Vec::new();
    let mut decide = |agent: &str, kind| {
        decisions.push(Decision {
            agent: agent.to_owned(),
            kind,
        });
    };
    for observation in sample {
        let parked = state.parked.contains(&observation.agent);
        let required = if observation.authoritative {
            1
        } else {
            policy.idle_samples.max(1)
        };
        let Some(record) = state.workers.get_mut(&observation.agent) else {
            // A worker is first recorded once its state is known, so a quiet start stays quiet.
            let Some(observed) = observation.state else {
                continue;
            };
            let announced = match observed {
                WorkerState::Blocked => {
                    decide(&observation.agent, NoticeKind::Blocked);
                    Some(NoticeKind::Blocked)
                }
                // Already idle when first seen: its idle period began before the watcher could
                // observe it, so it is neither announced nor reminded.
                WorkerState::Idle => Some(NoticeKind::Idle),
                WorkerState::Working => None,
            };
            state.workers.insert(
                observation.agent.clone(),
                WorkerRecord {
                    pane: observation.pane.clone(),
                    state: observed,
                    since_unix_ms: now,
                    idle_samples: u32::from(observed == WorkerState::Idle),
                    announced,
                    reminders: if observed == WorkerState::Idle {
                        MAX_REMINDERS
                    } else {
                        0
                    },
                    transcript: observation.transcript.clone(),
                    offset: file_length(observation.transcript.as_deref()),
                    missing: 0,
                    exited_unix_ms: None,
                    quiet: observed == WorkerState::Idle,
                    last_seen_unix_ms: now,
                },
            );
            continue;
        };
        record.missing = 0;
        record.last_seen_unix_ms = now;
        record.pane.clone_from(&observation.pane);
        if let Some(path) = &observation.transcript {
            match &record.transcript {
                Some(previous) if previous == path => {}
                // A different transcript is a new conversation: all of it is unread.
                Some(_) => {
                    record.transcript = Some(path.clone());
                    record.offset = 0;
                }
                // First learned now: what is already there predates the watcher's knowledge.
                None => {
                    record.transcript = Some(path.clone());
                    record.offset = file_length(Some(path));
                }
            }
        }
        let Some(observed) = observation.state else {
            continue;
        };
        let returned = record.exited_unix_ms.take().is_some();
        if observed != record.state || returned {
            let quiet = std::mem::take(&mut record.quiet);
            record.state = observed;
            record.since_unix_ms = now;
            record.reminders = 0;
            record.idle_samples = u32::from(observed == WorkerState::Idle);
            match observed {
                WorkerState::Working => {
                    if record.announced.take().is_some() && !quiet {
                        decide(&observation.agent, NoticeKind::Working);
                    }
                }
                WorkerState::Blocked => {
                    record.announced = Some(NoticeKind::Blocked);
                    decide(&observation.agent, NoticeKind::Blocked);
                }
                // An unconfirmed idle edge leaves any standing notice in place, so a later
                // `working` still withdraws a blocked or exited notice.
                WorkerState::Idle => {
                    if record.idle_samples >= required && record.idle_edge(parked) {
                        decide(&observation.agent, NoticeKind::Idle);
                    }
                }
            }
            continue;
        }
        if observed != WorkerState::Idle {
            continue;
        }
        record.idle_samples = record.idle_samples.saturating_add(1);
        if record.announced != Some(NoticeKind::Idle) {
            if record.idle_samples >= required && record.idle_edge(parked) {
                decide(&observation.agent, NoticeKind::Idle);
            }
            continue;
        }
        if parked {
            continue;
        }
        let wait = policy
            .remind_after_ms
            .saturating_mul(1_u64 << record.reminders.min(16));
        if record.reminders < MAX_REMINDERS
            && policy.remind_after_ms > 0
            && now.saturating_sub(record.since_unix_ms) >= wait
        {
            record.reminders += 1;
            record.since_unix_ms = now;
            decide(&observation.agent, NoticeKind::StillIdle);
        }
    }
    let listed_nobody = !listed_anything;
    let seen = sample
        .iter()
        .map(|observation| observation.agent.as_str())
        .collect::<HashSet<_>>();
    let mut forget = Vec::new();
    for (agent, record) in &mut state.workers {
        if seen.contains(agent.as_str()) {
            continue;
        }
        if let Some(exited) = record.exited_unix_ms {
            if now.saturating_sub(exited) > EXITED_MEMORY_MS {
                forget.push(agent.clone());
            }
            continue;
        }
        if listed_nobody {
            continue;
        }
        record.missing = record.missing.saturating_add(1);
        if record.missing >= policy.exit_samples.max(1) {
            record.exited_unix_ms = Some(now);
            record.announced = Some(NoticeKind::Exited);
            // The exited notice is posted, so the worker's return must withdraw or replace it.
            record.quiet = false;
            decide(agent, NoticeKind::Exited);
        }
    }
    for agent in forget {
        state.workers.remove(&agent);
    }
    decisions
}

/// Name characters allowed by the inbox, with everything else turned into hyphens.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Inbox worker name for a Herdr pane: its agent name when that is a valid inbox name, else a
/// name derived from the pane id.
fn worker_name(name: Option<&str>, pane: &str) -> String {
    match name.filter(|name| check_name("--from", name).is_ok()) {
        Some(name) => name.to_owned(),
        None => format!("pane-{}", sanitize(pane)),
    }
}

/// Make worker names unique within one sample. In a colliding group, the pane that already owns
/// the worker record of that name keeps it; every other member gets its pane id appended, and a
/// counter if that still collides. Renaming the owner would announce a live worker as exited.
fn disambiguate(observations: &mut [Observation], state: &WatchState) {
    let mut counts = HashMap::new();
    for observation in observations.iter() {
        *counts.entry(observation.agent.clone()).or_insert(0_u32) += 1;
    }
    let owns = |observation: &Observation| {
        state
            .workers
            .get(&observation.agent)
            .is_some_and(|record| record.pane == observation.pane)
    };
    let keeps = |observation: &Observation| counts[&observation.agent] == 1 || owns(observation);
    let mut used = observations
        .iter()
        .filter(|observation| keeps(observation))
        .map(|observation| observation.agent.clone())
        .collect::<HashSet<_>>();
    let keep = observations.iter().map(keeps).collect::<Vec<_>>();
    for (observation, keep) in observations.iter_mut().zip(keep) {
        if keep {
            continue;
        }
        let base = format!("{}-{}", observation.agent, sanitize(&observation.pane));
        let mut candidate = base.chars().take(64).collect::<String>();
        let mut index = 2;
        while !used.insert(candidate.clone()) {
            let suffix = format!("-{index}");
            candidate = base.chars().take(64 - suffix.len()).collect::<String>() + &suffix;
            index += 1;
        }
        observation.agent = candidate;
    }
}

#[derive(Deserialize)]
struct HerdrList {
    result: HerdrResult,
}

#[derive(Deserialize)]
struct HerdrResult {
    agents: Vec<HerdrAgent>,
}

#[derive(Deserialize)]
struct HerdrAgent {
    pane_id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_status: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeSession {
    #[serde(default)]
    pid: Option<serde_json::Value>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

/// Run a query command in its own process group with a deadline and return its stdout. On the
/// deadline the whole group is killed, so a grandchild holding stdout open cannot outlive it.
fn query(program: &Path, arguments: &[&str], timeout: Duration) -> Result<Vec<u8>> {
    let mut child = super::spawn_retrying(
        Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0),
    )
    .map_err(|error| {
        InboxError::unavailable(format!("cannot run {}: {error}", program.display()))
    })?;
    let group = i32::try_from(child.id()).unwrap_or(0);
    let kill_group = || {
        if group > 0 {
            // SAFETY: signalling our own child's process group; no memory is involved.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = sender.send(stdout.read_to_end(&mut bytes).map(|_| bytes));
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                kill_group();
                let _ = child.wait();
                return Err(InboxError::unavailable(format!(
                    "cannot wait for {}: {error}",
                    program.display()
                )));
            }
        }
    };
    // A descendant may still hold the pipe after the direct child exits. It gets a short grace,
    // then the group is killed: while it holds the pipe the group exists, so its id cannot have
    // been reused. A descendant that left the group and keeps the pipe is abandoned, not awaited.
    let grace = deadline
        .saturating_duration_since(Instant::now())
        .min(DESCENDANT_GRACE);
    let (read, status) = match receiver.recv_timeout(grace) {
        Ok(read) => (read, status),
        Err(_) => {
            kill_group();
            let read = receiver.recv_timeout(DESCENDANT_GRACE).map_err(|_| {
                InboxError::unavailable(format!(
                    "{} left a process holding its output open",
                    program.display()
                ))
            })?;
            // Output cut off by the kill is not a complete answer.
            (read, None)
        }
    };
    let bytes = read
        .map_err(|error| InboxError::io(&format!("cannot read {}", program.display()), &error))?;
    match status {
        Some(status) if status.success() => Ok(bytes),
        Some(status) => Err(InboxError::unavailable(format!(
            "{} {} exited {status}",
            program.display(),
            arguments.join(" ")
        ))),
        None => Err(InboxError::unavailable(format!(
            "{} {} did not finish within {} seconds",
            program.display(),
            arguments.join(" "),
            timeout.as_secs()
        ))),
    }
}

/// `HERDR_PANE_ID` of a process, read from `<proc_root>/<pid>/environ`.
fn pane_of_process(proc_root: &Path, pid: &str) -> Option<String> {
    let environ = fs::read(proc_root.join(pid).join("environ")).ok()?;
    environ.split(|byte| *byte == 0).find_map(|entry| {
        entry
            .strip_prefix(b"HERDR_PANE_ID=")
            .map(|value| String::from_utf8_lossy(value).into_owned())
    })
}

/// `<projects>/<any project>/<session>.jsonl`, if it exists.
fn transcript_of_session(projects: &Path, session: &str) -> Option<PathBuf> {
    fs::read_dir(projects)
        .ok()?
        .flatten()
        .map(|project| project.path().join(format!("{session}.jsonl")))
        .find(|path| path.is_file())
}

struct Sources<'a> {
    herdr: &'a Path,
    claude: &'a Path,
    projects: &'a Path,
    proc_root: &'a Path,
    timeout: Duration,
}

/// Claude sessions keyed by pane; `None` when Claude could not be asked. When two sessions claim
/// one pane, the lowest process id (normally the parent) wins, so the choice is stable.
fn claude_sessions(sources: &Sources<'_>) -> Option<BTreeMap<String, ClaudeSession>> {
    let bytes = query(sources.claude, &["agents", "--json"], sources.timeout).ok()?;
    let sessions = serde_json::from_slice::<Vec<ClaudeSession>>(&bytes).ok()?;
    let mut by_pane: BTreeMap<String, (u64, ClaudeSession)> = BTreeMap::new();
    for session in sessions {
        let pid = match &session.pid {
            Some(serde_json::Value::String(pid)) => pid.clone(),
            Some(serde_json::Value::Number(pid)) => pid.to_string(),
            _ => continue,
        };
        let Ok(number) = pid.parse::<u64>() else {
            continue;
        };
        let Some(pane) = pane_of_process(sources.proc_root, &pid) else {
            continue;
        };
        if by_pane.get(&pane).is_none_or(|(held, _)| number < *held) {
            by_pane.insert(pane, (number, session));
        }
    }
    Some(
        by_pane
            .into_iter()
            .map(|(pane, (_, session))| (pane, session))
            .collect(),
    )
}

/// Take one sample of every Herdr pane that hosts an agent.
/// Returns the observations and whether Herdr listed any pane at all.
fn sample(sources: &Sources<'_>, ignored: &[String]) -> Result<(Vec<Observation>, bool)> {
    let listing: HerdrList =
        serde_json::from_slice(&query(sources.herdr, &["agent", "list"], sources.timeout)?)
            .map_err(|error| {
                InboxError::unavailable(format!("cannot parse `herdr agent list`: {error}"))
            })?;
    let claude = claude_sessions(sources);
    let listed_anything = !listing.result.agents.is_empty();
    let mut observations = Vec::new();
    for agent in listing.result.agents {
        let (Some(status), Some(harness)) = (agent.agent_status.as_deref(), agent.agent.as_deref())
        else {
            continue;
        };
        let name = worker_name(agent.name.as_deref(), &agent.pane_id);
        if ignored.contains(&name) {
            continue;
        }
        let herdr_state = match status {
            "blocked" => Some(WorkerState::Blocked),
            "idle" | "done" => Some(WorkerState::Idle),
            "working" => Some(WorkerState::Working),
            _ => None,
        };
        let session = claude
            .as_ref()
            .and_then(|sessions| sessions.get(&agent.pane_id));
        let (state, authoritative) = match session.and_then(|session| session.status.as_deref()) {
            _ if herdr_state == Some(WorkerState::Blocked) => (herdr_state, session.is_some()),
            Some("idle") => (Some(WorkerState::Idle), true),
            Some("busy") => (Some(WorkerState::Working), true),
            // Claude could not be asked, so its screen state is not trusted this sample.
            _ if claude.is_none() && harness == "claude" => (None, false),
            _ => (herdr_state, false),
        };
        let transcript = session
            .and_then(|session| session.session_id.as_deref())
            .and_then(|session| transcript_of_session(sources.projects, session));
        observations.push(Observation {
            agent: name,
            pane: agent.pane_id,
            state,
            authoritative,
            transcript,
        });
    }
    Ok((observations, listed_anything))
}

/// The last assistant prose in a Claude transcript between `start` and its end, and the end
/// offset (the byte after the last complete line). Reads at most the final 8 MiB of the range;
/// a line cut by that limit is skipped because it does not parse.
pub(crate) fn last_assistant_text(path: &Path, start: u64) -> io::Result<(Option<String>, u64)> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = start.min(length);
    let from = start.max(length.saturating_sub(TRANSCRIPT_TAIL_BYTES));
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.take(length - from).read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    let mut last = None;
    for line in bytes[..complete].split(|byte| *byte == b'\n') {
        let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if record.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = record
            .pointer("/message/content")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        let text = content
            .iter()
            .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.trim().is_empty() {
            last = Some(text);
        }
    }
    Ok((last, from + complete as u64))
}

/// The last non-empty lines of a pane, above its bottom rows (prompt box and footer).
fn terminal_tail(herdr: &Path, pane: &str, timeout: Duration) -> Option<String> {
    let bytes = query(
        herdr,
        &[
            "pane",
            "read",
            pane,
            "--source",
            "recent-unwrapped",
            "--lines",
            "80",
        ],
        timeout,
    )
    .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
    let body = &lines[..lines.len().saturating_sub(6)];
    let nonempty = body
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let kept = &nonempty[nonempty.len().saturating_sub(12)..];
    (!kept.is_empty()).then(|| kept.join("\n"))
}

fn notice_text(
    decision: &Decision,
    record: &mut WorkerRecord,
    sources: &Sources<'_>,
    exit_samples: u32,
    now: u64,
) -> (String, Option<Cursor>) {
    let header = match decision.kind {
        NoticeKind::Idle => "finished its turn and is waiting for input".to_owned(),
        NoticeKind::Blocked => {
            "is waiting on outside input (permission dialog or question)".to_owned()
        }
        NoticeKind::StillIdle => format!(
            "is still idle (reminder {} of {MAX_REMINDERS})",
            record.reminders
        ),
        NoticeKind::Exited => {
            let mut text =
                format!("has not been listed by Herdr for {exit_samples} samples in a row");
            if record.last_seen_unix_ms > 0 {
                let lag = now.saturating_sub(record.last_seen_unix_ms) / 1000;
                let lag = if lag < 120 {
                    format!("{lag} s")
                } else {
                    format!("{} min", (lag + 30) / 60)
                };
                text.push_str(&format!(
                    "; last listed at {} {}, {lag} before this notice",
                    super::utc_date(record.last_seen_unix_ms),
                    super::utc_clock(record.last_seen_unix_ms),
                ));
            }
            text
        }
        _ => String::new(),
    };
    let mut cursor = None;
    let mut body = None;
    if decision.kind == NoticeKind::Idle {
        if let Some(path) = record.transcript.clone() {
            if let Ok((text, end)) = last_assistant_text(&path, record.offset) {
                if end > record.offset {
                    cursor = Some(Cursor {
                        path: path.to_string_lossy().into_owned(),
                        start: record.offset,
                        end,
                    });
                }
                record.offset = end;
                body = text.map(|text| format!("last message:\n{text}"));
            }
        }
    }
    if body.is_none() && matches!(decision.kind, NoticeKind::Idle | NoticeKind::Blocked) {
        body = terminal_tail(sources.herdr, &record.pane, sources.timeout)
            .map(|tail| format!("terminal tail:\n{tail}"));
    }
    let mut text = format!("pane {}: {header}", record.pane);
    if let Some(body) = body {
        text.push('\n');
        text.push_str(&body);
    }
    let (text, _) = cut(&text, MAX_TEXT_BYTES);
    (text.to_owned(), cursor)
}

#[derive(Serialize)]
struct Posted {
    agent: String,
    kind: NoticeKind,
    outcome: &'static str,
}

/// One sample: read state, observe, decide, and post.
///
/// A decision's effect on its worker's record is committed (and `watch.json` saved) only after its
/// notice was posted. If a post fails, that worker and every worker whose notice was not posted
/// yet keep their previous records, so the next sample decides the same notices again instead of
/// believing they were sent.
fn cycle(
    inbox: &Inbox,
    sources: &Sources<'_>,
    ignored: &[String],
    policy: Policy,
    max_live: usize,
) -> Result<serde_json::Value> {
    let state_path = inbox.root.join("watch.json");
    let previous = fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<WatchState>(&bytes).ok())
        .unwrap_or_default();
    let now = now_ms()?;
    let (mut observations, listed_anything) = sample(sources, ignored)?;
    disambiguate(&mut observations, &previous);
    let mut next = previous.clone();
    next.schema = 1;
    next.parked = read_parked(inbox);
    let decisions = step(&mut next, &observations, listed_anything, now, policy);
    // Start from the new state with every worker that has a pending notice rolled back.
    let mut committed = next.clone();
    for decision in &decisions {
        match previous.workers.get(&decision.agent) {
            Some(record) => {
                committed
                    .workers
                    .insert(decision.agent.clone(), record.clone());
            }
            None => {
                committed.workers.remove(&decision.agent);
            }
        }
    }
    let mut posted = Vec::new();
    for decision in &decisions {
        let (text, cursor) = match (decision.kind, next.workers.get_mut(&decision.agent)) {
            (NoticeKind::Working, _) | (_, None) => (String::new(), None),
            (_, Some(record)) => notice_text(decision, record, sources, policy.exit_samples, now),
        };
        let result = inbox.post(
            &PostRequest {
                agent: &decision.agent,
                kind: decision.kind,
                text: &text,
                key: None,
                cursor,
                max_live,
                stale_after: DEFAULT_STALE_SECONDS,
            },
            now,
        );
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                inbox.write_json(".", "watch.json", &committed)?;
                return Err(error);
            }
        };
        match next.workers.get(&decision.agent) {
            Some(record) => {
                committed
                    .workers
                    .insert(decision.agent.clone(), record.clone());
            }
            None => {
                committed.workers.remove(&decision.agent);
            }
        }
        inbox.write_json(".", "watch.json", &committed)?;
        posted.push(Posted {
            agent: decision.agent.clone(),
            kind: decision.kind,
            outcome: outcome.outcome,
        });
    }
    inbox.write_json(".", "watch.json", &committed)?;
    Ok(serde_json::json!({
        "sampled_unix_ms": now,
        "workers": observations.len(),
        "posted": posted,
    }))
}

#[derive(Default, Deserialize, Serialize)]
struct Parked {
    #[serde(default)]
    schema: u32,
    workers: std::collections::BTreeSet<String>,
}

/// Parked workers as recorded: none when the file is missing, an error when it cannot be read or
/// is not schema 1. Writers must not overwrite what they cannot read.
fn read_parked_strict(inbox: &Inbox) -> Result<std::collections::BTreeSet<String>> {
    let path = inbox.root.join("parked.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => {
            return Err(InboxError::io(
                &format!("cannot read {}", path.display()),
                &error,
            ));
        }
    };
    match serde_json::from_slice::<Parked>(&bytes) {
        Ok(parked) if parked.schema == 1 => Ok(parked.workers),
        Ok(parked) => Err(InboxError {
            code: 1,
            message: format!(
                "{} has schema {}; this agentctl understands schema 1 and will not rewrite it",
                path.display(),
                parked.schema
            ),
        }),
        Err(error) => Err(InboxError {
            code: 1,
            message: format!(
                "{} is unreadable ({error}); fix or remove it before parking",
                path.display()
            ),
        }),
    }
}

/// Parked workers for the watcher. An unreadable file reads as none parked: that only makes the
/// watcher noisier, never quieter.
fn read_parked(inbox: &Inbox) -> std::collections::BTreeSet<String> {
    read_parked_strict(inbox).unwrap_or_default()
}

/// `agentctl inbox park` arguments.
#[derive(Args)]
pub(crate) struct ParkArgs {
    #[command(flatten)]
    target: Target,
    /// Worker to park or unpark; repeatable. Without any, only prints the parked set
    #[arg(long = "worker", value_name = "NAME")]
    workers: Vec<String>,
    /// Unpark the named workers instead of parking them
    #[arg(long)]
    off: bool,
}

/// Run `agentctl inbox park`: change the parked set and print it as JSON.
pub(crate) fn run_park(registry: &Path, args: ParkArgs) -> Result<i32> {
    for worker in &args.workers {
        check_name("--worker", worker)?;
    }
    let inbox = Inbox::open(registry, &args.target.coordinator)?;
    let _guard = inbox.lock(".lock", true)?;
    let mut parked = Parked {
        schema: 1,
        workers: read_parked_strict(&inbox)?,
    };
    if args.workers.is_empty() {
        print_json(&parked.workers)?;
        return Ok(0);
    }
    for worker in args.workers {
        if args.off {
            parked.workers.remove(&worker);
        } else {
            parked.workers.insert(worker);
        }
    }
    inbox.write_json(".", "parked.json", &parked)?;
    print_json(&parked.workers)?;
    Ok(0)
}

/// Run `agentctl inbox watch`.
pub(crate) fn run(registry: &Path, herdr: &Path, args: WatchArgs) -> Result<i32> {
    let inbox = Inbox::open(registry, &args.target.coordinator)?;
    let _watching = inbox.lock(".watch.lock", false).map_err(|error| {
        if error.exit_code() == EXIT_BUSY {
            InboxError::busy(format!(
                "another watcher already runs for {}",
                args.target.coordinator
            ))
        } else {
            error
        }
    })?;
    let projects = match args.claude_projects {
        Some(path) => path,
        None => std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".claude")
            .join("projects"),
    };
    let sources = Sources {
        herdr,
        claude: &args.claude_bin,
        projects: &projects,
        proc_root: &args.proc_root,
        timeout: QUERY_TIMEOUT,
    };
    let mut ignored = args.exclude.clone();
    ignored.push(args.target.coordinator.clone());
    let policy = Policy {
        idle_samples: args.idle_samples,
        exit_samples: args.exit_samples,
        remind_after_ms: args.remind_minutes.saturating_mul(60_000),
    };
    loop {
        match cycle(&inbox, &sources, &ignored, policy, DEFAULT_MAX_LIVE) {
            Ok(report) => print_json(&report)?,
            Err(error) if !args.once => {
                // A continuous watcher outlives transient failures; the next sample retries.
                eprintln!("agentctl: inbox watch sample failed: {error}");
                print_json(&serde_json::json!({ "error": error.to_string() }))?;
            }
            Err(error) => return Err(error),
        }
        if args.once {
            return Ok(0);
        }
        std::thread::sleep(Duration::from_secs(args.interval.max(1)));
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{exclusive, shared};
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    /// `step` for samples where Herdr listed exactly the observed panes.
    fn step_listed(
        state: &mut WatchState,
        sample: &[Observation],
        now: u64,
        policy: Policy,
    ) -> Vec<Decision> {
        step(state, sample, !sample.is_empty(), now, policy)
    }

    const POLICY: Policy = Policy {
        idle_samples: 2,
        exit_samples: 3,
        remind_after_ms: 1_000,
    };

    fn seen(agent: &str, state: WorkerState, authoritative: bool) -> Observation {
        Observation {
            agent: agent.into(),
            pane: format!("w:{agent}"),
            state: Some(state),
            authoritative,
            transcript: None,
        }
    }

    fn kinds(decisions: &[Decision]) -> Vec<(&str, NoticeKind)> {
        decisions
            .iter()
            .map(|decision| (decision.agent.as_str(), decision.kind))
            .collect()
    }

    fn run_states(
        state: &mut WatchState,
        agent: &str,
        authoritative: bool,
        states: &[WorkerState],
    ) -> Vec<NoticeKind> {
        let mut all = Vec::new();
        for (index, observed) in states.iter().enumerate() {
            let now = 10 * index as u64;
            all.extend(
                step_listed(state, &[seen(agent, *observed, authoritative)], now, POLICY)
                    .into_iter()
                    .map(|decision| decision.kind),
            );
        }
        all
    }

    #[test]
    fn a_first_sample_announces_only_blocked_workers_and_first_seen_idle_is_not_reminded() {
        let _serial = shared();
        let mut state = WatchState::default();
        let sample = [
            seen("a", WorkerState::Idle, true),
            seen("b", WorkerState::Working, true),
            seen("c", WorkerState::Blocked, false),
        ];
        assert_eq!(
            kinds(&step_listed(&mut state, &sample, 0, POLICY)),
            [("c", NoticeKind::Blocked)]
        );
        assert_eq!(state.workers.len(), 3);
        let later = step_listed(&mut state, &sample, 100_000, POLICY);
        assert!(
            later.is_empty(),
            "no reminder for an idle period never announced: {later:?}"
        );
    }

    #[test]
    fn the_end_of_a_never_posted_idle_period_posts_nothing() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(&mut state, &[seen("a", WorkerState::Idle, true)], 0, POLICY);
        let resumed = step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            10,
            POLICY,
        );
        assert!(resumed.is_empty(), "{resumed:?}");
        let idle = step_listed(
            &mut state,
            &[seen("a", WorkerState::Idle, true)],
            20,
            POLICY,
        );
        assert_eq!(kinds(&idle), [("a", NoticeKind::Idle)]);
        let back = step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            30,
            POLICY,
        );
        assert_eq!(kinds(&back), [("a", NoticeKind::Working)]);
    }

    #[test]
    fn a_quiet_worker_that_exits_and_returns_working_withdraws_the_exited_notice() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(&mut state, &[seen("a", WorkerState::Idle, true)], 0, POLICY);
        let mut decided = Vec::new();
        for now in [10, 20, 30] {
            decided.extend(step(&mut state, &[], true, now, POLICY));
        }
        assert_eq!(kinds(&decided), [("a", NoticeKind::Exited)]);
        let back = step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            40,
            POLICY,
        );
        assert_eq!(kinds(&back), [("a", NoticeKind::Working)]);
    }

    fn parked(state: &mut WatchState, agent: &str) {
        state.parked.insert(agent.to_owned());
    }

    #[test]
    fn a_parked_worker_gets_no_idle_or_reminder_notices() {
        let _serial = shared();
        let mut state = WatchState::default();
        parked(&mut state, "kvm");
        let mut decided = Vec::new();
        let states = [
            WorkerState::Working,
            WorkerState::Idle,
            WorkerState::Idle,
            WorkerState::Working,
            WorkerState::Idle,
        ];
        for (index, observed) in states.into_iter().enumerate() {
            let now = 10_000 * index as u64;
            decided.extend(step_listed(
                &mut state,
                &[seen("kvm", observed, true)],
                now,
                POLICY,
            ));
        }
        for now in (60_000..200_000).step_by(1_000) {
            decided.extend(step_listed(
                &mut state,
                &[seen("kvm", WorkerState::Idle, true)],
                now,
                POLICY,
            ));
        }
        assert!(decided.is_empty(), "{decided:?}");
    }

    #[test]
    fn a_parked_worker_still_reports_blocked_exited_and_replaces_a_standing_notice() {
        use WorkerState::{Blocked, Idle, Working};
        let _serial = shared();
        let mut state = WatchState::default();
        parked(&mut state, "kvm");
        let kinds_seen = run_states(&mut state, "kvm", true, &[Working, Blocked, Idle, Working]);
        assert_eq!(
            kinds_seen,
            [NoticeKind::Blocked, NoticeKind::Idle, NoticeKind::Working],
            "the idle replaces the standing blocked notice and the resume withdraws it"
        );
        let mut decided = Vec::new();
        for now in [100, 110, 120] {
            decided.extend(step(&mut state, &[], true, now, POLICY));
        }
        assert_eq!(kinds(&decided), [("kvm", NoticeKind::Exited)]);
    }

    #[test]
    fn unparking_restores_notices_from_the_next_idle_period() {
        use WorkerState::{Idle, Working};
        let _serial = shared();
        let mut state = WatchState::default();
        parked(&mut state, "kvm");
        assert!(run_states(&mut state, "kvm", true, &[Working, Idle]).is_empty());
        state.parked.clear();
        let mut after = Vec::new();
        for (now, observed) in [(20, Idle), (30, Working), (40, Idle)] {
            after.extend(step_listed(
                &mut state,
                &[seen("kvm", observed, true)],
                now,
                POLICY,
            ));
        }
        assert_eq!(
            kinds(&after),
            [("kvm", NoticeKind::Idle)],
            "the parked idle period stays quiet; the next one is announced"
        );
    }

    #[test]
    fn an_exit_notice_says_when_the_worker_was_last_listed() {
        let _serial = exclusive();
        let directory = scratch();
        let herdr = directory.join("herdr");
        executable(&herdr, "exit 1");
        let sources = Sources {
            herdr: &herdr,
            claude: &herdr,
            projects: &directory,
            proc_root: &directory,
            timeout: Duration::from_secs(5),
        };
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            1_790_712_924_000,
            POLICY,
        );
        let mut record = state.workers["a"].clone();
        let decision = Decision {
            agent: "a".into(),
            kind: NoticeKind::Exited,
        };
        let (text, _) = notice_text(
            &decision,
            &mut record,
            &sources,
            3,
            1_790_712_924_000 + 21 * 60_000,
        );
        assert!(
            text.contains("last listed at 2026-09-29 20:15:24Z, 21 min before this notice"),
            "{text}"
        );
        let (short, _) = notice_text(
            &decision,
            &mut record,
            &sources,
            3,
            1_790_712_924_000 + 90_000,
        );
        assert!(
            short.contains("20:15:24Z, 90 s before this notice"),
            "{short}"
        );
        record.last_seen_unix_ms = 0;
        let (old, _) = notice_text(&decision, &mut record, &sources, 3, 5);
        assert!(
            !old.contains("last listed"),
            "a record without the time says nothing about it"
        );
    }

    #[test]
    fn the_exit_notice_reports_the_last_sample_that_listed_the_worker() {
        let _serial = shared();
        let mut state = WatchState::default();
        let t1 = 1_790_712_924_000;
        let t2 = t1 + 600_000;
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            t1,
            POLICY,
        );
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            t2,
            POLICY,
        );
        for now in [t2 + 30_000, t2 + 60_000, t2 + 90_000] {
            step(&mut state, &[], true, now, POLICY);
        }
        assert_eq!(
            state.workers["a"].last_seen_unix_ms, t2,
            "the later listing, not the first sighting"
        );
    }

    #[test]
    fn a_parked_screen_rule_pane_gets_no_idle_after_the_required_samples() {
        use WorkerState::{Idle, Working};
        let _serial = shared();
        let mut state = WatchState::default();
        parked(&mut state, "codex");
        let kinds_seen = run_states(
            &mut state,
            "codex",
            false,
            &[Working, Idle, Idle, Idle, Working],
        );
        assert!(kinds_seen.is_empty(), "{kinds_seen:?}");
    }

    #[test]
    fn parking_after_an_idle_was_posted_stops_its_reminders() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        let posted = step_listed(
            &mut state,
            &[seen("a", WorkerState::Idle, true)],
            10,
            POLICY,
        );
        assert_eq!(kinds(&posted), [("a", NoticeKind::Idle)]);
        parked(&mut state, "a");
        let mut later = Vec::new();
        for now in (1_000..20_000).step_by(500) {
            later.extend(step_listed(
                &mut state,
                &[seen("a", WorkerState::Idle, true)],
                now,
                POLICY,
            ));
        }
        assert!(later.is_empty(), "no reminders while parked: {later:?}");
    }

    #[test]
    fn a_suppressed_idle_period_earns_no_reminders_after_unparking() {
        let _serial = shared();
        let mut state = WatchState::default();
        parked(&mut state, "a");
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        assert!(step_listed(
            &mut state,
            &[seen("a", WorkerState::Idle, true)],
            10,
            POLICY
        )
        .is_empty());
        state.parked.clear();
        let mut later = Vec::new();
        for now in (1_000..20_000).step_by(500) {
            later.extend(step_listed(
                &mut state,
                &[seen("a", WorkerState::Idle, true)],
                now,
                POLICY,
            ));
        }
        assert!(later.is_empty(), "{later:?}");
    }

    #[test]
    fn the_park_command_edits_the_parked_set_that_cycles_read() {
        let _serial = shared();
        let directory = scratch();
        let registry = directory.join("registry");
        let park = |workers: &[&str], off: bool| {
            run_park(
                &registry,
                ParkArgs {
                    target: Target {
                        coordinator: "coord".into(),
                    },
                    workers: workers.iter().map(|worker| (*worker).to_owned()).collect(),
                    off,
                },
            )
            .unwrap()
        };
        park(&["kvm", "lander"], false);
        park(&["lander"], true);
        let inbox = Inbox::open(&registry, "coord").unwrap();
        assert_eq!(read_parked(&inbox).into_iter().collect::<Vec<_>>(), ["kvm"]);
        let path = inbox.root.join("parked.json");
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        park(&[], false);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            modified,
            "printing the parked set must not write it"
        );
        let corrupt = br#"{"schema":1,"workers":["kvm","lander"],}"#;
        fs::write(&path, corrupt).unwrap();
        assert!(
            read_parked(&inbox).is_empty(),
            "for the watcher an unreadable file parks nobody"
        );
        let attempt = |workers: &[&str], off: bool| {
            run_park(
                &registry,
                ParkArgs {
                    target: Target {
                        coordinator: "coord".into(),
                    },
                    workers: workers.iter().map(|worker| (*worker).to_owned()).collect(),
                    off,
                },
            )
        };
        for (workers, off) in [
            (&[][..], false),
            (&["newone"][..], false),
            (&["kvm"][..], true),
        ] {
            assert_eq!(
                attempt(workers, off).unwrap_err().exit_code(),
                1,
                "{workers:?} {off}"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                corrupt,
                "an unreadable set is left untouched"
            );
        }
        let newer = br#"{"schema":2,"workers":["kvm"],"extra":1}"#;
        fs::write(&path, newer).unwrap();
        assert_eq!(attempt(&["x"], false).unwrap_err().exit_code(), 1);
        assert_eq!(
            fs::read(&path).unwrap(),
            newer,
            "a newer schema is not rewritten"
        );
        let bad = run_park(
            &registry,
            ParkArgs {
                target: Target {
                    coordinator: "coord".into(),
                },
                workers: vec!["Bad Name".into()],
                off: false,
            },
        );
        assert_eq!(bad.unwrap_err().exit_code(), 2);
    }

    #[test]
    fn a_claude_idle_edge_is_announced_at_once_and_a_resume_withdraws_it() {
        let _serial = shared();
        let mut state = WatchState::default();
        let at = |state: &mut WatchState, observed, now| {
            kinds(&step_listed(
                state,
                &[seen("a", observed, true)],
                now,
                POLICY,
            ))
            .into_iter()
            .map(|(_, kind)| kind)
            .collect::<Vec<_>>()
        };
        assert!(at(&mut state, WorkerState::Working, 0).is_empty());
        assert_eq!(
            at(&mut state, WorkerState::Idle, 10),
            [NoticeKind::Idle],
            "in the same sample"
        );
        assert!(
            at(&mut state, WorkerState::Idle, 20).is_empty(),
            "one idle period is announced once"
        );
        assert_eq!(
            at(&mut state, WorkerState::Working, 30),
            [NoticeKind::Working]
        );
        assert!(at(&mut state, WorkerState::Working, 40).is_empty());
    }

    #[test]
    fn a_screen_rule_idle_edge_must_hold_for_the_required_samples() {
        let _serial = shared();
        use WorkerState::{Idle, Working};
        let mut state = WatchState::default();
        let kinds = run_states(
            &mut state,
            "x",
            false,
            &[Working, Idle, Working, Idle, Idle],
        );
        assert_eq!(
            kinds,
            [NoticeKind::Idle],
            "the one-sample blip is neither announced nor withdrawn"
        );
    }

    #[test]
    fn a_blocked_notice_is_withdrawn_even_after_an_unconfirmed_idle_blip() {
        let _serial = shared();
        use WorkerState::{Blocked, Idle, Working};
        let mut state = WatchState::default();
        let kinds = run_states(
            &mut state,
            "x",
            false,
            &[Working, Blocked, Idle, Working, Working],
        );
        assert_eq!(kinds, [NoticeKind::Blocked, NoticeKind::Working]);
        let mut state = WatchState::default();
        let kinds = run_states(&mut state, "x", false, &[Working, Blocked, Idle, Idle]);
        assert_eq!(
            kinds,
            [NoticeKind::Blocked, NoticeKind::Idle],
            "a confirmed idle replaces it"
        );
    }

    #[test]
    fn reminders_back_off_and_stop_after_three() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        step_listed(&mut state, &[seen("a", WorkerState::Idle, true)], 0, POLICY);
        let mut reminded_at = Vec::new();
        for now in (100..20_000).step_by(100) {
            if !step_listed(
                &mut state,
                &[seen("a", WorkerState::Idle, true)],
                now,
                POLICY,
            )
            .is_empty()
            {
                reminded_at.push(now);
            }
        }
        assert_eq!(
            reminded_at,
            [1_000, 3_000, 7_000],
            "waits of 1 s, 2 s and 4 s, then none"
        );
    }

    #[test]
    fn exited_needs_consecutive_absences_and_a_return_is_announced() {
        let _serial = shared();
        let mut state = WatchState::default();
        let working = [
            seen("a", WorkerState::Working, true),
            seen("b", WorkerState::Working, true),
        ];
        step_listed(&mut state, &working, 0, POLICY);
        let only_b = [seen("b", WorkerState::Working, true)];
        assert!(step_listed(&mut state, &only_b, 10, POLICY).is_empty());
        assert!(
            step_listed(&mut state, &working, 20, POLICY).is_empty(),
            "a return resets the count"
        );
        assert!(step_listed(&mut state, &only_b, 30, POLICY).is_empty());
        assert!(step_listed(&mut state, &only_b, 40, POLICY).is_empty());
        assert_eq!(
            kinds(&step_listed(&mut state, &only_b, 50, POLICY)),
            [("a", NoticeKind::Exited)]
        );
        assert!(
            step_listed(&mut state, &only_b, 60, POLICY).is_empty(),
            "exited is announced once"
        );
        let back_idle = [
            seen("a", WorkerState::Idle, true),
            seen("b", WorkerState::Working, true),
        ];
        assert_eq!(
            kinds(&step_listed(&mut state, &back_idle, 70, POLICY)),
            [("a", NoticeKind::Idle)]
        );
        for now in [80, 90, 100] {
            step_listed(&mut state, &only_b, now, POLICY);
        }
        assert_eq!(
            kinds(&step_listed(&mut state, &working, 110, POLICY)),
            [("a", NoticeKind::Working)],
            "a return while working withdraws the exited notice"
        );
    }

    #[test]
    fn an_empty_listing_is_unknown_not_everyone_leaving() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        for now in [10, 20, 30, 40, 50] {
            assert!(step_listed(&mut state, &[], now, POLICY).is_empty());
        }
        assert_eq!(state.workers["a"].missing, 0);
    }

    #[test]
    fn an_unknown_state_changes_nothing() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Idle, true)],
            10,
            POLICY,
        );
        let unknown = Observation {
            state: None,
            ..seen("a", WorkerState::Idle, false)
        };
        for now in [20, 30, 40] {
            assert!(
                step_listed(&mut state, std::slice::from_ref(&unknown), now, POLICY).is_empty()
            );
        }
        assert_eq!(
            kinds(&step_listed(
                &mut state,
                &[seen("a", WorkerState::Working, true)],
                50,
                POLICY
            )),
            [("a", NoticeKind::Working)]
        );
    }

    #[test]
    fn a_transcript_offset_survives_a_missing_path_and_resets_for_a_new_one() {
        let _serial = shared();
        let directory = scratch();
        let first = directory.join("one.jsonl");
        let second = directory.join("two.jsonl");
        fs::write(&first, b"0123456789\n").unwrap();
        fs::write(&second, b"abc\n").unwrap();
        let mut state = WatchState::default();
        let with = |path: &Path| Observation {
            transcript: Some(path.to_path_buf()),
            ..seen("a", WorkerState::Working, true)
        };
        step_listed(&mut state, &[with(&first)], 0, POLICY);
        assert_eq!(
            state.workers["a"].offset, 11,
            "a first sighting starts at the current end"
        );
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            10,
            POLICY,
        );
        assert_eq!(
            (
                state.workers["a"].transcript.as_deref(),
                state.workers["a"].offset
            ),
            (Some(first.as_path()), 11)
        );
        step_listed(&mut state, &[with(&second)], 20, POLICY);
        assert_eq!(
            state.workers["a"].offset, 0,
            "a different transcript is unread from the start"
        );
    }

    #[test]
    fn worker_names_fall_back_to_the_pane_id_and_collisions_are_split() {
        let _serial = shared();
        assert_eq!(worker_name(Some("kvm"), "wJ:p38"), "kvm");
        assert_eq!(worker_name(None, "wJ:p3A"), "pane-wj-p3a");
        assert_eq!(worker_name(Some("Has Space"), "w1:p2"), "pane-w1-p2");
        let mut observations = vec![
            seen("worker", WorkerState::Blocked, false),
            seen("worker", WorkerState::Working, false),
            seen("solo", WorkerState::Idle, false),
        ];
        observations[0].pane = "w:p5".into();
        observations[1].pane = "w:p6".into();
        disambiguate(&mut observations, &WatchState::default());
        let names = observations
            .iter()
            .map(|o| o.agent.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["worker-w-p5", "worker-w-p6", "solo"]);
        let mut clash = vec![
            seen("pane-w-p3", WorkerState::Idle, false),
            seen("pane-w-p3", WorkerState::Idle, false),
        ];
        clash[0].pane = "w:p3".into();
        clash[1].pane = "w-p3".into();
        disambiguate(&mut clash, &WatchState::default());
        assert_ne!(clash[0].agent, clash[1].agent);
        assert!(clash.iter().all(|o| check_name("--from", &o.agent).is_ok()));
    }

    fn scratch() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agentctl-watch-test-{}-{}",
            std::process::id(),
            super::super::hash_hex(
                &[format!("{:?}", std::time::SystemTime::now()).as_bytes()],
                6
            )
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn executable(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_last_assistant_text_since_the_offset_is_extracted() {
        let _serial = shared();
        let directory = scratch();
        let path = directory.join("s.jsonl");
        let first =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"old answer"}]}}"#;
        let tool =
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
        let last = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Pushed 3f2a9c1."}]}}"#;
        fs::write(&path, format!("{first}\n{tool}\n{last}\n{{\"partial")).unwrap();
        let (text, end) = last_assistant_text(&path, 0).unwrap();
        assert_eq!(text.as_deref(), Some("Pushed 3f2a9c1."));
        let complete = format!("{first}\n{tool}\n{last}\n").len() as u64;
        assert_eq!(
            end, complete,
            "a partial final line is left for the next read"
        );
        let (text, again) = last_assistant_text(&path, end).unwrap();
        assert_eq!((text, again), (None, end));
        let (text, _) = last_assistant_text(&path, u64::MAX).unwrap();
        assert_eq!(text, None, "an offset past the end reads nothing");
    }

    fn fixture(directory: &Path, claude_body: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let herdr = directory.join("herdr");
        let claude = directory.join("claude");
        let listing = r#"{"result":{"agents":[
            {"pane_id":"w:p1","name":"coord","agent":"claude","agent_status":"idle"},
            {"pane_id":"w:p2","name":"busy-one","agent":"claude","agent_status":"idle"},
            {"pane_id":"w:p3","name":"codex-one","agent":"codex","agent_status":"blocked"},
            {"pane_id":"w:p4","agent_status":"idle"}
        ]}}"#;
        fs::write(directory.join("list.json"), listing).unwrap();
        executable(
            &herdr,
            &format!("cat {}", directory.join("list.json").display()),
        );
        executable(&claude, claude_body);
        let proc_root = directory.join("proc");
        for (pid, pane) in [("101", "w:p2"), ("250", "w:p2")] {
            fs::create_dir_all(proc_root.join(pid)).unwrap();
            fs::write(
                proc_root.join(pid).join("environ"),
                format!("A=1\0HERDR_PANE_ID={pane}\0B=2\0"),
            )
            .unwrap();
        }
        let projects = directory.join("projects");
        fs::create_dir_all(projects.join("p")).unwrap();
        fs::write(projects.join("p/s-1.jsonl"), b"").unwrap();
        (herdr, claude, proc_root, projects)
    }

    fn summary(observations: &[Observation]) -> Vec<(&str, Option<WorkerState>, bool, bool)> {
        observations
            .iter()
            .map(|o| {
                (
                    o.agent.as_str(),
                    o.state,
                    o.authoritative,
                    o.transcript.is_some(),
                )
            })
            .collect()
    }

    #[test]
    fn a_sample_joins_claude_state_to_herdr_panes_through_proc() {
        let _serial = exclusive();
        let directory = scratch();
        let (herdr, claude, proc_root, projects) = fixture(
            &directory,
            "echo '[{\"pid\":\"250\",\"sessionId\":\"s-child\",\"status\":\"idle\"},{\"pid\":101,\"sessionId\":\"s-1\",\"status\":\"busy\"},{\"id\":\"bg\",\"state\":\"blocked\"}]'",
        );
        let sources = Sources {
            herdr: &herdr,
            claude: &claude,
            projects: &projects,
            proc_root: &proc_root,
            timeout: Duration::from_secs(10),
        };
        let (observations, listed) = sample(&sources, &["coord".to_owned()]).unwrap();
        assert!(listed);
        assert_eq!(
            summary(&observations),
            [
                ("busy-one", Some(WorkerState::Working), true, true),
                ("codex-one", Some(WorkerState::Blocked), false, false)
            ],
            "the lower pid wins the pane; Claude's busy overrides Herdr's idle; the coordinator and agentless panes are skipped"
        );
    }

    #[test]
    fn without_claude_a_claude_pane_is_unknown_rather_than_herdr_idle() {
        let _serial = exclusive();
        let directory = scratch();
        let (herdr, claude, proc_root, projects) = fixture(&directory, "exit 1");
        let sources = Sources {
            herdr: &herdr,
            claude: &claude,
            projects: &projects,
            proc_root: &proc_root,
            timeout: Duration::from_secs(10),
        };
        let (observations, listed) = sample(&sources, &["coord".to_owned()]).unwrap();
        assert!(listed);
        assert_eq!(
            summary(&observations),
            [
                ("busy-one", None, false, false),
                ("codex-one", Some(WorkerState::Blocked), false, false)
            ]
        );
    }

    #[test]
    fn a_descendant_left_holding_the_pipe_after_a_clean_exit_is_killed_at_the_deadline() {
        let _serial = exclusive();
        let directory = scratch();
        let lingering = directory.join("lingering");
        executable(&lingering, "echo partial\n(sleep 30) &\nexit 0");
        let started = Instant::now();
        let error = query(&lingering, &[], Duration::from_secs(1)).unwrap_err();
        assert!(error.to_string().contains("did not finish"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let quick = directory.join("quick");
        executable(&quick, "echo done");
        assert_eq!(
            query(&quick, &[], Duration::from_secs(5)).unwrap(),
            b"done\n"
        );
    }

    #[test]
    fn a_query_timeout_kills_descendants_holding_the_pipe() {
        let _serial = exclusive();
        let directory = scratch();
        let slow = directory.join("slow");
        executable(&slow, "(sleep 30; echo late) &\nwait");
        let started = Instant::now();
        let error = query(&slow, &[], Duration::from_secs(1)).unwrap_err();
        let elapsed = started.elapsed();
        assert!(
            error
                .to_string()
                .contains("did not finish within 1 seconds"),
            "{error}"
        );
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(10),
            "{elapsed:?}"
        );
    }

    #[test]
    fn the_last_worker_leaving_is_announced_while_the_coordinator_is_still_listed() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        let mut announced = Vec::new();
        for now in [10, 20, 30, 40] {
            announced.extend(step(&mut state, &[], true, now, POLICY));
        }
        assert_eq!(kinds(&announced), [("a", NoticeKind::Exited)]);
    }

    #[test]
    fn an_exited_worker_is_forgotten_after_a_day() {
        let _serial = shared();
        let mut state = WatchState::default();
        step_listed(
            &mut state,
            &[seen("a", WorkerState::Working, true)],
            0,
            POLICY,
        );
        for now in [10, 20, 30] {
            step(&mut state, &[], true, now, POLICY);
        }
        assert!(state.workers["a"].exited_unix_ms.is_some());
        step(&mut state, &[], true, 30 + EXITED_MEMORY_MS, POLICY);
        assert!(state.workers.contains_key("a"), "kept for exactly a day");
        step(&mut state, &[], true, 31 + EXITED_MEMORY_MS, POLICY);
        assert!(!state.workers.contains_key("a"));
    }

    #[test]
    fn a_worker_first_seen_with_an_unknown_state_starts_quietly_once_known() {
        let _serial = shared();
        let mut state = WatchState::default();
        let unknown = Observation {
            state: None,
            ..seen("a", WorkerState::Idle, false)
        };
        assert!(step_listed(&mut state, &[unknown], 0, POLICY).is_empty());
        assert!(
            state.workers.is_empty(),
            "nothing is recorded before the state is known"
        );
        assert!(step_listed(
            &mut state,
            &[seen("a", WorkerState::Idle, true)],
            10,
            POLICY
        )
        .is_empty());
    }

    #[test]
    fn a_colliding_newcomer_is_renamed_and_the_owner_keeps_its_name() {
        let _serial = shared();
        let mut state = WatchState::default();
        let mut owner = seen("worker", WorkerState::Working, true);
        owner.pane = "w:p5".into();
        step_listed(&mut state, std::slice::from_ref(&owner), 0, POLICY);
        let mut newcomer = seen("worker", WorkerState::Idle, true);
        newcomer.pane = "w:p6".into();
        let mut sample = vec![newcomer, owner];
        disambiguate(&mut sample, &state);
        let names = sample.iter().map(|o| o.agent.as_str()).collect::<Vec<_>>();
        assert_eq!(names, ["worker-w-p6", "worker"]);
        for now in [10, 20, 30, 40] {
            let decisions = step_listed(&mut state, &sample, now, POLICY);
            assert!(
                !decisions
                    .iter()
                    .any(|decision| decision.kind == NoticeKind::Exited),
                "the owner is still listed: {decisions:?}"
            );
        }
    }

    #[test]
    fn a_failed_post_leaves_its_worker_to_be_decided_again() {
        let _serial = exclusive();
        let directory = scratch();
        let listing = directory.join("list.json");
        let herdr = directory.join("herdr");
        let claude = directory.join("claude");
        executable(&herdr, &format!("cat {}", listing.display()));
        executable(&claude, "exit 1");
        let list = |status: &str| {
            fs::write(
                &listing,
                format!(
                    r#"{{"result":{{"agents":[{{"pane_id":"w:a","name":"wa","agent":"codex","agent_status":"{status}"}},{{"pane_id":"w:b","name":"wb","agent":"codex","agent_status":"{status}"}}]}}}}"#
                ),
            )
            .unwrap();
        };
        let registry = directory.join("registry");
        let inbox = Inbox::open(&registry, "coord").unwrap();
        let sources = Sources {
            herdr: &herdr,
            claude: &claude,
            projects: &directory,
            proc_root: &directory,
            timeout: Duration::from_secs(10),
        };
        let filler = PostRequest {
            agent: "filler",
            kind: NoticeKind::Message,
            text: "occupies one slot",
            key: None,
            cursor: None,
            max_live: 10,
            stale_after: DEFAULT_STALE_SECONDS,
        };
        inbox.post(&filler, 1).unwrap();
        // cycle() reads the real clock, so reminders are off to keep the test independent of load.
        let policy = Policy {
            remind_after_ms: 0,
            ..POLICY
        };
        list("working");
        cycle(&inbox, &sources, &[], policy, 2).unwrap();
        list("idle");
        cycle(&inbox, &sources, &[], policy, 2).unwrap();
        let error = cycle(&inbox, &sources, &[], policy, 2).unwrap_err();
        assert_eq!(error.exit_code(), EXIT_BUSY, "wb's idle does not fit");
        let posted = |inbox: &Inbox| {
            inbox
                .live(now_ms().unwrap(), DEFAULT_STALE_SECONDS)
                .unwrap()
                .into_iter()
                .map(|notice| (notice.agent, notice.kind))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            posted(&inbox),
            [
                ("filler".to_owned(), NoticeKind::Message),
                ("wa".to_owned(), NoticeKind::Idle)
            ]
        );
        let delivered = super::super::DeliveryTarget {
            via: "print",
            session: None,
            limit: usize::MAX,
        };
        inbox
            .deliver(
                now_ms().unwrap(),
                100_000,
                DEFAULT_STALE_SECONDS,
                &delivered,
                |_| Ok(()),
            )
            .unwrap();
        let report = cycle(&inbox, &sources, &[], policy, 2).unwrap();
        assert_eq!(report["posted"][0]["agent"], "wb", "{report}");
        assert_eq!(posted(&inbox), [("wb".to_owned(), NoticeKind::Idle)]);
    }
}
