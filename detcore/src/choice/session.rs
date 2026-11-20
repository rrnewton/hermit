/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! One guest run's choice sequence: the source that answers it, the index of
//! the next request, the draw announced before the run mode's choice boundary
//! ran ([`ChoiceSession::announce`]), and the record. A run that answers
//! choices owns exactly one [`ChoiceSession`], built when the run is built and
//! dropped with it, so each run (each of `--verify`'s two runs, for example)
//! reads its tape from the start and records from index 0, with nothing to
//! reset.

use std::fs::File;
use std::io::Write;

use super::ChoicePoint;
use super::ChoiceRefusal;
use super::ChoiceRequest;
use super::ChoiceSite;
use super::ChoiceSource;
use super::source::SocketSource;
use super::source::TapeSource;
use super::source::request_json;
use crate::config::Config;
use crate::types::DetTid;

/// A run's choice sequence. See the module documentation.
pub struct ChoiceSession {
    source: Box<dyn ChoiceSource>,
    next_index: u64,
    /// The draw announced before the run mode's choice boundary ran, until
    /// that draw takes it ([`Self::announce`]).
    announced: Option<Announced>,
    record: Option<File>,
}

/// A draw announced before the run mode's choice boundary ran: the request
/// it takes, its site and the thread that makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Announced {
    index: u64,
    site: ChoiceSite,
    dtid: DetTid,
}

impl std::fmt::Debug for ChoiceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChoiceSession")
            .field("next_index", &self.next_index)
            .field("announced", &self.announced)
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
            announced: None,
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

    /// The index the next request will take: the number of requests made so
    /// far. Requests are made one at a time in run order (Hermit serializes
    /// the guest), so a read just before a draw sees the index that draw
    /// takes. Reading it changes nothing.
    pub fn next_index(&self) -> u64 {
        self.next_index
    }

    /// Announce the next request: the run mode's choice boundary is about to
    /// make thread `dtid`'s draw of a point at `site`
    /// ([`crate::ChoiceBoundary`]), and that draw takes the next request.
    /// Returns the request's index. The draw checks that it is the one
    /// announced ([`Self::choose`]). A second announcement before it ends the
    /// run as a fault of Hermit's own ([`fail_run`]): a boundary with no draw
    /// after it would have acted at a request some other point takes.
    pub fn announce(&mut self, dtid: DetTid, site: ChoiceSite) -> u64 {
        let announced = Announced {
            index: self.next_index,
            site,
            dtid,
        };
        if let Some(pending) = self.announced.replace(announced) {
            fail_run(undrawn_announcement(pending, dtid, site));
        }
        announced.index
    }

    /// Resolve one choice point: the source's answer, checked against the
    /// point's range and recorded. The point must be the draw last announced
    /// ([`Self::announce`]): a draw not announced ends the run as a fault of
    /// Hermit's own ([`ChoiceSession::fail_run_at`]). A refusal, or a record
    /// that cannot be written, ends it as a refusal
    /// ([`ChoiceSession::refuse_run`]).
    pub fn choose(&mut self, point: ChoicePoint) -> u64 {
        let req = self.request(point);
        if let Some(reason) = unannounced_draw(self.announced.take(), &req) {
            self.fail_run_at(&req, &reason);
        }
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
        self.record_stop(req, reason);
        stop_run(format!("choice {}: {reason}", req.index))
    }

    /// End the run at `req` for `reason`, a fault of Hermit's own
    /// (`internal:`): recorded as [`Self::refuse_run`] records a refusal, and
    /// reported as an internal failure ([`fail_run`]).
    fn fail_run_at(&mut self, req: &ChoiceRequest, reason: &str) -> ! {
        self.record_stop(req, reason);
        fail_run(format!("choice {}: {reason}", req.index))
    }

    /// Append `req` to the record, if there is one, with `"refused":reason`:
    /// the request where the run stopped.
    fn record_stop(&mut self, req: &ChoiceRequest, reason: &str) {
        if let Some(f) = self.record.as_mut() {
            let mut line = request_json(req);
            line.truncate(line.len() - 1);
            let reason = serde_json::Value::String(reason.to_owned());
            line.push_str(&format!(",\"refused\":{reason}}}\n"));
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Finish the session of a run whose guest finished: every announced
    /// draw must have been made. Called once, explicitly, when the run's
    /// global state cleans up after its scheduler has shut down
    /// (`GlobalState::clean_up`), on every backend. An announcement without
    /// its draw ends the run as a fault of Hermit's own ([`fail_run`]); a
    /// later announcement would have caught it, but at the end of a run there
    /// is none. A run that ends otherwise (a refusal, a fault, a signal) does
    /// not finish its session, and dropping a session checks nothing.
    pub fn finish(&mut self) {
        if let Some(reason) = undrawn_at_finish(self.announced.take()) {
            fail_run(reason);
        }
    }
}

impl std::fmt::Display for Announced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request {}, site {}, thread {}",
            self.index,
            self.site.name(),
            self.dtid
        )
    }
}

/// Why `req` cannot be drawn after `announced`, the announcement it takes:
/// `None` when it is the draw announced, at the same request, site and
/// thread. Either case is a fault of Hermit's own, never a controller's or
/// the guest's: its reason says so (`internal:`), and it ends the run as one
/// ([`fail_run`]).
fn unannounced_draw(announced: Option<Announced>, req: &ChoiceRequest) -> Option<String> {
    let drawn = Announced {
        index: req.index,
        site: req.site,
        dtid: req.dtid,
    };
    match announced {
        Some(announced) if announced == drawn => None,
        Some(announced) => Some(format!(
            "internal: draw is not the one its boundary announced (drawn: {drawn}; announced: \
             {announced})"
        )),
        None => Some(format!(
            "internal: draw without its boundary announcement ({drawn})"
        )),
    }
}

/// Why thread `dtid`'s announcement of a draw at `site` is refused while
/// `pending`, an earlier announcement, has had no draw: a fault of Hermit's
/// own (`internal:`).
fn undrawn_announcement(pending: Announced, dtid: DetTid, site: ChoiceSite) -> String {
    format!(
        "choice {}: internal: boundary announcement without its draw (announced: {pending}; \
         then thread {dtid} announced site {})",
        pending.index,
        site.name()
    )
}

/// Why a session that finishes with `announced` still pending is refused: a
/// boundary announcement without its draw, a fault of Hermit's own
/// (`internal:`). `None` when every announced draw was made.
fn undrawn_at_finish(announced: Option<Announced>) -> Option<String> {
    announced.map(|pending| {
        format!(
            "choice {}: internal: boundary announcement without its draw when the run finished \
             (announced: {pending})",
            pending.index
        )
    })
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

/// End the run for `message`, a choice source that cannot serve it: an
/// option, a source or a record that cannot be used, or an answer refused.
/// The container child exits with Hermit's policy-refusal status, as
/// `tool_global::unrecoverable_shutdown` does: the parent reports it as a
/// refusal (exit 122, `class=policy-refusal`), not as a failure of Hermit's.
/// A fault of Hermit's own in the protocol (an `internal:` reason) is never
/// this: it ends the run through [`fail_run`].
fn stop_run(message: String) -> ! {
    eprintln!("HERMIT_CHOICE_REFUSED: {message}");
    crate::guest_log::flush_unsent();
    std::process::exit(detcore_model::HERMIT_POLICY_REFUSAL_EXIT)
}

/// What a process does just before a fault of Hermit's own in the choice
/// protocol exits it with `HERMIT_INTERNAL_FAILURE_EXIT` ([`fail_run`]). The
/// status is all that crosses the process boundary, and it is not this
/// fault's alone, so a parent that forked the process learns through this
/// hook that the failure was the choice protocol's.
pub type ChoiceInternalExitHook = fn();

static CHOICE_INTERNAL_EXIT_HOOK: std::sync::Mutex<Option<ChoiceInternalExitHook>> =
    std::sync::Mutex::new(None);

/// Install (or, with `None`, remove) the [`ChoiceInternalExitHook`]. A process
/// forked after this inherits it.
pub fn set_choice_internal_exit_hook(hook: Option<ChoiceInternalExitHook>) {
    *CHOICE_INTERNAL_EXIT_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = hook;
}

/// End the run for `message`, a fault of Hermit's own in the choice protocol
/// (an `internal:` reason: a draw its boundary did not announce, or an
/// announcement without its draw), never a source's, a controller's or the
/// guest's. The container child exits with Hermit's internal-failure status,
/// after the [`ChoiceInternalExitHook`]: the parent reports it as a failure
/// of Hermit's (exit 125, `HERMIT_INTERNAL_FAILURE` with
/// `cause=choice-internal`), not as a refusal.
fn fail_run(message: String) -> ! {
    eprintln!("HERMIT_INTERNAL_FAILURE class=choice-internal {message}");
    crate::guest_log::flush_unsent();
    let hook = *CHOICE_INTERNAL_EXIT_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(hook) = hook {
        hook();
    }
    std::process::exit(detcore_model::HERMIT_INTERNAL_FAILURE_EXIT)
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

    /// `point`'s draw, announced first as its choice boundary announces it.
    fn announced_choose(session: &mut ChoiceSession, point: ChoicePoint) -> u64 {
        let index = session.announce(point.dtid, point.site);
        assert_eq!(index, session.next_index(), "announcing draws nothing");
        session.choose(point)
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
            assert_eq!(
                announced_choose(&mut session, point(4)),
                4,
                "byte 0 takes the default"
            );
            assert_eq!(
                announced_choose(&mut session, point(4)),
                10,
                "byte 255 is the top of the range"
            );
            assert_eq!(
                announced_choose(&mut session, point(40)),
                10,
                "the default is clamped"
            );
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

    /// An announcement names the next request, its site and its thread, and
    /// the draw that takes it matches all three. A draw with no announcement
    /// before it, or whose announcement names another request, site or
    /// thread, is refused as Hermit's own fault (`internal:`).
    #[test]
    fn a_draw_takes_the_request_announced_or_is_refused() {
        let mut session = ChoiceSession::new(Box::new(TapeSource::new(vec![])), None);
        assert_eq!(announced_choose(&mut session, point(4)), 4);
        let index = session.announce(DetTid::from_raw(3), ChoiceSite::Priority);
        assert_eq!(index, 1, "the announcement names the next request");
        let announced = session.announced.expect("pending until its draw");
        let mut req = crate::choice::tests::req(ChoiceKind::SchedPriority, 5);
        req.index = 1;
        req.site = ChoiceSite::Priority;
        assert_eq!(unannounced_draw(Some(announced), &req), None);
        assert_eq!(
            unannounced_draw(None, &req).unwrap(),
            "internal: draw without its boundary announcement (request 1, site priority, \
             thread 3)"
        );
        let mismatched = |announced| unannounced_draw(Some(announced), &req).unwrap();
        assert_eq!(
            mismatched(Announced {
                site: ChoiceSite::ChaosSlice,
                ..announced
            }),
            "internal: draw is not the one its boundary announced (drawn: request 1, site \
             priority, thread 3; announced: request 1, site chaos_slice, thread 3)"
        );
        assert_eq!(
            mismatched(Announced {
                dtid: DetTid::from_raw(4),
                ..announced
            }),
            "internal: draw is not the one its boundary announced (drawn: request 1, site \
             priority, thread 3; announced: request 1, site priority, thread 4)"
        );
        assert_eq!(
            mismatched(Announced {
                index: 0,
                ..announced
            }),
            "internal: draw is not the one its boundary announced (drawn: request 1, site \
             priority, thread 3; announced: request 0, site priority, thread 3)"
        );

        // The draw takes the announcement: the next one starts afresh.
        assert_eq!(session.choose(point(4)), 4);
        assert_eq!(session.announced, None);
        assert_eq!(announced_choose(&mut session, point(4)), 4);
    }

    /// A second announcement before the first one's draw names both, as
    /// Hermit's own fault.
    #[test]
    fn an_announcement_without_its_draw_is_named() {
        let pending = Announced {
            index: 7,
            site: ChoiceSite::ChaosSlice,
            dtid: DetTid::from_raw(3),
        };
        assert_eq!(
            undrawn_announcement(pending, DetTid::from_raw(5), ChoiceSite::Priority),
            "choice 7: internal: boundary announcement without its draw (announced: request 7, \
             site chaos_slice, thread 3; then thread 5 announced site priority)"
        );
    }

    /// What the choice internal-fault exit hook prints in a test's child, so
    /// the parent sees that it ran, and when.
    const HOOK_RAN: &str = "test: the choice internal-fault exit hook ran";

    fn print_hook_ran() {
        eprintln!("{HOOK_RAN}");
    }

    /// Runs the calling test's body (`body`) in a child copy of this test
    /// binary, on a session over an empty tape that records to a file, and
    /// requires the child to end as a fault of Hermit's own: the
    /// internal-failure status, the line `HERMIT_INTERNAL_FAILURE
    /// class=choice-internal {expected}`, then the exit hook (installed in the
    /// child), and no refusal line. Returns the record's lines. In the child,
    /// runs `body`, which ends the process; a body that returns lets the child
    /// pass, which fails the parent's check.
    fn failed_in_child(
        name: &str,
        expected: &str,
        body: impl FnOnce(&mut ChoiceSession),
    ) -> Vec<serde_json::Value> {
        const CHILD: &str = "HERMIT_CHOICE_FAULT_CHILD";
        const RECORD: &str = "HERMIT_CHOICE_FAULT_RECORD";
        let test = format!("{}::{name}", module_path!().split_once("::").unwrap().1);
        if std::env::var(CHILD).as_deref() == Ok(test.as_str()) {
            set_choice_internal_exit_hook(Some(print_hook_ran));
            let record = File::create(std::env::var_os(RECORD).unwrap()).unwrap();
            let mut session = ChoiceSession::new(Box::new(TapeSource::new(vec![])), Some(record));
            body(&mut session);
            return Vec::new();
        }
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("record.jsonl");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(CHILD, &test)
            .env(RECORD, &record)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(detcore_model::HERMIT_INTERNAL_FAILURE_EXIT),
            "{test}: the child did not end as an internal failure\n{stderr}"
        );
        let line = format!("HERMIT_INTERNAL_FAILURE class=choice-internal {expected}\n");
        let at = stderr
            .find(&line)
            .unwrap_or_else(|| panic!("{test}: missing {line:?}\n{stderr}"));
        assert!(
            stderr[at + line.len()..].contains(HOOK_RAN),
            "{test}: the exit hook did not run after the fault's line\n{stderr}"
        );
        assert!(
            !stderr.contains("HERMIT_CHOICE_REFUSED"),
            "{test}: a fault of Hermit's own was reported as a refusal\n{stderr}"
        );
        std::fs::read_to_string(&record)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// A draw no boundary announced ends the run as Hermit's own fault, and
    /// the record's last line is that request, refused by the same name.
    #[test]
    fn an_unannounced_draw_ends_the_run_as_an_internal_fault() {
        let reason = "internal: draw without its boundary announcement (request 1, site \
                      priority, thread 3)";
        let lines = failed_in_child(
            "an_unannounced_draw_ends_the_run_as_an_internal_fault",
            &format!("choice 1: {reason}"),
            |session| {
                announced_choose(session, point(4));
                session.choose(point(4));
            },
        );
        assert_eq!(lines.len(), 2, "the answered request, then the refused one");
        assert_eq!(lines[1]["i"], 1);
        assert_eq!(lines[1]["refused"], reason);
        assert!(
            lines[1].get("v").is_none(),
            "a refused request has no answer"
        );
    }

    /// A draw at another site than the one announced ends the run as
    /// Hermit's own fault, naming both.
    #[test]
    fn a_draw_at_another_site_than_announced_ends_the_run_as_an_internal_fault() {
        let reason = "internal: draw is not the one its boundary announced (drawn: request 0, \
                      site priority, thread 3; announced: request 0, site chaos_slice, thread 3)";
        let lines = failed_in_child(
            "a_draw_at_another_site_than_announced_ends_the_run_as_an_internal_fault",
            &format!("choice 0: {reason}"),
            |session| {
                session.announce(DetTid::from_raw(3), ChoiceSite::ChaosSlice);
                session.choose(point(4));
            },
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["refused"], reason);
    }

    /// A draw by another thread than the one announced ends the run as
    /// Hermit's own fault, naming both.
    #[test]
    fn a_draw_by_another_thread_than_announced_ends_the_run_as_an_internal_fault() {
        let reason = "internal: draw is not the one its boundary announced (drawn: request 0, \
                      site priority, thread 3; announced: request 0, site priority, thread 4)";
        let lines = failed_in_child(
            "a_draw_by_another_thread_than_announced_ends_the_run_as_an_internal_fault",
            &format!("choice 0: {reason}"),
            |session| {
                session.announce(DetTid::from_raw(4), ChoiceSite::Priority);
                session.choose(point(4));
            },
        );
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["refused"], reason);
    }

    /// A second announcement before the first one's draw ends the run as
    /// Hermit's own fault. No draw came, so there is no request to record.
    #[test]
    fn an_announcement_without_its_draw_ends_the_run_as_an_internal_fault() {
        let lines = failed_in_child(
            "an_announcement_without_its_draw_ends_the_run_as_an_internal_fault",
            "choice 1: internal: boundary announcement without its draw (announced: request 1, \
             site chaos_slice, thread 3; then thread 3 announced site priority)",
            |session| {
                announced_choose(session, point(4));
                session.announce(DetTid::from_raw(3), ChoiceSite::ChaosSlice);
                session.announce(DetTid::from_raw(3), ChoiceSite::Priority);
            },
        );
        assert_eq!(lines.len(), 1, "only the answered request");
        assert!(lines[0].get("v").is_some());
    }

    /// A session that finishes with an announcement undrawn ends the run as
    /// Hermit's own fault, naming the announcement: no later announcement is
    /// left to catch it. No draw came, so there is no request to record.
    #[test]
    fn a_session_that_finishes_with_an_announcement_undrawn_ends_the_run_as_an_internal_fault() {
        let lines = failed_in_child(
            "a_session_that_finishes_with_an_announcement_undrawn_ends_the_run_as_an_internal_fault",
            "choice 1: internal: boundary announcement without its draw when the run finished \
             (announced: request 1, site chaos_slice, thread 3)",
            |session| {
                announced_choose(session, point(4));
                session.announce(DetTid::from_raw(3), ChoiceSite::ChaosSlice);
                session.finish();
            },
        );
        assert_eq!(lines.len(), 1, "only the answered request");
        assert!(lines[0].get("v").is_some());
    }

    /// A session whose every announcement was drawn finishes and goes on as
    /// it was: finishing checks the announcement and nothing else.
    #[test]
    fn a_session_whose_announcements_were_all_drawn_finishes() {
        let mut session = ChoiceSession::new(Box::new(TapeSource::new(vec![])), None);
        session.finish();
        assert_eq!(announced_choose(&mut session, point(4)), 4);
        session.finish();
        assert_eq!(session.next_index(), 1);
        assert_eq!(undrawn_at_finish(None), None);
    }
}
