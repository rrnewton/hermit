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
    /// Pin copy k to CPU `cpus[k % len]` (empty: keep the template's CPU).
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
    /// In each copy, before anything else: rebuild what fork did not copy.
    pub repair: fn() -> Result<String, String>,
}

static HOOKS: OnceLock<BackendHooks> = OnceLock::new();

/// Installs the running backend's hooks, once per process, and, for a spec
/// that forks at a choice point, the choice pause hook that takes the copies.
pub fn install_backend_hooks(hooks: BackendHooks) {
    let _ = HOOKS.set(hooks);
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
/// copy per id, and `{"release":true}`, after which it ends without running
/// on. Each copy connects back with `{"hello":{"timeline":id,...}}` and is
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
    /// Pin copy k to CPU `cpus[k % len]` (empty: keep the template's CPU).
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
    let template = match capture_template(hooks) {
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
            let mut forked = Vec::new();
            for id in ids {
                let started = Instant::now();
                match fork_for_copy() {
                    Ok(0) => {
                        become_timeline(
                            &dir,
                            &cpus,
                            id as usize,
                            &place,
                            &template.hooks,
                            &template.records,
                            &template.parked,
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
/// `libcfork` forks through glibc, `noresurrect` re-creates no thread.
fn debug_switch(name: &str) -> bool {
    std::env::var("HERMIT_TIMELINE_DEBUG")
        .map(|value| value.split(',').any(|part| part == name))
        .unwrap_or(false)
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
    }
    send_control(&serde_json::json!({ "exit": code }));
    // SAFETY: _exit ends the process at once; nothing runs after it.
    unsafe { libc::_exit(code) };
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

/// Gives this copy its own eventfds and epoll instances, which `fork()`
/// shares with the template and every other copy: an event one copy waits
/// for could otherwise be taken by another. An eventfd is re-created with
/// its count and flags; an epoll instance with every registration its
/// fdinfo lists, against the copy's own descriptors (re-created first).
/// Descriptors that shared one object share its replacement.
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
}

/// Freezes this run as a template. Nothing may log from here until the
/// parked threads are released: a parked thread could hold the log
/// writer's lock.
fn capture_template(hooks: BackendHooks) -> Result<Template, String> {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // Every other thread stops in a signal handler with its context saved,
    // so the copies can re-create it.
    let park_started = Instant::now();
    let parked = crate::fork_threads::park_others(std::time::Duration::from_secs(5))?;
    let park_us = micros(park_started);
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
    let records_us = micros(records_started);
    let offsets = outputs()
        .into_iter()
        .map(|(fd, name)| (format!("{name}_offset"), offset_of(fd).into()))
        .collect();
    Ok(Template {
        hooks,
        parked,
        park_us,
        prepare_us,
        prepared,
        records,
        records_us,
        offsets,
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
    let template = match capture_template(hooks) {
        Ok(template) => template,
        Err(error) => {
            let _ = std::fs::write(spec.dir.join("refused.txt"), format!("{error}\n"));
            tracing::warn!("{TIMELINE_FORK_ENV}: capture refused: {error}");
            return None;
        }
    };
    let Template {
        hooks,
        parked,
        park_us,
        prepare_us,
        prepared,
        records,
        records_us,
        offsets,
    } = template;
    let mut children = Vec::new();
    let all_started = Instant::now();
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
                    &spec.dir, &spec.cpus, index, &place, &hooks, &records, &parked, forked,
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
                &spec.dir, &spec.cpus, index, &place, &hooks, &records, &parked, forked,
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
    parked.release();
    let summary = serde_json::json!({
        "backend": hooks.name,
        "parked_threads": parked.count(),
        "park_us": park_us,
        "spec": format!("{spec:?}"),
        "place": place,
        "copies": spec.copies,
        "parallel": spec.parallel,
        "all_copies_wall_us": micros(all_started),
        "prepare_us": prepare_us,
        "prepare": prepared,
        "file_records": records.len(),
        "file_records_us": records_us,
        "offsets": offsets,
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

#[allow(clippy::too_many_arguments)]
fn become_timeline(
    dir: &std::path::Path,
    cpus: &[usize],
    index: usize,
    place: &serde_json::Value,
    hooks: &BackendHooks,
    records: &[FileRecord],
    parked: &crate::fork_threads::Parked,
    forked: Instant,
) {
    let started = Instant::now();
    TIMELINE.store(index as i64, Ordering::Relaxed);
    let _ = TIMELINE_DIR.set(dir.to_path_buf());
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
    // threads get the same set below.
    let cpu = (!cpus.is_empty()).then(|| cpus[index % cpus.len()]);
    let affinity = cpu.map(|cpu| {
        // SAFETY: a zeroed set with one CPU.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            set
        }
    });
    if let Some(set) = &affinity {
        // SAFETY: applies to this thread, the copy's only one so far.
        if unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), set) } != 0 {
            fail("pinning", std::io::Error::last_os_error().to_string());
        }
    }
    let repair_started = Instant::now();
    let repaired = match (hooks.repair)() {
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
}
