//! Durable coordinator inbox: notices about workers, coalesced and delivered as one batch.
//!
//! A coordinator that supervises several long-running workers needs to hear when one of them
//! stops, gets stuck, or reports something, without reading every worker's output on a timer and
//! without several writers typing into its session at once. This module keeps those notices in a
//! file-per-notice queue under the registry, applies the coalescing rules below, and renders the
//! queue as one prioritized text block.
//!
//! Layout under `<registry>/.inbox/<coordinator>/`:
//!
//! * `live/<created-ms>-<id>.json` — one queued notice per file.
//! * `claimed/<claim-ms>-<batch>.json` — a batch handed to an adapter whose outcome is unknown.
//!   The next `deliver` re-sends it, unchanged and to the same adapter, before new notices.
//! * `delivered/<claim-ms>-<batch>.json` — delivered batches, newest [`DELIVERED_KEEP`] kept.
//! * `released/<claim-ms>-<batch>.json` — claimed batches an operator gave up on.
//! * `.lock` — held for every mutation; `.deliver.lock` — held for a whole delivery.
//!
//! Coalescing: a worker has at most one live *state* notice (`blocked`, `exited`, `idle`,
//! `still-idle`, `progress`); a newer one replaces it, widens its transcript range, and never
//! lowers its priority. Posting `working` withdraws the live state notice, because the worker
//! resumed on its own. `message` notices are never coalesced; `--id` makes a repeat a no-op.
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Subcommand, ValueEnum};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

mod watch;

/// Most notices one coordinator's queue holds; further posts are refused as busy.
pub(crate) const DEFAULT_MAX_LIVE: usize = 200;
/// Default byte budget for one rendered batch, header and trailer included.
pub(crate) const DEFAULT_RENDER_BYTES: usize = 3000;
/// Largest notice text accepted, in bytes.
pub(crate) const MAX_TEXT_BYTES: usize = 4000;
/// Largest transcript path accepted in `--cursor`, in bytes.
pub(crate) const MAX_CURSOR_PATH_BYTES: usize = 1024;
/// Bytes of one notice's text shown in a rendered batch before it is cut.
pub(crate) const RENDER_TEXT_BYTES: usize = 600;
/// Seconds after which an undelivered priority-3 notice expires.
pub(crate) const DEFAULT_STALE_SECONDS: u64 = 86_400;
/// Delivered batch records kept; duplicate `--id` keys are recognized while their batch is kept.
pub(crate) const DELIVERED_KEEP: usize = 200;
/// Largest batch `agentcloudctl notify` is given, in bytes; the batch budget is capped to it.
pub(crate) const MAX_NOTIFY_BYTES: usize = 100_000;
/// Default seconds one `agentcloudctl notify` may run before it is killed.
pub(crate) const DEFAULT_NOTIFY_TIMEOUT_SECONDS: u64 = 120;
/// Stand-in with the length of a real batch id, used while choosing which notices fit.
const BATCH_ID_PLACEHOLDER: &str = "????????????????";
/// Batch id shown by `render`; the same length as a real id, so render and deliver choose the
/// same notices under the same budget.
const PREVIEW_BATCH_ID: &str = "preview-not-sent";
/// How long a non-blocking lock retries before reporting busy: zero in production, where a busy
/// lock means another process really holds it. In the crate's own tests every test shares one
/// process, and a child forked by any test thread (in any module) inherits the other threads'
/// lock descriptors until its exec, keeping a just-released lock held for a moment; tests
/// therefore retry briefly. A lock that stays held still reports busy, which the concurrent
/// delivery test asserts.
#[cfg(not(test))]
const TEST_LOCK_RETRY: Duration = Duration::ZERO;
#[cfg(test)]
const TEST_LOCK_RETRY: Duration = Duration::from_millis(500);
/// Busy: the queue is full or another delivery is running.
const EXIT_BUSY: i32 = crate::error::EXIT_BUSY;
/// The delivery adapter failed; the batch stays claimed for a retry with the same key.
const EXIT_UNAVAILABLE: i32 = 69;

/// What happened to the worker, or what it said.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum NoticeKind {
    /// Waiting on a permission dialog, a question, or other outside input.
    Blocked,
    /// The worker's session or pane ended.
    Exited,
    /// The worker finished a turn and is waiting for input.
    Idle,
    /// An explicit message from the worker; never coalesced.
    Message,
    /// A reminder that an already-reported idle worker is still idle.
    StillIdle,
    /// New output while the worker keeps working.
    Progress,
    /// The worker resumed; withdraws its live state notice and stores nothing.
    Working,
}

impl NoticeKind {
    fn priority(self) -> u8 {
        match self {
            Self::Blocked | Self::Exited => 1,
            Self::Idle | Self::Message | Self::Working => 2,
            Self::StillIdle | Self::Progress => 3,
        }
    }

    fn is_state(self) -> bool {
        !matches!(self, Self::Message | Self::Working)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Exited => "exited",
            Self::Idle => "idle",
            Self::Message => "message",
            Self::StillIdle => "still-idle",
            Self::Progress => "progress",
            Self::Working => "working",
        }
    }
}

/// A byte range of a worker transcript that the notice covers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Cursor {
    path: String,
    start: u64,
    end: u64,
}

/// One queued notice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Notice {
    schema: u32,
    id: String,
    created_unix_ms: u64,
    updated_unix_ms: u64,
    kind: NoticeKind,
    priority: u8,
    agent: String,
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cursor: Option<Cursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(default)]
    replaced: u32,
}

/// A batch handed to an adapter.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Batch {
    schema: u32,
    id: String,
    coordinator: String,
    created_unix_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivered_unix_ms: Option<u64>,
    via: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    attempts: u32,
    text: String,
    notices: Vec<Notice>,
}

/// Where a delivery goes; a claimed batch is only ever re-sent to the same target.
#[derive(Clone, Copy, Debug)]
struct DeliveryTarget<'a> {
    via: &'a str,
    session: Option<&'a str>,
    /// Largest batch the adapter accepts, in bytes.
    limit: usize,
}

/// Failure with the process exit code the CLI returns.
#[derive(Debug)]
pub(crate) struct InboxError {
    code: i32,
    message: String,
}

impl InboxError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            code: 2,
            message: message.into(),
        }
    }

    fn io(context: &str, error: &io::Error) -> Self {
        Self {
            code: 1,
            message: format!("{context}: {error}"),
        }
    }

    fn busy(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_BUSY,
            message: message.into(),
        }
    }

    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_UNAVAILABLE,
            message: message.into(),
        }
    }

    /// Exit code for this failure.
    pub(crate) fn exit_code(&self) -> i32 {
        self.code
    }
}

impl std::fmt::Display for InboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

type Result<T> = std::result::Result<T, InboxError>;

/// `agentctl inbox` arguments.
#[derive(Args)]
pub(crate) struct InboxArgs {
    #[command(subcommand)]
    command: InboxCommand,
}

#[derive(Subcommand)]
enum InboxCommand {
    /// Print a short introduction to the coordinator inbox
    Quickstart,
    /// Print the coordinator inbox reference: layout, coalescing, ordering, and delivery
    Userguide,
    /// Queue a notice about a worker for a coordinator
    #[command(
        after_help = "Examples:\n  agentctl inbox post --to coord --from kvm --kind idle --text 'Pushed 3f2a9c1; tests green'\n  agentctl inbox post --to coord --from kvm --kind message --id kvm-report-7 --text-file report.txt\n  agentctl inbox post --to coord --from kvm --kind working\n\nA worker keeps at most one live state notice (blocked, exited, idle, still-idle, progress): a\nnewer one replaces it and never lowers its priority. --kind working withdraws it and stores\nnothing. message notices are never coalesced; a repeated --id is a no-op that prints the id of\nthe notice already queued or delivered under that key."
    )]
    Post(Post),
    /// List queued notices as JSON, in delivery order
    #[command(after_help = "Example: agentctl inbox list --to coord")]
    List(ListArgs),
    /// Print the next batch exactly as delivery would send it, without claiming it
    #[command(after_help = "Example: agentctl inbox render --to coord --max-bytes 2000")]
    Render(RenderArgs),
    /// Claim the next batch, hand it to an adapter, and record the outcome
    #[command(
        after_help = "Examples:\n  agentctl inbox deliver --to coord --via print\n  agentctl inbox deliver --to coord --via agentcloud-notify --session SESSION_ID\n\nprint writes the batch to stdout (nothing for an empty queue); it is at-least-once, because a\nbatch whose record cannot be written after printing is printed again next time.\nagentcloud-notify runs `agentcloudctl notify --mode cli-script` with the idempotency key\nagentctl-inbox-<coordinator>-<batch>, caps the batch at 100000 bytes, and prints\n{\"delivered\": N}. If notify fails or times out the batch stays claimed, deliver exits 69, and\nthe next deliver re-sends the same batch with the same key to the same session. A claimed batch\nis never re-sent to a different adapter or session: that exits 2 until the batch is delivered\nor released with `agentctl inbox release`."
    )]
    Deliver(DeliverArgs),
    /// Give up on a claimed batch: move it aside, or put its notices back in the queue
    #[command(
        after_help = "Examples:\n  agentctl inbox release --to coord --batch 5e0c7a9d31f2b8a4\n  agentctl inbox release --to coord --batch 5e0c7a9d31f2b8a4 --requeue\n\nUse this when a claimed batch can no longer reach its adapter or session. Without --requeue the\nbatch moves to released/ and its notices are not delivered. With --requeue its notices return\nto the queue and may reach the coordinator twice if the failed attempt actually landed; a\nrequeued state notice is folded into its worker's newer one at the higher priority."
    )]
    Release(ReleaseArgs),
    /// Watch Herdr workers and post idle, blocked, exited, and reminder notices as their state changes
    #[command(
        after_help = "Examples:\n  agentctl inbox watch --to coord --once\n  agentctl inbox watch --to coord --interval 30 --exclude chat-bridge\n\nEach sample lists Herdr panes and asks `claude agents --json` for the busy or idle state of\nevery Claude session, joined to its pane through HERDR_PANE_ID in /proc/<pid>/environ. Claude's\nstate is used for Claude panes; other panes use Herdr's state and must look idle for\n--idle-samples samples in a row. Workers seen for the first time are recorded without a notice\nunless blocked. An idle notice carries the last assistant message written to the worker's Claude\ntranscript since its previous notice, with that byte range as its cursor, or else the tail of the\nterminal. State lives in <registry>/.inbox/<coordinator>/watch.json; one watcher runs per\ncoordinator (a second exits 75). Each sample prints one JSON line of what it posted. The watcher\nonly posts: deliver batches with `agentctl inbox deliver`."
    )]
    Watch(watch::WatchArgs),
    /// Mark workers parked so the watcher posts no idle or still-idle notices for them
    #[command(
        after_help = "Examples:\n  agentctl inbox park --to coord --worker kvm\n  agentctl inbox park --to coord --worker kvm --off\n  agentctl inbox park --to coord\n\nA parked worker still gets blocked, exited and working notices, and an idle notice that\nreplaces a standing blocked or exited one. Its other idle periods are not announced and earn\nno reminders, including after it is unparked, until it works and goes idle again. The parked set\nlives in <registry>/.inbox/<coordinator>/parked.json; the command prints it as JSON, and without\n--worker only prints it. An unreadable parked.json, or one with an unknown schema, is refused with\nexit 1 and left untouched."
    )]
    Park(watch::ParkArgs),
}

#[derive(Args)]
struct Target {
    /// Coordinator whose inbox this is (1-64 lowercase letters, digits, dots, underscores, hyphens)
    #[arg(long = "to", value_name = "COORDINATOR")]
    coordinator: String,
}

#[derive(Args)]
struct Post {
    #[command(flatten)]
    target: Target,
    /// Worker the notice is about (same character rule as --to)
    #[arg(long = "from", value_name = "AGENT")]
    agent: String,
    /// What happened: blocked and exited are priority 1; idle and message 2; still-idle and progress 3
    #[arg(long, value_enum)]
    kind: NoticeKind,
    /// Notice text (up to 4000 bytes); give this or --text-file, except with --kind working
    #[arg(long, conflicts_with = "text_file")]
    text: Option<String>,
    /// Read the notice text from this file (up to 4000 bytes)
    #[arg(long, value_name = "PATH")]
    text_file: Option<PathBuf>,
    /// Idempotency key for a message notice; posting the same key again changes nothing
    #[arg(long, value_name = "KEY")]
    id: Option<String>,
    /// Transcript range covered, as PATH:START:END byte offsets (START <= END; PATH up to 1024 bytes, no control characters)
    #[arg(long, value_name = "PATH:START:END")]
    cursor: Option<String>,
    /// Most live notices the queue may hold before posts are refused with exit 75
    #[arg(long, default_value_t = DEFAULT_MAX_LIVE, value_name = "N")]
    max_live: usize,
    /// Seconds after which an undelivered priority-3 notice is dropped as stale
    #[arg(long, default_value_t = DEFAULT_STALE_SECONDS, value_name = "SECONDS")]
    stale_after: u64,
}

#[derive(Args)]
struct ListArgs {
    #[command(flatten)]
    target: Target,
    /// Seconds after which an undelivered priority-3 notice is dropped as stale
    #[arg(long, default_value_t = DEFAULT_STALE_SECONDS, value_name = "SECONDS")]
    stale_after: u64,
}

#[derive(Args)]
struct RenderArgs {
    #[command(flatten)]
    target: Target,
    /// Byte budget for the batch including its header and trailer; notices that do not fit stay queued (the first always fits)
    #[arg(long, default_value_t = DEFAULT_RENDER_BYTES, value_name = "BYTES")]
    max_bytes: usize,
    /// Seconds after which an undelivered priority-3 notice is dropped as stale
    #[arg(long, default_value_t = DEFAULT_STALE_SECONDS, value_name = "SECONDS")]
    stale_after: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum Via {
    /// Write the batch to stdout; printing counts as delivery (at-least-once)
    Print,
    /// Deliver into an agentcloud session with `agentcloudctl notify --mode cli-script`
    AgentcloudNotify,
}

impl Via {
    fn name(self) -> &'static str {
        match self {
            Self::Print => "print",
            Self::AgentcloudNotify => "agentcloud-notify",
        }
    }
}

#[derive(Args)]
struct DeliverArgs {
    #[command(flatten)]
    render: RenderArgs,
    /// Delivery adapter
    #[arg(long, value_enum)]
    via: Via,
    /// agentcloud session that receives the batch (agentcloud-notify only)
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Seconds one agentcloudctl notify may run before it is killed and the batch stays claimed (agentcloud-notify only)
    #[arg(long, default_value_t = DEFAULT_NOTIFY_TIMEOUT_SECONDS, value_name = "SECONDS")]
    notify_timeout: u64,
}

#[derive(Args)]
struct ReleaseArgs {
    #[command(flatten)]
    target: Target,
    /// Id of the claimed batch (the part after the claim time in its claimed/ file name)
    #[arg(long, value_name = "BATCH")]
    batch: String,
    /// Put the batch's notices back in the queue instead of moving the batch to released/
    #[arg(long)]
    requeue: bool,
}

/// Run `agentctl inbox`.
pub(crate) fn run(
    registry: &Path,
    herdr: &Path,
    agentcloudctl: &Path,
    args: InboxArgs,
) -> Result<i32> {
    let now = now_ms()?;
    match args.command {
        InboxCommand::Quickstart => {
            print!("{}", crate::INBOX_QUICKSTART);
            Ok(0)
        }
        InboxCommand::Userguide => {
            print!("{}", crate::INBOX_USER_GUIDE);
            Ok(0)
        }
        InboxCommand::Post(post) => {
            let inbox = Inbox::open(registry, &post.target.coordinator)?;
            let text = match (&post.text, &post.text_file) {
                (Some(text), None) => text.clone(),
                (None, Some(path)) => fs::read_to_string(path).map_err(|error| {
                    InboxError::io(&format!("cannot read {}", path.display()), &error)
                })?,
                (None, None) if post.kind == NoticeKind::Working => String::new(),
                (None, None) => return Err(InboxError::usage("give --text or --text-file")),
                (Some(_), Some(_)) => unreachable!("clap rejects --text with --text-file"),
            };
            let cursor = post.cursor.as_deref().map(parse_cursor).transpose()?;
            let outcome = inbox.post(
                &PostRequest {
                    agent: &post.agent,
                    kind: post.kind,
                    text: &text,
                    key: post.id.as_deref(),
                    cursor,
                    max_live: post.max_live,
                    stale_after: post.stale_after,
                },
                now,
            )?;
            print_json(&outcome)?;
            Ok(0)
        }
        InboxCommand::List(list) => {
            let inbox = Inbox::open(registry, &list.target.coordinator)?;
            let notices = inbox.live(now, list.stale_after)?;
            print_json(&notices)?;
            Ok(0)
        }
        InboxCommand::Render(render) => {
            let inbox = Inbox::open(registry, &render.target.coordinator)?;
            let notices = inbox.live(now, render.stale_after)?;
            let (text, _) = render_batch(
                &render.target.coordinator,
                PREVIEW_BATCH_ID,
                &notices,
                render.max_bytes,
            );
            print!("{text}");
            Ok(0)
        }
        InboxCommand::Deliver(deliver) => {
            let coordinator = deliver.render.target.coordinator.clone();
            let session = deliver.session.as_deref().filter(|value| !value.is_empty());
            match (deliver.via, session) {
                (Via::AgentcloudNotify, None) => {
                    return Err(InboxError::usage("--via agentcloud-notify needs --session"));
                }
                (Via::Print, Some(_)) => {
                    return Err(InboxError::usage(
                        "--session applies only to --via agentcloud-notify",
                    ));
                }
                _ => {}
            }
            let inbox = Inbox::open(registry, &coordinator)?;
            let timeout = Duration::from_secs(deliver.notify_timeout);
            let adapter = |batch: &Batch| -> Result<()> {
                match deliver.via {
                    Via::Print => {
                        let mut output = io::stdout().lock();
                        output
                            .write_all(batch.text.as_bytes())
                            .and_then(|()| output.flush())
                            .map_err(|error| InboxError::io("cannot write batch", &error))
                    }
                    Via::AgentcloudNotify => notify_agentcloud(
                        &inbox.root,
                        agentcloudctl,
                        session.unwrap_or_default(),
                        &idempotency_key(&coordinator, &batch.id),
                        &batch.text,
                        timeout,
                    ),
                }
            };
            let target = DeliveryTarget {
                via: deliver.via.name(),
                session,
                limit: match deliver.via {
                    Via::Print => usize::MAX,
                    Via::AgentcloudNotify => MAX_NOTIFY_BYTES,
                },
            };
            let delivered = inbox.deliver(
                now,
                deliver.render.max_bytes,
                deliver.render.stale_after,
                &target,
                adapter,
            )?;
            if deliver.via != Via::Print {
                print_json(&serde_json::json!({ "delivered": delivered }))?;
            }
            Ok(0)
        }
        InboxCommand::Watch(value) => watch::run(registry, herdr, value),
        InboxCommand::Park(value) => watch::run_park(registry, value),
        InboxCommand::Release(release) => {
            let inbox = Inbox::open(registry, &release.target.coordinator)?;
            let outcome = inbox.release(&release.batch, release.requeue)?;
            print_json(&outcome)?;
            Ok(0)
        }
    }
}

fn idempotency_key(coordinator: &str, batch: &str) -> String {
    format!("agentctl-inbox-{coordinator}-{batch}")
}

struct PostRequest<'a> {
    agent: &'a str,
    kind: NoticeKind,
    text: &'a str,
    key: Option<&'a str>,
    cursor: Option<Cursor>,
    max_live: usize,
    stale_after: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct PostOutcome {
    /// Id of the stored notice, or of the notice already holding a repeated key.
    id: Option<String>,
    /// `stored`, `replaced`, `duplicate`, `withdrawn`, or `nothing-to-withdraw`.
    outcome: &'static str,
    /// Id of the notice this one replaced or withdrew.
    #[serde(skip_serializing_if = "Option::is_none")]
    previous: Option<String>,
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct ReleaseOutcome {
    batch: String,
    /// `released` or `requeued`.
    outcome: &'static str,
    /// Notices returned to the queue.
    requeued: usize,
    /// State notices folded into their worker's newer live state notice, which keeps the higher
    /// of the two priorities.
    merged: usize,
}

struct Inbox {
    root: PathBuf,
    coordinator: String,
}

impl Inbox {
    fn open(registry: &Path, coordinator: &str) -> Result<Self> {
        check_name("--to", coordinator)?;
        let root = registry.join(".inbox").join(coordinator);
        for directory in ["live", "claimed", "delivered", "released"] {
            fs::create_dir_all(root.join(directory)).map_err(|error| {
                InboxError::io(&format!("cannot create {}", root.display()), &error)
            })?;
        }
        Ok(Self {
            root,
            coordinator: coordinator.to_owned(),
        })
    }

    fn lock(&self, name: &str, wait: bool) -> Result<fs::File> {
        let path = self.root.join(name);
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| InboxError::io(&format!("cannot open {}", path.display()), &error))?;
        if wait {
            file.lock_exclusive().map_err(|error| {
                InboxError::io(&format!("cannot lock {}", path.display()), &error)
            })?;
        } else {
            let deadline = std::time::Instant::now() + TEST_LOCK_RETRY;
            while file.try_lock_exclusive().is_err() {
                if std::time::Instant::now() >= deadline {
                    return Err(InboxError::busy(format!(
                        "another process holds {}; retry later",
                        path.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(file)
    }

    fn read_dir_json<T: for<'de> Deserialize<'de>>(
        &self,
        directory: &str,
    ) -> Result<Vec<(PathBuf, T)>> {
        let path = self.root.join(directory);
        let mut entries = Vec::new();
        for entry in fs::read_dir(&path)
            .map_err(|error| InboxError::io(&format!("cannot list {}", path.display()), &error))?
        {
            let entry = entry.map_err(|error| {
                InboxError::io(&format!("cannot list {}", path.display()), &error)
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let bytes = fs::read(entry.path()).map_err(|error| {
                InboxError::io(&format!("cannot read {}", entry.path().display()), &error)
            })?;
            let value = serde_json::from_slice(&bytes).map_err(|error| InboxError {
                code: 1,
                message: format!("corrupt inbox record {}: {error}", entry.path().display()),
            })?;
            entries.push((entry.path(), value));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(entries)
    }

    /// Record files in a directory, in name order, without parsing them.
    fn record_paths(&self, directory: &str) -> Result<Vec<PathBuf>> {
        let path = self.root.join(directory);
        let mut paths = Vec::new();
        for entry in fs::read_dir(&path)
            .map_err(|error| InboxError::io(&format!("cannot list {}", path.display()), &error))?
        {
            let entry = entry.map_err(|error| {
                InboxError::io(&format!("cannot list {}", path.display()), &error)
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with('.') && name.ends_with(".json") {
                paths.push(entry.path());
            }
        }
        paths.sort();
        Ok(paths)
    }

    /// Delivered records that parse as `T`. Delivered records are history: an unreadable one is
    /// skipped so that it cannot stop new posts or deliveries.
    fn delivered_records<T: for<'de> Deserialize<'de>>(&self) -> Result<Vec<T>> {
        Ok(self
            .record_paths("delivered")?
            .into_iter()
            .filter_map(|path| {
                fs::read(path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            })
            .collect())
    }

    /// Write a record by rename, then sync the directory so the rename itself is durable.
    fn write_json(&self, directory: &str, name: &str, value: &impl Serialize) -> Result<PathBuf> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|error| InboxError {
            code: 1,
            message: format!("cannot encode inbox record: {error}"),
        })?;
        let parent = self.root.join(directory);
        let final_path = parent.join(name);
        let staging = parent.join(format!(".{name}.{}.tmp", std::process::id()));
        let mut file = fs::File::create(&staging).map_err(|error| {
            InboxError::io(&format!("cannot write {}", staging.display()), &error)
        })?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| {
                InboxError::io(&format!("cannot write {}", staging.display()), &error)
            })?;
        fs::rename(&staging, &final_path).map_err(|error| {
            InboxError::io(&format!("cannot publish {}", final_path.display()), &error)
        })?;
        fs::File::open(&parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                InboxError::io(&format!("cannot sync {}", parent.display()), &error)
            })?;
        Ok(final_path)
    }

    fn remove(path: &Path, what: &str) -> Result<()> {
        fs::remove_file(path)
            .map_err(|error| InboxError::io(&format!("cannot {what} {}", path.display()), &error))
    }

    /// Live notices in delivery order, after dropping stale priority-3 notices.
    fn live(&self, now: u64, stale_after: u64) -> Result<Vec<Notice>> {
        let _guard = self.lock(".lock", true)?;
        self.live_locked(now, stale_after)
    }

    fn live_locked(&self, now: u64, stale_after: u64) -> Result<Vec<Notice>> {
        let mut notices = self
            .unexpired_locked(now, stale_after)?
            .into_iter()
            .map(|(_, notice)| notice)
            .collect::<Vec<_>>();
        notices.sort_by(|left, right| {
            (left.priority, left.created_unix_ms, &left.id).cmp(&(
                right.priority,
                right.created_unix_ms,
                &right.id,
            ))
        });
        Ok(notices)
    }

    /// Live notice files, after removing stale priority-3 notices; caller holds `.lock`.
    fn unexpired_locked(&self, now: u64, stale_after: u64) -> Result<Vec<(PathBuf, Notice)>> {
        let mut notices = Vec::new();
        for (path, notice) in self.read_dir_json::<Notice>("live")? {
            if notice.priority == 3
                && now.saturating_sub(notice.updated_unix_ms) > stale_after.saturating_mul(1000)
            {
                Self::remove(&path, "expire")?;
                continue;
            }
            notices.push((path, notice));
        }
        Ok(notices)
    }

    fn post(&self, request: &PostRequest<'_>, now: u64) -> Result<PostOutcome> {
        check_name("--from", request.agent)?;
        if request.text.len() > MAX_TEXT_BYTES {
            return Err(InboxError::usage(format!(
                "notice text is {} bytes; the limit is {MAX_TEXT_BYTES}",
                request.text.len()
            )));
        }
        if request.key.is_some() && request.kind != NoticeKind::Message {
            return Err(InboxError::usage("--id applies only to --kind message"));
        }
        if request.kind == NoticeKind::Working
            && (!request.text.is_empty() || request.cursor.is_some())
        {
            return Err(InboxError::usage("--kind working takes no text or cursor"));
        }
        let _guard = self.lock(".lock", true)?;
        let live = self.unexpired_locked(now, request.stale_after)?;
        if let Some(key) = request.key {
            if let Some((_, existing)) = live
                .iter()
                .find(|(_, notice)| notice.key.as_deref() == Some(key))
            {
                return Ok(PostOutcome {
                    id: Some(existing.id.clone()),
                    outcome: "duplicate",
                    previous: None,
                });
            }
            if let Some(id) = self.batched_key(key)? {
                return Ok(PostOutcome {
                    id: Some(id),
                    outcome: "duplicate",
                    previous: None,
                });
            }
        }
        let previous = (request.kind.is_state() || request.kind == NoticeKind::Working)
            .then(|| {
                live.iter()
                    .find(|(_, notice)| notice.agent == request.agent && notice.kind.is_state())
            })
            .flatten();
        if request.kind == NoticeKind::Working {
            return Ok(match previous {
                Some((path, notice)) => {
                    Self::remove(path, "withdraw")?;
                    PostOutcome {
                        id: None,
                        outcome: "withdrawn",
                        previous: Some(notice.id.clone()),
                    }
                }
                None => PostOutcome {
                    id: None,
                    outcome: "nothing-to-withdraw",
                    previous: None,
                },
            });
        }
        if previous.is_none() && live.len() >= request.max_live {
            return Err(InboxError::busy(format!(
                "inbox for {} already holds {} notices (limit {})",
                self.coordinator,
                live.len(),
                request.max_live
            )));
        }
        let id = notice_id(request.agent, request.kind, request.key, now);
        let (created, cursor, replaced, priority) = match previous {
            Some((_, old)) => (
                old.created_unix_ms,
                merge_cursor(old.cursor.as_ref(), request.cursor.as_ref()),
                old.replaced.saturating_add(1),
                old.priority.min(request.kind.priority()),
            ),
            None => (now, request.cursor.clone(), 0, request.kind.priority()),
        };
        let notice = Notice {
            schema: 1,
            id: id.clone(),
            created_unix_ms: created,
            updated_unix_ms: now,
            kind: request.kind,
            priority,
            agent: request.agent.to_owned(),
            text: request.text.to_owned(),
            cursor,
            key: request.key.map(str::to_owned),
            replaced,
        };
        self.write_json("live", &format!("{created:013}-{id}.json"), &notice)?;
        if let Some((path, old)) = previous {
            Self::remove(path, "replace")?;
            return Ok(PostOutcome {
                id: Some(id),
                outcome: "replaced",
                previous: Some(old.id.clone()),
            });
        }
        Ok(PostOutcome {
            id: Some(id),
            outcome: "stored",
            previous: None,
        })
    }

    /// Id of the notice that already carries `key` in a claimed or kept delivered batch.
    fn batched_key(&self, key: &str) -> Result<Option<String>> {
        let claimed = self
            .read_dir_json::<Batch>("claimed")?
            .into_iter()
            .map(|(_, batch)| batch);
        for batch in claimed.chain(self.delivered_records::<Batch>()?) {
            if let Some(notice) = batch
                .notices
                .iter()
                .find(|notice| notice.key.as_deref() == Some(key))
            {
                return Ok(Some(notice.id.clone()));
            }
        }
        Ok(None)
    }

    /// Remove live copies of notices that a claimed or delivered batch already holds. A crash
    /// between writing a claim and unlinking its live files leaves such copies behind.
    fn drop_batched_live_copies(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct Held {
            notices: Vec<HeldNotice>,
        }
        #[derive(Deserialize)]
        struct HeldNotice {
            id: String,
        }
        let mut batched = std::collections::HashSet::new();
        for (_, batch) in self.read_dir_json::<Held>("claimed")? {
            batched.extend(batch.notices.into_iter().map(|notice| notice.id));
        }
        for batch in self.delivered_records::<Held>()? {
            batched.extend(batch.notices.into_iter().map(|notice| notice.id));
        }
        for (path, notice) in self.read_dir_json::<Notice>("live")? {
            if batched.contains(&notice.id) {
                Self::remove(&path, "drop duplicate")?;
            }
        }
        Ok(())
    }

    /// Claim, deliver, and record one batch; returns the number of notices delivered.
    fn deliver(
        &self,
        now: u64,
        max_bytes: usize,
        stale_after: u64,
        target: &DeliveryTarget<'_>,
        adapter: impl Fn(&Batch) -> Result<()>,
    ) -> Result<usize> {
        let _delivery = self.lock(".deliver.lock", false)?;
        let (path, mut batch) = {
            let _guard = self.lock(".lock", true)?;
            self.drop_batched_live_copies()?;
            if let Some((path, batch)) = self.read_dir_json::<Batch>("claimed")?.into_iter().next()
            {
                if batch.via != target.via || batch.session.as_deref() != target.session {
                    return Err(InboxError::usage(format!(
                        "batch {} is claimed for {}{}; re-run deliver with that adapter and session, or run `agentctl inbox release --to {} --batch {}`",
                        batch.id,
                        batch.via,
                        batch
                            .session
                            .as_deref()
                            .map(|session| format!(" session {session}"))
                            .unwrap_or_default(),
                        self.coordinator,
                        batch.id
                    )));
                }
                (path, batch)
            } else {
                let notices = self.live_locked(now, stale_after)?;
                if notices.is_empty() {
                    return Ok(0);
                }
                // The id depends on which notices fit, so render with a placeholder of the same
                // length first; the budget arithmetic is unchanged by the substitution.
                let (text, taken) = render_batch(
                    &self.coordinator,
                    BATCH_ID_PLACEHOLDER,
                    &notices,
                    max_bytes.min(target.limit),
                );
                let notices = notices.into_iter().take(taken).collect::<Vec<_>>();
                let id = batch_id(&self.coordinator, target, &notices);
                let text = text.replacen(
                    &format!("(batch {BATCH_ID_PLACEHOLDER})"),
                    &format!("(batch {id})"),
                    1,
                );
                if text.len() > target.limit {
                    return Err(InboxError::usage(format!(
                        "the first notice alone renders to {} bytes, over the {} limit of {} bytes",
                        text.len(),
                        target.via,
                        target.limit
                    )));
                }
                let batch = Batch {
                    schema: 1,
                    id: id.clone(),
                    coordinator: self.coordinator.clone(),
                    created_unix_ms: now,
                    delivered_unix_ms: None,
                    via: target.via.to_owned(),
                    session: target.session.map(str::to_owned),
                    attempts: 0,
                    text,
                    notices,
                };
                let path = self.write_json("claimed", &format!("{now:013}-{id}.json"), &batch)?;
                self.drop_batched_live_copies()?;
                (path, batch)
            }
        };
        batch.attempts = batch.attempts.saturating_add(1);
        let attempt = adapter(&batch);
        let _guard = self.lock(".lock", true)?;
        if let Err(error) = attempt {
            self.write_json("claimed", &file_name(&path), &batch)?;
            return Err(InboxError::unavailable(format!(
                "batch {} stays claimed after attempt {} and the next deliver re-sends it with the same key: {error}",
                batch.id, batch.attempts
            )));
        }
        batch.delivered_unix_ms = Some(now_ms()?);
        self.write_json("delivered", &file_name(&path), &batch)?;
        Self::remove(&path, "retire")?;
        self.prune_delivered()?;
        Ok(batch.notices.len())
    }

    fn release(&self, batch_id: &str, requeue: bool) -> Result<ReleaseOutcome> {
        let _delivery = self.lock(".deliver.lock", false)?;
        let _guard = self.lock(".lock", true)?;
        let Some((path, batch)) = self
            .read_dir_json::<Batch>("claimed")?
            .into_iter()
            .find(|(_, batch)| batch.id == batch_id)
        else {
            return Err(InboxError::usage(format!(
                "no claimed batch {batch_id} in the inbox for {}",
                self.coordinator
            )));
        };
        let mut outcome = ReleaseOutcome {
            batch: batch.id.clone(),
            outcome: if requeue { "requeued" } else { "released" },
            requeued: 0,
            merged: 0,
        };
        if requeue {
            let live = self.read_dir_json::<Notice>("live")?;
            for notice in &batch.notices {
                let newer = live.iter().find(|(_, current)| {
                    notice.kind.is_state()
                        && current.agent == notice.agent
                        && current.kind.is_state()
                });
                if let Some((live_path, current)) = newer {
                    if notice.priority < current.priority {
                        let raised = Notice {
                            priority: notice.priority,
                            ..current.clone()
                        };
                        self.write_json("live", &file_name(live_path), &raised)?;
                    }
                    outcome.merged += 1;
                    continue;
                }
                self.write_json(
                    "live",
                    &format!("{:013}-{}.json", notice.created_unix_ms, notice.id),
                    notice,
                )?;
                outcome.requeued += 1;
            }
            Self::remove(&path, "requeue")?;
        } else {
            self.write_json("released", &file_name(&path), &batch)?;
            Self::remove(&path, "release")?;
        }
        Ok(outcome)
    }

    fn prune_delivered(&self) -> Result<()> {
        let delivered = self.record_paths("delivered")?;
        let excess = delivered.len().saturating_sub(DELIVERED_KEEP);
        for path in delivered.into_iter().take(excess) {
            Self::remove(&path, "prune")?;
        }
        Ok(())
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_owned()
}

fn check_name(flag: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        });
    if valid {
        Ok(())
    } else {
        Err(InboxError::usage(format!(
            "{flag} must be 1-64 lowercase letters, digits, dots, underscores or hyphens, starting with a letter or digit: {value:?}"
        )))
    }
}

fn parse_cursor(value: &str) -> Result<Cursor> {
    let invalid = || {
        InboxError::usage(format!(
            "--cursor must be PATH:START:END with START <= END, and PATH at most {MAX_CURSOR_PATH_BYTES} bytes without control characters"
        ))
    };
    let (rest, end) = value.rsplit_once(':').ok_or_else(invalid)?;
    let (path, start) = rest.rsplit_once(':').ok_or_else(invalid)?;
    let start: u64 = start.parse().map_err(|_| invalid())?;
    let end: u64 = end.parse().map_err(|_| invalid())?;
    if path.is_empty()
        || path.len() > MAX_CURSOR_PATH_BYTES
        || path.chars().any(char::is_control)
        || start > end
    {
        return Err(invalid());
    }
    Ok(Cursor {
        path: path.to_owned(),
        start,
        end,
    })
}

fn merge_cursor(old: Option<&Cursor>, new: Option<&Cursor>) -> Option<Cursor> {
    match (old, new) {
        (Some(old), Some(new)) if old.path == new.path => Some(Cursor {
            path: new.path.clone(),
            start: old.start.min(new.start),
            end: old.end.max(new.end),
        }),
        (_, Some(new)) => Some(new.clone()),
        (old, None) => old.cloned(),
    }
}

fn now_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| InboxError {
            code: 1,
            message: format!("system clock is before 1970: {error}"),
        })?;
    Ok(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

fn hash_hex(parts: &[&[u8]], bytes: usize) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize()[..bytes]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn notice_id(agent: &str, kind: NoticeKind, key: Option<&str>, now: u64) -> String {
    let nonce = format!("{}-{now}-{:?}", std::process::id(), SystemTime::now());
    hash_hex(
        &[
            agent.as_bytes(),
            kind.label().as_bytes(),
            key.unwrap_or_default().as_bytes(),
            nonce.as_bytes(),
        ],
        6,
    )
}

/// Batch id from the coordinator, the delivery target, and the ids of the notices it carries.
/// It is independent of time, so a retry to the same target reuses the same idempotency key, and
/// it differs per adapter and session, so requeued notices sent to another session get a new key.
fn batch_id(coordinator: &str, target: &DeliveryTarget<'_>, notices: &[Notice]) -> String {
    let ids = notices
        .iter()
        .map(|notice| notice.id.as_str())
        .collect::<Vec<_>>()
        .join(",");
    hash_hex(
        &[
            coordinator.as_bytes(),
            target.via.as_bytes(),
            target.session.unwrap_or_default().as_bytes(),
            ids.as_bytes(),
        ],
        8,
    )
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let day_of_year = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = u32::try_from(day_of_year - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn utc_clock(ms: u64) -> String {
    let seconds = ms / 1000;
    let rest = seconds % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn utc_date(ms: u64) -> String {
    let (year, month, day) = civil_from_days(i64::try_from(ms / 86_400_000).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}")
}

fn cut(text: &str, limit: usize) -> (&str, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// Replace control characters other than newline and tab, so notice text cannot move the cursor
/// or overwrite earlier lines when the batch is shown in a terminal.
fn printable(text: &str) -> String {
    text.chars()
        .map(|character| {
            let separator = matches!(character, '\u{2028}' | '\u{2029}');
            if (character.is_control() || separator) && character != '\n' && character != '\t' {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect()
}

fn render_notice(notice: &Notice) -> String {
    let mut block = format!(
        "[P{} {}] {} at {} {}",
        notice.priority,
        notice.kind.label(),
        notice.agent,
        utc_date(notice.updated_unix_ms),
        utc_clock(notice.updated_unix_ms)
    );
    if notice.replaced > 0 {
        block.push_str(&format!(
            " (updated {}x since {})",
            notice.replaced,
            utc_clock(notice.created_unix_ms)
        ));
    }
    block.push('\n');
    let (text, truncated) = cut(notice.text.trim_end(), RENDER_TEXT_BYTES);
    for line in printable(text).lines() {
        block.push_str("  ");
        block.push_str(line);
        block.push('\n');
    }
    if truncated {
        block.push_str(&format!(
            "  [cut at {RENDER_TEXT_BYTES} of {} bytes]\n",
            notice.text.len()
        ));
    }
    if let Some(cursor) = &notice.cursor {
        block.push_str(&format!(
            "  transcript: {} bytes {}..{}\n",
            printable(&cursor.path),
            cursor.start,
            cursor.end
        ));
    }
    block
}

fn render_header(coordinator: &str, batch: &str, taken: usize, total: usize) -> String {
    format!("agentctl inbox for {coordinator}: {taken} of {total} notices (batch {batch})\n")
}

fn render_trailer(coordinator: &str, remaining: usize) -> String {
    if remaining == 0 {
        String::new()
    } else {
        format!("{remaining} more stay queued: agentctl inbox list --to {coordinator}\n")
    }
}

/// Render notices (already in delivery order) into one batch; returns the text and how many
/// notices it includes. The first notice is always included; each later one only while the whole
/// text, header and trailer included, stays within `max_bytes`.
fn render_batch(
    coordinator: &str,
    batch: &str,
    notices: &[Notice],
    max_bytes: usize,
) -> (String, usize) {
    if notices.is_empty() {
        return (String::new(), 0);
    }
    let total = notices.len();
    let mut body = render_notice(&notices[0]);
    let mut taken = 1;
    for notice in &notices[1..] {
        let block = render_notice(notice);
        let size = render_header(coordinator, batch, taken + 1, total).len()
            + body.len()
            + block.len()
            + render_trailer(coordinator, total - taken - 1).len();
        if size > max_bytes {
            break;
        }
        body.push_str(&block);
        taken += 1;
    }
    let mut text = render_header(coordinator, batch, taken, total);
    text.push_str(&body);
    text.push_str(&render_trailer(coordinator, total - taken));
    (text, taken)
}

/// Spawn a command, retrying briefly while its executable is busy for writing (`ETXTBSY`): that
/// happens when the file is being replaced, or while a concurrently forked process still holds an
/// inherited write descriptor to it before its own exec.
pub(crate) fn spawn_retrying(command: &mut Command) -> io::Result<std::process::Child> {
    let mut attempt = 0;
    loop {
        match command.spawn() {
            Err(error) if error.raw_os_error() == Some(libc::ETXTBSY) && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            result => return result,
        }
    }
}

fn notify_agentcloud(
    scratch: &Path,
    agentcloudctl: &Path,
    session: &str,
    key: &str,
    text: &str,
    timeout: Duration,
) -> Result<()> {
    if text.len() > MAX_NOTIFY_BYTES {
        return Err(InboxError::unavailable(format!(
            "batch is {} bytes; notify accepts {MAX_NOTIFY_BYTES}",
            text.len()
        )));
    }
    // One delivery runs at a time per inbox (`.deliver.lock`), so a fixed name cannot collide and
    // a file left by a killed delivery is overwritten by the next one.
    let stderr_path = scratch.join(".notify-stderr");
    let stderr = fs::File::create(&stderr_path).map_err(|error| {
        InboxError::io(&format!("cannot create {}", stderr_path.display()), &error)
    })?;
    let mut child = spawn_retrying(
        Command::new(agentcloudctl)
            .args([
                "notify",
                "--session",
                session,
                "--mode",
                "cli-script",
                "--idempotency-key",
                key,
                "--text",
                text,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr),
    )
    .map_err(|error| {
        let _ = fs::remove_file(&stderr_path);
        InboxError::unavailable(format!("cannot run {}: {error}", agentcloudctl.display()))
    })?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(InboxError::unavailable(format!(
                    "cannot wait for {}: {error}",
                    agentcloudctl.display()
                )));
            }
        }
    };
    let mut detail = Vec::new();
    if let Ok(file) = fs::File::open(&stderr_path) {
        let _ = io::Read::read_to_end(&mut io::Read::take(file, 4096), &mut detail);
    }
    let detail = String::from_utf8_lossy(&detail);
    let _ = fs::remove_file(&stderr_path);
    match status {
        Some(status) if status.success() => Ok(()),
        Some(status) => Err(InboxError::unavailable(format!(
            "{} notify exited {status}: {}",
            agentcloudctl.display(),
            detail.trim()
        ))),
        None => Err(InboxError::unavailable(format!(
            "{} notify did not finish within {} seconds and was killed",
            agentcloudctl.display(),
            timeout.as_secs()
        ))),
    }
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, value)
        .map_err(io::Error::from)
        .and_then(|()| output.write_all(b"\n"))
        .map_err(|error| InboxError::io("cannot write output", &error))
}

/// Test-only coordination. Every test in this binary shares one process, and a child forked by one
/// test thread briefly inherits the other threads' descriptors until its exec: an inbox lock stays
/// held, or a freshly written fake executable stays open for writing. Tests that spawn processes
/// take the guard exclusively; tests that depend on lock timing take it shared.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

    static PROCESS_GUARD: RwLock<()> = RwLock::new(());

    pub(crate) fn shared() -> RwLockReadGuard<'static, ()> {
        PROCESS_GUARD
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn exclusive() -> RwLockWriteGuard<'static, ()> {
        PROCESS_GUARD
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A scratch directory unique across threads and runs.
    pub(crate) fn scratch(prefix: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let index = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{index}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{exclusive, shared};
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt as _;

    const PRINT: DeliveryTarget<'static> = DeliveryTarget {
        via: "print",
        session: None,
        limit: usize::MAX,
    };
    const NOTIFY: DeliveryTarget<'static> = DeliveryTarget {
        via: "agentcloud-notify",
        session: Some("session-a"),
        limit: MAX_NOTIFY_BYTES,
    };

    fn scratch() -> PathBuf {
        super::test_support::scratch("agentctl-inbox-test")
    }

    fn request<'a>(agent: &'a str, kind: NoticeKind, text: &'a str) -> PostRequest<'a> {
        PostRequest {
            agent,
            kind,
            text,
            key: None,
            cursor: None,
            max_live: DEFAULT_MAX_LIVE,
            stale_after: DEFAULT_STALE_SECONDS,
        }
    }

    fn post(inbox: &Inbox, agent: &str, kind: NoticeKind, text: &str, now: u64) -> PostOutcome {
        inbox.post(&request(agent, kind, text), now).unwrap()
    }

    fn agents(notices: &[Notice]) -> Vec<&str> {
        notices.iter().map(|notice| notice.agent.as_str()).collect()
    }

    #[test]
    fn newer_state_replaces_older_and_widens_the_range() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        let first = PostRequest {
            cursor: Some(parse_cursor("/t.jsonl:100:200").unwrap()),
            ..request("kvm", NoticeKind::Progress, "running tests")
        };
        assert_eq!(inbox.post(&first, 1_000).unwrap().outcome, "stored");
        let second = PostRequest {
            kind: NoticeKind::Idle,
            text: "done: 3f2a9c1",
            cursor: Some(parse_cursor("/t.jsonl:200:400").unwrap()),
            ..first
        };
        assert_eq!(inbox.post(&second, 2_000).unwrap().outcome, "replaced");
        let live = inbox.live(2_000, DEFAULT_STALE_SECONDS).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(
            (
                live[0].kind,
                live[0].priority,
                live[0].created_unix_ms,
                live[0].replaced
            ),
            (NoticeKind::Idle, 2, 1_000, 1)
        );
        assert_eq!(
            live[0].cursor,
            Some(Cursor {
                path: "/t.jsonl".into(),
                start: 100,
                end: 400
            })
        );
    }

    #[test]
    fn a_replacement_never_lowers_priority_so_it_cannot_expire_away() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "kvm", NoticeKind::Idle, "idle", 0);
        post(&inbox, "kvm", NoticeKind::StillIdle, "still idle", 1);
        post(&inbox, "b", NoticeKind::Blocked, "needs approval", 2);
        post(&inbox, "b", NoticeKind::Progress, "moving again", 3);
        let live = inbox.live(1_000_000, 0).unwrap();
        let summary = live
            .iter()
            .map(|notice| (notice.agent.as_str(), notice.kind, notice.priority))
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("b", NoticeKind::Progress, 1),
                ("kvm", NoticeKind::StillIdle, 2)
            ]
        );
    }

    #[test]
    fn working_withdraws_the_state_notice_but_not_messages() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "kvm", NoticeKind::Idle, "idle", 1);
        post(&inbox, "kvm", NoticeKind::Message, "note", 2);
        assert_eq!(
            post(&inbox, "kvm", NoticeKind::Working, "", 3).outcome,
            "withdrawn"
        );
        assert_eq!(
            post(&inbox, "kvm", NoticeKind::Working, "", 4).outcome,
            "nothing-to-withdraw"
        );
        let live = inbox.live(5, DEFAULT_STALE_SECONDS).unwrap();
        assert_eq!(
            live.iter().map(|notice| notice.kind).collect::<Vec<_>>(),
            vec![NoticeKind::Message]
        );
    }

    #[test]
    fn messages_are_never_coalesced_and_keys_are_idempotent_across_delivery() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        let keyed = |now| {
            let request = PostRequest {
                key: Some("kvm-report-1"),
                ..request("kvm", NoticeKind::Message, "report")
            };
            inbox.post(&request, now).unwrap()
        };
        let stored = keyed(1);
        assert_eq!(stored.outcome, "stored");
        assert_eq!(
            keyed(2),
            PostOutcome {
                outcome: "duplicate",
                ..stored.clone()
            }
        );
        post(&inbox, "kvm", NoticeKind::Message, "second", 3);
        assert_eq!(inbox.live(4, DEFAULT_STALE_SECONDS).unwrap().len(), 2);
        assert_eq!(
            inbox
                .deliver(5, 10_000, DEFAULT_STALE_SECONDS, &PRINT, |_| Ok(()))
                .unwrap(),
            2
        );
        assert_eq!(
            keyed(6),
            PostOutcome {
                outcome: "duplicate",
                ..stored
            },
            "a delivered key must not be queued again, and must name the delivered notice"
        );
        assert!(inbox.live(7, DEFAULT_STALE_SECONDS).unwrap().is_empty());
    }

    #[test]
    fn order_is_priority_then_age() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Progress, "p3", 1);
        post(&inbox, "b", NoticeKind::Idle, "p2 older", 2);
        post(&inbox, "c", NoticeKind::Blocked, "p1", 3);
        post(&inbox, "d", NoticeKind::Message, "p2 newer", 4);
        assert_eq!(
            agents(&inbox.live(5, DEFAULT_STALE_SECONDS).unwrap()),
            ["c", "b", "d", "a"]
        );
    }

    #[test]
    fn stale_priority_three_notices_expire_and_others_do_not() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::StillIdle, "old reminder", 0);
        post(&inbox, "b", NoticeKind::Idle, "old idle", 0);
        assert_eq!(agents(&inbox.live(10_001, 10).unwrap()), ["b"]);
    }

    #[test]
    fn a_full_queue_refuses_new_workers_but_accepts_replacements_and_ignores_stale_notices() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        let limited = |agent, kind| PostRequest {
            max_live: 1,
            stale_after: 10,
            ..request(agent, kind, "x")
        };
        inbox.post(&limited("a", NoticeKind::Idle), 1).unwrap();
        let refused = inbox.post(&limited("b", NoticeKind::Idle), 2).unwrap_err();
        assert_eq!(refused.exit_code(), EXIT_BUSY);
        assert_eq!(
            inbox
                .post(&limited("a", NoticeKind::Idle), 3)
                .unwrap()
                .outcome,
            "replaced"
        );
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        inbox.post(&limited("a", NoticeKind::Progress), 0).unwrap();
        let outcome = inbox
            .post(&limited("b", NoticeKind::Blocked), 10_001)
            .unwrap();
        assert_eq!(
            outcome.outcome, "stored",
            "a stale notice must not hold capacity"
        );
    }

    #[test]
    fn render_keeps_the_whole_budget_but_always_includes_the_first_notice() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Blocked, &"x".repeat(900), 1);
        post(&inbox, "b", NoticeKind::Idle, "short", 2);
        let notices = inbox.live(3, DEFAULT_STALE_SECONDS).unwrap();
        let (text, taken) = render_batch("coord", "b1", &notices, 100);
        assert_eq!(taken, 1);
        assert!(text.starts_with(
            "agentctl inbox for coord: 1 of 2 notices (batch b1)\n[P1 blocked] a at 1970-01-01 00:00:00Z\n"
        ));
        assert!(text.contains(&format!("[cut at {RENDER_TEXT_BYTES} of 900 bytes]")));
        assert!(text.ends_with("1 more stay queued: agentctl inbox list --to coord\n"));
        let (all, taken) = render_batch("coord", "b2", &notices, 10_000);
        assert_eq!(taken, 2);
        assert!(!all.contains("more stay queued"));
        let (exact, taken) = render_batch("coord", "b2", &notices, all.len());
        assert_eq!(
            (exact.len(), taken),
            (all.len(), 2),
            "a batch that fits exactly is kept whole"
        );
        let (_, taken) = render_batch("coord", "b2", &notices, all.len() - 1);
        assert_eq!(
            taken, 1,
            "one byte short: the header and trailer count against the budget"
        );
    }

    #[test]
    fn control_characters_cannot_forge_lines() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(
            &inbox,
            "a",
            NoticeKind::Idle,
            "ok\r[P1 blocked] fake\x1b[2K\u{2028}[P1 exited] x",
            1,
        );
        let (text, _) = render_batch(
            "coord",
            "b",
            &inbox.live(2, DEFAULT_STALE_SECONDS).unwrap(),
            10_000,
        );
        assert!(!text.contains('\r') && !text.contains('\x1b') && !text.contains('\u{2028}'));
        assert_eq!(text.lines().filter(|line| line.starts_with('[')).count(), 1);
        assert!(parse_cursor("/x\n[P1 blocked] w9:1:2").is_err());
    }

    #[test]
    fn notify_batches_are_capped_and_a_huge_budget_cannot_jam_the_queue() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        // Each notice renders to about 1.7 KB (600 bytes of text plus a 1000-byte path), so 70
        // of them cannot fit in one notify-sized batch.
        let text = "y".repeat(590);
        let cursor = format!("/{}:0:1", "p".repeat(1000));
        for index in 0..70 {
            let agent = format!("w{index}");
            let request = PostRequest {
                cursor: Some(parse_cursor(&cursor).unwrap()),
                ..request(&agent, NoticeKind::Idle, &text)
            };
            inbox.post(&request, index).unwrap();
        }
        let sizes = RefCell::new(Vec::new());
        let mut delivered = 0;
        while !inbox.live(1_000, DEFAULT_STALE_SECONDS).unwrap().is_empty() {
            delivered += inbox
                .deliver(1_000, usize::MAX, DEFAULT_STALE_SECONDS, &NOTIFY, |batch| {
                    sizes.borrow_mut().push(batch.text.len());
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(delivered, 70);
        let sizes = sizes.into_inner();
        assert!(
            sizes.len() >= 2,
            "70 notices of ~1.7 KB need more than one batch"
        );
        assert!(
            sizes.iter().all(|size| *size <= MAX_NOTIFY_BYTES),
            "{sizes:?}"
        );
    }

    #[test]
    fn a_failed_delivery_is_resent_unchanged_before_new_notices() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Idle, "first", 1);
        let seen = RefCell::new(Vec::new());
        let failed = inbox
            .deliver(2, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |batch| {
                seen.borrow_mut()
                    .push((batch.id.clone(), batch.text.clone()));
                Err(InboxError::busy("adapter down"))
            })
            .unwrap_err();
        assert_eq!(failed.exit_code(), EXIT_UNAVAILABLE);
        post(
            &inbox,
            "b",
            NoticeKind::Idle,
            "arrived after the failure",
            3,
        );
        let count = Cell::new(0);
        let delivered = inbox
            .deliver(4_000, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |batch| {
                seen.borrow_mut()
                    .push((batch.id.clone(), batch.text.clone()));
                count.set(batch.notices.len());
                assert_eq!(batch.attempts, 2);
                Ok(())
            })
            .unwrap();
        assert_eq!((delivered, count.get()), (1, 1));
        let seen = seen.into_inner();
        assert_eq!(
            seen[0], seen[1],
            "the retry must reuse the batch id and text"
        );
        assert_eq!(
            agents(&inbox.live(5, DEFAULT_STALE_SECONDS).unwrap()),
            ["b"]
        );
    }

    #[test]
    fn a_claimed_batch_is_not_resent_to_another_adapter_or_session_until_released() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Idle, "first", 1);
        inbox
            .deliver(2, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |_| {
                Err(InboxError::busy("adapter down"))
            })
            .unwrap_err();
        let other_session = DeliveryTarget {
            session: Some("session-b"),
            ..NOTIFY
        };
        for target in [PRINT, other_session] {
            let error = inbox
                .deliver(3, 10_000, DEFAULT_STALE_SECONDS, &target, |_| {
                    panic!("a mismatched retry must not reach the adapter")
                })
                .unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert!(error.to_string().contains("agentctl inbox release"));
        }
        let claimed = inbox.read_dir_json::<Batch>("claimed").unwrap();
        let id = claimed[0].1.id.clone();
        post(&inbox, "a", NoticeKind::Progress, "newer state", 4);
        let outcome = inbox.release(&id, true).unwrap();
        assert_eq!((outcome.requeued, outcome.merged), (0, 1));
        let live = inbox.live(4, DEFAULT_STALE_SECONDS).unwrap();
        assert_eq!(
            (live[0].kind, live[0].priority),
            (NoticeKind::Progress, 2),
            "the requeued idle raises the newer progress notice to its priority"
        );
        inbox
            .deliver(5, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |_| {
                Err(InboxError::busy("adapter down"))
            })
            .unwrap_err();
        let id = inbox.read_dir_json::<Batch>("claimed").unwrap()[0]
            .1
            .id
            .clone();
        post(&inbox, "a", NoticeKind::Blocked, "needs approval", 5);
        assert_eq!(inbox.release(&id, true).unwrap().merged, 1);
        let live = inbox.live(5, DEFAULT_STALE_SECONDS).unwrap();
        assert_eq!(
            (live.len(), live[0].kind, live[0].priority),
            (1, NoticeKind::Blocked, 1),
            "a requeued priority-2 notice never lowers the newer priority-1 notice"
        );
        assert_eq!(
            inbox
                .deliver(5, 10_000, DEFAULT_STALE_SECONDS, &PRINT, |_| Ok(()))
                .unwrap(),
            1
        );
        post(&inbox, "c", NoticeKind::Message, "m", 6);
        inbox
            .deliver(7, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |_| {
                Err(InboxError::busy("adapter down"))
            })
            .unwrap_err();
        let id = inbox.read_dir_json::<Batch>("claimed").unwrap()[0]
            .1
            .id
            .clone();
        assert_eq!(inbox.release(&id, false).unwrap().outcome, "released");
        assert_eq!(inbox.read_dir_json::<Batch>("released").unwrap().len(), 1);
        assert!(inbox.live(8, DEFAULT_STALE_SECONDS).unwrap().is_empty());
        assert_eq!(inbox.release("absent", false).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn requeued_notices_sent_to_another_session_get_a_new_idempotency_key() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Message, "report", 1);
        let ids = RefCell::new(Vec::new());
        inbox
            .deliver(2, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |batch| {
                ids.borrow_mut().push(batch.id.clone());
                Err(InboxError::busy("session gone"))
            })
            .unwrap_err();
        let first = ids.borrow()[0].clone();
        let outcome = inbox.release(&first, true).unwrap();
        assert_eq!((outcome.requeued, outcome.merged), (1, 0));
        let other_session = DeliveryTarget {
            session: Some("session-b"),
            ..NOTIFY
        };
        inbox
            .deliver(3, 10_000, DEFAULT_STALE_SECONDS, &other_session, |batch| {
                ids.borrow_mut().push(batch.id.clone());
                Ok(())
            })
            .unwrap();
        let ids = ids.into_inner();
        assert_ne!(
            idempotency_key("coord", &ids[0]),
            idempotency_key("coord", &ids[1]),
            "the new session must not be deduplicated against the old session's key"
        );
    }

    #[test]
    fn render_and_deliver_choose_the_same_notices() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        for index in 0..12 {
            post(
                &inbox,
                &format!("w{index}"),
                NoticeKind::Idle,
                &"z".repeat(200),
                index,
            );
        }
        let notices = inbox.live(100, DEFAULT_STALE_SECONDS).unwrap();
        for budget in (1_500..3_200).step_by(7) {
            let (_, previewed) = render_batch("coord", PREVIEW_BATCH_ID, &notices, budget);
            let (_, claimed) = render_batch("coord", BATCH_ID_PLACEHOLDER, &notices, budget);
            assert_eq!(previewed, claimed, "budget {budget}");
        }
        assert_eq!(PREVIEW_BATCH_ID.len(), BATCH_ID_PLACEHOLDER.len());
        assert_eq!(
            BATCH_ID_PLACEHOLDER.len(),
            batch_id("coord", &PRINT, &notices).len()
        );
    }

    #[test]
    fn live_copies_left_by_an_interrupted_claim_are_not_delivered_twice() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Blocked, "needs approval", 1);
        let notices = inbox.live(2, DEFAULT_STALE_SECONDS).unwrap();
        let id = batch_id("coord", &NOTIFY, &notices);
        let batch = Batch {
            schema: 1,
            id: id.clone(),
            coordinator: "coord".into(),
            created_unix_ms: 2,
            delivered_unix_ms: None,
            via: "agentcloud-notify".into(),
            session: Some("session-a".into()),
            attempts: 0,
            text: render_batch("coord", &id, &notices, 10_000).0,
            notices,
        };
        inbox
            .write_json("claimed", &format!("{:013}-{id}.json", 2), &batch)
            .unwrap();
        let sent = RefCell::new(Vec::new());
        let deliver = || {
            inbox
                .deliver(3, 10_000, DEFAULT_STALE_SECONDS, &NOTIFY, |batch| {
                    sent.borrow_mut().push(batch.id.clone());
                    Ok(())
                })
                .unwrap()
        };
        assert_eq!(deliver(), 1);
        assert_eq!(
            deliver(),
            0,
            "the leftover live copy must not become a second batch"
        );
        assert_eq!(sent.into_inner(), [id]);
    }

    #[test]
    fn an_unreadable_delivered_record_does_not_block_posts_or_deliveries() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        fs::write(
            inbox.root.join("delivered/0000000000001-bad.json"),
            b"{not json",
        )
        .unwrap();
        let keyed = PostRequest {
            key: Some("k"),
            ..request("a", NoticeKind::Message, "m")
        };
        assert_eq!(inbox.post(&keyed, 1).unwrap().outcome, "stored");
        assert_eq!(
            inbox
                .deliver(2, 10_000, DEFAULT_STALE_SECONDS, &PRINT, |_| Ok(()))
                .unwrap(),
            1,
            "a successful delivery must report success despite the unreadable record"
        );
        assert_eq!(inbox.post(&keyed, 3).unwrap().outcome, "duplicate");
    }

    #[test]
    fn a_second_concurrent_delivery_is_refused_as_busy() {
        let _serial = shared();
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        post(&inbox, "a", NoticeKind::Idle, "x", 1);
        let other = Inbox::open(&registry, "coord").unwrap();
        let delivered = inbox
            .deliver(2, 10_000, DEFAULT_STALE_SECONDS, &PRINT, |_| {
                let error = other
                    .deliver(2, 10_000, DEFAULT_STALE_SECONDS, &PRINT, |_| Ok(()))
                    .unwrap_err();
                assert_eq!(error.exit_code(), EXIT_BUSY);
                Ok(())
            })
            .unwrap();
        assert_eq!(delivered, 1);
    }

    #[test]
    fn names_cursors_and_sizes_are_validated() {
        let _serial = shared();
        assert!(check_name("--to", "coord").is_ok());
        assert!(check_name("--to", "sub-worker_1.a").is_ok());
        for bad in ["", "Coord", "-x", "a/b", "a b", &"a".repeat(65)] {
            assert_eq!(
                check_name("--to", bad).unwrap_err().exit_code(),
                2,
                "{bad:?}"
            );
        }
        assert_eq!(
            parse_cursor("/a:b:1:2").unwrap(),
            Cursor {
                path: "/a:b".into(),
                start: 1,
                end: 2
            }
        );
        let long = format!("/{}:1:2", "p".repeat(MAX_CURSOR_PATH_BYTES));
        for bad in [
            "/a:2:1",
            "/a:1",
            ":1:2",
            "/a:x:2",
            "/a\t:1:2",
            long.as_str(),
        ] {
            assert!(parse_cursor(bad).is_err(), "{bad:?}");
        }
        let registry = scratch();
        let inbox = Inbox::open(&registry, "coord").unwrap();
        let big = "x".repeat(MAX_TEXT_BYTES + 1);
        assert_eq!(
            inbox
                .post(&request("a", NoticeKind::Idle, &big), 1)
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn utc_formatting_matches_known_instants() {
        let _serial = shared();
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_790_712_924_000), "2026-09-29");
        assert_eq!(utc_clock(1_790_712_924_000), "20:15:24Z");
        assert_eq!(utc_clock(1_709_210_096_000), "12:34:56Z");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
    }

    fn cli(registry: &Path, arguments: &[&str]) -> i32 {
        let mut all = vec![
            OsString::from("--registry"),
            registry.as_os_str().to_owned(),
        ];
        all.extend(arguments.iter().map(OsString::from));
        crate::cli::main(all)
    }

    fn fake_agentcloudctl(directory: &Path, body: &str) -> PathBuf {
        let path = directory.join("fake-agentcloudctl");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn cli_exit_codes_match_the_userguide() {
        let _serial = exclusive();
        let registry = scratch();
        let post = ["inbox", "post", "--to", "coord", "--from", "a", "--kind"];
        assert_eq!(
            cli(&registry, &[&post[..], &["idle", "--text", "x"]].concat()),
            0
        );
        assert_eq!(cli(&registry, &[&post[..], &["idle"]].concat()), 2);
        assert_eq!(
            cli(
                &registry,
                &[&post[..], &["idle", "--text", "x", "--id", "k"]].concat()
            ),
            2
        );
        assert_eq!(
            cli(
                &registry,
                &[
                    "inbox",
                    "deliver",
                    "--to",
                    "coord",
                    "--via",
                    "agentcloud-notify"
                ]
            ),
            2
        );
        assert_eq!(
            cli(
                &registry,
                &[
                    "inbox",
                    "deliver",
                    "--to",
                    "coord",
                    "--via",
                    "print",
                    "--session",
                    "s"
                ]
            ),
            2
        );
        assert_eq!(
            cli(
                &registry,
                &[&post[..], &["idle", "--text", "x", "--max-live", "1"]].concat()
            ),
            0,
            "a replacement is accepted at capacity"
        );
        assert_eq!(
            cli(
                &registry,
                &[
                    "inbox",
                    "post",
                    "--to",
                    "coord",
                    "--from",
                    "b",
                    "--kind",
                    "idle",
                    "--text",
                    "x",
                    "--max-live",
                    "1"
                ]
            ),
            EXIT_BUSY
        );
        assert_eq!(
            cli(
                &registry,
                &["inbox", "deliver", "--to", "coord", "--via", "print"]
            ),
            0
        );
    }

    #[test]
    fn cli_notify_passes_the_documented_argv_and_retries_with_the_same_key() {
        let _serial = exclusive();
        let registry = scratch();
        let log = registry.join("argv.log");
        let flag = registry.join("fail");
        let fake = fake_agentcloudctl(
            &registry,
            &format!(
                "printf '%s\\n' \"$@\" >> {log}; echo --- >> {log}; if [ -e {flag} ]; then echo boom >&2; exit 3; fi",
                log = log.display(),
                flag = flag.display()
            ),
        );
        let fake = fake.to_str().unwrap();
        assert_eq!(
            cli(
                &registry,
                &[
                    "inbox",
                    "post",
                    "--to",
                    "coord",
                    "--from",
                    "a",
                    "--kind",
                    "blocked",
                    "--text",
                    "needs approval"
                ]
            ),
            0
        );
        fs::write(&flag, "").unwrap();
        let deliver = [
            "--agentcloudctl-bin",
            fake,
            "inbox",
            "deliver",
            "--to",
            "coord",
            "--via",
            "agentcloud-notify",
            "--session",
            "session-a",
        ];
        assert_eq!(cli(&registry, &deliver), EXIT_UNAVAILABLE);
        fs::remove_file(&flag).unwrap();
        assert_eq!(cli(&registry, &deliver), 0);
        assert_eq!(cli(&registry, &deliver), 0, "an empty queue sends nothing");
        let calls = fs::read_to_string(&log).unwrap();
        let calls = calls
            .split("---\n")
            .filter(|call| !call.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0], calls[1]);
        let argv = calls[0].lines().collect::<Vec<_>>();
        assert_eq!(
            &argv[..7],
            [
                "notify",
                "--session",
                "session-a",
                "--mode",
                "cli-script",
                "--idempotency-key",
                argv[6]
            ]
        );
        assert!(argv[6].starts_with("agentctl-inbox-coord-") && argv[6].len() == 21 + 16);
        assert_eq!(argv[7], "--text");
        assert!(argv[8].starts_with("agentctl inbox for coord: 1 of 1 notices (batch "));
    }

    #[test]
    fn cli_notify_times_out_and_keeps_the_batch_claimed() {
        let _serial = exclusive();
        let registry = scratch();
        let fake = fake_agentcloudctl(&registry, "exec sleep 30");
        let fake = fake.to_str().unwrap();
        cli(
            &registry,
            &[
                "inbox", "post", "--to", "coord", "--from", "a", "--kind", "idle", "--text", "x",
            ],
        );
        let started = Instant::now();
        let code = cli(
            &registry,
            &[
                "--agentcloudctl-bin",
                fake,
                "inbox",
                "deliver",
                "--to",
                "coord",
                "--via",
                "agentcloud-notify",
                "--session",
                "s",
                "--notify-timeout",
                "1",
            ],
        );
        assert_eq!(code, EXIT_UNAVAILABLE);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_secs(1) && elapsed < Duration::from_secs(10),
            "the failure must come from the 1 s timeout, not from a spawn error: {elapsed:?}"
        );
        let inbox = Inbox::open(&registry, "coord").unwrap();
        assert_eq!(inbox.read_dir_json::<Batch>("claimed").unwrap().len(), 1);
    }
}
