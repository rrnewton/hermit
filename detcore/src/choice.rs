/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (fuzzpoc lane): choice points answered by an external controller.
//!
//! Under `--chaos`, Detcore resolves scheduling nondeterminism itself, from the
//! per-thread `chaos_prng`. Each such resolution is a *choice point*. With one
//! of `--choice-tape` or `--choice-controller`, Detcore still draws the value
//! it would have used (the *default*, so the seeded stream is consumed exactly
//! as before) and then asks a [`ChoiceSource`] for the value to use instead.
//!
//! Contract of a [`ChoiceSource`]:
//! - Promises: an answer lies in the request's `[lo, hi]` and depends only on
//!   the requests this run has made so far and the source's own configuration.
//! - May assume: requests arrive one at a time, in run order, with consecutive
//!   indices from 0. Hermit serializes the guest, so the order is a function
//!   of the previous answers and the guest's inputs.
//! - May refuse: a request it cannot answer (a replay whose record names a
//!   different kind or range at this index). The refusal names the index; the
//!   run stops there, which localizes a first divergence to one choice.
//!
//! A source that always answers the default reproduces the seeded run, so an
//! empty tape is the ordinary `--chaos --sched-seed` run.
//!
//! Scope of the prototype: the ptrace and KVM backends, where Detcore runs in
//! the Hermit process and one process runs one guest, so the controller is one
//! process-wide value (not the in-guest backends). Two kinds are
//! wired: timeslice lengths and priority redraws. `--choice-record` writes
//! every request and answer as JSON lines.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

use crate::types::DetTid;

/// What a choice point decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    /// The length, in RCBs, of the timeslice a thread is about to run.
    SchedSlice,
    /// The priority a thread takes at a chaos priority redraw (its own, or a
    /// child's at creation). Lower runs first.
    SchedPriority,
}

impl ChoiceKind {
    fn name(self) -> &'static str {
        match self {
            ChoiceKind::SchedSlice => "sched.slice",
            ChoiceKind::SchedPriority => "sched.priority",
        }
    }
}

/// One choice point, as the controller sees it.
#[derive(Debug, Clone)]
pub struct ChoiceRequest {
    /// Position in this run's choice sequence, from 0.
    pub index: u64,
    /// What is being decided.
    pub kind: ChoiceKind,
    /// Smallest admissible answer.
    pub lo: u64,
    /// Largest admissible answer.
    pub hi: u64,
    /// The value Detcore would use without a controller.
    pub default: u64,
    /// A natural magnitude for the decision (the mean slice length for
    /// [`ChoiceKind::SchedSlice`]), so byte encodings can be scale relative.
    pub scale: u64,
    /// The thread the decision is for.
    pub dtid: DetTid,
    /// That thread's committed retired-branch count when it asked: its own
    /// position in the run. (The global scheduler turn is not visible from the
    /// thread side; a later version adds it.)
    pub thread_rcbs: u64,
    /// Which draw site asked.
    pub site: &'static str,
}

/// A source's refusal to answer, naming the request it refused.
#[derive(Debug, Clone)]
pub struct ChoiceRefusal {
    /// The index of the refused request.
    pub index: u64,
    /// Why the source refused, for the run's error message.
    pub reason: String,
}

/// Answers choice requests. See the module documentation for the contract.
pub trait ChoiceSource: Send {
    /// The value to use for `req`, within `[req.lo, req.hi]`, or a refusal.
    fn answer(&mut self, req: &ChoiceRequest) -> Result<u64, ChoiceRefusal>;

    /// `req` was answered without asking this source (a pause hook's
    /// [`PauseAction::Answer`]). A source that reads its answers positionally
    /// moves past `req`, so that answering request `i` from the hook equals
    /// a tape whose byte `i` was replaced.
    fn skip(&mut self, _req: &ChoiceRequest) {}
}

/// Decodes answers from a byte tape, one byte per choice, front to back, in the
/// manner of libFuzzer's FuzzedDataProvider. Byte 0, or an exhausted tape,
/// takes the default, so the empty tape is the seeded run, and a mutation of
/// byte `j` changes about choice `j` only.
pub struct TapeSource {
    bytes: Vec<u8>,
    pos: usize,
}

impl TapeSource {
    /// A source that reads `bytes` front to back.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The value byte `b` (nonzero) selects for `req`.
    fn decode(req: &ChoiceRequest, b: u8) -> u64 {
        let v = match req.kind {
            // Scale relative and logarithmic: b = 128 is the mean slice, each
            // 16 steps doubles or halves it (mean/256 .. ~mean*256).
            ChoiceKind::SchedSlice => {
                let scale = req.scale.max(1) as f64;
                (scale * 2f64.powf((f64::from(b) - 128.0) / 16.0)).round() as u64
            }
            // 255 evenly spaced levels over the priority range.
            ChoiceKind::SchedPriority => {
                let span = req.hi - req.lo;
                req.lo + span * u64::from(b - 1) / 254
            }
        };
        v.clamp(req.lo, req.hi)
    }
}

impl ChoiceSource for TapeSource {
    fn skip(&mut self, _req: &ChoiceRequest) {
        self.pos += 1;
    }

    fn answer(&mut self, req: &ChoiceRequest) -> Result<u64, ChoiceRefusal> {
        let b = self.bytes.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        Ok(if b == 0 {
            req.default
        } else {
            Self::decode(req, b)
        })
    }
}

/// Asks a controller process over a Unix stream socket, one JSON line per
/// request and one per answer: `{"i":N,"v":V}` or `{"i":N,"default":true}`.
pub struct SocketSource {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl SocketSource {
    /// Connect to the controller listening at `path`.
    pub fn connect(path: &Path) -> std::io::Result<Self> {
        let writer = UnixStream::connect(path)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self { writer, reader })
    }
}

impl ChoiceSource for SocketSource {
    fn answer(&mut self, req: &ChoiceRequest) -> Result<u64, ChoiceRefusal> {
        let refuse = |reason: String| ChoiceRefusal {
            index: req.index,
            reason,
        };
        let mut line = request_json(req);
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .map_err(|e| refuse(format!("controller write failed: {e}")))?;
        let mut reply = String::new();
        let n = self
            .reader
            .read_line(&mut reply)
            .map_err(|e| refuse(format!("controller read failed: {e}")))?;
        if n == 0 {
            return Err(refuse("controller closed the socket".into()));
        }
        let parsed: serde_json::Value = serde_json::from_str(reply.trim())
            .map_err(|e| refuse(format!("controller reply is not JSON ({e}): {reply:?}")))?;
        if parsed.get("i").and_then(|i| i.as_u64()) != Some(req.index) {
            return Err(refuse(format!(
                "controller answered another index: {reply:?}"
            )));
        }
        if let Some(reason) = parsed.get("refuse").and_then(|r| r.as_str()) {
            return Err(refuse(format!("controller refused: {reason}")));
        }
        if parsed.get("default").and_then(|d| d.as_bool()) == Some(true) {
            return Ok(req.default);
        }
        let v = parsed
            .get("v")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| refuse(format!("controller reply has no \"v\": {reply:?}")))?;
        if v < req.lo || v > req.hi {
            return Err(refuse(format!(
                "controller answer {v} is outside [{}, {}]",
                req.lo, req.hi
            )));
        }
        Ok(v)
    }
}

/// What a pause hook asks [`choose`] to do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Ask the run's choice source, as without a hook.
    Continue,
    /// Use this value as the answer without asking the source (it is still
    /// recorded). A Timeline capture hook forks inside the hook and gives each
    /// copy its own answer to this request.
    Answer(u64),
}

/// A callback at every choice point, after the request is built and before it
/// is answered. Installed by a Timeline implementation; none by default.
pub type PauseHook = Box<dyn Fn(&ChoiceRequest) -> PauseAction + Send>;

static PAUSE_HOOK: Mutex<Option<PauseHook>> = Mutex::new(None);

/// Install (or with `None`, remove) the pause hook.
pub fn set_pause_hook(hook: Option<PauseHook>) {
    *PAUSE_HOOK.lock().unwrap() = hook;
}

/// The request as one JSON object (no trailing newline).
pub fn request_json(req: &ChoiceRequest) -> String {
    format!(
        "{{\"i\":{},\"kind\":\"{}\",\"lo\":{},\"hi\":{},\"default\":{},\"scale\":{},\"ctx\":{{\"dtid\":{},\"thread_rcbs\":{},\"site\":\"{}\"}}}}",
        req.index,
        req.kind.name(),
        req.lo,
        req.hi,
        req.default,
        req.scale,
        req.dtid,
        req.thread_rcbs,
        req.site
    )
}

struct Controller {
    source: Box<dyn ChoiceSource>,
    next_index: u64,
    record: Option<File>,
}

/// The controller of the current guest run: `None` when no choice flag was
/// given. Process-wide, which is sound on the ptrace and KVM backends, where
/// Detcore runs in the Hermit process and one process runs one guest at a time
/// (`--verify` runs its two guests one after the other). A later version keeps
/// it in per-run Detcore state instead.
static CONTROLLER: Mutex<Option<Controller>> = Mutex::new(None);

/// Start a guest run's choice sequence: build a fresh controller from the
/// configuration, replacing any previous run's, so that each run (for example
/// each of `--verify`'s two runs) reads its tape from the start and records
/// from index 0. Called where the run's root thread state is created.
pub fn start_run(cfg: &crate::config::Config) {
    let source: Box<dyn ChoiceSource> = if let Some(path) = &cfg.choice_tape {
        let bytes = std::fs::read(path)
            .unwrap_or_else(|e| panic!("--choice-tape {}: cannot read: {e}", path.display()));
        Box::new(TapeSource::new(bytes))
    } else if let Some(path) = &cfg.choice_controller {
        Box::new(SocketSource::connect(path).unwrap_or_else(|e| {
            panic!(
                "--choice-controller {}: cannot connect: {e}",
                path.display()
            )
        }))
    } else {
        *CONTROLLER.lock().unwrap() = None;
        return;
    };
    let record = cfg.choice_record.as_ref().map(|path| {
        let file = File::create(path)
            .unwrap_or_else(|e| panic!("--choice-record {}: cannot create: {e}", path.display()));
        // Host-fork timelines (PROTOTYPE): each copy records its own suffix.
        crate::timelines::register_output(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            "choices",
        );
        file
    });
    *CONTROLLER.lock().unwrap() = Some(Controller {
        source,
        next_index: 0,
        record,
    });
}

/// Continue the current run's choice record in a new file, truncating it, from
/// the next index on. A Timeline copy made by forking the Hermit process calls
/// this before its first choice, so that parent and copy do not share one file
/// offset; the copy's full record is the parent's prefix followed by this file.
pub fn reopen_record(path: &Path) -> std::io::Result<()> {
    let file = File::create(path)?;
    if let Some(c) = CONTROLLER.lock().unwrap().as_mut() {
        c.record = Some(file);
    }
    Ok(())
}

/// Whether a controller answers choice points in this run.
pub fn active() -> bool {
    CONTROLLER.lock().unwrap().is_some()
}

/// A choice point as a draw site describes it: a [`ChoiceRequest`] without
/// its index, which [`choose`] assigns.
#[derive(Debug, Clone, Copy)]
pub struct ChoicePoint {
    /// What is being decided.
    pub kind: ChoiceKind,
    /// Smallest admissible answer.
    pub lo: u64,
    /// Largest admissible answer.
    pub hi: u64,
    /// The value Detcore would use without a controller.
    pub default: u64,
    /// A natural magnitude for the decision ([`ChoiceRequest::scale`]).
    pub scale: u64,
    /// The thread the decision is for.
    pub dtid: DetTid,
    /// That thread's committed retired-branch count.
    pub thread_rcbs: u64,
    /// Which draw site asked.
    pub site: &'static str,
}

/// Resolve one choice point: the controller's answer, or `point.default` when
/// no controller is configured. A refusal ends the run with the refused index.
pub fn choose(point: ChoicePoint) -> u64 {
    let ChoicePoint {
        kind,
        lo,
        hi,
        default,
        scale,
        dtid,
        thread_rcbs,
        site,
    } = point;
    let mut guard = CONTROLLER.lock().unwrap();
    let Some(c) = guard.as_mut() else {
        return default;
    };
    let req = ChoiceRequest {
        index: c.next_index,
        kind,
        lo,
        hi,
        default: default.clamp(lo, hi),
        scale,
        dtid,
        thread_rcbs,
        site,
    };
    c.next_index += 1;
    let action = match PAUSE_HOOK.lock().unwrap().as_ref() {
        Some(hook) => hook(&req),
        None => PauseAction::Continue,
    };
    let v = match action {
        PauseAction::Answer(v) => {
            c.source.skip(&req);
            v.clamp(req.lo, req.hi)
        }
        PauseAction::Continue => match c.source.answer(&req) {
            Ok(v) => v,
            Err(refusal) => panic!(
                "choice source refused choice {}: {}",
                refusal.index, refusal.reason
            ),
        },
    };
    if let Some(f) = c.record.as_mut() {
        let mut line = request_json(&req);
        line.truncate(line.len() - 1);
        line.push_str(&format!(",\"v\":{v}}}\n"));
        let _ = f.write_all(line.as_bytes());
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(kind: ChoiceKind, default: u64) -> ChoiceRequest {
        ChoiceRequest {
            index: 0,
            kind,
            lo: 1,
            hi: 10_000,
            default,
            scale: 1000,
            dtid: DetTid::from_raw(3),
            thread_rcbs: 0,
            site: "test",
        }
    }

    #[test]
    fn empty_and_zero_bytes_take_the_default() {
        let mut empty = TapeSource::new(vec![]);
        assert_eq!(empty.answer(&req(ChoiceKind::SchedSlice, 77)).unwrap(), 77);
        let mut zero = TapeSource::new(vec![0, 0]);
        assert_eq!(zero.answer(&req(ChoiceKind::SchedPriority, 5)).unwrap(), 5);
        assert_eq!(zero.answer(&req(ChoiceKind::SchedSlice, 9)).unwrap(), 9);
    }

    #[test]
    fn slice_bytes_are_scale_relative_and_monotone() {
        let r = req(ChoiceKind::SchedSlice, 1);
        assert_eq!(TapeSource::decode(&r, 128), 1000);
        assert_eq!(TapeSource::decode(&r, 144), 2000);
        assert_eq!(TapeSource::decode(&r, 112), 500);
        let mut prev = 0;
        for b in 1..=255u8 {
            let v = TapeSource::decode(&r, b);
            assert!(v >= prev && (r.lo..=r.hi).contains(&v));
            prev = v;
        }
    }

    #[test]
    fn priority_bytes_span_the_range() {
        let r = req(ChoiceKind::SchedPriority, 1);
        assert_eq!(TapeSource::decode(&r, 1), r.lo);
        assert_eq!(TapeSource::decode(&r, 255), r.hi);
    }

    #[test]
    fn a_skipped_request_moves_the_tape_past_its_byte() {
        let mut tape = TapeSource::new(vec![7, 128]);
        tape.skip(&req(ChoiceKind::SchedSlice, 1));
        assert_eq!(tape.answer(&req(ChoiceKind::SchedSlice, 1)).unwrap(), 1000);
    }

    #[test]
    fn request_json_is_one_object() {
        let line = request_json(&req(ChoiceKind::SchedSlice, 7));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["kind"], "sched.slice");
        assert_eq!(v["ctx"]["dtid"], 3);
    }
}
