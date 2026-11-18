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
//!   indices from 0, each through `answer`, or through `skip` when a pause
//!   hook answered it instead. Hermit serializes the guest, so the order is a
//!   function of the previous answers and the guest's inputs.
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
//! A *pause hook* ([`set_pause_hook`]) sees each request before the source
//! does and may answer it itself; the source then skips the request, so a
//! hook's answer to request `i` replaces only byte `i` of a tape. It is the
//! entry point for a layer that pauses a run at a choice point, for example to
//! checkpoint it, and answers there; no command-line option installs one.
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
use std::sync::Arc;
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
    /// The thread that drew: for a child's starting priority (`child_priority`,
    /// `vfork_child_priority`), its parent.
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

    /// `req` was answered without asking this source, by a pause hook's
    /// [`PauseAction::Answer`]. A source that reads its answers by position
    /// moves past `req`, so that the hook's answer replaces only this
    /// request's. The default does nothing.
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
            ChoiceKind::SchedSlice => tape_slice(req.scale.max(1), b),
            // 255 evenly spaced levels over the priority range.
            ChoiceKind::SchedPriority => {
                let span = req.hi - req.lo;
                req.lo + span * u64::from(b - 1) / 254
            }
        };
        v.clamp(req.lo, req.hi)
    }
}

/// `2^(j/16)` for `j` in `0..16`, as fixed-point numbers with 60 fractional
/// bits, each rounded to nearest (computed at 100 decimal digits). Entry 0 is
/// exactly 1 and entry 8 is the square root of 2.
const SIXTEENTH_POWERS_Q60: [u64; 16] = [
    0x1000_0000_0000_0000,
    0x10b5_586c_f989_0f63,
    0x1172_b83c_7d51_7add,
    0x1238_7a6e_7562_3867,
    0x1306_fe0a_31b7_152e,
    0x13de_a64c_1234_2236,
    0x14bf_dad5_362a_271d,
    0x15ab_07dd_4854_2959,
    0x16a0_9e66_7f3b_cc91,
    0x17a1_1473_eb01_86d8,
    0x18ac_e542_2aa0_db5c,
    0x19c4_9182_a3f0_901c,
    0x1ae8_9f99_5ad3_ad5f,
    0x1c19_9bdd_8552_9c22,
    0x1d58_18dc_fba4_8726,
    0x1ea4_afa2_a490_d986,
];

/// The slice a nonzero tape byte `b` selects around `scale`:
/// `scale * 2^((b-128)/16)`, rounded half up. Integer arithmetic only, so a
/// tape decodes to the same slice on every host and Rust version (the reason
/// `scheduler::timed_wait_spread` avoids `f64::powf`, whose precision Rust
/// leaves unspecified). The only approximation is the table's 2^-61 relative
/// error, which changes no result for scales below 2^40 except within that
/// distance of a half.
fn tape_slice(scale: u64, b: u8) -> u64 {
    let k = i32::from(b) - 128;
    let octaves = k.div_euclid(16);
    let sixteenths = k.rem_euclid(16) as usize;
    // 2^(k/16) * 2^60 * 2^-shift = 2^(k/16) when shift = 60 - octaves, which
    // lies in 53..=68 for k in -128..=127.
    let shift = (60 - octaves) as u32;
    let product = u128::from(scale) * u128::from(SIXTEENTH_POWERS_Q60[sixteenths]);
    let rounded = (product + (1u128 << (shift - 1))) >> shift;
    u64::try_from(rounded).unwrap_or(u64::MAX)
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

    /// Moves past `req`'s byte, so the next request reads the byte after it.
    fn skip(&mut self, _req: &ChoiceRequest) {
        self.pos += 1;
    }
}

/// Asks a controller process over a Unix stream socket, one JSON line per
/// request and one per answer: `{"i":N,"v":V}` or `{"i":N,"default":true}`.
/// It keeps the default [`ChoiceSource::skip`], so the controller sees no line
/// for a request a pause hook answered: the next request's `"i"` jumps past it.
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
        // Exactly one of "v", "default" or "refuse", each of its documented
        // type. A reply naming two, or one of another type, is ambiguous and
        // is refused rather than read one way.
        let named: Vec<&str> = ["v", "default", "refuse"]
            .into_iter()
            .filter(|key| parsed.get(*key).is_some())
            .collect();
        if named.len() != 1 {
            return Err(refuse(format!(
                "controller reply must carry exactly one of \"v\", \"default\" or \"refuse\": \
                 {reply:?}"
            )));
        }
        if named[0] == "refuse" {
            return match parsed["refuse"].as_str() {
                Some(reason) => Err(refuse(format!("controller refused: {reason}"))),
                None => Err(refuse(format!(
                    "controller reply's \"refuse\" is not a string: {reply:?}"
                ))),
            };
        }
        if named[0] == "default" {
            return if parsed["default"] == serde_json::Value::Bool(true) {
                Ok(req.default)
            } else {
                Err(refuse(format!(
                    "controller reply's \"default\" is not true: {reply:?}"
                )))
            };
        }
        let v = parsed["v"].as_u64().ok_or_else(|| {
            refuse(format!(
                "controller reply's \"v\" is not an unsigned integer: {reply:?}"
            ))
        })?;
        if v < req.lo || v > req.hi {
            return Err(refuse(format!(
                "controller answer {v} is outside [{}, {}]",
                req.lo, req.hi
            )));
        }
        Ok(v)
    }
}

/// What a pause hook tells [`choose`] to do with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Ask the run's choice source, as without a hook.
    Continue,
    /// Answer with this value instead of asking the source, which skips the
    /// request ([`ChoiceSource::skip`]). The value is recorded like any
    /// answer; a value outside the request's `[lo, hi]` ends the run, as a
    /// controller's does ([`refuse_run`]).
    Answer(u64),
}

/// A callback at each choice point of a run that has a choice source, after
/// [`choose`] numbers the request and before the source answers it. It runs
/// inside [`choose`] with the run's controller locked, so it must not call
/// [`start_run`], [`active`] or [`choose`]. A hook that copies the process by
/// forking must fork from inside the hook, on its own thread: the copy's
/// version of that thread then holds the lock and releases it when [`choose`]
/// returns, where a fork from any other thread would leave the copy's
/// controller locked for good. A hook that panics ends the run as a refusal
/// ([`refuse_run`]): the panic is caught before it can unwind through the
/// locked controller. There is none unless [`set_pause_hook`] installs one.
pub type PauseHook = Arc<dyn Fn(&ChoiceRequest) -> PauseAction + Send + Sync>;

/// The installed pause hook. Process-wide, like the controller.
static PAUSE_HOOK: Mutex<Option<PauseHook>> = Mutex::new(None);

/// Install a pause hook, or with `None` remove it. It applies from the next
/// choice point on, in this run and later ones, until replaced.
pub fn set_pause_hook(hook: Option<PauseHook>) {
    *PAUSE_HOOK.lock().unwrap() = hook;
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
    /// The thread that drew: for a child's starting priority (`child_priority`,
    /// `vfork_child_priority`), its parent.
    pub dtid: DetTid,
    /// That thread's committed retired-branch count.
    pub thread_rcbs: u64,
    /// Which draw site asked.
    pub site: &'static str,
}

/// Resolve one choice point: the pause hook's answer if it gives one, else the
/// controller's, or `point.default` when no controller is configured (the hook
/// is not asked then). A refusal ends the run ([`refuse_run`]).
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
    // Cloned out, so the hook slot is not locked while the hook runs.
    let hook = PAUSE_HOOK.lock().unwrap().clone();
    let action = match hook {
        None => PauseAction::Continue,
        // A panic must not unwind out of the hook with the controller locked:
        // it ends the run as a refusal, like an answer the run cannot use.
        Some(hook) => match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(&req))) {
            Ok(action) => action,
            Err(payload) => refuse_run(
                c,
                &req,
                &format!("pause hook panicked: {}", panic_message(payload.as_ref())),
            ),
        },
    };
    let v = match action {
        PauseAction::Continue => match c.source.answer(&req).and_then(|v| within_range(&req, v)) {
            Ok(v) => v,
            Err(refusal) => refuse_run(c, &req, &refusal.reason),
        },
        PauseAction::Answer(v) if (req.lo..=req.hi).contains(&v) => {
            c.source.skip(&req);
            v
        }
        PauseAction::Answer(v) => refuse_run(
            c,
            &req,
            &format!("pause hook answer {v} is outside [{}, {}]", req.lo, req.hi),
        ),
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

/// The text a panic carried, for a refusal's reason: its message when it is a
/// string, as `panic!` makes it.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a payload that is not a string".to_owned())
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

    /// The slice decode is exact fixed point: the documented value rounded
    /// half up, the same on every host. 1000/16 = 62.5 rounds up. The two
    /// large scales are values a float decode (`f64::powf`, then `round`)
    /// gets one too high on glibc 2.34, measured: 6722268617794.4996 and
    /// 2226918284313.4999972 round down.
    #[test]
    fn slice_decode_is_exact_fixed_point() {
        assert_eq!(SIXTEENTH_POWERS_Q60[0], 1 << 60);
        for pair in SIXTEENTH_POWERS_Q60.windows(2) {
            assert!(pair[0] < pair[1]);
        }
        for (scale, b, slice) in [
            (1000, 128, 1000),
            (1000, 144, 2000),
            (1000, 112, 500),
            (1000, 136, 1414),
            (1000, 64, 63),
            (1000, 1, 4),
            (1000, 255, 245_146),
            (200_000_000, 116, 118_920_712),
            (200_000_000, 179, 1_822_061_816),
            (1, 255, 245),
            (35_561_209_233, 249, 6_722_268_617_794),
            (43_211_185_441, 219, 2_226_918_284_313),
            (u64::MAX, 255, u64::MAX),
        ] {
            assert_eq!(tape_slice(scale, b), slice, "scale {scale}, byte {b}");
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

    /// The controller and the pause hook are process-wide, so a test that sets
    /// them holds this lock; its guard clears both when it drops.
    static RUN_LOCK: Mutex<()> = Mutex::new(());

    struct RunGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for RunGuard {
        fn drop(&mut self) {
            set_pause_hook(None);
            start_run(&crate::config::Config::default());
        }
    }

    fn run_guard() -> RunGuard {
        RunGuard {
            _lock: RUN_LOCK.lock().unwrap_or_else(|e| e.into_inner()),
        }
    }

    /// Starts a run through `start_run` with `tape` as its `--choice-tape` and
    /// `dir/record` as its `--choice-record`, and returns the record's path.
    fn start_tape_run(dir: &Path, tape: &[u8]) -> std::path::PathBuf {
        let tape_path = dir.join("tape");
        std::fs::write(&tape_path, tape).unwrap();
        let record = dir.join("record");
        start_run(&crate::config::Config {
            choice_tape: Some(tape_path),
            choice_record: Some(record.clone()),
            ..Default::default()
        });
        record
    }

    fn slice_point() -> ChoicePoint {
        ChoicePoint {
            kind: ChoiceKind::SchedSlice,
            lo: 1,
            hi: 10_000,
            default: 77,
            scale: 1000,
            dtid: DetTid::from_raw(3),
            thread_rcbs: 0,
            site: "test",
        }
    }

    /// The answer on each record line, checking that each line starts with
    /// the schema.
    fn recorded(record: &Path) -> Vec<u64> {
        std::fs::read_to_string(record)
            .unwrap()
            .lines()
            .map(|line| {
                assert!(line.starts_with("{\"schema\":1,"), "{line}");
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                v["v"].as_u64().unwrap()
            })
            .collect()
    }

    #[test]
    fn a_pause_hook_answer_is_used_recorded_and_replaces_one_tape_byte() {
        let _run = run_guard();
        let dir = tempfile::tempdir().unwrap();
        // At scale 1000, bytes 144, 112 and 136 decode to 2000, 500 and 1414.
        let record = start_tape_run(dir.path(), &[144, 112, 136]);
        set_pause_hook(Some(Arc::new(|req: &ChoiceRequest| {
            if req.index == 1 {
                PauseAction::Answer(4242)
            } else {
                PauseAction::Continue
            }
        })));
        let answers: Vec<u64> = (0..3).map(|_| choose(slice_point())).collect();
        // Request 2 reads byte 2, not byte 1: the hook's answer replaced byte 1.
        assert_eq!(answers, [2000, 4242, 1414]);
        assert_eq!(recorded(&record), answers);
    }

    #[test]
    fn each_request_a_hook_answers_moves_the_tape_by_one_byte() {
        let _run = run_guard();
        let dir = tempfile::tempdir().unwrap();
        // At scale 1000, bytes 144, 112, 136 and 120 decode to 2000, 500, 1414
        // and 707.
        let record = start_tape_run(dir.path(), &[144, 112, 136, 120]);
        set_pause_hook(Some(Arc::new(|req: &ChoiceRequest| {
            if req.index.is_multiple_of(2) {
                PauseAction::Answer(4242)
            } else {
                PauseAction::Continue
            }
        })));
        let answers: Vec<u64> = (0..4).map(|_| choose(slice_point())).collect();
        // Requests 1 and 3 read bytes 1 and 3.
        assert_eq!(answers, [4242, 500, 4242, 707]);
        assert_eq!(recorded(&record), answers);
    }

    #[test]
    fn a_pause_hook_answer_out_of_range_ends_the_run_as_a_refusal() {
        const CHILD: &str = "HERMIT_TEST_CHOICE_PAUSE_REFUSAL_CHILD";
        const CASE: &str =
            "choice::tests::a_pause_hook_answer_out_of_range_ends_the_run_as_a_refusal";
        if let Some(dir) = std::env::var_os(CHILD) {
            // The child: choose() refuses, which exits the process.
            let _run = run_guard();
            start_tape_run(Path::new(&dir), &[]);
            set_pause_hook(Some(Arc::new(|_: &ChoiceRequest| {
                PauseAction::Answer(10_001)
            })));
            choose(slice_point());
            return;
        }
        // A refusal ends the process, so the run is this test alone in a child
        // process of this binary.
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CASE, "--nocapture", "--test-threads=1"])
            .env(CHILD, dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{output:?}"
        );
        let reason = "pause hook answer 10001 is outside [1, 10000]";
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("HERMIT_CHOICE_REFUSED: choice 0: {reason}")),
            "{output:?}"
        );
        // The record's one line is the refused request.
        let record = std::fs::read_to_string(dir.path().join("record")).unwrap();
        let lines: Vec<serde_json::Value> = record
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 1, "{record}");
        assert_eq!(lines[0]["schema"], SCHEMA);
        assert_eq!(lines[0]["i"], 0);
        assert_eq!(lines[0]["refused"], reason);
        assert!(lines[0].get("v").is_none(), "{record}");
    }

    #[test]
    fn a_panicking_pause_hook_ends_the_run_as_a_refusal() {
        const CHILD: &str = "HERMIT_TEST_CHOICE_PAUSE_PANIC_CHILD";
        const CASE: &str = "choice::tests::a_panicking_pause_hook_ends_the_run_as_a_refusal";
        if let Some(dir) = std::env::var_os(CHILD) {
            // The child: the hook panics at the second request, and choose()
            // refuses, which exits the process.
            let _run = run_guard();
            start_tape_run(Path::new(&dir), &[144, 112]);
            set_pause_hook(Some(Arc::new(|req: &ChoiceRequest| {
                if req.index == 1 {
                    panic!("the hook failed at request {}", req.index);
                }
                PauseAction::Continue
            })));
            choose(slice_point());
            choose(slice_point());
            return;
        }
        // A refusal ends the process, so the run is this test alone in a child
        // process of this binary.
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CASE, "--nocapture", "--test-threads=1"])
            .env(CHILD, dir.path())
            .output()
            .unwrap();
        // A refusal, not the test harness's failure status for a panic.
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
            "{output:?}"
        );
        let reason = "pause hook panicked: the hook failed at request 1";
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("HERMIT_CHOICE_REFUSED: choice 1: {reason}")),
            "{output:?}"
        );
        // The record holds request 0's answer (byte 144 at scale 1000 decodes
        // to 2000), then the refused request 1.
        let record = std::fs::read_to_string(dir.path().join("record")).unwrap();
        let lines: Vec<serde_json::Value> = record
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "{record}");
        assert_eq!(lines[0]["v"], 2000);
        assert_eq!(lines[1]["i"], 1);
        assert_eq!(lines[1]["refused"], reason);
        assert!(lines[1].get("v").is_none(), "{record}");
    }
}
