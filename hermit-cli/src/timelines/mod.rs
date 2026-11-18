/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): the backend-neutral `Timelines` layer.
//!
//! A *timeline* is one independent continuation of a run; a *checkpoint* is
//! a frozen run that timelines start from, any number of times. A search
//! driver uses only this trait, so an implementation can be swapped without
//! touching it. The guarantees, from the multiverse timelines design
//! (sections 6.1 to 6.5, with amendment (d) agreed with claude-fuzzpoc):
//!
//! - **P1 equivalence**: a copy of a checkpoint at request i, resumed with
//!   answers `a_i ..`, cannot be told from a fresh run with
//!   `a_0 .. a_(i-1), a_i ..` by its choice record, standard output, exit
//!   status or canonical information log: the checkpoint's observation
//!   followed by the copy's equals the fresh run's. At a START checkpoint
//!   that says a copy resumed with `None` is a fresh run.
//! - **P2 isolation**: copies share no mutable state with each other or
//!   with the checkpoint, their outputs and logs included.
//! - **P3 concurrency**: with [`Capabilities::parallel_copies`], copies run
//!   at the same time ([`Timelines::resume_many`]) without changing results.
//! - **P4**: a checkpoint never runs.
//! - **P5 cost accounting**: outcomes report their wall time.
//! - **P6**: after [`Timelines::discard`] the copy writes nothing more.
//! - **P7**: a call outside [`Timelines::capabilities`] is refused by name,
//!   never served by a degraded copy.
//!
//! Implementations: [`fork_server::ForkServerTimelines`] (claude-fuzzpoc's
//! process fork server, START only) and [`kvm::KvmTimelines`] (host-fork
//! copies of a KVM run, at START and at any choice request).

pub mod fork_server;
pub mod kvm;

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// An answer to a choice request (detcore::choice answers are `u64`).
pub type Value = u64;

/// A run to start: the hermit command line, without the per-copy output
/// paths, which each implementation adds.
#[derive(Clone, Debug)]
pub struct RunSpec {
    /// The command that starts hermit, wrapper first (for example
    /// `bin/safehermit --sh-deadline 600 target/release/hermit`).
    pub launcher: Vec<String>,
    /// Arguments before `run` (for example `--log=info --backend=kvm`).
    pub global_args: Vec<String>,
    /// Arguments of `run` before `--` (for example `--strict --chaos`).
    pub run_args: Vec<String>,
    /// The guest command.
    pub guest: Vec<String>,
    /// Where this run's checkpoints and copies keep their files.
    pub dir: PathBuf,
    /// Wall-clock bound for one copy.
    pub timeout: Duration,
}

/// What an implementation can do (amendment (d), d4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub mid_run_capture: bool,
    pub parallel_copies: bool,
}

/// A choice request as the driver sees it: detcore::choice's request,
/// decoded from its JSON form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRequest {
    pub index: u64,
    pub kind: String,
    pub lo: u64,
    pub hi: u64,
    pub default: u64,
    pub scale: u64,
    /// The request as the run sent it.
    pub json: String,
}

impl PendingRequest {
    /// Decodes detcore::choice::request_json's form. Unknown fields are
    /// ignored; a `schema` other than 1 is refused.
    pub fn from_json(value: &serde_json::Value) -> Result<Self, Refusal> {
        if let Some(schema) = value.get("schema")
            && schema.as_u64() != Some(1)
        {
            return Err(Refusal::new(
                "timeline-protocol",
                format!("request schema {schema} is not 1"),
            ));
        }
        let number = |key: &str| {
            value.get(key).and_then(|v| v.as_u64()).ok_or_else(|| {
                Refusal::new(
                    "timeline-protocol",
                    format!("request without {key}: {value}"),
                )
            })
        };
        Ok(Self {
            index: number("i")?,
            kind: value
                .get("kind")
                .and_then(|kind| kind.as_str())
                .unwrap_or_default()
                .to_string(),
            lo: number("lo")?,
            hi: number("hi")?,
            default: number("default")?,
            scale: number("scale")?,
            json: value.to_string(),
        })
    }
}

/// Where a checkpoint is cut (d1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cut {
    /// Before the guest exists and before request 0.
    Start,
    /// Paused at this unanswered request.
    Pending(PendingRequest),
}

/// What one run wrote to one output: a file, or its first `len` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub path: PathBuf,
    pub len: Option<u64>,
}

impl Part {
    pub fn whole(path: PathBuf) -> Self {
        Self { path, len: None }
    }

    pub fn read(&self) -> std::io::Result<Vec<u8>> {
        let mut bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        if let Some(len) = self.len {
            bytes.truncate(len as usize);
        }
        Ok(bytes)
    }
}

/// What a run showed: its outputs, its log and its choice record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    pub stdout: Option<Part>,
    pub stderr: Option<Part>,
    pub log: Option<Part>,
    pub record: Option<Part>,
}

impl Observation {
    /// This observation followed by `then`, output by output, written to
    /// `dir/<name>.<output>`: a copy's whole observation is its checkpoint's
    /// followed by its own.
    pub fn join(&self, then: &Observation, dir: &Path, name: &str) -> std::io::Result<Observation> {
        let join = |first: &Option<Part>, second: &Option<Part>, output: &str| {
            if first.is_none() && second.is_none() {
                return Ok(None);
            }
            let mut bytes = match first {
                Some(part) => part.read()?,
                None => Vec::new(),
            };
            if let Some(part) = second {
                bytes.extend(part.read()?);
            }
            let path = dir.join(format!("{name}.{output}"));
            std::fs::write(&path, bytes)?;
            Ok::<_, std::io::Error>(Some(Part::whole(path)))
        };
        Ok(Observation {
            stdout: join(&self.stdout, &then.stdout, "stdout")?,
            stderr: join(&self.stderr, &then.stderr, "stderr")?,
            log: join(&self.log, &then.log, "log")?,
            record: join(&self.record, &then.record, "choices")?,
        })
    }
}

/// How a copy ended.
#[derive(Clone, Debug)]
pub struct RunOutcome {
    /// Exit status, or 128 + signal.
    pub code: i32,
    /// The copy's own part of the observation, after its checkpoint's.
    pub observation: Observation,
    /// From the resume call to the copy's exit.
    pub wall: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopAt {
    End,
    Index(u64),
}

pub enum Stop<P> {
    Paused(P),
    Exited(RunOutcome),
}

/// A named refusal (R1 to R4 of the design, and protocol failures); the
/// checkpoint and the run are left as they were.
#[derive(Clone, Debug)]
pub struct Refusal {
    pub name: &'static str,
    pub reason: String,
    pub next_capturable: Option<u64>,
}

impl Refusal {
    pub fn new(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            reason: reason.into(),
            next_capturable: None,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.name, self.reason)
    }
}

impl std::error::Error for Refusal {}

/// `timeline-start-has-no-pending-request` (R4).
pub fn refuse_answer_at_start() -> Refusal {
    Refusal::new(
        "timeline-start-has-no-pending-request",
        "a START checkpoint has no pending request to answer; resume with None",
    )
}

pub trait Checkpoint {
    fn cut(&self) -> &Cut;
    /// The prefix length: 0 at START.
    fn index(&self) -> u64;
    /// What the run showed up to the cut; empty at START.
    fn observation(&self) -> &Observation;
}

/// Layer 3 of the design. One implementation per snapshot mechanism.
pub trait Timelines {
    type Checkpoint: Checkpoint;
    type Timeline;
    type PausedRun;

    fn capabilities(&self) -> Capabilities;

    /// A checkpoint at the START of a run (d3).
    fn start(&mut self, run: &RunSpec) -> Result<Self::Checkpoint, Refusal>;

    /// Freezes a run paused at an unanswered request. A refusal hands the
    /// run back, still paused: a refusal never loses the run.
    fn capture(
        &mut self,
        run: Self::PausedRun,
    ) -> Result<Self::Checkpoint, (Refusal, Self::PausedRun)>;

    /// `n` independent copies of `cp`; lazy at START (d5).
    fn fork(&mut self, cp: &Self::Checkpoint, n: usize) -> Result<Vec<Self::Timeline>, Refusal>;

    /// Runs the copy until `until`; `None` lets its own source answer its
    /// pending request too (d2).
    fn resume(
        &mut self,
        t: &mut Self::Timeline,
        answer: Option<Value>,
        until: StopAt,
    ) -> Result<Stop<Self::PausedRun>, Refusal>;

    /// Resumes several copies; with `parallel_copies`, they run at the same
    /// time. The default resumes them one after another.
    fn resume_many(
        &mut self,
        work: Vec<(&mut Self::Timeline, Option<Value>, StopAt)>,
    ) -> Vec<Result<Stop<Self::PausedRun>, Refusal>> {
        work.into_iter()
            .map(|(t, answer, until)| self.resume(t, answer, until))
            .collect()
    }

    /// Kills the copy's process and reaps it (d5, P6).
    fn discard(&mut self, t: Self::Timeline);

    fn release(&mut self, cp: Self::Checkpoint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_request_with_or_without_a_schema() {
        let plain: serde_json::Value = serde_json::from_str(
            r#"{"i":3,"kind":"sched.slice","lo":1,"hi":9,"default":5,"scale":4,"ctx":{"dtid":3}}"#,
        )
        .unwrap();
        let request = PendingRequest::from_json(&plain).unwrap();
        assert_eq!(
            (request.index, request.kind.as_str(), request.lo, request.hi),
            (3, "sched.slice", 1, 9)
        );
        assert_eq!((request.default, request.scale), (5, 4));
        let versioned: serde_json::Value = serde_json::from_str(
            r#"{"schema":1,"i":3,"kind":"sched.slice","lo":1,"hi":9,"default":5,"scale":4,"new":true}"#,
        )
        .unwrap();
        assert_eq!(PendingRequest::from_json(&versioned).unwrap().index, 3);
        let future: serde_json::Value = serde_json::from_str(
            r#"{"schema":2,"i":3,"kind":"sched.slice","lo":1,"hi":9,"default":5,"scale":4}"#,
        )
        .unwrap();
        assert_eq!(
            PendingRequest::from_json(&future).unwrap_err().name,
            "timeline-protocol"
        );
        let missing: serde_json::Value = serde_json::from_str(r#"{"i":3}"#).unwrap();
        assert!(PendingRequest::from_json(&missing).is_err());
    }

    #[test]
    fn joins_a_cut_prefix_with_a_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).unwrap();
            path
        };
        let prefix = Observation {
            stdout: Some(Part {
                path: file("template.stdout", "abcdef"),
                len: Some(3),
            }),
            ..Default::default()
        };
        let suffix = Observation {
            stdout: Some(Part::whole(file("copy.stdout", "XYZ"))),
            log: Some(Part::whole(file("copy.log", "L"))),
            ..Default::default()
        };
        let joined = prefix.join(&suffix, dir.path(), "joined").unwrap();
        assert_eq!(joined.stdout.unwrap().read().unwrap(), b"abcXYZ");
        assert_eq!(joined.log.unwrap().read().unwrap(), b"L");
        assert!(joined.record.is_none());
        let absent = Part::whole(dir.path().join("never-written"));
        assert!(absent.read().unwrap().is_empty());
    }
}
