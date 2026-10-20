/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The exit-order log (study branch): each hold and each release of an exit
//! by the virtual-completion exit order (`exit_order`) during `hermit record`,
//! so that `hermit replay` holds and releases the same exits at the same
//! points instead of deciding again.
//!
//! The decisions read only committed scheduler state, so a replay whose
//! schedule agrees with the recording reaches the same ones. Logging them
//! makes that a checked fact rather than an assumption, as the rejoin log
//! does for readmissions (`rejoin_log`): replay follows the log, and refuses a
//! logged point it has already passed.
//!
//! Each entry is keyed by `Scheduler::turn` and `Scheduler::empty_queue_wakes`,
//! as a rejoin is. The file, `exit-holds` in the recording directory, is text:
//! a header line, then one line per event, in order:
//!
//! ```text
//! hermit-exit-holds 1
//! <turn> <wakes> hold <dettid>
//! <turn> <wakes> release <dettid>
//! ```
//!
//! Each line is written with one `write` as it happens; a final line without
//! its newline means the recording was cut short, and replay refuses it.

use std::collections::VecDeque;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;

use crate::types::DetTid;

/// Name of the exit-order log within the recording directory.
pub(crate) const EXIT_ORDER_LOG_NAME: &str = "exit-holds";

const HEADER: &str = "hermit-exit-holds 1";

/// What the exit order did to an exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExitOrderEvent {
    /// The exit was held at its `Exit` request.
    Hold,
    /// A held exit was released at a step-2 pass.
    Release,
}

impl ExitOrderEvent {
    fn word(self) -> &'static str {
        match self {
            ExitOrderEvent::Hold => "hold",
            ExitOrderEvent::Release => "release",
        }
    }
}

/// One logged decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExitOrderEntry {
    pub(crate) turn: u64,
    /// `Scheduler::empty_queue_wakes` at the decision.
    pub(crate) wakes: u64,
    pub(crate) event: ExitOrderEvent,
    /// The thread whose exit was held or released.
    pub(crate) tid: DetTid,
}

impl ExitOrderEntry {
    /// Where the decision happened; both parts only grow during a run.
    pub(crate) fn point(&self) -> (u64, u64) {
        (self.turn, self.wakes)
    }

    fn to_line(self) -> String {
        format!(
            "{} {} {} {}\n",
            self.turn,
            self.wakes,
            self.event.word(),
            self.tid.as_raw()
        )
    }

    fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split(' ').collect();
        let [turn, wakes, event, tid] = words[..] else {
            return Err(format!("expected four fields in {line:?}"));
        };
        let number = |word: &str, what: &str| {
            word.parse::<u64>()
                .map_err(|_| format!("bad {what} in {line:?}"))
        };
        let event = match event {
            "hold" => ExitOrderEvent::Hold,
            "release" => ExitOrderEvent::Release,
            _ => return Err(format!("bad event in {line:?}")),
        };
        let tid = tid
            .parse::<i32>()
            .map(DetTid::from_raw)
            .map_err(|_| format!("bad thread in {line:?}"))?;
        Ok(ExitOrderEntry {
            turn: number(turn, "turn")?,
            wakes: number(wakes, "wake count")?,
            event,
            tid,
        })
    }
}

/// Record side: appends each decision to the log as it happens.
#[derive(Debug)]
pub(crate) struct ExitOrderWriter {
    file: File,
}

impl ExitOrderWriter {
    /// Create the log in `dir`, header included, so that a recording with no
    /// decisions still has one.
    pub(crate) fn create(dir: &Path) -> io::Result<Self> {
        let mut file = File::create(dir.join(EXIT_ORDER_LOG_NAME))?;
        file.write_all(format!("{HEADER}\n").as_bytes())?;
        Ok(ExitOrderWriter { file })
    }

    /// Append one decision with a single write.
    pub(crate) fn append(&mut self, entry: ExitOrderEntry) -> io::Result<()> {
        self.file.write_all(entry.to_line().as_bytes())
    }
}

/// Replay side: the whole log, in order. `None` when the recording has none.
pub(crate) fn read(dir: &Path) -> Result<Option<VecDeque<ExitOrderEntry>>, String> {
    let path = dir.join(EXIT_ORDER_LOG_NAME);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text).map(Some),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

fn parse(text: &str) -> Result<VecDeque<ExitOrderEntry>, String> {
    let Some(body) = text.strip_prefix(HEADER).and_then(|s| s.strip_prefix('\n')) else {
        return Err("missing exit-order log header".to_string());
    };
    if !body.is_empty() && !body.ends_with('\n') {
        return Err("the exit-order log ends with a partial line".to_string());
    }
    let mut entries = VecDeque::new();
    let mut last = (0, 0);
    for line in body.lines() {
        let entry = ExitOrderEntry::parse(line)?;
        if entry.point() < last {
            return Err(format!("entries out of order at {line:?}"));
        }
        last = entry.point();
        entries.push_back(entry);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(turn: u64, wakes: u64, event: ExitOrderEvent, tid: i32) -> ExitOrderEntry {
        ExitOrderEntry {
            turn,
            wakes,
            event,
            tid: DetTid::from_raw(tid),
        }
    }

    #[test]
    fn written_log_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = ExitOrderWriter::create(dir.path()).unwrap();
        let entries = [
            entry(7, 0, ExitOrderEvent::Hold, 21),
            entry(9, 1, ExitOrderEvent::Release, 21),
        ];
        for e in entries {
            writer.append(e).unwrap();
        }
        drop(writer);
        assert_eq!(
            read(dir.path()).unwrap(),
            Some(VecDeque::from(entries.to_vec()))
        );
    }

    #[test]
    fn a_missing_log_is_none_and_malformed_logs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);
        for bad in [
            "",
            "hermit-exit-holds 2\n",
            "hermit-exit-holds 1\n7 0 hold 21",
            "hermit-exit-holds 1\n7 0 park 21\n",
            "hermit-exit-holds 1\n7 0 hold\n",
            "hermit-exit-holds 1\n9 0 hold 21\n7 0 release 21\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }
}
