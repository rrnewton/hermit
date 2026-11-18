/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One guest run's choice sequence: the source that answers it, the index of
//! the next request, and the record. A run that answers choices owns exactly
//! one [`ChoiceSession`], built when the run is built and dropped with it, so
//! each run (each of `--verify`'s two runs, for example) reads its tape from
//! the start and records from index 0, with nothing to reset.

use std::fs::File;
use std::io::Write;

use super::ChoicePoint;
use super::ChoiceRefusal;
use super::ChoiceRequest;
use super::ChoiceSource;
use super::source::SocketSource;
use super::source::TapeSource;
use super::source::request_json;
use crate::config::Config;

/// A run's choice sequence. See the module documentation.
pub struct ChoiceSession {
    source: Box<dyn ChoiceSource>,
    next_index: u64,
    record: Option<File>,
}

impl std::fmt::Debug for ChoiceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChoiceSession")
            .field("next_index", &self.next_index)
            .field("record", &self.record.is_some())
            .finish_non_exhaustive()
    }
}

impl ChoiceSession {
    /// A session answered by `source`, writing its record to `record`.
    pub fn new(source: Box<dyn ChoiceSource>, record: Option<File>) -> Self {
        Self {
            source,
            next_index: 0,
            record,
        }
    }

    /// The session `cfg` asks for: `None` when it names no choice source
    /// (`--choice-tape` or `--choice-controller`). A source that cannot be
    /// opened, or a record that cannot be created, ends the run as a policy
    /// refusal before the guest starts ([`stop_run`]).
    pub fn from_config(cfg: &Config) -> Option<Self> {
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
            return None;
        };
        let record = cfg.choice_record.as_ref().map(|path| {
            File::create(path).unwrap_or_else(|e| {
                stop_run(format!(
                    "--choice-record {}: cannot create: {e}",
                    path.display()
                ))
            })
        });
        Some(Self::new(source, record))
    }

    /// Resolve one choice point: the source's answer, checked against the
    /// point's range and recorded. A refusal, or a record that cannot be
    /// written, ends the run ([`ChoiceSession::refuse_run`]).
    pub fn choose(&mut self, point: ChoicePoint) -> u64 {
        let req = self.request(point);
        let v = match self.source.answer(&req).and_then(|v| within_range(&req, v)) {
            Ok(v) => v,
            Err(refusal) => self.refuse_run(&req, &refusal.reason),
        };
        if let Err(error) = self.record_answer(&req, v) {
            self.refuse_run(&req, &format!("--choice-record write failed: {error}"));
        }
        v
    }

    /// The next request: `point` with this session's next index, and its
    /// default clamped into its range.
    fn request(&mut self, point: ChoicePoint) -> ChoiceRequest {
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
        let req = ChoiceRequest {
            index: self.next_index,
            kind,
            lo,
            hi,
            default: default.clamp(lo, hi),
            scale,
            dtid,
            thread_rcbs,
            site,
        };
        self.next_index += 1;
        req
    }

    /// Append `req` and its answer `v` to the record, if there is one.
    fn record_answer(&mut self, req: &ChoiceRequest, v: u64) -> std::io::Result<()> {
        let Some(f) = self.record.as_mut() else {
            return Ok(());
        };
        let mut line = request_json(req);
        line.truncate(line.len() - 1);
        line.push_str(&format!(",\"v\":{v}}}\n"));
        f.write_all(line.as_bytes())
    }

    /// End the run at `req`, which cannot be answered (or recorded) for
    /// `reason`. The refused request goes in the record with
    /// `"refused":reason`, so the record names where the run stopped, and the
    /// reason goes to stderr ([`stop_run`]).
    fn refuse_run(&mut self, req: &ChoiceRequest, reason: &str) -> ! {
        if let Some(f) = self.record.as_mut() {
            let mut line = request_json(req);
            line.truncate(line.len() - 1);
            let reason = serde_json::Value::String(reason.to_owned());
            line.push_str(&format!(",\"refused\":{reason}}}\n"));
            let _ = f.write_all(line.as_bytes());
        }
        stop_run(format!("choice {}: {reason}", req.index))
    }
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

/// End the run because thread `dettid` of a run with a choice source has no
/// handle to the run's session: its start grant arrived without one, as on a
/// backend whose responses cross a serialization boundary
/// ([`super::ChoiceHandle`]). Refused by name; never answered with the seeded
/// draw instead.
pub(crate) fn refuse_unattached(dettid: crate::types::DetTid) -> ! {
    stop_run(format!(
        "thread {dettid} has no handle to the run's choice session (its start grant carried \
         none); this backend does not deliver the in-process choice capability"
    ))
}

/// End the run for a thread offered a session other than the one it holds:
/// a run has one session, so two mean a delivery fault, never a choice.
pub(crate) fn refuse_other_session(dettid: crate::types::DetTid) -> ! {
    stop_run(format!(
        "thread {dettid} was offered a choice session other than the one it holds; a run \
         has one session"
    ))
}

/// End the run for `message`, a choice source that cannot serve it. The
/// container child exits with Hermit's policy-refusal status, as
/// `tool_global::unrecoverable_shutdown` does: the parent reports it as a
/// refusal (exit 122, `class=policy-refusal`), not as a failure of Hermit's.
fn stop_run(message: String) -> ! {
    eprintln!("HERMIT_CHOICE_REFUSED: {message}");
    crate::guest_log::flush_unsent();
    std::process::exit(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choice::ChoiceKind;
    use crate::types::DetTid;

    fn point(default: u64) -> ChoicePoint {
        ChoicePoint {
            kind: ChoiceKind::SchedPriority,
            lo: 1,
            hi: 10,
            default,
            scale: 0,
            dtid: DetTid::from_raw(3),
            thread_rcbs: 0,
            site: crate::choice::ChoiceSite::Priority,
        }
    }

    #[test]
    fn answers_outside_the_range_are_refused() {
        let r = crate::choice::tests::req(ChoiceKind::SchedPriority, 5);
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

    /// Each session numbers its own requests from 0 and records them in
    /// order, so a second run in the same process (`--verify`'s second leg)
    /// starts its sequence afresh without any reset.
    #[test]
    fn each_session_numbers_and_records_its_own_requests() {
        let dir = tempfile::tempdir().unwrap();
        for run in 0..2 {
            let path = dir.path().join(format!("record-{run}.jsonl"));
            let record = File::create(&path).unwrap();
            let mut session =
                ChoiceSession::new(Box::new(TapeSource::new(vec![0, 255])), Some(record));
            assert_eq!(session.choose(point(4)), 4, "byte 0 takes the default");
            assert_eq!(
                session.choose(point(4)),
                10,
                "byte 255 is the top of the range"
            );
            assert_eq!(session.choose(point(40)), 10, "the default is clamped");
            drop(session);
            let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let indices: Vec<u64> = lines.iter().map(|l| l["i"].as_u64().unwrap()).collect();
            assert_eq!(indices, [0, 1, 2]);
            let answers: Vec<u64> = lines.iter().map(|l| l["v"].as_u64().unwrap()).collect();
            assert_eq!(answers, [4, 10, 10]);
            assert_eq!(lines[2]["default"], 10);
        }
    }
}
