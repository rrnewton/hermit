/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): [`Timelines`] over host-fork copies
//! of a KVM run (detcore's `HERMIT_TIMELINE_CONTROL` mode).
//!
//! This driver listens on a Unix socket. A run started under its control
//! pauses at the choice request the driver names, connects, and waits for a
//! command. `capture` turns it into a template that forks copies on
//! request; each copy connects back with its own timeline number, paused at
//! the template's request, and is answered and resumed like any paused run.
//! START is a fresh launch, made when the copy is resumed. Each timeline
//! writes `dir/timeline-<n>.{stdout,stderr,log,choices}`.

use std::collections::HashMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use super::Batch;
use super::Capabilities;
use super::Checkpoint;
use super::CopyMode;
use super::CopyPolicy;
use super::Cut;
use super::Observation;
use super::Part;
use super::PendingRequest;
use super::Placement;
use super::Refusal;
use super::RunOutcome;
use super::RunSpec;
use super::Stop;
use super::StopAt;
use super::Timelines;
use super::Value;
use super::refuse_answer_at_start;

/// detcore::timelines::TIMELINE_CONTROL_ENV.
const CONTROL_ENV: &str = "HERMIT_TIMELINE_CONTROL";

/// The longest a protocol reply may take (a capture or fork of a large run).
const REPLY_BOUND: Duration = Duration::from_secs(120);

/// One process's connection: JSON lines both ways.
struct Connection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    /// The process at the other end, as this process's PID namespace sees it.
    pid: Option<i32>,
}

impl Connection {
    fn new(stream: UnixStream) -> Result<Self, Refusal> {
        let pid = peer_pid(&stream);
        let writer = stream
            .try_clone()
            .map_err(|e| Refusal::new("timeline-socket", e.to_string()))?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            pid,
        })
    }

    fn send(&mut self, message: &serde_json::Value) -> Result<(), Refusal> {
        let mut line = message.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| Refusal::new("timeline-connection-lost", e.to_string()))
    }

    /// The next message; `None` once the other end closed the connection.
    fn receive(&mut self, bound: Duration) -> Result<Option<serde_json::Value>, Refusal> {
        let _ = self
            .reader
            .get_ref()
            .set_read_timeout(Some(bound.max(Duration::from_millis(1))));
        let mut line = String::new();
        match self.reader.read_line(&mut line) {
            Ok(0) => Ok(None),
            Ok(_) => serde_json::from_str(&line).map(Some).map_err(|e| {
                Refusal::new("timeline-protocol", format!("not JSON ({e}): {line:?}"))
            }),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(Refusal::new(
                    "timeline-copy-timed-out",
                    format!("no message within {bound:?}"),
                ))
            }
            Err(e) => Err(Refusal::new("timeline-connection-lost", e.to_string())),
        }
    }

    /// The next message that has `key`, skipping reports such as `reaped`.
    fn expect(&mut self, key: &str, bound: Duration) -> Result<serde_json::Value, Refusal> {
        let deadline = Instant::now() + bound;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self.receive(remaining)?.ok_or_else(|| {
                Refusal::new(
                    "timeline-connection-lost",
                    format!("closed while waiting for {key}"),
                )
            })?;
            if let Some(reason) = message.get("refused") {
                return Err(Refusal::new("timeline-capture-refused", reason.to_string()));
            }
            if let Some(value) = message.get(key) {
                return Ok(value.clone());
            }
        }
    }
}

fn peer_pid(stream: &UnixStream) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: SO_PEERCRED into a buffer of the size it fills.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    (rc == 0 && credentials.pid > 0).then_some(credentials.pid)
}

pub struct KvmTimelines {
    listener: UnixListener,
    socket: PathBuf,
    dir: PathBuf,
    next_id: u64,
    /// Connections that said hello before anyone waited for them.
    arrived: HashMap<u64, Connection>,
}

/// A START checkpoint (no process) or a template paused at a request.
pub struct KvmCheckpoint {
    cut: Cut,
    index: u64,
    observation: Observation,
    run: RunSpec,
    template: Option<Arc<Mutex<Template>>>,
}

struct Template {
    connection: Connection,
    /// The template's process, when this driver started it.
    child: Option<Child>,
}

pub struct KvmTimeline {
    id: u64,
    state: CopyState,
}

enum CopyState {
    /// A copy of a START checkpoint: no process yet.
    Unstarted(RunSpec),
    /// A process forked by a template, paused at its request; it connects
    /// when it is ready.
    Forked(RunSpec, Option<Connection>),
    /// A dormant copy ([`CopyPolicy::BACKUP`]): no process yet. Its template
    /// forks it when it is first resumed, placed as its batch chose.
    Dormant(RunSpec, Arc<Mutex<Template>>, Placement),
    Done,
}

/// A run paused at an unanswered request.
pub struct KvmPaused {
    id: u64,
    connection: Connection,
    child: Option<Child>,
    request: PendingRequest,
    run: RunSpec,
}

impl KvmPaused {
    pub fn request(&self) -> &PendingRequest {
        &self.request
    }
    pub fn timeline(&self) -> u64 {
        self.id
    }
}

impl KvmTimeline {
    pub fn timeline(&self) -> u64 {
        self.id
    }
}

/// A copy that was sent on its way, not yet followed to its stop.
struct Running {
    id: u64,
    connection: Option<Connection>,
    child: Option<Child>,
    started: Instant,
    run: RunSpec,
}

impl KvmTimelines {
    /// A driver whose socket and timelines' files live in `dir`.
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let socket = dir.join("c.sock");
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        Ok(Self {
            listener,
            socket,
            dir,
            next_id: 0,
            arrived: HashMap::new(),
        })
    }

    fn path(&self, id: u64, output: &str) -> PathBuf {
        self.dir.join(format!("timeline-{id}.{output}"))
    }

    /// Timeline `id`'s files, whole, or cut at `offsets` (by output name).
    fn files(
        &self,
        id: u64,
        offsets: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Observation {
        let part = |output: &str| {
            let len = offsets.map(|offsets| {
                offsets
                    .get(&format!("{output}_offset"))
                    .and_then(|offset| offset.as_i64())
                    .filter(|&offset| offset >= 0)
                    .unwrap_or(0) as u64
            });
            Some(Part {
                path: self.path(id, output),
                len,
            })
        };
        Observation {
            stdout: part("stdout"),
            stderr: part("stderr"),
            log: part("log"),
            record: part("choices"),
        }
    }

    fn control_value(&self, id: u64, stop: Option<u64>) -> String {
        let mut value = format!(
            "socket={},id={id},dir={}",
            self.socket.display(),
            self.dir.display()
        );
        if let Some(stop) = stop {
            value.push_str(&format!(",stop={stop}"));
        }
        value
    }

    /// Starts a fresh run as timeline `id`, to pause at `stop` if given.
    fn launch(&mut self, run: &RunSpec, id: u64, stop: Option<u64>) -> Result<Child, Refusal> {
        let (program, launcher_args) = run
            .launcher
            .split_first()
            .ok_or_else(|| Refusal::new("timeline-run-spec", "empty launcher"))?;
        let create = |path: PathBuf| {
            std::fs::File::create(&path)
                .map_err(|e| Refusal::new("timeline-dir", format!("{}: {e}", path.display())))
        };
        Command::new(program)
            .args(launcher_args)
            .args(&run.global_args)
            .arg(format!("--log-file={}", self.path(id, "log").display()))
            .arg("run")
            .args(&run.run_args)
            .arg(format!(
                "--choice-record={}",
                self.path(id, "choices").display()
            ))
            .arg("--")
            .args(&run.guest)
            .env(CONTROL_ENV, self.control_value(id, stop))
            .stdin(Stdio::null())
            .stdout(create(self.path(id, "stdout"))?)
            .stderr(create(self.path(id, "stderr"))?)
            .spawn()
            .map_err(|e| Refusal::new("timeline-launch", e.to_string()))
    }

    /// Waits for timeline `id` to connect and say hello; `None` if its
    /// process (when this driver started it) ended first.
    fn connection_of(
        &mut self,
        id: u64,
        child: &mut Option<Child>,
        bound: Duration,
    ) -> Result<Option<Connection>, Refusal> {
        if let Some(connection) = self.arrived.remove(&id) {
            return Ok(Some(connection));
        }
        let deadline = Instant::now() + bound;
        self.listener
            .set_nonblocking(true)
            .map_err(|e| Refusal::new("timeline-socket", e.to_string()))?;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .map_err(|e| Refusal::new("timeline-socket", e.to_string()))?;
                    let mut connection = Connection::new(stream)?;
                    let hello = connection.expect("hello", Duration::from_secs(30))?;
                    let theirs = hello
                        .get("timeline")
                        .and_then(|id| id.as_u64())
                        .ok_or_else(|| {
                            Refusal::new(
                                "timeline-protocol",
                                format!("hello without timeline: {hello}"),
                            )
                        })?;
                    if theirs == id {
                        return Ok(Some(connection));
                    }
                    self.arrived.insert(theirs, connection);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Some(child) = child.as_mut()
                        && let Ok(Some(_)) = child.try_wait()
                    {
                        return Ok(None);
                    }
                    if Instant::now() > deadline {
                        return Err(Refusal::new(
                            "timeline-no-connection",
                            format!("timeline {id} did not connect within {bound:?}"),
                        ));
                    }
                    std::thread::sleep(Duration::from_micros(200));
                }
                Err(e) => return Err(Refusal::new("timeline-socket", e.to_string())),
            }
        }
    }

    /// Sends a copy on its way: launches it (START) or answers its pending
    /// request; `follow` then waits for its stop.
    fn begin(
        &mut self,
        t: &mut KvmTimeline,
        answer: Option<Value>,
        until: StopAt,
    ) -> Result<Running, Refusal> {
        let stop = match until {
            StopAt::End => None,
            StopAt::Index(j) => Some(j),
        };
        if let CopyState::Dormant(..) = t.state {
            let CopyState::Dormant(run, template, placement) =
                std::mem::replace(&mut t.state, CopyState::Done)
            else {
                unreachable!("matched above");
            };
            // Made now, the way a fork-and-run copy is made at fork.
            let made = CopyPolicy::FORK_AND_RUN;
            if let Err(refusal) = fork_from(&template, &[t.id], &made, &placement) {
                t.state = CopyState::Dormant(run, template, placement);
                return Err(refusal);
            }
            t.state = CopyState::Forked(run, None);
        }
        match std::mem::replace(&mut t.state, CopyState::Done) {
            CopyState::Dormant(..) => unreachable!("made above"),
            CopyState::Unstarted(run) => {
                if answer.is_some() {
                    t.state = CopyState::Unstarted(run);
                    return Err(refuse_answer_at_start());
                }
                let started = Instant::now();
                let mut child = Some(self.launch(&run, t.id, stop)?);
                let connection = match stop {
                    Some(_) => self.connection_of(t.id, &mut child, run.timeout)?,
                    None => None,
                };
                Ok(Running {
                    id: t.id,
                    connection,
                    child,
                    started,
                    run,
                })
            }
            CopyState::Forked(run, connection) => {
                let mut connection = match connection {
                    Some(connection) => connection,
                    None => self
                        .connection_of(t.id, &mut None, REPLY_BOUND)?
                        .ok_or_else(|| {
                            Refusal::new("timeline-copy-lost", format!("timeline {}", t.id))
                        })?,
                };
                connection.expect("paused", REPLY_BOUND)?;
                let command = match answer {
                    Some(value) => serde_json::json!({ "answer": value, "until": stop }),
                    None => serde_json::json!({ "source": true, "until": stop }),
                };
                let started = Instant::now();
                connection.send(&command)?;
                Ok(Running {
                    id: t.id,
                    connection: Some(connection),
                    child: None,
                    started,
                    run,
                })
            }
            CopyState::Done => Err(Refusal::new(
                "timeline-already-ran",
                format!("timeline {}", t.id),
            )),
        }
    }

    /// Reads a running copy's messages until it pauses or ends.
    fn follow(&mut self, running: Running) -> Result<Stop<KvmPaused>, Refusal> {
        let Running {
            id,
            connection,
            mut child,
            started,
            run,
        } = running;
        let deadline = started + run.timeout;
        if let Some(mut connection) = connection {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Refusal::new(
                        "timeline-copy-timed-out",
                        format!("timeline {id} ran past {:?}", run.timeout),
                    ));
                }
                let Some(message) = connection.receive(remaining)? else {
                    break;
                };
                if let Some(request) = message.get("paused") {
                    let request = PendingRequest::from_json(request)?;
                    return Ok(Stop::Paused(KvmPaused {
                        id,
                        connection,
                        child,
                        request,
                        run,
                    }));
                }
                if let Some(code) = message.get("exit").and_then(|code| code.as_i64()) {
                    if let Some(child) = child.as_mut() {
                        let _ = child.wait();
                    }
                    return Ok(Stop::Exited(RunOutcome {
                        code: code as i32,
                        observation: self.files(id, None),
                        wall: started.elapsed(),
                    }));
                }
            }
        }
        // No more messages: a process this driver started reports its status.
        if let Some(child) = child.as_mut() {
            let status = child
                .wait()
                .map_err(|e| Refusal::new("timeline-wait", e.to_string()))?;
            use std::os::unix::process::ExitStatusExt;
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            return Ok(Stop::Exited(RunOutcome {
                code,
                observation: self.files(id, None),
                wall: started.elapsed(),
            }));
        }
        Err(Refusal::new(
            "timeline-copy-lost",
            format!("timeline {id} closed its connection without an exit status"),
        ))
    }
}

/// Asks `template` to fork one copy per id, made under `copy` and placed by
/// `placement`.
fn fork_from(
    template: &Mutex<Template>,
    ids: &[u64],
    copy: &CopyPolicy,
    placement: &Placement,
) -> Result<(), Refusal> {
    let mut template = template.lock().unwrap_or_else(|e| e.into_inner());
    template.connection.send(&serde_json::json!({
        "fork": {
            "ids": ids,
            "memory": copy.memory.name(),
            "placement": placement.describe(),
        }
    }))?;
    let forked = template.connection.expect("forked", REPLY_BOUND)?;
    if let Some(error) = forked
        .as_array()
        .and_then(|copies| copies.iter().find_map(|copy| copy.get("error")))
    {
        return Err(Refusal::new("timeline-fork-failed", error.to_string()));
    }
    Ok(())
}

impl Checkpoint for KvmCheckpoint {
    fn cut(&self) -> &Cut {
        &self.cut
    }
    fn index(&self) -> u64 {
        self.index
    }
    fn observation(&self) -> &Observation {
        &self.observation
    }
}

impl Timelines for KvmTimelines {
    type Checkpoint = KvmCheckpoint;
    type Timeline = KvmTimeline;
    type PausedRun = KvmPaused;

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            mid_run_capture: true,
            parallel_copies: true,
        }
    }

    fn start(&mut self, run: &RunSpec) -> Result<KvmCheckpoint, Refusal> {
        Ok(KvmCheckpoint {
            cut: Cut::Start,
            index: 0,
            observation: Observation::default(),
            run: run.clone(),
            template: None,
        })
    }

    fn capture(&mut self, mut run: KvmPaused) -> Result<KvmCheckpoint, (Refusal, KvmPaused)> {
        if let Err(refusal) = run.connection.send(&serde_json::json!({ "capture": true })) {
            return Err((refusal, run));
        }
        let captured = match run.connection.expect("captured", REPLY_BOUND) {
            Ok(captured) => captured,
            Err(refusal) => {
                // A refused run pauses again at the same request.
                if refusal.name == "timeline-capture-refused" {
                    let _ = run.connection.expect("paused", REPLY_BOUND);
                }
                return Err((refusal, run));
            }
        };
        let offsets = captured
            .get("offsets")
            .and_then(|offsets| offsets.as_object())
            .cloned()
            .unwrap_or_default();
        Ok(KvmCheckpoint {
            cut: Cut::Pending(run.request.clone()),
            index: run.request.index,
            observation: self.files(run.id, Some(&offsets)),
            run: run.run,
            template: Some(Arc::new(Mutex::new(Template {
                connection: run.connection,
                child: run.child,
            }))),
        })
    }

    fn fork(&mut self, cp: &KvmCheckpoint, n: usize) -> Result<Vec<KvmTimeline>, Refusal> {
        self.fork_batch(cp, n, &Batch::default())
    }

    /// Copies made at fork take eager or lazy memory with eager descriptors
    /// and kernel objects; [`CopyPolicy::BACKUP`] makes nothing until a copy
    /// is resumed. Placement: a pool, self-placed or unpinned; hints wait for
    /// a scheduler that takes them.
    fn honors(&self, copy: &CopyPolicy, placement: &Placement) -> Result<(), Refusal> {
        let made_at_fork = matches!(copy.memory, CopyMode::Eager | CopyMode::Lazy)
            && copy.descriptors == CopyMode::Eager
            && copy.kernel_objects == CopyMode::Eager;
        if !made_at_fork && *copy != CopyPolicy::BACKUP {
            return Err(Refusal::new(
                "timeline-copy-policy-unsupported",
                format!(
                    "{}: KVM copies are made at fork (memory eager or lazy, descriptors and \
                     kernel objects eager) or wholly dormant",
                    copy.describe()
                ),
            ));
        }
        match placement {
            Placement::Pool(cpus) if cpus.is_empty() => Err(Refusal::new(
                "timeline-placement-unsupported",
                "a pool needs a CPU",
            )),
            Placement::Hinted(_) => Err(Refusal::new(
                "timeline-placement-unsupported",
                format!(
                    "{}: no scheduler here takes placement hints yet",
                    placement.describe()
                ),
            )),
            _ => Ok(()),
        }
    }

    fn fork_batch(
        &mut self,
        cp: &KvmCheckpoint,
        n: usize,
        batch: &Batch,
    ) -> Result<Vec<KvmTimeline>, Refusal> {
        let copy = batch.copy.unwrap_or(cp.run.copy);
        let placement = batch
            .placement
            .clone()
            .unwrap_or_else(|| cp.run.placement.clone());
        self.honors(&copy, &placement)?;
        let ids: Vec<u64> = (0..n)
            .map(|_| {
                self.next_id += 1;
                self.next_id
            })
            .collect();
        let copies = |state: &dyn Fn() -> CopyState| {
            ids.iter()
                .map(|&id| KvmTimeline { id, state: state() })
                .collect::<Vec<_>>()
        };
        let Some(template) = &cp.template else {
            // A START copy is a fresh run, launched when it is resumed: it has
            // nothing to copy, and it places itself as a fresh run does
            // (policies apply to copies forked from a captured run).
            return Ok(copies(&|| CopyState::Unstarted(cp.run.clone())));
        };
        if copy == CopyPolicy::BACKUP {
            return Ok(copies(&|| {
                CopyState::Dormant(cp.run.clone(), Arc::clone(template), placement.clone())
            }));
        }
        fork_from(template, &ids, &copy, &placement)?;
        Ok(copies(&|| CopyState::Forked(cp.run.clone(), None)))
    }

    fn resume(
        &mut self,
        t: &mut KvmTimeline,
        answer: Option<Value>,
        until: StopAt,
    ) -> Result<Stop<KvmPaused>, Refusal> {
        let running = self.begin(t, answer, until)?;
        self.follow(running)
    }

    fn resume_many(
        &mut self,
        work: Vec<(&mut KvmTimeline, Option<Value>, StopAt)>,
    ) -> Vec<Result<Stop<KvmPaused>, Refusal>> {
        // Every copy is sent on its way before any is waited for, so they run
        // at the same time.
        let started: Vec<Result<Running, Refusal>> = work
            .into_iter()
            .map(|(t, answer, until)| self.begin(t, answer, until))
            .collect();
        started
            .into_iter()
            .map(|running| running.and_then(|running| self.follow(running)))
            .collect()
    }

    fn discard(&mut self, mut t: KvmTimeline) {
        let state = std::mem::replace(&mut t.state, CopyState::Done);
        if let CopyState::Forked(_, connection) = state {
            let connection = match connection {
                Some(connection) => Some(connection),
                None => self
                    .connection_of(t.id, &mut None, Duration::from_secs(30))
                    .ok()
                    .flatten(),
            };
            if let Some(mut connection) = connection {
                if let Some(pid) = connection.pid {
                    // SAFETY: signals the copy, by its PID in this namespace.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                }
                // Its end of the connection closes when it is gone (P6).
                while let Ok(Some(_)) = connection.receive(Duration::from_secs(10)) {}
            }
        }
    }

    /// Ends the template and every copy of it still alive: on this backend
    /// copies are processes in the template's PID namespace and cannot
    /// outlive it. Finish or discard the copies you need first.
    fn release(&mut self, cp: KvmCheckpoint) {
        if let Some(template) = cp.template {
            let mut template = template.lock().unwrap_or_else(|e| e.into_inner());
            let released = template
                .connection
                .send(&serde_json::json!({ "release": true }))
                .is_ok()
                && template.connection.expect("released", REPLY_BOUND).is_ok();
            if !released {
                // It did not confirm: end it, by its process if this driver
                // started it, else by the PID its connection reports.
                match template.child.as_mut() {
                    Some(child) => {
                        let _ = child.kill();
                    }
                    None => {
                        if let Some(pid) = template.connection.pid {
                            // SAFETY: signals the template, by its PID here.
                            unsafe { libc::kill(pid, libc::SIGKILL) };
                        }
                    }
                }
            }
            if let Some(child) = template.child.as_mut() {
                let _ = child.wait();
            }
        }
    }
}
