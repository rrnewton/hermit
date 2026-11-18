/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): [`Timelines`] over claude-fuzzpoc's
//! process fork server (`HERMIT_FORK_SERVER`, `hermit run`'s fork_server.rs,
//! whose protocol claude-fuzzpoc owns).
//!
//! The server does a run's setup once and forks one child per `RUN`
//! request, so it offers START checkpoints only (amendment (d)): `start`
//! launches it and waits until it listens, `fork` is lazy, and
//! `resume(t, None, End)` sends one `RUN`. Its children run one at a time
//! (`parallel_copies: false`). Each `RUN` names the copy's own files as
//! `key=value` words (claude-fuzzpoc 0f0e402c8713): standard output and
//! error, its log (`log=`, so the server runs with a `--log-file`), its
//! choice record (`record=`, when the command line names
//! `--choice-record`) and, when [`ServedCopy::set_tape`] gave it one, its
//! own choice tape (`tape=`, when the command line names `--choice-tape`):
//! the copy's own source.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use super::Capabilities;
use super::Checkpoint;
use super::Cut;
use super::Observation;
use super::Part;
use super::Refusal;
use super::RunOutcome;
use super::RunSpec;
use super::Stop;
use super::StopAt;
use super::Timelines;
use super::Value;
use super::refuse_answer_at_start;

/// The variable the server reads (fork_server.rs `ENV`).
const SERVER_ENV: &str = "HERMIT_FORK_SERVER";

#[derive(Default)]
pub struct ForkServerTimelines {
    next_copy: u64,
}

/// The server behind a START checkpoint.
pub struct ServerCheckpoint {
    cut: Cut,
    observation: Observation,
    server: Child,
    socket: PathBuf,
    dir: PathBuf,
    timeout: Duration,
    /// Whether the command line names `--choice-record` and
    /// `--choice-tape`, which `record=` and `tape=` replace per RUN.
    records: bool,
    tapes: bool,
}

/// One copy; no process exists until it is resumed (d5).
pub struct ServedCopy {
    id: u64,
    socket: PathBuf,
    dir: PathBuf,
    timeout: Duration,
    records: bool,
    tapes: bool,
    tape: Option<PathBuf>,
    done: bool,
}

impl ServedCopy {
    /// Gives the copy its own choice tape: its own source (`tape=`).
    pub fn set_tape(&mut self, tape: PathBuf) {
        self.tape = Some(tape);
    }

    pub fn copy(&self) -> u64 {
        self.id
    }
}

/// The server never yields a paused run.
pub enum NeverPaused {}

impl Checkpoint for ServerCheckpoint {
    fn cut(&self) -> &Cut {
        &self.cut
    }
    fn index(&self) -> u64 {
        0
    }
    fn observation(&self) -> &Observation {
        &self.observation
    }
}

impl ForkServerTimelines {
    pub fn new() -> Self {
        Self::default()
    }
}

fn names(run_args: &[String], flag: &str) -> bool {
    run_args.iter().any(|arg| arg.starts_with(flag))
}

/// A path as one `key=value` word of the RUN line.
fn word(key: &str, path: &Path) -> Result<String, Refusal> {
    let text = path.display().to_string();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return Err(Refusal::new(
            "process-fork-server-path",
            format!("{key}= cannot carry {text:?}"),
        ));
    }
    Ok(format!("{key}={text}"))
}

fn wait_listening(socket: &Path, server: &mut Child, bound: Duration) -> Result<(), Refusal> {
    let started = Instant::now();
    loop {
        if UnixStream::connect(socket).is_ok() {
            return Ok(());
        }
        if let Ok(Some(status)) = server.try_wait() {
            return Err(Refusal::new(
                "process-fork-server-did-not-start",
                format!("the server exited before listening: {status}"),
            ));
        }
        if started.elapsed() > bound {
            let _ = server.kill();
            let _ = server.wait();
            return Err(Refusal::new(
                "process-fork-server-did-not-start",
                format!("not listening after {bound:?}"),
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn file(path: &Path) -> Result<std::fs::File, Refusal> {
    std::fs::File::create(path)
        .map_err(|e| Refusal::new("timeline-dir", format!("{}: {e}", path.display())))
}

impl Timelines for ForkServerTimelines {
    type Checkpoint = ServerCheckpoint;
    type Timeline = ServedCopy;
    type PausedRun = NeverPaused;

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            mid_run_capture: false,
            parallel_copies: false,
        }
    }

    fn start(&mut self, run: &RunSpec) -> Result<ServerCheckpoint, Refusal> {
        std::fs::create_dir_all(&run.dir)
            .map_err(|e| Refusal::new("timeline-dir", format!("{}: {e}", run.dir.display())))?;
        let socket = run.dir.join("fs.sock");
        let _ = std::fs::remove_file(&socket);
        let (program, launcher_args) = run
            .launcher
            .split_first()
            .ok_or_else(|| Refusal::new("timeline-run-spec", "empty launcher"))?;
        // A --log-file lets each RUN name its own log (log=); the server's
        // own stays empty.
        let mut server = Command::new(program)
            .args(launcher_args)
            .args(&run.global_args)
            .arg(format!(
                "--log-file={}",
                run.dir.join("serve.log").display()
            ))
            .arg("run")
            .args(&run.run_args)
            .arg("--")
            .args(&run.guest)
            .env(SERVER_ENV, &socket)
            .stdin(Stdio::null())
            .stdout(file(&run.dir.join("server.stdout"))?)
            .stderr(file(&run.dir.join("server.stderr"))?)
            .spawn()
            .map_err(|e| Refusal::new("process-fork-server-did-not-start", e.to_string()))?;
        wait_listening(&socket, &mut server, Duration::from_secs(120))?;
        Ok(ServerCheckpoint {
            cut: Cut::Start,
            observation: Observation::default(),
            server,
            socket,
            dir: run.dir.clone(),
            timeout: run.timeout,
            records: names(&run.run_args, "--choice-record="),
            tapes: names(&run.run_args, "--choice-tape="),
        })
    }

    fn capture(&mut self, run: NeverPaused) -> Result<ServerCheckpoint, (Refusal, NeverPaused)> {
        match run {}
    }

    fn fork(&mut self, cp: &ServerCheckpoint, n: usize) -> Result<Vec<ServedCopy>, Refusal> {
        Ok((0..n)
            .map(|_| {
                self.next_copy += 1;
                ServedCopy {
                    id: self.next_copy,
                    socket: cp.socket.clone(),
                    dir: cp.dir.clone(),
                    timeout: cp.timeout,
                    records: cp.records,
                    tapes: cp.tapes,
                    tape: None,
                    done: false,
                }
            })
            .collect())
    }

    fn resume(
        &mut self,
        t: &mut ServedCopy,
        answer: Option<Value>,
        until: StopAt,
    ) -> Result<Stop<NeverPaused>, Refusal> {
        if answer.is_some() {
            return Err(refuse_answer_at_start());
        }
        if let StopAt::Index(_) = until {
            return Err(Refusal::new(
                "process-fork-server-captures-only-at-start",
                "a served run cannot pause at a later request",
            ));
        }
        if t.done {
            return Err(Refusal::new(
                "timeline-already-ran",
                format!("copy {}", t.id),
            ));
        }
        if t.tape.is_some() && !t.tapes {
            return Err(Refusal::new(
                "process-fork-server-path",
                "tape= needs --choice-tape on the server's command line",
            ));
        }
        t.done = true;
        let own = |output: &str| t.dir.join(format!("copy-{}.{output}", t.id));
        let (stdout, stderr, log, record) =
            (own("stdout"), own("stderr"), own("log"), own("choices"));
        let mut line = vec![
            "RUN".to_string(),
            word("stdin", Path::new("/dev/null"))?,
            word("stdout", &stdout)?,
            word("stderr", &stderr)?,
            format!("timeout_ms={}", t.timeout.as_millis()),
            word("log", &log)?,
        ];
        if t.records {
            line.push(word("record", &record)?);
        }
        if let Some(tape) = &t.tape {
            line.push(word("tape", tape)?);
        }
        let started = Instant::now();
        let mut stream = UnixStream::connect(&t.socket)
            .map_err(|e| Refusal::new("process-fork-server-gone", e.to_string()))?;
        writeln!(stream, "{}", line.join(" "))
            .map_err(|e| Refusal::new("process-fork-server-gone", e.to_string()))?;
        let mut reply = String::new();
        BufReader::new(&stream)
            .read_line(&mut reply)
            .map_err(|e| Refusal::new("process-fork-server-gone", e.to_string()))?;
        let wall = started.elapsed();
        let words: Vec<&str> = reply.split_whitespace().collect();
        let code = match words.as_slice() {
            ["STATUS", code, _elapsed] => code
                .parse::<i32>()
                .map_err(|e| Refusal::new("process-fork-server-protocol", e.to_string()))?,
            ["TIMEOUT", _elapsed] => {
                return Err(Refusal::new(
                    "timeline-copy-timed-out",
                    format!("copy {} ran past {:?}", t.id, t.timeout),
                ));
            }
            _ => {
                return Err(Refusal::new(
                    "process-fork-server-protocol",
                    format!("unexpected reply {reply:?}"),
                ));
            }
        };
        Ok(Stop::Exited(RunOutcome {
            code,
            observation: Observation {
                stdout: Some(Part::whole(stdout)),
                stderr: Some(Part::whole(stderr)),
                log: Some(Part::whole(log)),
                record: t.records.then(|| Part::whole(record)),
            },
            wall,
        }))
    }

    fn discard(&mut self, t: ServedCopy) {
        // A copy has a process only inside resume, which returns after the
        // server reaped it; an unresumed copy has none.
        drop(t);
    }

    fn release(&mut self, mut cp: ServerCheckpoint) {
        if let Ok(mut stream) = UnixStream::connect(&cp.socket) {
            let _ = writeln!(stream, "QUIT");
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match cp.server.try_wait() {
                Ok(Some(_)) => break,
                _ if Instant::now() > deadline => {
                    let _ = cp.server.kill();
                    let _ = cp.server.wait();
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}
