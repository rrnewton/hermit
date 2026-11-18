/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Experimental: choice points answered by an external controller.
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
//! - May refuse: a request it cannot answer (a controller that replies
//!   `"refuse"`, or whose socket fails). The refusal names the index; the run
//!   stops there, which localizes a first divergence to one choice.
//!
//! A source that always answers the default reproduces the seeded run, so an
//! empty tape is the ordinary `--chaos --sched-seed` run.
//!
//! Scope: the ptrace backend, where Detcore runs in the Hermit process, so the
//! controller is one process-wide value; `hermit run` refuses the options on
//! the other backends. The wired choice points, by the record's `site`:
//! `chaos_slice` (a thread's timeslice length), `priority` (a thread's
//! priority redraw), `child_priority` (a new thread's starting priority) and
//! `vfork_child_priority` (a vfork child's). Detcore's other seeded chaos
//! draws are not choice points: the wake thresholds of
//! `--chaos-timed-wait-spread`, the per-thread mean of
//! `--chaos-timeslice-range`, and `--chaos-instruction-offset`.
//! `--chaos-target-races`, which sets a new thread's priority itself,
//! conflicts with the options. `--choice-record` writes every request and its
//! answer as JSON lines.
//!
//! Per run: [`start_run`], at the run's root thread state, opens the source
//! and creates (truncates) the record, so a `--verify` run's record holds the
//! choices of its last leg. A controller's answer has no deadline: Hermit
//! waits for it, so a run is bounded from outside (for example by
//! `bin/safehermit`'s deadline).
//!
//! Wire format, version [`SCHEMA`]: a request is one JSON object,
//! `{"schema":1,"i":N,"kind":K,"lo":L,"hi":H,"default":D,"scale":S,
//! "ctx":{"dtid":T,"thread_rcbs":R,"site":W}}`; a record line is the request
//! with `"v":V`, the answer used, appended, or `"refused":REASON` for the
//! request that stopped the run. A controller replies with one of
//! `{"i":N,"v":V}`, `{"i":N,"default":true}` or `{"i":N,"refuse":"reason"}`.

use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;

use crate::types::DetTid;

/// The version of the request and record JSON lines (see the module docs).
pub const SCHEMA: u32 = 1;

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
                (scale * tape_slice_factor(i32::from(b) - 128)).round() as u64
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

/// `2^(k/16)` for `k` in `-127..=127`, from IEEE 754 basic operations only, so
/// that a tape byte decodes to the same slice on every host and Rust version
/// (the reason `scheduler::timed_wait_spread` avoids `f64::powf`, whose
/// precision Rust leaves unspecified). Bit `i` of the sixteenths contributes
/// `2^(2^i/16)`, the `4 - i`-th repeated square root of 2, multiplied in from
/// the lowest bit up; the whole powers of two are exact.
fn tape_slice_factor(k: i32) -> f64 {
    // roots[i] = 2^(2^i / 16): 2^(1/16), 2^(1/8), 2^(1/4), 2^(1/2).
    let mut roots = [2f64.sqrt(); 4];
    for i in (0..3).rev() {
        roots[i] = roots[i + 1].sqrt();
    }
    let sixteenths = k.rem_euclid(16);
    let mut factor = 1.0;
    for (i, root) in roots.iter().enumerate() {
        if sixteenths >> i & 1 == 1 {
            factor *= root;
        }
    }
    let whole = k.div_euclid(16);
    let step = if whole < 0 { 0.5 } else { 2.0 };
    for _ in 0..whole.unsigned_abs() {
        factor *= step;
    }
    factor
}

impl ChoiceSource for TapeSource {
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

/// The request as one JSON object (no trailing newline).
pub fn request_json(req: &ChoiceRequest) -> String {
    format!(
        "{{\"schema\":{SCHEMA},\"i\":{},\"kind\":\"{}\",\"lo\":{},\"hi\":{},\"default\":{},\"scale\":{},\"ctx\":{{\"dtid\":{},\"thread_rcbs\":{},\"site\":\"{}\"}}}}",
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
/// given. Process-wide, which is sound only on the ptrace backend, where
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
        let bytes = std::fs::read(path).unwrap_or_else(|e| {
            stop_run(format!(
                "--choice-tape {}: cannot read: {e}",
                path.display()
            ))
        });
        Box::new(TapeSource::new(bytes))
    } else if let Some(path) = &cfg.choice_controller {
        Box::new(SocketSource::connect(path).unwrap_or_else(|e| {
            stop_run(format!(
                "--choice-controller {}: cannot connect: {e}",
                path.display()
            ))
        }))
    } else {
        *CONTROLLER.lock().unwrap() = None;
        return;
    };
    let record = cfg.choice_record.as_ref().map(|path| {
        File::create(path).unwrap_or_else(|e| {
            stop_run(format!(
                "--choice-record {}: cannot create: {e}",
                path.display()
            ))
        })
    });
    *CONTROLLER.lock().unwrap() = Some(Controller {
        source,
        next_index: 0,
        record,
    });
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
/// no controller is configured. A refusal ends the run ([`refuse_run`]).
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
    let v = match c.source.answer(&req).and_then(|v| within_range(&req, v)) {
        Ok(v) => v,
        Err(refusal) => refuse_run(c, &req, &refusal.reason),
    };
    if let Some(f) = c.record.as_mut() {
        let mut line = request_json(&req);
        line.truncate(line.len() - 1);
        line.push_str(&format!(",\"v\":{v}}}\n"));
        if let Err(error) = f.write_all(line.as_bytes()) {
            refuse_run(c, &req, &format!("--choice-record write failed: {error}"));
        }
    }
    v
}

/// The [`ChoiceSource`] contract's range promise, enforced once for every
/// source: an answer outside `req`'s `[lo, hi]` (a reserved priority, an
/// empty slice) is refused rather than applied.
fn within_range(req: &ChoiceRequest, v: u64) -> Result<u64, ChoiceRefusal> {
    if (req.lo..=req.hi).contains(&v) {
        Ok(v)
    } else {
        Err(ChoiceRefusal {
            index: req.index,
            reason: format!("source answer {v} is outside [{}, {}]", req.lo, req.hi),
        })
    }
}

/// End the run at `req`, which cannot be answered (or recorded) for `reason`.
/// The refused request goes in the record with `"refused":reason`, so the
/// record names where the run stopped, and the reason goes to stderr
/// ([`stop_run`]). The container child then exits with Hermit's
/// policy-refusal status, as
/// `tool_global::unrecoverable_shutdown` does: the parent reports it as a
/// refusal (exit 122, `class=policy-refusal`), not as a failure of Hermit's.
fn refuse_run(c: &mut Controller, req: &ChoiceRequest, reason: &str) -> ! {
    if let Some(f) = c.record.as_mut() {
        let mut line = request_json(req);
        line.truncate(line.len() - 1);
        let reason = serde_json::Value::String(reason.to_owned());
        line.push_str(&format!(",\"refused\":{reason}}}\n"));
        let _ = f.write_all(line.as_bytes());
    }
    stop_run(format!("choice {}: {reason}", req.index))
}

/// End the run for `message`, a choice source that cannot serve it, with
/// Hermit's policy-refusal status ([`refuse_run`]).
fn stop_run(message: String) -> ! {
    eprintln!("HERMIT_CHOICE_REFUSED: {message}");
    crate::guest_log::flush_unsent();
    std::process::exit(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
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

    /// The slice decode is exact: whole sixteenths of an octave are exact
    /// powers of two, and every factor is the product of correctly rounded
    /// square roots of 2 that the doc names, so it cannot vary by host.
    #[test]
    fn slice_factors_are_products_of_square_roots_of_two() {
        let r2 = 2f64.sqrt();
        let (r4, r8, r16) = (r2.sqrt(), r2.sqrt().sqrt(), r2.sqrt().sqrt().sqrt());
        assert_eq!(tape_slice_factor(0), 1.0);
        assert_eq!(tape_slice_factor(16), 2.0);
        assert_eq!(tape_slice_factor(-16), 0.5);
        assert_eq!(tape_slice_factor(112), 128.0);
        assert_eq!(tape_slice_factor(-128), 1.0 / 256.0);
        assert_eq!(tape_slice_factor(8), r2);
        assert_eq!(tape_slice_factor(1), r16);
        assert_eq!(tape_slice_factor(15), 1.0 * r16 * r8 * r4 * r2);
        assert_eq!(tape_slice_factor(-1), r16 * r8 * r4 * r2 * 0.5);
        assert_eq!(tape_slice_factor(127), r16 * r8 * r4 * r2 * 128.0);
        // Sixteenths of 10, 12 and 14 multiply two or three rounded roots, and
        // the product lands one ulp from glibc 2.34's `powf` (measured): these
        // bits pin the method, not just the value's neighbourhood.
        for (k, bits) in [
            (10, 0x3ff8_ace5_422a_a0dc_u64),
            (12, 0x3ffa_e89f_995a_d3ae),
            (14, 0x3ffd_5818_dcfb_a488),
            (-2, 0x3fed_5818_dcfb_a488),
            (126, 0x406d_5818_dcfb_a488),
            (-118, 0x3f78_ace5_422a_a0dc),
        ] {
            assert_eq!(tape_slice_factor(k).to_bits(), bits, "k = {k}");
        }
        for k in -127..127 {
            assert!(tape_slice_factor(k) < tape_slice_factor(k + 1), "k = {k}");
        }
    }

    #[test]
    fn answers_outside_the_range_are_refused() {
        let r = req(ChoiceKind::SchedPriority, 5);
        assert_eq!(within_range(&r, 1).unwrap(), 1);
        assert_eq!(within_range(&r, 10_000).unwrap(), 10_000);
        for v in [0, 10_001, u64::MAX] {
            let refusal = within_range(&r, v).unwrap_err();
            assert_eq!(
                refusal.reason,
                format!("source answer {v} is outside [1, 10000]")
            );
        }
    }

    #[test]
    fn priority_bytes_span_the_range() {
        let r = req(ChoiceKind::SchedPriority, 1);
        assert_eq!(TapeSource::decode(&r, 1), r.lo);
        assert_eq!(TapeSource::decode(&r, 255), r.hi);
    }

    #[test]
    fn request_json_is_one_object() {
        let line = request_json(&req(ChoiceKind::SchedSlice, 7));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["schema"], SCHEMA);
        assert_eq!(v["kind"], "sched.slice");
        assert_eq!(v["ctx"]["dtid"], 3);
    }
}
