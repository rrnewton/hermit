/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Host-fork timelines: copies of a run taken at a scheduler turn or at a
//! choice point (PROTOTYPE).
//!
//! With [`TIMELINE_FORK_ENV`] set to `turn=N,copies=K,dir=DIR`, the scheduler
//! stops at its first drain after committing turn N, where every guest thread
//! is parked (the point `--signal-at` uses), and the run forks K copies of
//! itself, one at a time. With `choice=I` instead of `turn=N` (and a choice
//! controller, `--choice-tape` or `--choice-controller`), the copies are
//! taken inside the pause hook of choice request I (`crate::choice`), and
//! `bytes=B0:B1:...` gives copy k the answer that tape byte Bk would give
//! (0 or absent: the run's own answer), so copies can take different futures. Each copy (a *timeline*) repairs what `fork()` does
//! not copy, through the hooks its backend installed
//! ([`install_backend_hooks`]), and then runs the rest of the guest as the
//! original would have. The parent (the *template*) waits for each copy in
//! turn and then continues its own run.
//!
//! Each timeline's standard output and standard error go to
//! `DIR/timeline-<k>.stdout` and `.stderr`; it starts with its own copy of
//! every regular file's and directory's position. `DIR/template.json` records
//! the template's output offsets at the fork, so a check can compare the
//! template's prefix followed by a timeline's suffix with a fresh run.
//!
//! None of this writes to the run's INFO log: what it measures goes to files
//! in DIR, so a timeline's log can be compared with a fresh run's.

use std::io::Write;
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// The environment variable that asks for timelines.
pub const TIMELINE_FORK_ENV: &str = "HERMIT_TIMELINE_FORK_AT";

/// Where and how many copies to take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkSpec {
    /// The first committed turn at or after which the copies are taken.
    pub turn: Option<u64>,
    /// The choice request at which the copies are taken.
    pub choice: Option<u64>,
    /// At a choice point: copy k answers it as tape byte `bytes[k]` would
    /// (0 or absent: the run's own answer).
    pub bytes: Vec<u8>,
    /// How many copies.
    pub copies: usize,
    /// Run the copies at the same time instead of one after another.
    pub parallel: bool,
    /// Pin copy k to CPU `cpus[k % len]` (empty: the installed
    /// [`CopyPlacement`] chooses, else the copy keeps the template's CPU).
    pub cpus: Vec<usize>,
    /// Where each copy's output and measurements go.
    pub dir: PathBuf,
}

impl ForkSpec {
    /// Parses `turn=N,copies=K,dir=DIR` or `choice=I,copies=K,dir=DIR[,bytes=B0:B1]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut turn = None;
        let mut choice = None;
        let mut bytes = Vec::new();
        let mut copies = 1;
        let mut parallel = false;
        let mut cpus = Vec::new();
        let mut dir = None;
        for part in text.split(',') {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("{TIMELINE_FORK_ENV}: {part:?} is not key=value"))?;
            match key {
                "turn" => turn = Some(value.parse().map_err(|e| format!("turn: {e}"))?),
                "choice" => choice = Some(value.parse().map_err(|e| format!("choice: {e}"))?),
                "bytes" => {
                    bytes = value
                        .split(':')
                        .map(|byte| byte.parse::<u8>().map_err(|e| format!("bytes: {e}")))
                        .collect::<Result<_, _>>()?
                }
                "copies" => copies = value.parse().map_err(|e| format!("copies: {e}"))?,
                "parallel" => parallel = value == "1" || value == "true",
                "cpus" => {
                    cpus = value
                        .split(':')
                        .map(|cpu| cpu.parse::<usize>().map_err(|e| format!("cpus: {e}")))
                        .collect::<Result<_, _>>()?
                }
                "dir" => dir = Some(PathBuf::from(value)),
                other => return Err(format!("{TIMELINE_FORK_ENV}: unknown key {other:?}")),
            }
        }
        if turn.is_some() == choice.is_some() {
            return Err(format!(
                "{TIMELINE_FORK_ENV}: exactly one of turn= and choice= is required"
            ));
        }
        Ok(Self {
            turn,
            choice,
            bytes,
            copies,
            parallel,
            cpus,
            dir: dir.ok_or_else(|| format!("{TIMELINE_FORK_ENV}: dir= is required"))?,
        })
    }
}

/// The spec in [`TIMELINE_FORK_ENV`], if set and valid; an invalid one is
/// reported and ignored.
pub fn spec_from_env() -> Option<ForkSpec> {
    let text = std::env::var(TIMELINE_FORK_ENV).ok()?;
    match ForkSpec::parse(&text) {
        Ok(spec) => Some(spec),
        Err(error) => {
            tracing::warn!("{error}");
            None
        }
    }
}

/// What a backend does around the fork. Each returns a one-line description
/// of what it did, for the measurements.
#[derive(Clone, Copy)]
pub struct BackendHooks {
    /// The backend's name.
    pub name: &'static str,
    /// In the template, before the first fork: read what the copies need.
    pub prepare: fn() -> Result<String, String>,
    /// In each copy, before anything else: rebuild what fork did not copy,
    /// giving the copy its guest memory as its driver chose (`None`: the
    /// backend's default).
    pub repair: fn(Option<MemoryCopy>) -> Result<String, String>,
    /// In each copy, as it ends: what the backend did while it ran.
    pub at_exit: fn() -> String,
}

static HOOKS: OnceLock<BackendHooks> = OnceLock::new();

/// The CPU a copy placed itself on and how it got it, or `None` to keep the
/// template's CPU.
pub type PlacedCpu = Option<(usize, String)>;

/// Where a copy runs, as its driver chose for the copy's batch (the driver's
/// placement policy, minus what only a driver handles). Not pinned unless
/// the driver asks (owner, Slack 6784: no pinning by default without
/// measured proof that it pays off).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyCpu {
    /// Pinned to `cpus[position % cpus.len()]`, CPUs the driver owns.
    Pool(Vec<usize>),
    /// Pinned to an idle CPU the installed [`CopyPlacement`] chooses and
    /// reserves, as a fresh pinned run does; without one, the template's.
    SelfPlaced,
    /// Not pinned, the default: any CPU the run may use
    /// ([`CopyPlacement::allowed`]), and the Linux scheduler places the copy.
    Unpinned,
}

impl CopyCpu {
    /// The pool `cpus`, or [`CopyCpu::Unpinned`] when it is empty.
    pub fn from_cpus(cpus: &[usize]) -> Self {
        if cpus.is_empty() {
            CopyCpu::Unpinned
        } else {
            CopyCpu::Pool(cpus.to_vec())
        }
    }

    /// Parses `pool:A:B`, `self` or `unpinned`.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "self" => Ok(CopyCpu::SelfPlaced),
            "unpinned" => Ok(CopyCpu::Unpinned),
            _ => {
                let cpus = text.strip_prefix("pool:").ok_or_else(|| {
                    format!("placement {text:?} is not pool:CPUS, self or unpinned")
                })?;
                cpus.split(':')
                    .map(|cpu| {
                        cpu.parse::<usize>()
                            .map_err(|e| format!("placement {text:?}: {e}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(CopyCpu::Pool)
            }
        }
    }
}

/// How a copy gets its guest memory, as its driver chose for its batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryCopy {
    /// Shared with the template until the copy writes it (copy-on-write).
    Lazy,
    /// Copied in full while the copy repairs.
    Eager,
}

impl MemoryCopy {
    /// Parses `lazy` or `eager`.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "lazy" => Ok(MemoryCopy::Lazy),
            "eager" => Ok(MemoryCopy::Eager),
            _ => Err(format!("memory {text:?} is not lazy or eager")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            MemoryCopy::Lazy => "lazy",
            MemoryCopy::Eager => "eager",
        }
    }
}

/// The embedding program's CPU placement for copies (Hermit's pinned-CPU
/// placement and reservation). Without it, a [`CopyCpu::SelfPlaced`] copy
/// keeps the template's CPU, so copies resumed at once share that one CPU.
#[derive(Clone, Copy)]
pub struct CopyPlacement {
    /// In the template, before each batch of self-placed forks: refresh what
    /// copies choose from (a load sample).
    pub before_fork: fn(),
    /// In each self-placed copy, first: choose and reserve a CPU for the copy
    /// at this position in its batch, and say how.
    pub place: fn(usize) -> Result<PlacedCpu, String>,
    /// In each unpinned copy, first: every CPU the run may use, as it could
    /// before it pinned itself.
    pub allowed: fn() -> Vec<usize>,
}

static COPY_PLACEMENT: OnceLock<CopyPlacement> = OnceLock::new();

/// Installs the copy placement, once per process, before any fork.
pub fn install_copy_placement(placement: CopyPlacement) {
    let _ = COPY_PLACEMENT.set(placement);
}

/// Refreshes the copy placement's sample before forking self-placed copies.
fn before_placed_forks(cpu: &CopyCpu) {
    if *cpu == CopyCpu::SelfPlaced
        && let Some(placement) = COPY_PLACEMENT.get()
    {
        (placement.before_fork)();
    }
}

/// Installs the running backend's hooks, once per process, and, for a spec
/// that forks at a choice point, the choice pause hook that takes the copies.
pub fn install_backend_hooks(hooks: BackendHooks) {
    let _ = HOOKS.set(hooks);
    // A run that may be copied keeps its calls in flight from the start, and
    // its copies refuse what they would share (owner, Slack 6804).
    if (control_from_env().is_some() || spec_from_env().is_some())
        && !crate::timeline_sharing::install()
    {
        tracing::warn!("timelines: another call gate is installed; copies refuse nothing");
    }
    if let Some(control) = control_from_env() {
        // A driver decides everything; a static fork spec would compete.
        if spec_from_env().is_some() {
            tracing::warn!("{TIMELINE_FORK_ENV} is ignored under {TIMELINE_CONTROL_ENV}");
        }
        install_control(control);
        return;
    }
    if let Some(spec) = spec_from_env().filter(|spec| spec.choice.is_some()) {
        install_choice_fork(spec);
    }
}

fn install_choice_fork(spec: ForkSpec) {
    use crate::choice::PauseAction;
    let Some(at) = spec.choice else {
        return;
    };
    crate::choice::set_pause_hook(Some(Box::new(
        move |request: &crate::choice::ChoiceRequest| {
            if request.index != at {
                return PauseAction::Continue;
            }
            let place = serde_json::json!({
                "choice": at,
                "request": crate::choice::request_json(request),
            });
            let Some(copy) = fork_copies(&spec, place) else {
                return PauseAction::Continue;
            };
            match spec.bytes.get(copy).copied().unwrap_or(0) {
                0 => PauseAction::Continue,
                byte => {
                    use crate::choice::ChoiceSource;
                    let answer = crate::choice::TapeSource::new(vec![byte])
                        .answer(request)
                        .unwrap_or(request.default);
                    PauseAction::Answer(answer)
                }
            }
        },
    )));
}

// ---- Control mode: KVM timelines driven through the Timelines trait ----

/// The environment variable that puts a run under a timeline driver's
/// control: `socket=PATH,id=N,dir=DIR[,stop=J][,cpus=A:B]`. The run pauses at
/// choice request J (if given), connects to the driver's Unix socket and
/// waits for a command, one JSON object per line: `{"answer":V,"until":J'}`
/// answers the request, `{"source":true,"until":J'}` lets the run's own
/// source answer it (each naming the next request to pause at, or null),
/// and `{"capture":true}` turns the run into a template. A template reports
/// `{"captured":...}` and then serves `{"fork":{"ids":[...]}}`, forking one
/// copy per id, and `{"release":true}`, which ends every copy still alive
/// (they live in the template's PID namespace) and then the template, which
/// never runs on. Each copy connects back with `{"hello":{"timeline":id,...}}` and is
/// paused at the template's request. A copy reports `{"exit":code}` when its
/// guest ends.
pub const TIMELINE_CONTROL_ENV: &str = "HERMIT_TIMELINE_CONTROL";

/// What [`TIMELINE_CONTROL_ENV`] says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlSpec {
    /// The driver's socket.
    pub socket: PathBuf,
    /// This run's timeline number; each copy gets its own from the driver.
    pub id: u64,
    /// Where copies write their outputs and measurements.
    pub dir: PathBuf,
    /// The choice request to pause at.
    pub stop: Option<u64>,
    /// Pin copy k to CPU `cpus[k % len]` (empty: the installed
    /// [`CopyPlacement`] chooses, else the copy keeps the template's CPU).
    pub cpus: Vec<usize>,
}

impl ControlSpec {
    /// Parses `socket=PATH,id=N,dir=DIR[,stop=J][,cpus=A:B]`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut socket = None;
        let mut id = None;
        let mut dir = None;
        let mut stop = None;
        let mut cpus = Vec::new();
        for part in text.split(',') {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("{TIMELINE_CONTROL_ENV}: {part:?} is not key=value"))?;
            match key {
                "socket" => socket = Some(PathBuf::from(value)),
                "id" => id = Some(value.parse().map_err(|e| format!("id: {e}"))?),
                "dir" => dir = Some(PathBuf::from(value)),
                "stop" => stop = Some(value.parse().map_err(|e| format!("stop: {e}"))?),
                "cpus" => {
                    cpus = value
                        .split(':')
                        .map(|cpu| cpu.parse::<usize>().map_err(|e| format!("cpus: {e}")))
                        .collect::<Result<_, _>>()?
                }
                other => return Err(format!("{TIMELINE_CONTROL_ENV}: unknown key {other:?}")),
            }
        }
        Ok(Self {
            socket: socket.ok_or_else(|| format!("{TIMELINE_CONTROL_ENV}: socket= is required"))?,
            id: id.ok_or_else(|| format!("{TIMELINE_CONTROL_ENV}: id= is required"))?,
            dir: dir.ok_or_else(|| format!("{TIMELINE_CONTROL_ENV}: dir= is required"))?,
            stop,
            cpus,
        })
    }
}

/// The control spec in the environment, if set and valid; an invalid one is
/// reported and ignored.
pub fn control_from_env() -> Option<ControlSpec> {
    let text = std::env::var(TIMELINE_CONTROL_ENV).ok()?;
    match ControlSpec::parse(&text) {
        Ok(spec) => Some(spec),
        Err(error) => {
            tracing::warn!("{error}");
            None
        }
    }
}

/// One process's connection to the driver.
struct Connection {
    reader: std::io::BufReader<std::os::unix::net::UnixStream>,
    writer: std::os::unix::net::UnixStream,
}

impl Connection {
    fn open(socket: &std::path::Path) -> std::io::Result<Self> {
        let stream = std::os::unix::net::UnixStream::connect(socket)?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: std::io::BufReader::new(stream),
            writer,
        })
    }

    fn send(&mut self, message: &serde_json::Value) -> std::io::Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes())
    }

    /// The next message, or `None` at the end of the stream.
    fn receive(&mut self) -> std::io::Result<Option<serde_json::Value>> {
        use std::io::BufRead;
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

struct Control {
    spec: ControlSpec,
    connection: Option<Connection>,
    /// The template this process was copied from, and the request at which.
    parent: Option<(u64, u64)>,
}

static CONTROL: std::sync::Mutex<Option<Control>> = std::sync::Mutex::new(None);

fn control() -> std::sync::MutexGuard<'static, Option<Control>> {
    CONTROL.lock().unwrap_or_else(|e| e.into_inner())
}

/// A driver's command to a paused run.
enum Command {
    Answer { value: u64, until: Option<u64> },
    Source { until: Option<u64> },
    Capture,
}

fn parse_command(message: &serde_json::Value) -> Result<Command, String> {
    let until = message.get("until").and_then(|until| until.as_u64());
    if let Some(value) = message.get("answer").and_then(|value| value.as_u64()) {
        return Ok(Command::Answer { value, until });
    }
    if message.get("source").is_some() {
        return Ok(Command::Source { until });
    }
    if message.get("capture").is_some() {
        return Ok(Command::Capture);
    }
    Err(format!("unknown command {message}"))
}

/// Connects if this process has not yet, reports the pause at `request`,
/// and waits for the driver's command.
fn pause_at(request: &crate::choice::ChoiceRequest) -> Result<Command, String> {
    let mut guard = control();
    let control = guard.as_mut().ok_or("no control")?;
    if control.connection.is_none() {
        let mut connection = Connection::open(&control.spec.socket)
            .map_err(|e| format!("connect {}: {e}", control.spec.socket.display()))?;
        connection
            .send(&serde_json::json!({
                "hello": {
                    "timeline": control.spec.id,
                    "parent": control.parent.map(|(parent, _)| parent),
                    "at": control.parent.map(|(_, at)| at),
                }
            }))
            .map_err(|e| format!("hello: {e}"))?;
        control.connection = Some(connection);
    }
    let connection = control.connection.as_mut().ok_or("no connection")?;
    let request: serde_json::Value =
        serde_json::from_str(&crate::choice::request_json(request)).map_err(|e| e.to_string())?;
    connection
        .send(&serde_json::json!({ "paused": request }))
        .map_err(|e| format!("paused: {e}"))?;
    let message = connection
        .receive()
        .map_err(|e| format!("command: {e}"))?
        .ok_or("the driver closed the connection")?;
    parse_command(&message)
}

fn stop_index() -> Option<u64> {
    control().as_ref().and_then(|control| control.spec.stop)
}

fn set_stop(until: Option<u64>) {
    if let Some(control) = control().as_mut() {
        control.spec.stop = until;
    }
}

fn install_control(spec: ControlSpec) {
    use crate::choice::PauseAction;
    *control() = Some(Control {
        spec,
        connection: None,
        parent: None,
    });
    crate::choice::set_pause_hook(Some(Box::new(
        move |request: &crate::choice::ChoiceRequest| {
            // A copy comes back around this loop, paused at the same request.
            loop {
                if stop_index() != Some(request.index) {
                    return PauseAction::Continue;
                }
                match pause_at(request) {
                    Ok(Command::Answer { value, until }) => {
                        set_stop(until);
                        return PauseAction::Answer(value);
                    }
                    Ok(Command::Source { until }) => {
                        set_stop(until);
                        return PauseAction::Continue;
                    }
                    Ok(Command::Capture) => serve_template(request),
                    Err(error) => {
                        tracing::warn!("{TIMELINE_CONTROL_ENV}: {error}");
                        // SAFETY: _exit ends the process at once.
                        unsafe { libc::_exit(96) };
                    }
                }
            }
        },
    )));
}

fn send_control(message: &serde_json::Value) {
    if let Some(connection) = control()
        .as_mut()
        .and_then(|control| control.connection.as_mut())
    {
        let _ = connection.send(message);
    }
}

enum Received {
    Message(serde_json::Value),
    Timeout,
    Closed,
}

fn receive_control(timeout: std::time::Duration) -> Received {
    let mut guard = control();
    let Some(connection) = guard
        .as_mut()
        .and_then(|control| control.connection.as_mut())
    else {
        return Received::Closed;
    };
    let _ = connection.reader.get_ref().set_read_timeout(Some(timeout));
    let received = match connection.receive() {
        Ok(Some(message)) => Received::Message(message),
        Ok(None) => Received::Closed,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            Received::Timeout
        }
        Err(_) => Received::Closed,
    };
    let _ = connection.reader.get_ref().set_read_timeout(None);
    received
}

/// Reaps exited copies (every one of them when `block`), reporting each.
fn reap_children(children: &mut Vec<(u64, libc::pid_t)>, block: bool) {
    while !children.is_empty() {
        let mut status = 0;
        let flags = if block { 0 } else { libc::WNOHANG };
        // SAFETY: waits for a child of this template.
        let pid = unsafe { libc::waitpid(-1, &mut status, flags) };
        if pid == 0 {
            return;
        }
        if pid < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            children.clear();
            return;
        }
        if let Some(at) = children.iter().position(|&(_, child)| child == pid) {
            let (id, _) = children.remove(at);
            send_control(&serde_json::json!({
                "reaped": {
                    "timeline": id,
                    "exit_code": libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
                    "signal": libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status)),
                }
            }));
        }
    }
}

/// This run becomes a template at `request`: it reports the capture, then
/// forks copies on request until the driver releases it, and never runs on
/// (a checkpoint is a template). Returns only in a copy, which is then
/// paused at the same request with a connection of its own.
fn serve_template(request: &crate::choice::ChoiceRequest) {
    let place = serde_json::json!({
        "choice": request.index,
        "request": crate::choice::request_json(request),
    });
    let Some(hooks) = HOOKS.get().copied() else {
        send_control(
            &serde_json::json!({ "refused": "this backend installed no host-fork hooks" }),
        );
        return;
    };
    let (template_id, dir, cpus) = match control().as_ref() {
        Some(control) => (
            control.spec.id,
            control.spec.dir.clone(),
            control.spec.cpus.clone(),
        ),
        None => return,
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        send_control(&serde_json::json!({ "refused": format!("{}: {error}", dir.display()) }));
        return;
    }
    let template = match capture_template(hooks, Some((&dir, template_id))) {
        Ok(template) => template,
        Err(error) => {
            send_control(&serde_json::json!({ "refused": error }));
            return;
        }
    };
    send_control(&serde_json::json!({
        "captured": {
            "timeline": template_id,
            "park_us": template.park_us,
            "prepare_us": template.prepare_us,
            "records_us": template.records_us,
            "parked_threads": template.parked.count(),
            "offsets": template.offsets,
        }
    }));
    let mut children: Vec<(u64, libc::pid_t)> = Vec::new();
    loop {
        reap_children(&mut children, false);
        let message = match receive_control(std::time::Duration::from_millis(20)) {
            Received::Message(message) => message,
            Received::Timeout => continue,
            Received::Closed => {
                // The driver is gone: no copy outlives it unsupervised.
                for &(_, pid) in &children {
                    // SAFETY: signals a copy this template forked.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                reap_children(&mut children, true);
                // SAFETY: _exit ends the template at once.
                unsafe { libc::_exit(1) };
            }
        };
        if let Some(fork) = message.get("fork") {
            let ids: Vec<u64> = fork
                .get("ids")
                .and_then(|ids| ids.as_array())
                .map(|ids| ids.iter().filter_map(|id| id.as_u64()).collect())
                .unwrap_or_default();
            // The batch's policies; absent, the run's CPUs and the backend's
            // memory default.
            let policies = (
                fork.get("placement")
                    .and_then(|placement| placement.as_str())
                    .map_or_else(|| Ok(CopyCpu::from_cpus(&cpus)), CopyCpu::parse),
                fork.get("memory")
                    .and_then(|memory| memory.as_str())
                    .map(MemoryCopy::parse)
                    .transpose(),
            );
            let (cpu, memory) = match policies {
                (Ok(cpu), Ok(memory)) => (cpu, memory),
                (Err(error), _) | (_, Err(error)) => {
                    send_control(&serde_json::json!({ "forked": [{ "error": error }] }));
                    continue;
                }
            };
            let mut forked = Vec::new();
            before_placed_forks(&cpu);
            for (position, id) in ids.into_iter().enumerate() {
                let started = Instant::now();
                match fork_for_copy() {
                    Ok(0) => {
                        become_timeline(
                            &dir,
                            &cpu,
                            memory,
                            id as usize,
                            position,
                            &place,
                            &template,
                            started,
                        );
                        if let Some(control) = control().as_mut() {
                            // Dropping closes this copy's descriptors of the
                            // template's connection; the template keeps its own.
                            control.connection = None;
                            control.parent = Some((template_id, request.index));
                            control.spec.id = id;
                            control.spec.stop = Some(request.index);
                        }
                        return;
                    }
                    Ok(pid) => {
                        children.push((id, pid));
                        forked.push(serde_json::json!({
                            "timeline": id,
                            "fork_us": micros(started),
                        }));
                    }
                    Err(error) => {
                        forked.push(serde_json::json!({ "timeline": id, "error": error }));
                    }
                }
            }
            send_control(&serde_json::json!({ "forked": forked }));
        } else if message.get("release").is_some() {
            // Copies are processes in this template's PID namespace and
            // cannot outlive it; one still waiting for its command would be
            // waited for forever. Release ends every copy still alive.
            for &(_, pid) in &children {
                // SAFETY: signals a copy this template forked.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            reap_children(&mut children, true);
            send_control(&serde_json::json!({ "released": template_id }));
            // SAFETY: _exit ends the template at once; it never runs on.
            unsafe { libc::_exit(0) };
        } else {
            send_control(&serde_json::json!({ "error": format!("unknown command {message}") }));
        }
    }
}

/// Descriptors, besides standard output and standard error, that each
/// timeline writes to a file of its own (`DIR/timeline-<k>.<name>`).
static OUTPUTS: std::sync::Mutex<Vec<(RawFd, &'static str)>> = std::sync::Mutex::new(Vec::new());

/// Gives each timeline its own copy of what the run writes to `fd` (the
/// run's `--log-file`, say), named `name`; the template keeps the original.
pub fn register_output(fd: RawFd, name: &'static str) {
    let mut outputs = OUTPUTS.lock().unwrap_or_else(|e| e.into_inner());
    outputs.retain(|&(_, other)| other != name);
    outputs.push((fd, name));
}

fn outputs() -> Vec<(RawFd, &'static str)> {
    let mut outputs = vec![(1, "stdout"), (2, "stderr")];
    outputs.extend(
        OUTPUTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .copied(),
    );
    outputs
}

static TIMELINE: AtomicI64 = AtomicI64::new(-1);

/// PROTOTYPE debugging switches in `HERMIT_TIMELINE_DEBUG` (comma list):
/// `libcfork` forks through glibc, `noresurrect` re-creates no thread,
/// `overshoot=N` records one skid overshoot in timeline N as it starts, as if
/// its precise timer had passed its target (to test that such a copy is
/// refused, as a fresh run is).
fn debug_switch(name: &str) -> bool {
    std::env::var("HERMIT_TIMELINE_DEBUG")
        .map(|value| value.split(',').any(|part| part == name))
        .unwrap_or(false)
}

/// The value of a `name=value` switch in `HERMIT_TIMELINE_DEBUG`.
fn debug_value(name: &str) -> Option<String> {
    let value = std::env::var("HERMIT_TIMELINE_DEBUG").ok()?;
    value.split(',').find_map(|part| {
        part.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
            .map(str::to_string)
    })
}

static SEGV_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn report_fault(
    signal: libc::c_int,
    info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    // Async-signal-safe: formats into a stack buffer and writes it once.
    let fd = SEGV_FD.load(Ordering::Relaxed);
    let mut buffer = [0u8; 256];
    let mut at = 0;
    let mut put = |text: &[u8]| {
        for &byte in text {
            if at < buffer.len() {
                buffer[at] = byte;
                at += 1;
            }
        }
    };
    let hex = |value: u64, put: &mut dyn FnMut(&[u8])| {
        let mut digits = [0u8; 18];
        digits[0] = b'0';
        digits[1] = b'x';
        for i in 0..16 {
            let nibble = ((value >> ((15 - i) * 4)) & 0xf) as u8;
            digits[2 + i] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            };
        }
        put(&digits);
    };
    // SAFETY: the kernel passes a valid siginfo and ucontext.
    let (address, rip, rsp, tid) = unsafe {
        let address = (*info).si_addr() as u64;
        let context = context as *const libc::ucontext_t;
        let gregs = (*context).uc_mcontext.gregs;
        (
            address,
            gregs[libc::REG_RIP as usize] as u64,
            gregs[libc::REG_RSP as usize] as u64,
            libc::syscall(libc::SYS_gettid) as u64,
        )
    };
    put(b"signal ");
    hex(signal as u64, &mut put);
    put(b" addr ");
    hex(address, &mut put);
    put(b" rip ");
    hex(rip, &mut put);
    put(b" rsp ");
    hex(rsp, &mut put);
    put(b" tid ");
    hex(tid, &mut put);
    put(b" report_fault ");
    hex(report_fault as *const () as u64, &mut put);
    put(b"\n");
    if fd >= 0 {
        // SAFETY: writes the formatted bytes to a descriptor this copy opened.
        unsafe { libc::write(fd, buffer.as_ptr().cast(), at) };
    }
    // SAFETY: back to the default action, which ends the process.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

fn install_fault_report(dir: &std::path::Path, index: usize) {
    let Ok(file) = std::fs::File::create(dir.join(format!("timeline-{index}.fault"))) else {
        return;
    };
    SEGV_FD.store(std::os::fd::IntoRawFd::into_raw_fd(file), Ordering::Relaxed);
    for signal in [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL] {
        // SAFETY: installs a handler that only formats and writes.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = report_fault as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}
static TIMELINE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The phrase in every refusal of a copy's use of what it shares with its
/// template and the other copies (owner, Slack 6804; `timeline_sharing`).
pub use crate::timeline_sharing::REFUSAL_PHRASE as SHARING_REFUSAL_PHRASE;

/// This process's timeline number, when it is a copy.
pub fn timeline_index() -> Option<usize> {
    let index = TIMELINE.load(Ordering::Relaxed);
    (index >= 0).then_some(index as usize)
}

/// Ends a timeline's process with `code` once its run is over, recording the
/// code, without running the destructors of what it shares with the
/// template (its temporary files, its CPU lease). Returns in the template.
pub fn exit_timeline(code: i32) {
    let Some(index) = timeline_index() else {
        return;
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    if let Some(dir) = TIMELINE_DIR.get() {
        let _ = std::fs::write(
            dir.join(format!("timeline-{index}.exit")),
            format!("{code}\n"),
        );
        if let Some(hooks) = HOOKS.get() {
            let _ = std::fs::write(
                dir.join(format!("timeline-{index}.backend")),
                format!("{}\n", (hooks.at_exit)()),
            );
        }
    }
    send_control(&serde_json::json!({ "exit": code }));
    // SAFETY: _exit ends the process at once; nothing runs after it.
    unsafe { libc::_exit(code) };
}

unsafe extern "C" {
    /// glibc's `on_exit(3)`: `function` runs at `exit(3)` with its status.
    fn on_exit(
        function: extern "C" fn(libc::c_int, *mut libc::c_void),
        argument: *mut libc::c_void,
    ) -> libc::c_int;
}

extern "C" fn exit_as_timeline(status: libc::c_int, _: *mut libc::c_void) {
    exit_timeline(status);
}

/// Makes every `exit(3)` of this copy record its status as
/// [`exit_timeline`] does, so that a run Detcore ends from inside, such as a
/// refusal (`unrecoverable_shutdown` calls `std::process::exit`), reaches the
/// driver as an exit and not as a lost copy. [`exit_timeline`] ends the
/// process with `_exit`, so the template's own exit handlers do not run in
/// the copy.
fn record_exit_at_process_exit() {
    // SAFETY: registers a function with no captured state.
    unsafe { on_exit(exit_as_timeline, std::ptr::null_mut()) };
}

/// Whether two descriptors of this process name one open file description.
fn same_description(a: RawFd, b: RawFd) -> bool {
    const KCMP_FILE: libc::c_long = 0;
    // SAFETY: getpid has no preconditions.
    let pid = unsafe { libc::getpid() } as libc::c_long;
    // SAFETY: kcmp compares two descriptors of this process; no memory.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_kcmp,
            pid,
            pid,
            KCMP_FILE,
            a as libc::c_long,
            b as libc::c_long,
        )
    };
    rc == 0
}

fn open_fds() -> Vec<RawFd> {
    // Raw getdents64 (util::find_in_directory), never glibc's opendir.
    let mut fds: Vec<RawFd> = Vec::new();
    let _ = crate::util::find_in_directory(std::path::Path::new("/proc/self/fd"), |name| {
        if let Some(fd) = name.to_str().and_then(|name| name.parse().ok()) {
            fds.push(fd);
        }
        None::<()>
    });
    fds.sort_unstable();
    fds
}

/// A regular file or directory descriptor whose position each timeline
/// gets its own copy of.
struct FileRecord {
    fd: RawFd,
    status: libc::c_int,
    offset: libc::off_t,
}

fn is_output(fd: RawFd) -> bool {
    outputs()
        .iter()
        .any(|&(output, _)| fd == output || same_description(fd, output))
}

fn record_files() -> Vec<FileRecord> {
    let mut records = Vec::new();
    for fd in open_fds() {
        if fd <= 2 || is_output(fd) {
            continue;
        }
        let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{fd}")) else {
            continue;
        };
        if target.to_string_lossy().starts_with("/memfd:") {
            continue;
        }
        // SAFETY: fstat of a descriptor into a zeroed buffer.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: as above.
        if unsafe { libc::fstat(fd, &mut stat) } < 0 {
            continue;
        }
        let kind = stat.st_mode & libc::S_IFMT;
        if kind != libc::S_IFREG && kind != libc::S_IFDIR {
            continue;
        }
        // SAFETY: fcntl and lseek on a live descriptor.
        let status = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: as above.
        let offset = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
        if status < 0 || offset < 0 {
            continue;
        }
        records.push(FileRecord { fd, status, offset });
    }
    records
}

fn dup_onto(from: RawFd, onto: RawFd) -> bool {
    // SAFETY: fcntl on a live descriptor.
    let flags = unsafe { libc::fcntl(onto, libc::F_GETFD) };
    let cloexec = if flags >= 0 && flags & libc::FD_CLOEXEC != 0 {
        libc::O_CLOEXEC
    } else {
        0
    };
    // SAFETY: both are descriptors of this process; dup3 replaces `onto`.
    unsafe { libc::dup3(from, onto, cloexec) >= 0 }
}

/// Gives this timeline its own open file description for each recorded
/// file, at the template's position, with its status flags.
fn reopen_files(records: &[FileRecord]) -> usize {
    let mut reopened = 0;
    for record in records {
        let access = record.status & libc::O_ACCMODE;
        let flags = access
            | (record.status & (libc::O_APPEND | libc::O_NONBLOCK | libc::O_DIRECTORY))
            | libc::O_CLOEXEC;
        let path = format!("/proc/self/fd/{}\0", record.fd);
        // SAFETY: a NUL-terminated path; a new descriptor.
        let fd = unsafe { libc::open(path.as_ptr().cast(), flags) };
        if fd < 0 {
            continue;
        }
        // SAFETY: lseek on the new descriptor.
        unsafe { libc::lseek(fd, record.offset, libc::SEEK_SET) };
        if dup_onto(fd, record.fd) {
            reopened += 1;
        }
        // SAFETY: the duplicate lives on behind record.fd.
        unsafe { libc::close(fd) };
    }
    reopened
}

/// Sends every descriptor that shares one of the run's outputs (standard
/// output, standard error, each registered output) to this timeline's own
/// file for it. A descriptor goes with the first output it shares.
fn redirect_outputs(dir: &std::path::Path, index: usize) -> std::io::Result<()> {
    let fds = open_fds();
    let mut claimed: Vec<RawFd> = Vec::new();
    let mut groups = Vec::new();
    for (output, name) in outputs() {
        let targets: Vec<RawFd> = fds
            .iter()
            .copied()
            .filter(|&fd| !claimed.contains(&fd) && (fd == output || same_description(fd, output)))
            .collect();
        claimed.extend(&targets);
        groups.push((name, targets));
    }
    for (name, targets) in groups {
        if targets.is_empty() {
            continue;
        }
        let file = std::fs::File::create(dir.join(format!("timeline-{index}.{name}")))?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&file);
        for fd in targets {
            if !dup_onto(raw, fd) {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

fn offset_of(fd: RawFd) -> i64 {
    // SAFETY: lseek on a descriptor; -1 for a pipe or terminal.
    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }
}

/// How long a cut waits for a piped output to drain, and then for its file
/// to settle.
const PIPED_OUTPUT_BOUND: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a piped output's file must keep one size to count as settled.
const PIPED_OUTPUT_SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// An output's offset at the cut. A file gives it directly. A pipe does
/// not: safehermit's byte cap reads standard error through one and copies
/// it into the file the driver opened for this timeline (`file`). Then the
/// cut waits until the pipe is empty and the file has stopped growing, and
/// takes the file's size: this run writes nothing more, as a template.
/// With no file named (the `HERMIT_TIMELINE_FORK` route), a pipe's offset
/// stays unknown (-1), as before. Refused rather than cut at a wrong
/// offset: a pipe that does not drain, a file that does not settle or is
/// missing, or an output that is neither seekable nor a pipe.
fn cut_offset(fd: RawFd, file: Option<&std::path::Path>) -> Result<i64, String> {
    cut_offset_within(fd, file, PIPED_OUTPUT_BOUND, PIPED_OUTPUT_SETTLE)
}

fn cut_offset_within(
    fd: RawFd,
    file: Option<&std::path::Path>,
    bound: std::time::Duration,
    settle: std::time::Duration,
) -> Result<i64, String> {
    let offset = offset_of(fd);
    let Some(file) = file else {
        return Ok(offset);
    };
    if offset >= 0 {
        return Ok(offset);
    }
    let drained_by = Instant::now() + bound;
    loop {
        let mut pending: libc::c_int = 0;
        // SAFETY: FIONREAD on a descriptor this process holds, into a local.
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut pending) } != 0 {
            return Err(format!(
                "descriptor {fd} is neither seekable nor a pipe ({}): its offset at the cut \
                 is unknown",
                std::io::Error::last_os_error()
            ));
        }
        if pending == 0 {
            break;
        }
        if Instant::now() > drained_by {
            return Err(format!(
                "descriptor {fd} is a pipe still holding {pending} bytes after {bound:?}: \
                 its offset at the cut is unknown"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let size = || {
        std::fs::metadata(file).map(|m| m.len()).map_err(|e| {
            format!(
                "descriptor {fd} is a pipe, and the file it drains into, {}: {e}",
                file.display()
            )
        })
    };
    let settled_by = Instant::now() + bound;
    let mut last = size()?;
    let mut same_since = Instant::now();
    while same_since.elapsed() < settle {
        if Instant::now() > settled_by {
            return Err(format!(
                "descriptor {fd} is a pipe, and the file it drains into, {}, kept growing \
                 for {bound:?}: its offset at the cut is unknown",
                file.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
        let now = size()?;
        if now != last {
            last = now;
            same_since = Instant::now();
        }
    }
    Ok(last as i64)
}

fn micros(since: Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

fn fdinfo(fd: RawFd) -> String {
    std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap_or_default()
}

fn fdinfo_field(info: &str, key: &str) -> Option<String> {
    info.lines()
        .find_map(|line| line.strip_prefix(key))
        .map(|rest| rest.trim_start_matches(':').trim().to_string())
}

/// The poll events `fd` reports at once; POLLHUP and POLLERR always count.
fn poll_now(fd: RawFd, events: libc::c_short) -> libc::c_short {
    let mut entry = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one pollfd and a zero timeout.
    if unsafe { libc::poll(&mut entry, 1, 0) } < 0 {
        return 0;
    }
    entry.revents
}

fn close_pair(pair: &[RawFd; 2]) {
    // SAFETY: two descriptors this copy created; their duplicates live on.
    unsafe {
        libc::close(pair[0]);
        libc::close(pair[1]);
    }
}

/// Puts `new` on every descriptor in `group`, with the group's O_NONBLOCK.
fn dup_group(new: RawFd, group: &[RawFd], status: libc::c_int, what: &str) -> Result<(), String> {
    // SAFETY: fcntl on a live descriptor.
    unsafe { libc::fcntl(new, libc::F_SETFL, status & libc::O_NONBLOCK) };
    for &fd in group {
        if !dup_onto(new, fd) {
            return Err(format!(
                "dup3 {what} onto {fd}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// What `privatize_pipes` and `privatize_socket_pairs` did.
#[derive(Default)]
struct StreamCounts {
    copied: usize,
    bytes: u64,
    lone: usize,
    external: usize,
}

/// Gives this copy its own pipe for each pipe `fork()` shares with the
/// template and every other copy (a guest pipe on KVM is a host pipe): a
/// copy's reader could take another copy's bytes, and would never see end
/// of file while the template holds a write end. The new pipe has the old
/// one's capacity and buffered bytes, copied with tee(2), which leaves the
/// shared pipe as the template and the other copies need it; each open file
/// description of each end gets its own description of the new end, with
/// its O_NONBLOCK flag. A pipe with no writer left anywhere (poll reports
/// POLLHUP) gets a new pipe whose write end is closed, so the copy reads
/// the buffered bytes and then end of file, as the template would; one with
/// no reader left anywhere (POLLERR) gets one whose read end is closed. A
/// pipe with an end held outside this process is left shared and counted
/// as external. Packet-mode, signal-driven and read-write pipe descriptors
/// are refused.
fn privatize_pipes(
    fds: &[RawFd],
    kind: &dyn Fn(RawFd) -> String,
    flags_of: &dyn Fn(RawFd) -> libc::c_int,
) -> Result<StreamCounts, String> {
    let mut by_pipe: Vec<(String, Vec<RawFd>)> = Vec::new();
    for &fd in fds {
        let target = kind(fd);
        if !target.starts_with("pipe:") {
            continue;
        }
        match by_pipe.iter_mut().find(|(name, _)| *name == target) {
            Some((_, group)) => group.push(fd),
            None => by_pipe.push((target, vec![fd])),
        }
    }
    let mut counts = StreamCounts::default();
    for (name, group) in by_pipe {
        if group.iter().any(|&fd| is_output(fd)) {
            continue;
        }
        let mut readers = Vec::new();
        let mut writers = Vec::new();
        for &fd in &group {
            let flags = flags_of(fd);
            if flags & (libc::O_DIRECT | libc::O_ASYNC) != 0 {
                return Err(format!(
                    "{name} (descriptor {fd}) is packet-mode or signal-driven: not copied"
                ));
            }
            match flags & libc::O_ACCMODE {
                libc::O_RDONLY => readers.push(fd),
                libc::O_WRONLY => writers.push(fd),
                _ => {
                    return Err(format!(
                        "{name} (descriptor {fd}) is read-write: not copied"
                    ));
                }
            }
        }
        let lone = match (readers.first(), writers.first()) {
            (Some(_), Some(_)) => false,
            (Some(&reader), None) if poll_now(reader, libc::POLLIN) & libc::POLLHUP != 0 => true,
            (None, Some(&writer)) if poll_now(writer, libc::POLLOUT) & libc::POLLERR != 0 => true,
            _ => {
                counts.external += 1;
                continue;
            }
        };
        let model = readers.first().or(writers.first()).copied().unwrap_or(-1);
        let mut new = [-1; 2];
        // SAFETY: room for both descriptors; closed below after their duplicates.
        if unsafe { libc::pipe2(new.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!("pipe2: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: fcntl on live pipe descriptors.
        let capacity = unsafe { libc::fcntl(model, libc::F_GETPIPE_SZ) };
        // SAFETY: as above.
        if capacity > 0 && unsafe { libc::fcntl(new[1], libc::F_SETPIPE_SZ, capacity) } != capacity
        {
            let error = std::io::Error::last_os_error();
            close_pair(&new);
            return Err(format!(
                "{name}: capacity {capacity} not reproduced: {error}"
            ));
        }
        if let Some(&reader) = readers.first() {
            let mut pending: libc::c_int = 0;
            // SAFETY: FIONREAD into an int.
            unsafe { libc::ioctl(reader, libc::FIONREAD, &mut pending) };
            if pending > 0 {
                // SAFETY: tee between two pipes; no memory is passed.
                let copied =
                    unsafe { libc::tee(reader, new[1], pending as usize, libc::SPLICE_F_NONBLOCK) };
                if copied != pending as isize {
                    close_pair(&new);
                    return Err(format!(
                        "{name}: tee copied {copied} of {pending} buffered bytes"
                    ));
                }
                counts.bytes += pending as u64;
            }
        }
        for (ends, new_end, access) in [
            (&readers, new[0], libc::O_RDONLY),
            (&writers, new[1], libc::O_WRONLY),
        ] {
            let mut descriptions: Vec<Vec<RawFd>> = Vec::new();
            for &fd in ends.iter() {
                match descriptions
                    .iter_mut()
                    .find(|description| same_description(description[0], fd))
                {
                    Some(description) => description.push(fd),
                    None => descriptions.push(vec![fd]),
                }
            }
            for (k, description) in descriptions.iter().enumerate() {
                let source = if k == 0 {
                    new_end
                } else {
                    // Another open file description of the same end.
                    let path = format!("/proc/self/fd/{new_end}\0");
                    // SAFETY: a NUL-terminated path; a new descriptor.
                    let fd = unsafe { libc::open(path.as_ptr().cast(), access | libc::O_CLOEXEC) };
                    if fd < 0 {
                        let error = std::io::Error::last_os_error();
                        close_pair(&new);
                        return Err(format!("{name}: reopening an end: {error}"));
                    }
                    fd
                };
                let result = dup_group(source, description, flags_of(description[0]), "pipe");
                if k > 0 {
                    // SAFETY: the duplicates keep the description open.
                    unsafe { libc::close(source) };
                }
                if let Err(error) = result {
                    close_pair(&new);
                    return Err(error);
                }
            }
        }
        // An end nobody in this process holds is closed here: end of file
        // after the buffered bytes, or EPIPE, as in the template.
        close_pair(&new);
        counts.copied += 1;
        if lone {
            counts.lone += 1;
        }
    }
    Ok(counts)
}

/// A Unix socket as the kernel's sock_diag interface describes it.
struct UnixSocket {
    kind: libc::c_int,
    listening: bool,
    named: bool,
    /// The peer's inode: `Some(0)` when the peer has been closed, `None`
    /// when the socket was never connected.
    peer: Option<u32>,
    /// RCV_SHUTDOWN (1) and SEND_SHUTDOWN (2).
    shutdown: u8,
}

/// The Unix socket with inode `inode`, from sock_diag (`UNIX_DIAG_PEER`,
/// `UNIX_DIAG_NAME`, and `UNIX_DIAG_SHUTDOWN`, which the kernel always
/// sends). `None` when no Unix socket has that inode.
fn unix_socket(inode: u32) -> Result<Option<UnixSocket>, String> {
    const NETLINK_SOCK_DIAG: libc::c_int = 4;
    const SOCK_DIAG_BY_FAMILY: u16 = 20;
    const UDIAG_SHOW_NAME: u32 = 0x1;
    const UDIAG_SHOW_PEER: u32 = 0x4;
    const UNIX_DIAG_NAME: u16 = 0;
    const UNIX_DIAG_PEER: u16 = 2;
    const UNIX_DIAG_SHUTDOWN: u16 = 6;
    const TCP_LISTEN: u8 = 10;
    #[repr(C)]
    struct Request {
        header: libc::nlmsghdr,
        family: u8,
        protocol: u8,
        pad: u16,
        states: u32,
        inode: u32,
        show: u32,
        cookie: [u32; 2],
    }
    // SAFETY: a new netlink socket, closed below.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        )
    };
    if fd < 0 {
        return Err(format!(
            "sock_diag socket: {}",
            std::io::Error::last_os_error()
        ));
    }
    let request = Request {
        header: libc::nlmsghdr {
            nlmsg_len: std::mem::size_of::<Request>() as u32,
            nlmsg_type: SOCK_DIAG_BY_FAMILY,
            nlmsg_flags: libc::NLM_F_REQUEST as u16,
            nlmsg_seq: 1,
            nlmsg_pid: 0,
        },
        family: libc::AF_UNIX as u8,
        protocol: 0,
        pad: 0,
        states: u32::MAX,
        inode,
        show: UDIAG_SHOW_NAME | UDIAG_SHOW_PEER,
        // INET_DIAG_NOCOOKIE: look the socket up by inode alone.
        cookie: [u32::MAX, u32::MAX],
    };
    let mut reply = [0u8; 1024];
    // SAFETY: sends the request struct and receives into the buffer.
    let received = unsafe {
        let sent = libc::send(
            fd,
            (&request as *const Request).cast(),
            std::mem::size_of::<Request>(),
            0,
        );
        if sent < 0 {
            -1
        } else {
            libc::recv(fd, reply.as_mut_ptr().cast(), reply.len(), 0)
        }
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: the socket opened above.
    unsafe { libc::close(fd) };
    if received < 0 {
        return Err(format!("sock_diag: {error}"));
    }
    let received = received as usize;
    let header = std::mem::size_of::<libc::nlmsghdr>();
    // unix_diag_msg: family, type, state, pad, inode (4), cookie (8).
    if received < header + 16 {
        return Ok(None); // NLMSG_ERROR (ENOENT): not a Unix socket
    }
    if u16::from_ne_bytes([reply[4], reply[5]]) == libc::NLMSG_ERROR as u16 {
        return Ok(None);
    }
    let mut socket = UnixSocket {
        kind: reply[header + 1] as libc::c_int,
        listening: reply[header + 2] == TCP_LISTEN,
        named: false,
        peer: None,
        shutdown: 0,
    };
    let mut at = header + 16;
    while at + 4 <= received {
        let length = u16::from_ne_bytes([reply[at], reply[at + 1]]) as usize;
        let attribute = u16::from_ne_bytes([reply[at + 2], reply[at + 3]]);
        if length < 4 || at + length > received {
            break;
        }
        let value = &reply[at + 4..at + length];
        match attribute {
            UNIX_DIAG_NAME => socket.named = !value.is_empty(),
            UNIX_DIAG_PEER if value.len() >= 4 => {
                socket.peer = Some(u32::from_ne_bytes([value[0], value[1], value[2], value[3]]));
            }
            UNIX_DIAG_SHUTDOWN if !value.is_empty() => socket.shutdown = value[0],
            _ => {}
        }
        at += (length + 3) & !3;
    }
    Ok(Some(socket))
}

fn int_option(fd: RawFd, option: libc::c_int) -> libc::c_int {
    let mut value: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: an int-sized SOL_SOCKET option into an int.
    unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut length,
        )
    };
    value
}

/// Gives the new socket `new` the buffer sizes and SO_PASSCRED of `old`.
fn copy_socket_options(old: RawFd, new: RawFd) -> Result<(), String> {
    for option in [libc::SO_SNDBUF, libc::SO_RCVBUF, libc::SO_PASSCRED] {
        let wanted = int_option(old, option);
        if int_option(new, option) == wanted {
            continue;
        }
        // The kernel doubles a buffer size it is given.
        let given = if option == libc::SO_PASSCRED {
            wanted
        } else {
            wanted / 2
        };
        // SAFETY: an int-sized SOL_SOCKET option.
        unsafe {
            libc::setsockopt(
                new,
                libc::SOL_SOCKET,
                option,
                (&given as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if int_option(new, option) != wanted {
            return Err(format!("socket option {option}: {wanted} not reproduced"));
        }
    }
    Ok(())
}

/// Gives this copy its own Unix socket pair for each pair `fork()` shares
/// with the template and every other copy (a guest socketpair on KVM is a
/// host one), and for each socket whose peer has been closed: a new pair
/// whose second end is closed, so the copy reads what was waiting and then
/// end of file. Bytes waiting at an end are read with MSG_PEEK, which
/// leaves the shared socket as it was, and sent from the new peer; buffer
/// sizes, SO_PASSCRED, O_NONBLOCK and the shutdown state are copied. A
/// socket whose peer is outside this process is left shared (external).
/// Refused: a datagram or seqpacket socket with messages waiting (MSG_PEEK
/// cannot copy them with their boundaries), a connected socket with an
/// address, and a listening socket, which no copy can have its own of.
fn privatize_socket_pairs(
    fds: &[RawFd],
    kind: &dyn Fn(RawFd) -> String,
    flags_of: &dyn Fn(RawFd) -> libc::c_int,
) -> Result<StreamCounts, String> {
    let mut ends: Vec<(u32, Vec<RawFd>)> = Vec::new();
    for &fd in fds {
        let Some(inode) = kind(fd)
            .strip_prefix("socket:[")
            .and_then(|rest| rest.strip_suffix(']'))
            .and_then(|inode| inode.parse::<u32>().ok())
        else {
            continue;
        };
        if is_output(fd) {
            continue;
        }
        match ends.iter_mut().find(|(other, _)| *other == inode) {
            Some((_, group)) => group.push(fd),
            None => ends.push((inode, vec![fd])),
        }
    }
    let waiting = |fd: RawFd| -> usize {
        let mut count: libc::c_int = 0;
        // SAFETY: FIONREAD into an int.
        unsafe { libc::ioctl(fd, libc::FIONREAD, &mut count) };
        count.max(0) as usize
    };
    let mut counts = StreamCounts::default();
    let mut done: Vec<u32> = Vec::new();
    for (inode, group) in &ends {
        if done.contains(inode) {
            continue;
        }
        let Some(socket) = unix_socket(*inode)? else {
            continue; // not a Unix socket
        };
        if socket.listening {
            return Err(format!(
                "listening Unix socket {inode}: copies would share it; not copied"
            ));
        }
        // A closed peer shows as inode 0, or as an inode sock_diag no longer
        // finds; a live one outside this process makes the socket external.
        let peer = match socket.peer {
            None => continue, // never connected
            Some(peer) => match ends.iter().find(|(other, _)| peer != 0 && *other == peer) {
                Some((_, peer_group)) => Some((peer, peer_group)),
                None if peer != 0 && unix_socket(peer)?.is_some() => {
                    counts.external += 1;
                    continue;
                }
                None => None,
            },
        };
        let peer_socket = match peer {
            Some((peer, _)) => unix_socket(peer)?,
            None => None,
        };
        if socket.named || peer_socket.as_ref().is_some_and(|peer| peer.named) {
            return Err(format!(
                "connected Unix socket {inode} has an address: not copied"
            ));
        }
        done.push(*inode);
        let mut new = [-1; 2];
        // SAFETY: room for both descriptors; closed below after their duplicates.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                socket.kind | libc::SOCK_CLOEXEC,
                0,
                new.as_mut_ptr(),
            )
        } != 0
        {
            return Err(format!("socketpair: {}", std::io::Error::last_os_error()));
        }
        let mut steps: Vec<(RawFd, RawFd, RawFd)> = vec![(group[0], new[0], new[1])];
        if let Some((peer, peer_group)) = peer {
            done.push(peer);
            steps.push((peer_group[0], new[1], new[0]));
        }
        let mut copy = || -> Result<(), String> {
            for &(old, end, _) in &steps {
                copy_socket_options(old, end)?;
            }
            // Bytes waiting at an end were sent by its peer: the new peer
            // sends them.
            for &(old, _, sender) in &steps {
                let count = waiting(old);
                if count == 0 {
                    continue;
                }
                if socket.kind != libc::SOCK_STREAM {
                    return Err(format!(
                        "Unix socket {inode}: messages waiting on a non-stream socket: not copied"
                    ));
                }
                let mut buffer = vec![0u8; count];
                // SAFETY: peeks into the buffer, consuming nothing.
                let peeked = unsafe {
                    libc::recv(
                        old,
                        buffer.as_mut_ptr().cast(),
                        count,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                };
                // SAFETY: sends what was peeked.
                let sent = if peeked > 0 {
                    unsafe {
                        libc::send(
                            sender,
                            buffer.as_ptr().cast(),
                            peeked as usize,
                            libc::MSG_DONTWAIT,
                        )
                    }
                } else {
                    0
                };
                if peeked != count as isize || sent != peeked {
                    return Err(format!(
                        "Unix socket {inode}: copied {sent} of {count} waiting bytes"
                    ));
                }
                counts.bytes += count as u64;
            }
            for (shutdown, end) in [(socket.shutdown, new[0])]
                .into_iter()
                .chain(peer_socket.as_ref().map(|peer| (peer.shutdown, new[1])))
            {
                let how = match shutdown & 3 {
                    1 => libc::SHUT_RD,
                    2 => libc::SHUT_WR,
                    3 => libc::SHUT_RDWR,
                    _ => continue,
                };
                // SAFETY: shutdown of a socket this copy owns.
                unsafe { libc::shutdown(end, how) };
            }
            dup_group(new[0], group, flags_of(group[0]), "socket")?;
            if let Some((_, peer_group)) = peer {
                dup_group(new[1], peer_group, flags_of(peer_group[0]), "socket")?;
            }
            Ok(())
        };
        let result = copy();
        // A closed peer stays closed: the copy's end sees what was waiting,
        // then end of file.
        close_pair(&new);
        result?;
        counts.copied += 1;
        if peer.is_none() {
            counts.lone += 1;
        }
    }
    Ok(counts)
}

/// Gives this copy its own pipes, Unix socket pairs, eventfds and epoll
/// instances, which `fork()` shares with the template and every other copy:
/// an event or a byte one copy waits for could otherwise be taken by
/// another. Pipes and socket pairs go first (see `privatize_pipes` and
/// `privatize_socket_pairs`), since an epoll instance may watch them. An
/// eventfd is re-created with its count and flags; an epoll instance with
/// every registration its fdinfo lists, against the copy's own descriptors
/// (re-created first). Descriptors that shared one object share its
/// replacement.
fn privatize_kernel_objects() -> Result<serde_json::Value, String> {
    let fds = open_fds();
    let kind = |fd: RawFd| {
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|target| target.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    let groups = |wanted: &str| {
        let mut groups: Vec<Vec<RawFd>> = Vec::new();
        for &fd in &fds {
            if kind(fd) != wanted {
                continue;
            }
            match groups
                .iter_mut()
                .find(|group| same_description(group[0], fd))
            {
                Some(group) => group.push(fd),
                None => groups.push(vec![fd]),
            }
        }
        groups
    };
    let flags_of = |fd: RawFd| {
        // SAFETY: fcntl on a live descriptor.
        unsafe { libc::fcntl(fd, libc::F_GETFL) }
    };
    let pipes = privatize_pipes(&fds, &kind, &flags_of)?;
    let sockets = privatize_socket_pairs(&fds, &kind, &flags_of)?;
    let mut eventfds = 0;
    for group in groups("anon_inode:[eventfd]") {
        let info = fdinfo(group[0]);
        let count: u64 = fdinfo_field(&info, "eventfd-count")
            .and_then(|count| u64::from_str_radix(&count, 16).ok())
            .unwrap_or(0);
        let semaphore = fdinfo_field(&info, "eventfd-semaphore").as_deref() == Some("1");
        let mut flags = libc::EFD_CLOEXEC;
        if flags_of(group[0]) & libc::O_NONBLOCK != 0 {
            flags |= libc::EFD_NONBLOCK;
        }
        if semaphore {
            flags |= libc::EFD_SEMAPHORE;
        }
        // SAFETY: a new descriptor, closed below after its duplicates.
        let new = unsafe { libc::eventfd(0, flags) };
        if new < 0 {
            return Err(format!("eventfd: {}", std::io::Error::last_os_error()));
        }
        if count > 0 {
            // SAFETY: writes the 8-byte count to the new eventfd.
            unsafe { libc::write(new, (&count as *const u64).cast(), 8) };
        }
        for &fd in &group {
            if !dup_onto(new, fd) {
                return Err(format!(
                    "dup3 eventfd onto {fd}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // SAFETY: the object lives on behind the group's numbers.
        unsafe { libc::close(new) };
        eventfds += 1;
    }
    let mut epolls = 0;
    let mut registrations = 0;
    for group in groups("anon_inode:[eventpoll]") {
        let info = fdinfo(group[0]);
        // SAFETY: a new descriptor, closed below after its duplicates.
        let new = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if new < 0 {
            return Err(format!(
                "epoll_create1: {}",
                std::io::Error::last_os_error()
            ));
        }
        for line in info.lines().filter(|line| line.starts_with("tfd:")) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let value = |key: &str| {
                fields
                    .iter()
                    .position(|field| *field == key)
                    .and_then(|at| fields.get(at + 1))
                    .copied()
            };
            let (Some(target), Some(events), Some(data)) =
                (value("tfd:"), value("events:"), value("data:"))
            else {
                continue;
            };
            let (Ok(target), Ok(events), Ok(data)) = (
                target.parse::<RawFd>(),
                u32::from_str_radix(events, 16),
                u64::from_str_radix(data, 16),
            ) else {
                continue;
            };
            let mut event = libc::epoll_event { events, u64: data };
            // SAFETY: registers a live descriptor of this process.
            if unsafe { libc::epoll_ctl(new, libc::EPOLL_CTL_ADD, target, &mut event) } < 0 {
                return Err(format!(
                    "epoll_ctl ADD {target}: {}",
                    std::io::Error::last_os_error()
                ));
            }
            registrations += 1;
        }
        for &fd in &group {
            if !dup_onto(new, fd) {
                return Err(format!(
                    "dup3 epoll onto {fd}: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // SAFETY: the object lives on behind the group's numbers.
        unsafe { libc::close(new) };
        epolls += 1;
    }
    Ok(serde_json::json!({
        "pipes": pipes.copied,
        "pipes_closed_end": pipes.lone,
        "pipe_bytes": pipes.bytes,
        "pipes_external": pipes.external,
        "socket_pairs": sockets.copied,
        "sockets_closed_peer": sockets.lone,
        "socket_bytes": sockets.bytes,
        "sockets_external": sockets.external,
        "eventfds": eventfds,
        "epolls": epolls,
        "registrations": registrations,
    }))
}

fn fork_for_copy() -> Result<libc::pid_t, String> {
    if debug_switch("libcfork") {
        // SAFETY: glibc's fork; the debugging comparison for fork_process.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!("fork: {}", std::io::Error::last_os_error()));
        }
        return Ok(pid);
    }
    crate::fork_threads::fork_process()
}

/// A run frozen for copying: every other thread parked with its context
/// saved, the backend's state read, and the positions of its files.
struct Template {
    hooks: BackendHooks,
    parked: crate::fork_threads::Parked,
    park_us: u64,
    prepare_us: u64,
    prepared: String,
    records: Vec<FileRecord>,
    records_us: u64,
    /// Each output's offset at the cut, as `<name>_offset`.
    offsets: serde_json::Map<String, serde_json::Value>,
    /// The unnamed regular files open at the cut, as (device, inode): the
    /// template and every copy reach them (`timeline_sharing`).
    unnamed: std::collections::HashSet<(u64, u64)>,
}

/// Freezes this run as a template. Nothing may log from here until the
/// parked threads are released: a parked thread could hold the log
/// writer's lock. `own` is where the driver writes this run's outputs
/// (its directory and timeline number), for the offsets of piped ones.
fn capture_template(
    hooks: BackendHooks,
    own: Option<(&std::path::Path, u64)>,
) -> Result<Template, String> {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Every other thread stops in a signal handler with its context saved,
    // so the copies can re-create it.
    let park_started = Instant::now();
    let parked = crate::fork_threads::park_others(std::time::Duration::from_secs(5))?;
    let park_us = micros(park_started);
    // A copy could write a file the guest maps shared with no syscall to
    // refuse, so such a capture is refused (owner, Slack 6804).
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    if let Some(name) = crate::timeline_sharing::shared_file_mapping(&maps) {
        parked.release();
        return Err(format!(
            "the guest maps {name} shared, and each copy would share its pages with the \
             template and the other copies; shared file mappings are {}",
            crate::timeline_sharing::REFUSAL_PHRASE
        ));
    }
    let prepare_started = Instant::now();
    let prepared = (hooks.prepare)();
    let prepare_us = micros(prepare_started);
    let prepared = match prepared {
        Ok(description) => description,
        Err(error) => {
            parked.release();
            return Err(error);
        }
    };
    let records_started = Instant::now();
    let records = record_files();
    let unnamed = crate::timeline_sharing::unnamed_files(&open_fds());
    let records_us = micros(records_started);
    let mut offsets = serde_json::Map::new();
    for (fd, name) in outputs() {
        let file = own.map(|(dir, id)| dir.join(format!("timeline-{id}.{name}")));
        match cut_offset(fd, file.as_deref()) {
            Ok(offset) => {
                offsets.insert(format!("{name}_offset"), offset.into());
            }
            Err(error) => {
                parked.release();
                return Err(format!("{name}: {error}"));
            }
        }
    }
    Ok(Template {
        hooks,
        parked,
        park_us,
        prepare_us,
        prepared,
        records,
        records_us,
        offsets,
        unnamed,
    })
}

/// Takes the copies `spec` asks for, at `place` (a description, for the
/// measurements). Returns `None` in the template once every copy has exited,
/// and `Some(k)` in copy k once it is repaired, so that the caller goes on
/// with the run.
pub(crate) fn fork_copies(spec: &ForkSpec, place: serde_json::Value) -> Option<usize> {
    let Some(hooks) = HOOKS.get().copied() else {
        tracing::warn!("{TIMELINE_FORK_ENV}: this backend installed no host-fork hooks");
        return None;
    };
    if let Err(error) = std::fs::create_dir_all(&spec.dir) {
        tracing::warn!("{TIMELINE_FORK_ENV}: {}: {error}", spec.dir.display());
        return None;
    }
    let template = match capture_template(hooks, None) {
        Ok(template) => template,
        Err(error) => {
            let _ = std::fs::write(spec.dir.join("refused.txt"), format!("{error}\n"));
            tracing::warn!("{TIMELINE_FORK_ENV}: capture refused: {error}");
            return None;
        }
    };
    let mut children = Vec::new();
    let all_started = Instant::now();
    let cpu = CopyCpu::from_cpus(&spec.cpus);
    before_placed_forks(&cpu);
    if spec.parallel {
        // Every copy starts now; the template waits for all of them. Copies
        // only read the template's memory, which stays still while it waits.
        let mut running = Vec::new();
        for index in 0..spec.copies {
            let forked = Instant::now();
            let pid = match fork_for_copy() {
                Ok(pid) => pid,
                Err(error) => {
                    children.push(serde_json::json!({ "index": index, "error": error }));
                    break;
                }
            };
            if pid == 0 {
                become_timeline(
                    &spec.dir, &cpu, None, index, index, &place, &template, forked,
                );
                return Some(index);
            }
            running.push((index, pid, forked, micros(forked)));
        }
        while !running.is_empty() {
            running.retain(|&(index, pid, forked, fork_us)| {
                let mut status = 0;
                // SAFETY: polls one copy forked above.
                let waited = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
                if waited == 0
                    || (waited < 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR))
                {
                    return true;
                }
                children.push(serde_json::json!({
                    "index": index,
                    "pid": pid,
                    "fork_us": fork_us,
                    "wall_us": micros(forked),
                    "exit_code": (waited == pid && libc::WIFEXITED(status))
                        .then(|| libc::WEXITSTATUS(status)),
                    "signal": (waited == pid && libc::WIFSIGNALED(status))
                        .then(|| libc::WTERMSIG(status)),
                }));
                false
            });
            if !running.is_empty() {
                std::thread::sleep(std::time::Duration::from_micros(200));
            }
        }
    }
    for index in 0..spec.copies {
        if spec.parallel {
            break;
        }
        let forked = Instant::now();
        // Every other thread is parked with its context saved; the copy
        // re-creates them (become_timeline).
        let pid = match fork_for_copy() {
            Ok(pid) => pid,
            Err(error) => {
                children.push(serde_json::json!({ "index": index, "error": error }));
                break;
            }
        };
        if pid == 0 {
            become_timeline(
                &spec.dir, &cpu, None, index, index, &place, &template, forked,
            );
            return Some(index);
        }
        let fork_us = micros(forked);
        let mut status = 0;
        let waited = loop {
            // SAFETY: waits for the copy just forked.
            let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
            if waited < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break waited;
        };
        let wall_us = micros(forked);
        if waited != pid {
            // A copy left running would read the template's guest memory
            // while the template changes it: stop it before going on.
            let error = std::io::Error::last_os_error();
            // SAFETY: signals and reaps the copy forked above.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                libc::waitpid(pid, std::ptr::null_mut(), 0);
            }
            children.push(
                serde_json::json!({ "index": index, "pid": pid, "wait_error": error.to_string() }),
            );
            break;
        }
        children.push(serde_json::json!({
            "index": index,
            "pid": pid,
            "fork_us": fork_us,
            "wall_us": wall_us,
            "exit_code": libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status)),
            "signal": libc::WIFSIGNALED(status).then(|| libc::WTERMSIG(status)),
        }));
    }
    template.parked.release();
    let summary = serde_json::json!({
        "backend": template.hooks.name,
        "parked_threads": template.parked.count(),
        "park_us": template.park_us,
        "spec": format!("{spec:?}"),
        "place": place,
        "copies": spec.copies,
        "parallel": spec.parallel,
        "all_copies_wall_us": micros(all_started),
        "prepare_us": template.prepare_us,
        "prepare": template.prepared,
        "file_records": template.records.len(),
        "file_records_us": template.records_us,
        "offsets": template.offsets,
        "children": children,
    });
    let _ = std::fs::write(
        spec.dir.join("template.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        ),
    );
    None
}

/// Ends copy `index` before any of its guest code runs, as a run Detcore
/// refuses ends: `hermit: <message>` on its own standard error, then
/// `HERMIT_POLICY_REFUSAL_EXIT`, recorded where its driver and template
/// look for it. Its threads are not re-created yet and may hold locks, so
/// this takes none (no logging, no flushing, no control message: the copy
/// has no connection of its own yet).
fn refuse_copy(dir: &std::path::Path, index: usize, message: &str) -> ! {
    let line = format!("hermit: {message}\n");
    // SAFETY: writes from a live buffer to this copy's standard error.
    let _ = unsafe { libc::write(2, line.as_ptr().cast(), line.len()) };
    let code = detcore_model::HERMIT_POLICY_REFUSAL_EXIT;
    let _ = std::fs::write(
        dir.join(format!("timeline-{index}.exit")),
        format!("{code}\n"),
    );
    // SAFETY: _exit ends the process at once.
    unsafe { libc::_exit(code) }
}

/// Turns this new process into copy `index`, the `position`-th of the batch
/// its template forked together.
#[allow(clippy::too_many_arguments)]
fn become_timeline(
    dir: &std::path::Path,
    cpu: &CopyCpu,
    memory: Option<MemoryCopy>,
    index: usize,
    position: usize,
    place: &serde_json::Value,
    template: &Template,
    forked: Instant,
) {
    let Template {
        hooks,
        records,
        parked,
        unnamed,
        ..
    } = template;
    let started = Instant::now();
    TIMELINE.store(index as i64, Ordering::Relaxed);
    let _ = TIMELINE_DIR.set(dir.to_path_buf());
    if debug_value("overshoot").and_then(|value| value.parse::<usize>().ok()) == Some(index) {
        reverie::record_skid_overshoot();
    }
    let fail = |what: &str, error: String| -> ! {
        let _ = std::fs::write(
            dir.join(format!("timeline-{index}.error")),
            format!("{what}: {error}\n"),
        );
        // SAFETY: _exit ends the process at once.
        unsafe { libc::_exit(97) };
    };
    // First, so that copies started together repair on their own CPUs
    // rather than queueing on the template's pinned CPU; the re-created
    // threads get the same set below. A pool gives copy j of a batch the
    // pool's CPU j; self-placement chooses and reserves one, as for a fresh
    // run, by the copy's position in its batch, so that copy j of every batch
    // goes back to the CPU copy j had before when it is still free; an
    // unpinned copy may run on every CPU the run may use.
    let (cpus, placement): (Option<Vec<usize>>, String) = match cpu {
        CopyCpu::Pool(pool) if !pool.is_empty() => (
            Some(vec![pool[position % pool.len()]]),
            "the driver's pool".into(),
        ),
        CopyCpu::Pool(_) => (None, "the template's: an empty pool".into()),
        CopyCpu::SelfPlaced => match COPY_PLACEMENT
            .get()
            .map(|placement| (placement.place)(position))
        {
            Some(Ok(Some((cpu, how)))) => (Some(vec![cpu]), how),
            Some(Ok(None)) => (None, "the template's: placement chose none".into()),
            Some(Err(error)) => (None, format!("the template's: placement failed: {error}")),
            None => (None, "the template's: no placement".into()),
        },
        CopyCpu::Unpinned => match COPY_PLACEMENT.get().map(|placement| (placement.allowed)()) {
            Some(allowed) if !allowed.is_empty() => {
                let how = format!("unpinned: the {} CPUs the run may use", allowed.len());
                (Some(allowed), how)
            }
            _ => (
                None,
                "unpinned: the template's affinity, as the run did not pin itself".into(),
            ),
        },
    };
    let affinity = cpus.as_ref().map(|cpus| {
        // SAFETY: a zeroed set with these CPUs.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            for &cpu in cpus {
                libc::CPU_SET(cpu, &mut set);
            }
            set
        }
    });
    if let Some(set) = &affinity {
        // SAFETY: applies to this thread, the copy's only one so far.
        if unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set) } != 0 {
            fail("pinning", std::io::Error::last_os_error().to_string());
        }
    }
    // A single CPU, for the summary.
    let cpu = cpus
        .as_ref()
        .filter(|cpus| cpus.len() == 1)
        .map(|cpus| cpus[0]);
    let repair_started = Instant::now();
    let repaired = match (hooks.repair)(memory) {
        Ok(description) => description,
        Err(error) => fail("backend repair", error),
    };
    let repair_us = micros(repair_started);
    install_fault_report(dir, index);
    let objects_started = Instant::now();
    let objects = match privatize_kernel_objects() {
        Ok(objects) => objects,
        Err(error) => fail("private kernel objects", error),
    };
    let objects_us = micros(objects_started);
    let files_started = Instant::now();
    let reopened = reopen_files(records);
    let files_us = micros(files_started);
    if let Err(error) = redirect_outputs(dir, index) {
        fail("redirecting output", error.to_string());
    }
    record_exit_at_process_exit();
    let own_outputs = [1, 2]
        .into_iter()
        .chain(outputs().into_iter().map(|(fd, _)| fd))
        .collect::<Vec<_>>();
    let copy = crate::timeline_sharing::CopyContext {
        index,
        outputs: crate::timeline_sharing::identities(&own_outputs),
        inherited_unnamed: unnamed.clone(),
    };
    match crate::timeline_sharing::become_copy(copy) {
        Ok(None) => {}
        // Refused before any guest code runs in the copy.
        Ok(Some(message)) => refuse_copy(dir, index, &message),
        Err(error) => fail("sharing gate", error),
    }
    let summary = serde_json::json!({
        "index": index,
        // SAFETY: getpid has no preconditions.
        "pid": unsafe { libc::getpid() },
        "place": place,
        "fork_to_child_us": started.duration_since(forked).as_micros() as u64,
        "repair_us": repair_us,
        "repair": repaired,
        "kernel_objects": objects,
        "kernel_objects_us": objects_us,
        "files_reopened": reopened,
        "files_us": files_us,
        "cpu": cpu,
        "cpus": cpus.as_ref().map(Vec::len),
        "placement": placement,
        "memory": memory.map_or("the backend's default", MemoryCopy::name),
        "ready_us": micros(forked),
    });
    let _ = std::fs::write(
        dir.join(format!("timeline-{index}.json")),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&summary).unwrap_or_default()
        ),
    );
    // Last, once everything they share is the copy's own: the other threads.
    let threads_started = Instant::now();
    if debug_switch("noresurrect") {
        return;
    }
    match parked.resurrect(affinity.as_ref()) {
        Ok(threads) => {
            let _ = std::fs::write(
                dir.join(format!("timeline-{index}.threads")),
                format!("{threads} {}\n", micros(threads_started)),
            );
        }
        Err(error) => fail("re-creating threads", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_spec() {
        assert_eq!(
            ForkSpec::parse("turn=40,copies=3,dir=/tmp/x").unwrap(),
            ForkSpec {
                turn: Some(40),
                choice: None,
                bytes: vec![],
                copies: 3,
                parallel: false,
                cpus: vec![],
                dir: PathBuf::from("/tmp/x"),
            }
        );
        assert_eq!(
            ForkSpec::parse("choice=7,copies=2,dir=/d,bytes=0:144").unwrap(),
            ForkSpec {
                turn: None,
                choice: Some(7),
                bytes: vec![0, 144],
                copies: 2,
                parallel: false,
                cpus: vec![],
                dir: PathBuf::from("/d"),
            }
        );
        assert!(ForkSpec::parse("turn=1,choice=2,dir=/d").is_err());
        let parallel = ForkSpec::parse("turn=5,copies=4,parallel=1,cpus=3:5,dir=/d").unwrap();
        assert!(parallel.parallel);
        assert_eq!(parallel.cpus, vec![3, 5]);
        assert!(ForkSpec::parse("copies=3,dir=/tmp/x").is_err());
        assert!(ForkSpec::parse("turn=4,dir=/tmp/x,colour=red").is_err());
    }

    #[test]
    fn parses_a_control_spec() {
        assert_eq!(
            ControlSpec::parse("socket=/d/c.sock,id=2,dir=/d,stop=3,cpus=4:9").unwrap(),
            ControlSpec {
                socket: PathBuf::from("/d/c.sock"),
                id: 2,
                dir: PathBuf::from("/d"),
                stop: Some(3),
                cpus: vec![4, 9],
            }
        );
        let plain = ControlSpec::parse("socket=/s,id=0,dir=/d").unwrap();
        assert_eq!(plain.stop, None);
        assert!(plain.cpus.is_empty());
        assert!(ControlSpec::parse("id=1,dir=/d").is_err());
        assert!(ControlSpec::parse("socket=/s,dir=/d").is_err());
        assert!(ControlSpec::parse("socket=/s,id=1").is_err());
        assert!(ControlSpec::parse("socket=/s,id=x,dir=/d").is_err());
        assert!(ControlSpec::parse("socket=/s,id=1,dir=/d,turn=3").is_err());
    }

    #[test]
    fn parses_commands() {
        let command = |text: &str| parse_command(&serde_json::from_str(text).unwrap());
        assert!(matches!(
            command(r#"{"answer":7,"until":12}"#),
            Ok(Command::Answer {
                value: 7,
                until: Some(12)
            })
        ));
        assert!(matches!(
            command(r#"{"source":true,"until":null}"#),
            Ok(Command::Source { until: None })
        ));
        assert!(matches!(
            command(r#"{"capture":true}"#),
            Ok(Command::Capture)
        ));
        assert!(command(r#"{"answer":"seven"}"#).is_err());
        assert!(command(r#"{"fork":{"ids":[1]}}"#).is_err());
    }

    fn link(fd: RawFd) -> String {
        std::fs::read_link(format!("/proc/self/fd/{fd}"))
            .map(|target| target.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn flags(fd: RawFd) -> libc::c_int {
        // SAFETY: fcntl on a descriptor.
        unsafe { libc::fcntl(fd, libc::F_GETFL) }
    }

    fn put(fd: RawFd, bytes: &[u8]) {
        // SAFETY: writes from a live buffer.
        let written = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
        assert_eq!(
            written,
            bytes.len() as isize,
            "{}",
            std::io::Error::last_os_error()
        );
    }

    fn take(fd: RawFd) -> Vec<u8> {
        let mut buffer = [0u8; 64];
        // SAFETY: reads into a live buffer.
        let got = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        assert!(got >= 0, "read: {}", std::io::Error::last_os_error());
        buffer[..got as usize].to_vec()
    }

    fn new_pipe() -> [RawFd; 2] {
        let mut fds = [-1; 2];
        // SAFETY: room for two descriptors.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        fds
    }

    fn new_socket_pair(kind: libc::c_int) -> [RawFd; 2] {
        let mut fds = [-1; 2];
        // SAFETY: room for two descriptors.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                kind | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        assert_eq!(rc, 0);
        fds
    }

    fn dup(fd: RawFd) -> RawFd {
        // SAFETY: duplicates a live descriptor.
        unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) }
    }

    fn close(fds: &[RawFd]) {
        for &fd in fds {
            // SAFETY: closes a descriptor the test opened.
            unsafe { libc::close(fd) };
        }
    }

    fn counts(counts: StreamCounts) -> (usize, u64, usize, usize) {
        (counts.copied, counts.bytes, counts.lone, counts.external)
    }

    /// A new, empty file for one test, named after it.
    fn test_file(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hermit-timelines-{}-{name}.stderr",
            std::process::id()
        ));
        std::fs::File::create(&path).unwrap();
        path
    }

    /// Drains `reader` into `file` the way safehermit's byte cap drains
    /// standard error: from another thread, late. It waits `before_read`,
    /// reads what is there, waits `before_write`, writes it, and so on until
    /// the pipe closes.
    fn drain_into(
        reader: RawFd,
        file: &std::path::Path,
        before_read: std::time::Duration,
        before_write: std::time::Duration,
    ) -> std::thread::JoinHandle<()> {
        let mut out = std::fs::OpenOptions::new().append(true).open(file).unwrap();
        std::thread::spawn(move || {
            std::thread::sleep(before_read);
            let mut buffer = [0u8; 4096];
            loop {
                // SAFETY: reads into the local buffer.
                let got = unsafe { libc::read(reader, buffer.as_mut_ptr().cast(), buffer.len()) };
                if got <= 0 {
                    break;
                }
                std::thread::sleep(before_write);
                out.write_all(&buffer[..got as usize]).unwrap();
            }
        })
    }

    #[test]
    fn a_seekable_output_is_cut_at_its_offset() {
        let path = test_file("seekable");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"start\n").unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        assert_eq!(cut_offset(fd, Some(&path)), Ok(6));
        assert_eq!(cut_offset(fd, None), Ok(6));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_piped_output_is_cut_at_the_size_of_the_file_it_drains_into() {
        // The drainer reads 150 ms late, longer than the settle time, and
        // writes 30 ms after each read. A cut that did not wait for the pipe
        // to empty, or for the file to settle after it, would see fewer than
        // the 6 bytes written before it.
        let path = test_file("piped");
        let [reader, writer] = new_pipe();
        let drainer = drain_into(
            reader,
            &path,
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(30),
        );
        put(writer, b"start\n");
        assert_eq!(cut_offset(writer, Some(&path)), Ok(6));
        // Without a file to read, a pipe's offset stays unknown.
        assert_eq!(cut_offset(writer, None), Ok(-1));
        close(&[writer]);
        drainer.join().unwrap();
        close(&[reader]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_piped_output_that_does_not_drain_is_refused() {
        let path = test_file("undrained");
        let [reader, writer] = new_pipe();
        put(writer, b"nobody reads this\n");
        let refused = cut_offset_within(
            writer,
            Some(&path),
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(10),
        )
        .unwrap_err();
        assert!(refused.contains("still holding 18 bytes"), "{refused}");
        close(&[reader, writer]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_piped_output_without_its_file_is_refused() {
        let [reader, writer] = new_pipe();
        let missing = std::env::temp_dir().join(format!(
            "hermit-timelines-{}-missing.stderr",
            std::process::id()
        ));
        let refused = cut_offset(writer, Some(&missing)).unwrap_err();
        assert!(refused.contains("the file it drains into"), "{refused}");
        close(&[reader, writer]);
    }

    #[test]
    fn a_copy_gets_its_own_pipe_with_the_buffered_bytes() {
        let [reader, writer] = new_pipe();
        put(writer, b"hello");
        // A second description of the read end, nonblocking.
        let path = format!("/proc/self/fd/{reader}\0");
        // SAFETY: a NUL-terminated path.
        let second = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_NONBLOCK) };
        assert!(second >= 0);
        let shared = dup(reader);
        let result = privatize_pipes(&[reader, second, writer], &link, &flags).unwrap();
        assert_eq!(counts(result), (1, 5, 0, 0));
        assert_ne!(link(reader), link(shared));
        assert_eq!(link(reader), link(second));
        assert!(!same_description(reader, second));
        assert_eq!(flags(second) & libc::O_NONBLOCK, libc::O_NONBLOCK);
        assert_eq!(flags(reader) & libc::O_NONBLOCK, 0);
        put(writer, b"!");
        assert_eq!(take(reader), b"hello!");
        // The shared pipe still has its bytes for the template.
        assert_eq!(take(shared), b"hello");
        close(&[writer]);
        assert_eq!(take(reader), b"");
        close(&[reader, second, shared]);
    }

    #[test]
    fn a_pipe_without_a_writer_ends_after_its_bytes() {
        let [reader, writer] = new_pipe();
        put(writer, b"abc");
        close(&[writer]);
        let shared = dup(reader);
        let result = privatize_pipes(&[reader], &link, &flags).unwrap();
        assert_eq!(counts(result), (1, 3, 1, 0));
        assert_eq!(take(reader), b"abc");
        assert_eq!(take(reader), b"");
        assert_eq!(take(shared), b"abc");
        close(&[reader, shared]);
    }

    #[test]
    fn a_pipe_with_a_writer_elsewhere_stays_shared() {
        let [reader, writer] = new_pipe();
        let before = link(reader);
        let result = privatize_pipes(&[reader], &link, &flags).unwrap();
        assert_eq!(counts(result), (0, 0, 0, 1));
        assert_eq!(link(reader), before);
        close(&[reader, writer]);
    }

    #[test]
    fn a_copy_gets_its_own_socket_pair_with_the_waiting_bytes() {
        let [left, right] = new_socket_pair(libc::SOCK_STREAM);
        put(right, b"ping");
        put(left, b"pong");
        let shared = dup(left);
        let result = privatize_socket_pairs(&[left, right], &link, &flags).unwrap();
        assert_eq!(counts(result), (1, 8, 0, 0));
        assert_ne!(link(left), link(shared));
        assert_eq!(take(left), b"ping");
        assert_eq!(take(right), b"pong");
        assert_eq!(take(shared), b"ping");
        put(left, b"z");
        assert_eq!(take(right), b"z");
        close(&[left, right, shared]);
    }

    #[test]
    fn a_socket_whose_peer_closed_ends_after_its_bytes() {
        let [left, right] = new_socket_pair(libc::SOCK_STREAM);
        put(right, b"bye");
        close(&[right]);
        let result = privatize_socket_pairs(&[left], &link, &flags).unwrap();
        assert_eq!(counts(result), (1, 3, 1, 0));
        assert_eq!(take(left), b"bye");
        assert_eq!(take(left), b"");
        close(&[left]);
    }

    #[test]
    fn a_socket_with_a_live_peer_elsewhere_stays_shared() {
        let [left, right] = new_socket_pair(libc::SOCK_STREAM);
        let before = link(left);
        let result = privatize_socket_pairs(&[left], &link, &flags).unwrap();
        assert_eq!(counts(result), (0, 0, 0, 1));
        assert_eq!(link(left), before);
        close(&[left, right]);
    }

    #[test]
    fn refuses_a_datagram_pair_with_messages_waiting() {
        let [left, right] = new_socket_pair(libc::SOCK_DGRAM);
        put(right, b"m");
        let before = link(left);
        let error = privatize_socket_pairs(&[left, right], &link, &flags)
            .err()
            .unwrap();
        assert!(error.contains("non-stream"), "{error}");
        assert_eq!(link(left), before);
        close(&[left, right]);
    }
}
