/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The rejoin log: the scheduler turn at which each backgrounded call
//! rejoined the run queue during `hermit record`, so that `hermit replay`
//! readmits it at the same turn.
//!
//! A backgrounded call (`BlockingExternalIO`, or an `rt_sigsuspend` outside the
//! scheduler) runs in the host kernel while guest turns continue. Under record,
//! `step2c_process_io_blockers` readmits it at the first scheduler pass that
//! finds it finished, so the turn depends on host timing. Replay serves many
//! of these calls from the recording, and they finish at a different moment,
//! so readmitting them "when finished" can change the schedule
//! (<https://github.com/rrnewton/hermit/pull/3908>). Record therefore logs
//! each readmission, and replay obeys the log instead of host timing.
//!
//! `Scheduler::turn` advances only on a committed or skipped turn, never while
//! a scheduler pass spins, so it names the same point in both runs as long as
//! the two schedules agree up to it. It does not advance when the scheduler,
//! with nothing to run, moves virtual time to the next timer, so each entry
//! also carries `Scheduler::empty_queue_wakes`, the count of those moves.
//! Record may readmit a call either before or after such a move within one
//! turn, and the pair tells replay which.
//!
//! The file, `rejoins` in the recording directory, is text: a header line, then
//! one line per readmission, in order:
//!
//! ```text
//! hermit-rejoins 1
//! <turn> <wakes> <dettid>:<syscall ordinal> [<dettid>:<syscall ordinal> ...]
//! ```
//!
//! The syscall ordinal is the call's `ExternalOpId::sequence`, so replay can
//! check that the parked call is the one the recording readmitted. Each line
//! is written with one `write` as it happens, and a final line without its
//! newline means the recording was cut short; replay refuses such a log.
//!
//! A log that also records a decision point where Hermit decided otherwise
//! than main's code on main's state ([`Difference`]) is headed
//! `hermit-rejoins 2`, which main refuses at load, and holds each such entry
//! as one JSON line among the readmissions, in the same order. The writer
//! promotes the header in place before the first such entry, so a log that
//! records none is byte for byte as above; a `hermit-rejoins 2` log with no
//! such entry is refused (its writer stopped between the two writes).

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;

use super::timed_waiters::SignalTimerId;
use super::timed_waiters::TimedEvent;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;

/// Name of the rejoin log within the recording directory.
pub(crate) const REJOIN_LOG_NAME: &str = "rejoins";

const HEADER: &str = "hermit-rejoins 1";

/// The header of a log holding a [`Difference`]; the same length as
/// [`HEADER`], so that the writer can promote it in place.
const MARKED_HEADER: &str = "hermit-rejoins 2";

/// One entry of the rejoin log, or of a preemption record's readmissions
/// (`PreemptionRecord::rejoins`): a readmission, or a difference from main's
/// decision at a decision point, in one order.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize
)]
#[serde(untagged)]
pub(crate) enum LogEntry {
    Difference(Difference),
    Rejoin(Rejoin),
}

impl LogEntry {
    fn point(&self) -> (u64, u64) {
        match self {
            LogEntry::Difference(difference) => (difference.turn, difference.wakes),
            LogEntry::Rejoin(rejoin) => rejoin.point(),
        }
    }
}

/// A decision point (a committed copy reaching a background call, a
/// background grant, a death decision) where Hermit decided otherwise than
/// main's code on main's state, the oracle, at the position `log_rejoin`
/// gives a readmission, with both sides' decisions. Nothing writes one yet;
/// a log or record holding one marks its run as one main cannot replay.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize
)]
#[serde(deny_unknown_fields)]
pub(crate) struct Difference {
    pub(crate) difference: DifferenceKind,
    pub(crate) turn: u64,
    /// `Scheduler::empty_queue_wakes` at the decision.
    pub(crate) wakes: u64,
    /// Every timed event popped in the turn before the decision, in order.
    pub(crate) popped: Vec<PoppedEvent>,
    /// The thread the decision was about and its call's
    /// `ExternalOpId::sequence`.
    pub(crate) call: (DetTid, u64),
    /// The signal Hermit armed the call with, if any.
    pub(crate) ours: Option<i32>,
    /// The signal the oracle would have armed it with, if any.
    pub(crate) oracle: Option<i32>,
}

/// What Hermit did at a [`Difference`] that the oracle would not.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DifferenceKind {
    /// Armed where the oracle would not, or with another signal.
    Armed,
    /// Did not arm where the oracle would.
    Declined,
    /// Refused the run, by the named refusal.
    Refused(String),
    /// Engaged a death handoff of the named kind.
    Held(String),
    /// Raised a shield the oracle would not.
    Shielded,
}

impl Difference {
    /// Whether the two decisions are signals, and say what the kind says,
    /// and a refusal's or a handoff's name is one.
    fn check(&self) -> Result<(), String> {
        let signal = |decision: Option<i32>| decision.is_none_or(|s| (1..=64).contains(&s));
        let named = |name: &str| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
        };
        let consistent = match &self.difference {
            DifferenceKind::Armed => self.ours.is_some() && self.ours != self.oracle,
            DifferenceKind::Declined => self.ours.is_none() && self.oracle.is_some(),
            DifferenceKind::Refused(name) => named(name) && self.ours.is_none(),
            DifferenceKind::Held(kind) => named(kind),
            DifferenceKind::Shielded => true,
        };
        if signal(self.ours) && signal(self.oracle) && consistent {
            Ok(())
        } else {
            Err(format!(
                "a {:?} difference at turn {} records the decisions {:?} (Hermit) and {:?} \
                 (main)",
                self.difference, self.turn, self.ours, self.oracle
            ))
        }
    }
}

/// One readmission: the calls `step2c_process_io_blockers` readmitted together,
/// in `DetTid` order, and the point at which it did.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize
)]
pub(crate) struct Rejoin {
    pub(crate) turn: u64,
    /// `Scheduler::empty_queue_wakes` at the readmission.
    pub(crate) wakes: u64,
    /// Each readmitted thread and the `ExternalOpId::sequence` of its call.
    pub(crate) calls: Vec<(DetTid, u64)>,
    /// The timed events the scheduler popped in this turn before the
    /// readmission, in order, each with the deadline it popped at
    /// (`Scheduler::note_timed_pop`), in a `hermit run` preemption record. A
    /// schedule replay readmits at the recorded turn once it has made these
    /// pops, by whatever step, rather than at the recorded wake count, which
    /// its lagging virtual time can reach by another step
    /// (`Scheduler::rejoin_place`). `None` in `hermit record`'s text log,
    /// which does not carry it, and in records written before the field
    /// existed, which replay by the wake count as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) popped: Option<Vec<PoppedEvent>>,
}

/// A timed event, as a readmission names one it followed
/// ([`Rejoin::popped`]): the thread whose timed wait ended, or the timer that
/// fired.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize
)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TimedEventId {
    /// A thread's timed wait: a sleep, or a wait with a timeout.
    Thread(DetTid),
    /// A process's `ITIMER_REAL` (`alarm`, `setitimer`).
    Alarm(DetPid),
    /// A process's POSIX timer, by its timer ID.
    Posix(DetPid, i32),
    /// The `SIGCHLD` of a child's exit, to its parent.
    ChildExit { child: DetPid, parent: DetPid },
}

impl From<TimedEvent> for TimedEventId {
    fn from(event: TimedEvent) -> Self {
        match event {
            TimedEvent::ThreadEvt(tid) => TimedEventId::Thread(tid),
            TimedEvent::SignalEvt(SignalTimerId::Alarm(pid), _, _) => TimedEventId::Alarm(pid),
            TimedEvent::SignalEvt(SignalTimerId::Posix(pid, timer), _, _) => {
                TimedEventId::Posix(pid, timer)
            }
            TimedEvent::SignalEvt(SignalTimerId::ChildExit { child, parent }, _, _) => {
                TimedEventId::ChildExit { child, parent }
            }
        }
    }
}

/// One pop of a timed event: the event, and the deadline it popped at. A
/// replay has made it once that event has no pending entry due at or before
/// that deadline (`Scheduler::pop_made`). A periodic timer fires again under
/// the same name, and how many of its periods fire depends on virtual time
/// (a late timer skips the periods it missed), so the deadline, not a count
/// of pops, tells which firing it was.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize
)]
pub(crate) struct PoppedEvent {
    pub(crate) event: TimedEventId,
    pub(crate) deadline: LogicalTime,
}

impl Rejoin {
    /// Where the readmission happened; both parts only grow during a run.
    pub(crate) fn point(&self) -> (u64, u64) {
        (self.turn, self.wakes)
    }

    fn to_line(&self) -> String {
        let mut line = format!("{} {}", self.turn, self.wakes);
        for (dtid, sequence) in &self.calls {
            line.push_str(&format!(" {}:{}", dtid.as_raw(), sequence));
        }
        line.push('\n');
        line
    }

    fn parse(line: &str) -> Result<Self, String> {
        let mut words = line.split(' ');
        let turn = words
            .next()
            .and_then(|word| word.parse::<u64>().ok())
            .ok_or_else(|| format!("bad turn in {line:?}"))?;
        let wakes = words
            .next()
            .and_then(|word| word.parse::<u64>().ok())
            .ok_or_else(|| format!("bad wake count in {line:?}"))?;
        let mut calls: Vec<(DetTid, u64)> = Vec::new();
        for word in words {
            let call = word
                .split_once(':')
                .and_then(|(dtid, sequence)| {
                    Some((
                        DetTid::from_raw(dtid.parse::<i32>().ok()?),
                        sequence.parse::<u64>().ok()?,
                    ))
                })
                .ok_or_else(|| format!("bad call {word:?} in {line:?}"))?;
            if calls.last().is_some_and(|(last, _)| *last >= call.0) {
                return Err(format!("threads out of order in {line:?}"));
            }
            calls.push(call);
        }
        if calls.is_empty() {
            return Err(format!("no calls in {line:?}"));
        }
        Ok(Rejoin {
            turn,
            wakes,
            calls,
            popped: None,
        })
    }
}

/// Record side: appends each readmission to the log as it happens.
#[derive(Debug)]
pub(crate) struct RejoinWriter {
    file: File,
    /// Whether the header says the log holds a [`Difference`].
    marked: bool,
}

impl RejoinWriter {
    /// Create the log in `dir`, header included, so that a recording with no
    /// readmissions still has one.
    pub(crate) fn create(dir: &Path) -> io::Result<Self> {
        let mut file = File::create(dir.join(REJOIN_LOG_NAME))?;
        file.write_all(format!("{HEADER}\n").as_bytes())?;
        Ok(RejoinWriter {
            file,
            marked: false,
        })
    }

    /// Append one readmission with a single write.
    pub(crate) fn append(&mut self, rejoin: &Rejoin) -> io::Result<()> {
        self.file.write_all(rejoin.to_line().as_bytes())
    }

    /// Append one difference with a single write, after promoting the header.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn append_difference(&mut self, difference: &Difference) -> io::Result<()> {
        self.promote()?;
        let mut line = serde_json::to_string(difference).map_err(io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())
    }

    /// Overwrite the header with [`MARKED_HEADER`] in place, once, and flush
    /// it before any difference follows.
    #[cfg_attr(not(test), allow(dead_code))]
    fn promote(&mut self) -> io::Result<()> {
        if !self.marked {
            std::os::unix::fs::FileExt::write_all_at(&self.file, MARKED_HEADER.as_bytes(), 0)?;
            self.file.sync_data()?;
            self.marked = true;
        }
        Ok(())
    }
}

/// Whether `entries` is a log a replay can follow: its entries must be in
/// the order the recording made them, since replay walks them once, and each
/// difference must state its decisions as its kind says.
pub(crate) fn ordered(entries: &[LogEntry]) -> Result<(), String> {
    for pair in entries.windows(2) {
        if pair[0].point() > pair[1].point() {
            return Err(format!(
                "readmissions out of order: turn {} comes after turn {}",
                pair[1].point().0,
                pair[0].point().0
            ));
        }
    }
    entries.iter().try_for_each(|entry| match entry {
        LogEntry::Difference(difference) => difference.check(),
        LogEntry::Rejoin(_) => Ok(()),
    })
}

/// The readmissions of `entries`, for a replay to follow. A difference is
/// refused: this Hermit makes no decision of those kinds to compare with it.
pub(crate) fn readmissions(
    entries: impl IntoIterator<Item = LogEntry>,
) -> Result<Vec<Rejoin>, String> {
    entries
        .into_iter()
        .map(|entry| match entry {
            LogEntry::Rejoin(rejoin) => Ok(rejoin),
            LogEntry::Difference(difference) => Err(format!(
                "it records a decision at turn {} where Hermit decided otherwise than main \
                 ({:?}), which this Hermit cannot replay",
                difference.turn, difference.difference
            )),
        })
        .collect()
}

/// Replay side: read the readmissions the recording in `dir` logged.
pub(crate) fn read(dir: &Path) -> Result<VecDeque<Rejoin>, String> {
    readmissions(read_log(dir)?).map(VecDeque::from)
}

/// Read every entry of the log the recording in `dir` wrote.
pub(crate) fn read_log(dir: &Path) -> Result<VecDeque<LogEntry>, String> {
    let path = dir.join(REJOIN_LOG_NAME);
    let text =
        std::fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    parse(&text)
}

fn parse(text: &str) -> Result<VecDeque<LogEntry>, String> {
    let mut lines = text.split_inclusive('\n');
    let header = lines.next();
    let marked = header == Some(&format!("{MARKED_HEADER}\n"));
    if !marked && header != Some(&format!("{HEADER}\n")) {
        return Err(format!("the log does not start with {HEADER:?}"));
    }
    let mut entries: VecDeque<LogEntry> = VecDeque::new();
    for line in lines {
        let line = line
            .strip_suffix('\n')
            .ok_or_else(|| format!("the last line, {line:?}, is incomplete"))?;
        let entry = if marked && line.starts_with('{') {
            let difference: Difference = serde_json::from_str(line)
                .map_err(|err| format!("bad difference {line:?}: {err}"))?;
            difference.check()?;
            LogEntry::Difference(difference)
        } else {
            LogEntry::Rejoin(Rejoin::parse(line)?)
        };
        if entries
            .back()
            .is_some_and(|last| last.point() > entry.point())
        {
            return Err(format!("readmissions out of order at {line:?}"));
        }
        entries.push_back(entry);
    }
    if marked
        && !entries
            .iter()
            .any(|entry| matches!(entry, LogEntry::Difference(_)))
    {
        return Err(format!(
            "the log starts with {MARKED_HEADER:?} but records no difference: its recording \
             stopped before writing one"
        ));
    }
    Ok(entries)
}

/// Replay could not follow the rejoin log, so it would readmit a backgrounded
/// call at a point the recording did not; the scheduler loop ends the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RejoinRefusal {
    pub(crate) reason: String,
}

impl std::fmt::Display for RejoinRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hermit replay refused to continue: {}, so a backgrounded call would rejoin \
             the schedule at a different point than in the recording",
            self.reason
        )
    }
}

impl std::error::Error for RejoinRefusal {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejoin(turn: u64, wakes: u64, calls: &[(i32, u64)]) -> Rejoin {
        Rejoin {
            turn,
            wakes,
            calls: calls
                .iter()
                .map(|(dtid, sequence)| (DetTid::from_raw(*dtid), *sequence))
                .collect(),
            popped: None,
        }
    }

    #[test]
    fn written_log_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = RejoinWriter::create(dir.path()).unwrap();
        assert_eq!(read(dir.path()).unwrap(), VecDeque::new());
        let written = vec![
            rejoin(4, 0, &[(3, 17)]),
            rejoin(4, 1, &[(3, 19), (5, 2)]),
            rejoin(6, 1, &[(5, 3)]),
        ];
        for entry in &written {
            writer.append(entry).unwrap();
        }
        assert_eq!(read(dir.path()).unwrap(), VecDeque::from(written));
    }

    #[test]
    fn malformed_logs_are_refused() {
        let missing = tempfile::tempdir().unwrap();
        assert!(read(missing.path()).is_err());
        for text in [
            "",
            "hermit-rejoins 2\n",
            "hermit-rejoins 1\n7 0 3:4",
            "hermit-rejoins 1\n7 0\n",
            "hermit-rejoins 1\n7 3:4\n",
            "hermit-rejoins 1\n7 0 3\n",
            "hermit-rejoins 1\n7 0 5:1 3:4\n",
            "hermit-rejoins 1\n7 0 3:4\n6 0 3:5\n",
            "hermit-rejoins 1\n7 1 3:4\n7 0 3:5\n",
        ] {
            assert!(parse(text).is_err(), "{text:?}");
        }
    }

    fn difference(
        kind: DifferenceKind,
        turn: u64,
        ours: Option<i32>,
        oracle: Option<i32>,
    ) -> Difference {
        Difference {
            difference: kind,
            turn,
            wakes: 1,
            popped: vec![PoppedEvent {
                event: TimedEventId::Posix(DetPid::from_raw(3), 2),
                deadline: LogicalTime::from_nanos(4_000),
            }],
            call: (DetTid::from_raw(5), 6),
            ours,
            oracle,
        }
    }

    /// A log is main's bytes until its first difference, which promotes the
    /// header in place; every kind of difference then reads back in order
    /// with the readmissions, while a replay of its readmissions refuses it.
    #[test]
    fn differences_promote_the_header_and_read_back_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = RejoinWriter::create(dir.path()).unwrap();
        writer.append(&rejoin(4, 0, &[(3, 17)])).unwrap();
        let path = dir.path().join(REJOIN_LOG_NAME);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "hermit-rejoins 1\n4 0 3:17\n"
        );
        let written = vec![
            LogEntry::Rejoin(rejoin(4, 0, &[(3, 17)])),
            LogEntry::Difference(difference(DifferenceKind::Armed, 4, Some(10), None)),
            LogEntry::Difference(difference(DifferenceKind::Declined, 5, None, Some(17))),
            LogEntry::Rejoin(rejoin(5, 1, &[(5, 6)])),
            LogEntry::Difference(difference(
                DifferenceKind::Refused("extio-unknown-mask".to_owned()),
                6,
                None,
                Some(10),
            )),
            LogEntry::Difference(difference(
                DifferenceKind::Held("settle".to_owned()),
                6,
                None,
                None,
            )),
            LogEntry::Difference(difference(DifferenceKind::Shielded, 7, Some(10), Some(10))),
        ];
        for entry in &written[1..] {
            match entry {
                LogEntry::Rejoin(rejoin) => writer.append(rejoin).unwrap(),
                LogEntry::Difference(difference) => writer.append_difference(difference).unwrap(),
            }
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("hermit-rejoins 2\n4 0 3:17\n{"), "{text}");
        assert_eq!(read_log(dir.path()).unwrap(), VecDeque::from(written));
        assert!(read(dir.path()).unwrap_err().contains("(Armed)"));
    }

    /// A writer stopped between promoting the header and writing the first
    /// difference leaves a `hermit-rejoins 2` log with no difference, which
    /// is refused.
    #[test]
    fn a_promoted_header_without_a_difference_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = RejoinWriter::create(dir.path()).unwrap();
        writer.append(&rejoin(4, 0, &[(3, 17)])).unwrap();
        writer.promote().unwrap();
        assert!(
            read_log(dir.path())
                .unwrap_err()
                .contains("records no difference")
        );
    }

    /// A `hermit-rejoins 2` log reads its differences among its
    /// readmissions; a difference must be well formed, in order, and under
    /// that header.
    #[test]
    fn a_marked_log_needs_a_well_formed_difference() {
        let line = |kind: &str, ours: &str, oracle: &str| {
            format!(
                r#"{{"difference":{kind},"turn":7,"wakes":0,"popped":[],"call":[3,4],"ours":{ours},"oracle":{oracle}}}"#
            )
        };
        let good = line(r#""armed""#, "10", "null");
        let named = line(r#"{"refused":"extio-x"}"#, "null", "10");
        assert_eq!(
            parse(&format!(
                "hermit-rejoins 2\n7 0 3:4\n{good}\n{named}\n8 0 3:5\n"
            ))
            .map(|entries| entries.len()),
            Ok(4)
        );
        for bad in [
            format!("hermit-rejoins 1\n{good}\n"),
            format!("hermit-rejoins 2\n{good}"),
            format!("hermit-rejoins 2\n8 0 3:4\n{good}\n"),
            line(r#""armed""#, "10", "10"),
            line(r#""armed""#, "null", "10"),
            line(r#""declined""#, "10", "12"),
            line(r#"{"refused":"extio-x"}"#, "10", "null"),
            line(r#""refused""#, "null", "null"),
            line(r#"{"held":"Settle"}"#, "null", "null"),
            line(r#"{"held":""}"#, "null", "null"),
            line(r#""rearmed""#, "10", "null"),
            line(r#""armed""#, "65", "null"),
            line(r#""shielded""#, "0", "null"),
            good.replace(r#""wakes":0,"#, ""),
            good.replace('}', r#","calls":[]}"#),
        ]
        .map(|bad| {
            if bad.starts_with('{') {
                format!("hermit-rejoins 2\n{bad}\n")
            } else {
                bad
            }
        }) {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
    }
}
