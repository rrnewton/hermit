/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Parser;
use colored::Colorize;
use detcore::edit_distance::iterable_bubble_sort;
use detcore::preemptions::HistoryIdentity;
use detcore::preemptions::PreemptionRecord;
use detcore::preemptions::boundary_recording::Recording;
use detcore_model::pid::DetTid;
use detcore_model::schedule::SchedEvent;
use detcore_model::schedule::boundary::Record;
use detcore_model::schedule::boundary::Schedule;

use crate::CommonOpts;

#[derive(Debug, Parser)]
pub enum SchedTraceOpts {
    /// Inspect and report statistics on one schedule.
    Inspect(InspectOpts),

    /// Just dump the raw schedule
    Print(PrintOpts),

    /// Difference between two preempt files
    Diff(DiffOpts),

    /// Run the interpolation between two preempt files
    Interpolate(InterpolateOpts),
}

impl SchedTraceOpts {
    pub fn main(&self, common_args: &CommonOpts) -> anyhow::Result<bool> {
        match self {
            SchedTraceOpts::Diff(x) => x.run(common_args),
            SchedTraceOpts::Interpolate(x) => x.run(common_args),
            SchedTraceOpts::Inspect(x) => x.run(common_args),
            SchedTraceOpts::Print(x) => x.run(common_args),
        }
    }
}

#[derive(Debug, Parser)]
pub struct DiffOpts {
    /// First schedule file to compare.
    file_a: PathBuf,
    /// Second schedule file to compare.
    file_b: PathBuf,
}

impl DiffOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let first_schedule = legacy_schedule(&self.file_a, "schedule diff/interpolation")?;
        let second_schedule = legacy_schedule(&self.file_b, "schedule diff/interpolation")?;
        println!("first schedule length: {}", first_schedule.len());
        println!("second schedule length: {}", second_schedule.len());
        let bubbles = iterable_bubble_sort(&first_schedule, &second_schedule);
        eprintln!(
            ":: {}",
            format!(
                "Diffs found in schedules : Swap distance  = {}, Edit distance = {} ",
                bubbles.swap_distance(),
                bubbles.edit_distance()
            )
            .yellow()
            .bold()
        );
        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct InspectOpts {
    /// Schedule to inspect.
    pub sched_file: PathBuf,

    /// Print O(N) output proportional to the length of the schedule.
    #[clap(long, short = 'v')]
    pub verbose: bool,
}

/// Return all events which are right before a preemption.
fn preempt_events(sched: &[SchedEvent]) -> Vec<SchedEvent> {
    let mut acc = Vec::new();
    for ix in 0..sched.len().saturating_sub(1) {
        if sched[ix].dettid != sched[ix + 1].dettid {
            acc.push(sched[ix].clone());
        }
    }
    acc
}

/// Split apart an event series into a per-thread record.
fn per_thread_events(events: &Vec<SchedEvent>) -> BTreeMap<DetTid, Vec<SchedEvent>> {
    let mut acc = BTreeMap::new();
    for evt in events {
        let vec: &mut Vec<SchedEvent> = acc.entry(evt.dettid).or_default();
        vec.push(evt.clone());
    }
    acc
}

impl InspectOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let schedule = match Recording::read(&self.sched_file)? {
            Recording::Legacy(record) => record.into_global(),
            Recording::Boundaries(record) => {
                print!("{}", inspect_boundaries(record.schedule(), self.verbose)?);
                return Ok(true);
            }
        };
        println!("Schedule length: {}", schedule.len());

        let preempts = preempt_events(&schedule);
        println!("Context-switch/preemption events: {}", preempts.len());
        println!("Per thread pre-preemption events:");
        let per_thread_preempts = per_thread_events(&preempts);
        for (tid, vec) in &per_thread_preempts {
            println!("  tid {}: {} preemptions", tid, vec.len());
            if self.verbose {
                for evt in vec {
                    println!("    {}", evt);
                }
            }
        }

        if self.verbose {
            println!("Full context switch event list in order, with before/after events:");
            let mut count = 0;
            for ix in 0..schedule.len().saturating_sub(1) {
                if schedule[ix].dettid != schedule[ix + 1].dettid {
                    count += 1;
                    println!(" {: >2}: {} => {}", count, schedule[ix], schedule[ix + 1]);
                }
            }
        }

        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct PrintOpts {
    /// Schedule to inspect.
    pub sched_file: PathBuf,

    /// Print the indices as well as the raw events.
    #[clap(long, short = 'i')]
    indices: bool,

    /// Print only events from the given tid, leaving a blank line anywhere the tid was preempted.
    #[clap(long)]
    tid: Option<i32>,

    /// Strip times before printing.
    #[clap(long)]
    strip_times: bool,
}

impl PrintOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let mut recording = Recording::read(&self.sched_file)?;
        if self.strip_times {
            recording.strip_times();
        }
        let schedule = match recording {
            Recording::Legacy(record) => record.into_global(),
            Recording::Boundaries(record) => {
                print!(
                    "{}",
                    print_boundaries(record.schedule(), self.indices, self.tid)?
                );
                return Ok(true);
            }
        };
        let mut last_skipped = false;
        let mut thread_counts: BTreeMap<DetTid, u64> = BTreeMap::new();
        for (global_ix, ev) in schedule.iter().enumerate() {
            let entry = thread_counts.entry(ev.dettid).or_default();
            let current_thread_ix: u64 = *entry;
            *entry += 1;

            if let Some(tid) = self.tid {
                if ev.dettid.as_raw() == tid {
                    last_skipped = false;
                } else {
                    if !last_skipped {
                        println!();
                    }
                    last_skipped = true;
                    continue;
                }
            }

            let mut ev = ev.clone();
            if self.strip_times {
                ev.end_time = None;
            }

            if self.indices {
                println!(
                    "{: <9} {}",
                    format!("({},{})", global_ix, current_thread_ix),
                    ev
                );
            } else {
                println!("{}", ev);
            }
        }
        Ok(true)
    }
}

#[derive(Debug, Parser)]
pub struct InterpolateOpts {
    /// Interpolate file a and b by how much percentage. Currently, it takes an integer between 0 and 100.
    interpolate: u8,

    /// First log to compare.
    file_a: PathBuf,

    /// Second log to compare.
    file_b: PathBuf,
}

/// What interpolation reads from its two inputs. Each path is read once; the
/// settings, identity and events all come from those same bytes, so replacing
/// a file afterwards cannot change what is written.
struct InterpolationInputs {
    latency: Option<u64>,
    copy: Option<bool>,
    identity: Option<HistoryIdentity>,
    first_schedule: Vec<SchedEvent>,
    second_schedule: Vec<SchedEvent>,
}

impl InterpolateOpts {
    pub fn run(&self, _common_args: &CommonOpts) -> anyhow::Result<bool> {
        let inputs = self.read_inputs()?;
        // let i = 0;
        let dir = tempfile::Builder::new()
            .prefix("hermit_internal")
            .tempdir()?;
        let tmpdir_path = dir.keep();
        eprintln!(":: Temp workspace: {}", tmpdir_path.display());
        self.write_interpolation(inputs, &tmpdir_path);
        Ok(true)
    }

    /// Reads each input once and checks, in order, that their
    /// `--pipe-wake-latency`, signal-state copy and RCB coordinate identity
    /// agree, and that both are legacy schedules.
    fn read_inputs(&self) -> anyhow::Result<InterpolationInputs> {
        let read = |path: &std::path::Path| {
            std::fs::read(path).map_err(|e| {
                anyhow::anyhow!("cannot read preemption record {}: {}", path.display(), e)
            })
        };
        let (bytes_a, bytes_b) = (read(&self.file_a)?, read(&self.file_b)?);
        let a = (self.file_a.as_path(), bytes_a.as_slice());
        let b = (self.file_b.as_path(), bytes_b.as_slice());
        let latency = Self::common_pipe_wake_latency(a, b)?;
        let copy = Self::common_signal_state_copy(a, b)?;
        let first = Recording::decode_from(a.0, a.1)?;
        let second = Recording::decode_from(b.0, b.1)?;
        let identity = Self::common_identity((a.0, &first), (b.0, &second))?;
        let operation = "schedule diff/interpolation";
        Ok(InterpolationInputs {
            latency,
            copy,
            identity,
            first_schedule: first.into_legacy(operation)?.into_global(),
            second_schedule: second.into_legacy(operation)?.into_global(),
        })
    }

    /// Writes the interpolation of `inputs` into `dir` and returns its path.
    fn write_interpolation(
        &self,
        inputs: InterpolationInputs,
        dir: &std::path::Path,
    ) -> std::path::PathBuf {
        let (_swap_dist, _edit_dist, requested_schedule) = {
            let mut bubbles = iterable_bubble_sort(&inputs.first_schedule, &inputs.second_schedule);
            (
                bubbles.swap_distance(),
                bubbles.edit_distance(),
                bubbles
                    .interpolate(self.interpolate)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        let sched_path = dir.join(format!("interpolate_{}.events", self.interpolate));
        let next_sched = PreemptionRecord::from_sched_events(requested_schedule, inputs.identity)
            .with_pipe_wake_latency(inputs.latency)
            .with_signal_state_copy(inputs.copy);
        next_sched.write_to_disk(&sched_path).unwrap();
        sched_path
    }

    /// The `--pipe-wake-latency` both schedules were recorded under, which
    /// the interpolated schedule carries; schedules recorded under different
    /// values are refused (<https://github.com/rrnewton/hermit/issues/4380>).
    /// Each input is a path and the bytes read from it.
    fn common_pipe_wake_latency(
        (file_a, bytes_a): (&std::path::Path, &[u8]),
        (file_b, bytes_b): (&std::path::Path, &[u8]),
    ) -> anyhow::Result<Option<u64>> {
        let read = |path: &std::path::Path, bytes: &[u8]| {
            detcore::preemptions::recorded_pipe_wake_latency_in(path, bytes)
                .map_err(anyhow::Error::msg)
        };
        let (a, b) = (read(file_a, bytes_a)?, read(file_b, bytes_b)?);
        if a != b {
            anyhow::bail!(
                "{} and {} were recorded under different --pipe-wake-latency values ({:?} and \
                 {:?}); no interpolation of them can be replayed",
                file_a.display(),
                file_b.display(),
                a,
                b
            );
        }
        Ok(a)
    }

    /// The RCB coordinate identity both schedules state, which the
    /// interpolated schedule carries unchanged; schedules stating different
    /// identities are refused. Each input is a path and its decoded recording.
    fn common_identity(
        (file_a, a): (&std::path::Path, &Recording),
        (file_b, b): (&std::path::Path, &Recording),
    ) -> anyhow::Result<Option<HistoryIdentity>> {
        detcore::clock_identity::combined_identity(
            (&file_a.display().to_string(), a.identity()),
            (&file_b.display().to_string(), b.identity()),
        )
        .map_err(anyhow::Error::msg)
    }

    /// The signal-state copy setting the interpolated schedule carries: the
    /// one the schedules state, where they agree or only one states it.
    /// Schedules recorded under different settings are refused. Each input
    /// is a path and the bytes read from it.
    fn common_signal_state_copy(
        (file_a, bytes_a): (&std::path::Path, &[u8]),
        (file_b, bytes_b): (&std::path::Path, &[u8]),
    ) -> anyhow::Result<Option<bool>> {
        let read = |path: &std::path::Path, bytes: &[u8]| {
            detcore::preemptions::recorded_signal_state_copy_in(path, bytes)
                .map_err(anyhow::Error::msg)
        };
        match (read(file_a, bytes_a)?, read(file_b, bytes_b)?) {
            (Some(a), Some(b)) if a != b => anyhow::bail!(
                "{} and {} were recorded with the signal-state copy {} and {}; no \
                 interpolation of them can be replayed",
                file_a.display(),
                file_b.display(),
                if a { "on" } else { "off" },
                if b { "on" } else { "off" },
            ),
            (a, b) => Ok(a.or(b)),
        }
    }
}

fn legacy_schedule(
    path: &std::path::Path,
    operation: &'static str,
) -> anyhow::Result<Vec<SchedEvent>> {
    Ok(Recording::read(path)?.into_legacy(operation)?.into_global())
}

fn record_tid(record: &Record) -> DetTid {
    match record {
        Record::Event(event) => event.dettid,
        Record::TimesliceEnd(boundary) => boundary.identity.owner.tid,
    }
}

// Inspection is read-only and names stored indices in the complete stream.
// It never projects away markers to pass them to a legacy editor.
fn inspect_boundaries(schedule: &Schedule, verbose: bool) -> anyhow::Result<String> {
    use std::fmt::Write;
    let records = schedule.records();
    let boundaries = records
        .iter()
        .filter(|r| matches!(r, Record::TimesliceEnd(_)))
        .count();
    let switches = records
        .windows(2)
        .filter(|p| record_tid(&p[0]) != record_tid(&p[1]))
        .count();
    let mut result = format!(
        "Slice-boundary schedule: {} stored records, {} ordinary events, {} slice boundaries\nContext-switch endpoints: {}\n",
        records.len(),
        records.len() - boundaries,
        boundaries,
        switches,
    );
    if verbose {
        for (index, record) in records.iter().enumerate() {
            writeln!(result, "{index}: {}", serde_json::to_string(record)?)?;
        }
    }
    Ok(result)
}

fn print_boundaries(
    schedule: &Schedule,
    indices: bool,
    tid: Option<i32>,
) -> anyhow::Result<String> {
    use std::fmt::Write;
    let mut result = String::new();
    let mut counts = BTreeMap::<DetTid, u64>::new();
    let mut last_skipped = false;
    for (index, record) in schedule.records().iter().enumerate() {
        let owner = record_tid(record);
        let count = counts.entry(owner).or_default();
        let thread_index = *count;
        *count += 1;
        if tid.is_some_and(|tid| tid != owner.as_raw()) {
            if !last_skipped {
                result.push('\n');
            }
            last_skipped = true;
            continue;
        }
        last_skipped = false;
        if indices {
            write!(result, "{: <9} ", format!("({index},{thread_index})"))?;
        }
        writeln!(result, "{}", serde_json::to_string(record)?)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_inspection_keeps_every_record_index_and_editors_refuse_the_format() {
        use detcore::preemptions::boundary_recording::BoundaryRecording;
        use detcore_model::schedule::boundary::Boundary;
        use detcore_model::schedule::boundary::Cause;
        use detcore_model::schedule::boundary::Entry;
        use detcore_model::schedule::boundary::Identity;
        use detcore_model::schedule::boundary::Owner;
        use detcore_model::schedule::boundary::Phase;
        let tid = DetTid::from_raw(3);
        let owner = Owner {
            tid,
            thread_generation: 0,
            birth_tid: tid,
            birth_generation: 0,
            address_space_creator: tid,
            address_space_generation: 0,
        };
        let schedule = Schedule::new(vec![
            Record::Event(SchedEvent::branches(tid, 3)),
            Record::TimesliceEnd(Boundary {
                identity: Identity {
                    owner,
                    cause: Cause::SoftwareCheckin,
                    phase: Phase::Prehook,
                    entry: Entry::Cpuid,
                    committed_rcbs: 3,
                    checkin: 1,
                    slice: 1,
                },
                count: 1,
                start_rip: None,
                end_rip: None,
                end_time: Some(detcore_model::time::LogicalTime::from_nanos(19)),
            }),
            Record::Event(SchedEvent::branches(DetTid::from_raw(4), 2)),
        ])
        .unwrap();
        let inspection = inspect_boundaries(&schedule, true).unwrap();
        assert!(inspection.contains("3 stored records, 2 ordinary events, 1 slice boundaries"));
        assert!(inspection.contains("Context-switch endpoints: 1"));
        for (index, record) in schedule.records().iter().enumerate() {
            assert!(inspection.contains(&format!(
                "{index}: {}\n",
                serde_json::to_string(record).unwrap()
            )));
        }
        let printed = print_boundaries(&schedule, false, None).unwrap();
        let decoded: Vec<Record> = printed
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(decoded, schedule.records());
        let filtered = print_boundaries(&schedule, true, Some(3)).unwrap();
        assert!(filtered.starts_with("(0,0)"));
        assert!(filtered.lines().nth(1).unwrap().starts_with("(1,1)"));
        assert_eq!(filtered.lines().count(), 3);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recording.json");
        let recording = Recording::Boundaries(
            BoundaryRecording::new(PreemptionRecord::from_sched_events(vec![], None), schedule)
                .unwrap(),
        );
        recording.write(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(legacy_schedule(&path, "schedule diff/interpolation").is_err());
        let options = CommonOpts::parse_from(["test"]);
        assert!(
            DiffOpts {
                file_a: path.clone(),
                file_b: path.clone()
            }
            .run(&options)
            .is_err()
        );
        assert!(
            InterpolateOpts {
                interpolate: 50,
                file_a: path.clone(),
                file_b: path.clone()
            }
            .run(&options)
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        assert!(
            inspect_boundaries(&Schedule::new(vec![]).unwrap(), true)
                .unwrap()
                .contains("0 stored records")
        );
        assert!(preempt_events(&[]).is_empty());
    }

    /// Two schedules recorded under different signal-state copy settings
    /// cannot be interpolated; one that does not say takes the other's.
    #[test]
    fn interpolate_refuses_schedules_recorded_under_different_signal_state_copy_settings() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, copy: Option<bool>| {
            let path = dir.path().join(name);
            let record =
                PreemptionRecord::from_sched_events(Vec::new(), None).with_signal_state_copy(copy);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            path
        };
        let unsaid = write("unsaid.json", None);
        let on = write("on.json", Some(true));
        let off = write("off.json", Some(false));
        let common = |a: &PathBuf, b: &PathBuf| {
            let (bytes_a, bytes_b) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
            InterpolateOpts::common_signal_state_copy(
                (a.as_path(), bytes_a.as_slice()),
                (b.as_path(), bytes_b.as_slice()),
            )
        };
        assert_eq!(common(&unsaid, &unsaid).unwrap(), None);
        assert_eq!(common(&on, &on).unwrap(), Some(true));
        assert_eq!(common(&unsaid, &off).unwrap(), Some(false));
        assert_eq!(common(&on, &unsaid).unwrap(), Some(true));
        for (a, b) in [(&on, &off), (&off, &on)] {
            let error = common(a, b).unwrap_err();
            assert!(error.to_string().contains("signal-state copy"), "{error}");
        }
    }

    /// The interpolated schedule states the identity both inputs state, a
    /// qualified, unqualified or absent one alike, and two schedules stating
    /// different identities cannot be interpolated.
    #[test]
    fn interpolate_keeps_the_common_identity_and_refuses_different_ones() {
        let dir = tempfile::tempdir().unwrap();
        let known = Recording::decode(
            br#"{"rcb_history":{"format":2,"identity":"rcb:cond-retired:user:v1","recording":{"per_thread":{},"global":[]}}}"#,
        )
        .unwrap()
        .identity()
        .cloned()
        .unwrap();
        let unqualified = HistoryIdentity::Unqualified("claim-unset".to_owned());
        let write = |name: &str, identity: Option<HistoryIdentity>| {
            let path = dir.path().join(name);
            PreemptionRecord::from_sched_events(Vec::new(), identity)
                .write_to_disk(&path)
                .unwrap();
            path
        };
        let known_a = write("known-a.json", Some(known.clone()));
        let known_b = write("known-b.json", Some(known.clone()));
        let unqualified_a = write("unqualified-a.json", Some(unqualified.clone()));
        let unqualified_b = write("unqualified-b.json", Some(unqualified.clone()));
        let legacy = write("legacy.json", None);
        let common = |a: &PathBuf, b: &PathBuf| {
            InterpolateOpts::common_identity(
                (a.as_path(), &Recording::read(a).unwrap()),
                (b.as_path(), &Recording::read(b).unwrap()),
            )
        };
        assert_eq!(common(&known_a, &known_b).unwrap(), Some(known));
        assert_eq!(
            common(&unqualified_a, &unqualified_b).unwrap(),
            Some(unqualified)
        );
        assert_eq!(common(&legacy, &legacy).unwrap(), None);
        for (a, b) in [
            (&known_a, &unqualified_a),
            (&unqualified_b, &known_b),
            (&known_a, &legacy),
            (&legacy, &unqualified_a),
        ] {
            let error = common(a, b).unwrap_err();
            assert!(
                error.to_string().contains("hermit refused to combine"),
                "{error}"
            );
        }
    }

    /// Two schedules recorded under different `--pipe-wake-latency` values,
    /// off and `0` included, cannot be interpolated: no replay could
    /// reproduce the result (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn interpolate_refuses_schedules_recorded_under_different_pipe_wake_latencies() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, latency: Option<u64>| {
            let path = dir.path().join(name);
            let record = PreemptionRecord::from_sched_events(Vec::new(), None)
                .with_pipe_wake_latency(latency);
            std::fs::write(&path, serde_json::to_string(&record).unwrap()).unwrap();
            path
        };
        let off = write("off.json", None);
        let zero = write("zero.json", Some(0));
        let one_ms = write("one-ms.json", Some(1_000_000));
        let common = |a: &PathBuf, b: &PathBuf| {
            let (bytes_a, bytes_b) = (std::fs::read(a).unwrap(), std::fs::read(b).unwrap());
            InterpolateOpts::common_pipe_wake_latency(
                (a.as_path(), bytes_a.as_slice()),
                (b.as_path(), bytes_b.as_slice()),
            )
        };
        assert_eq!(common(&off, &off).unwrap(), None);
        assert_eq!(common(&zero, &zero).unwrap(), Some(0));
        for (a, b) in [(&off, &zero), (&zero, &one_ms), (&one_ms, &off)] {
            let error = common(a, b).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("different --pipe-wake-latency values"),
                "{error}"
            );
        }
    }

    /// Interpolation writes only what it read: after both inputs are read,
    /// replacing the files with legacy schedules of other events and settings
    /// changes neither the written bytes nor their identity, latency, copy
    /// setting or events. Inputs stating different identities are refused
    /// before anything is written.
    #[test]
    fn interpolation_writes_what_it_read_whatever_later_replaces_its_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let known = Recording::decode(
            br#"{"rcb_history":{"format":2,"identity":"rcb:cond-retired:user:v1","recording":{"per_thread":{},"global":[]}}}"#,
        )
        .unwrap()
        .identity()
        .cloned()
        .unwrap();
        let (three, five) = (DetTid::from_raw(3), DetTid::from_raw(5));
        let write = |name: &str, events: Vec<SchedEvent>, identity, latency, copy| {
            let path = dir.path().join(name);
            PreemptionRecord::from_sched_events(events, identity)
                .with_pipe_wake_latency(latency)
                .with_signal_state_copy(copy)
                .write_to_disk(&path)
                .unwrap();
            path
        };
        let schedule = |events: &[(DetTid, u32)]| {
            events
                .iter()
                .map(|&(tid, count)| SchedEvent::branches(tid, count))
                .collect::<Vec<_>>()
        };
        let first = schedule(&[(three, 1), (five, 2), (three, 3)]);
        let second = schedule(&[(five, 2), (three, 1), (three, 3)]);
        let file_a = write("a.json", first, Some(known.clone()), Some(0), Some(true));
        let file_b = write("b.json", second, Some(known.clone()), Some(0), Some(true));
        let options = InterpolateOpts {
            interpolate: 50,
            file_a: file_a.clone(),
            file_b: file_b.clone(),
        };
        let untouched = tempfile::tempdir().unwrap();
        let expected = std::fs::read(
            options.write_interpolation(options.read_inputs().unwrap(), untouched.path()),
        )
        .unwrap();

        let inputs = options.read_inputs().unwrap();
        let other = schedule(&[(five, 9), (three, 9)]);
        write("a.json", other.clone(), None, None, Some(false));
        write("b.json", other, None, None, Some(false));
        let replaced = tempfile::tempdir().unwrap();
        let written = std::fs::read(options.write_interpolation(inputs, replaced.path())).unwrap();
        assert_eq!(written, expected);
        let Recording::Legacy(record) = Recording::decode(&written).unwrap() else {
            panic!("not a legacy schedule")
        };
        assert_eq!(record.identity(), Some(&known));
        assert_eq!(record.pipe_wake_latency(), Some(0));
        assert_eq!(record.signal_state_copy(), Some(true));
        let events = record.into_global();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| event.count != 9), "{events:?}");

        write(
            "b.json",
            schedule(&[(three, 1)]),
            Some(HistoryIdentity::Unqualified("claim-unset".to_owned())),
            Some(0),
            Some(true),
        );
        write(
            "a.json",
            schedule(&[(three, 1)]),
            Some(known),
            Some(0),
            Some(true),
        );
        let error = options
            .read_inputs()
            .err()
            .expect("different identities are refused");
        assert!(
            error.to_string().contains("hermit refused to combine"),
            "{error}"
        );
    }
}
