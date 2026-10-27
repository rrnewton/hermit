/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::cell::Cell;
use std::cmp;
use std::collections::BTreeMap;
use std::fs::File;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;
use anyhow::bail;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::DetTid;
use detcore::types::SchedEvent;
use detcore::util::truncated;
use detcore_model::config::MaybeTimeslice;
use hermit::Error;
use rand::RngExt as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;
use regex::Regex;
use reverie::PrettyBacktrace;
use reverie::process::ExitStatus;
use reverie::process::Output;

use crate::analyze::budget;
use crate::analyze::budget::Budget;
use crate::analyze::budget::BudgetExpired;
use crate::analyze::budget::BudgetKind;
use crate::analyze::checkpoint::BisectInterval;
use crate::analyze::checkpoint::CHECKPOINT_VERSION;
use crate::analyze::checkpoint::Checkpoint;
use crate::analyze::checkpoint::DecidedEvents;
use crate::analyze::checkpoint::FileIdentity;
use crate::analyze::checkpoint::Identity;
use crate::analyze::checkpoint::IntervalResume;
use crate::analyze::checkpoint::Journal;
use crate::analyze::checkpoint::Phase;
use crate::analyze::checkpoint::PrefixBracket;
use crate::analyze::checkpoint::Progress;
use crate::analyze::consts::*;
use crate::analyze::rundata::RunData;
use crate::analyze::rundata::Stop;
use crate::analyze::rundata::TrialEnd;
use crate::analyze::rundata::TrialInconclusive;
use crate::analyze::stop;
use crate::analyze::stop::StopSignals;
use crate::analyze::stop::Stopped;
use crate::analyze::types::AnalyzeOpts;
use crate::analyze::types::CONFIRM_BOUND_FACTOR;
use crate::analyze::types::ContinuationTimeslice;
use crate::analyze::types::ExitStatusConstraint;
use crate::analyze::types::PartialReason;
use crate::analyze::types::PartialReport;
use crate::analyze::types::Report;
use crate::analyze::types::ReportCriticalEvent;
use crate::analyze::types::TrialTimeout;
use crate::analyze::workspace::Retention;
use crate::analyze::workspace::Workspace;
use crate::analyze::workspace::note;
use crate::container::RunTimeoutMarker;
use crate::global_opts::GlobalOpts;
use crate::logdiff::LogDiffCLIOpts;
use crate::run::RunOpts;
use crate::schedule_search::Config;
use crate::schedule_search::CriticalSchedule;
use crate::schedule_search::DivergentTraces;
use crate::schedule_search::InfeasibleSchedule;
use crate::schedule_search::NoFeasibleSchedule;
use crate::schedule_search::NoSingleSwap;
use crate::schedule_search::SearchStart;
use crate::schedule_search::TrialOutcome;
use crate::schedule_search::search_for_critical_schedule_observed;
use crate::schedule_search::trace_divergence;

/// Compare only preemptions, not recorded schedules.
fn preempt_files_equal(path1: &Path, path2: &Path) -> bool {
    let mut pr1 = PreemptionReader::new(path1).load_all();
    let mut pr2 = PreemptionReader::new(path2).load_all();
    pr1.preemptions_only();
    pr2.preemptions_only();
    pr1 == pr2
}

fn yellow_msg(msg: &str) {
    eprintln!(":: {}", msg.yellow().bold());
}

/// What stack1 and stack2 of a `--prefix-search` report are, given the
/// thread the target schedule runs at the deciding event (`chosen`) and the
/// one the scheduler runs there without that event (`instead`, `None` when
/// the run without it ended first). Two threads are a pair whose order
/// decides the outcome. One thread is not a pair at all: the scheduler runs
/// it there either way, so the report says so instead of calling the thread
/// a race with itself.
fn stack_pair_sentences(
    chosen: detcore::types::DetTid,
    instead: Option<detcore::types::DetTid>,
) -> String {
    if instead == Some(chosen) {
        format!(
            "stack1 and stack2 both belong to thread {chosen}: the scheduler runs it at that \
             point with or without the event, so they are not two threads racing.\nWhat the \
             event can change is how thread {chosen} runs from there, not which thread runs: look \
             at what it does before another thread runs, and at that thread.\n"
        )
    } else {
        "stack1 is the thread the target schedule runs at that point; stack2 is the thread that \
         runs there instead when the scheduler continues without chaos.\nThe outcome depends on \
         which of the two runs first: add synchronization that makes the order irrelevant, or \
         that enforces the passing order.\n"
            .to_owned()
    }
}

/// The baseline run (from `.0`) did not finish: a wall-clock bound ended it,
/// or Hermit refused it, so it recorded no schedule to bisect against.
#[derive(Debug)]
pub struct BaselineUnfinished(&'static str);

impl std::fmt::Display for BaselineUnfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the baseline run ({}) did not finish, so it recorded no schedule to bisect against",
            self.0
        )
    }
}

impl std::error::Error for BaselineUnfinished {}

/// `--minimize`'s knock-outs removed every intervention and the replay still
/// meets the target, so there is no baseline schedule to bisect against.
#[derive(Debug)]
pub struct NothingToKnockOut;

impl std::fmt::Display for NothingToKnockOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "--minimize knocked out every intervention and the run still meets the target, so \
             there is no baseline schedule to bisect against"
        )
    }
}

impl std::error::Error for NothingToKnockOut {}

/// `--minimize`'s baseline: knock interventions out of `record`, the latest
/// first, until `replay(try, knocked_out)` misses the target and returns the
/// run that did; [`NothingToKnockOut`] when none is left. Each knock-out
/// starts from the record the previous one replayed, not from what that run
/// recorded: a run's recording also holds the priority changes chaos makes
/// after the replayed record ends, which every run makes again, so knocking
/// those out never ends.
fn knock_out_until_missed<T>(
    mut record: PreemptionRecord,
    mut replay: impl FnMut(usize, &PreemptionRecord) -> anyhow::Result<Option<T>>,
) -> anyhow::Result<T> {
    for attempt in 1.. {
        let Some(knocked_out) = knock_out_latest(&record) else {
            break;
        };
        if let Some(missed) = replay(attempt, &knocked_out)? {
            return Ok(missed);
        }
        record = knocked_out;
    }
    Err(NothingToKnockOut.into())
}

/// `record` with its latest intervention knocked out, for `--minimize`'s
/// baseline; `None` when it has none left. Threads at the default priority
/// with no change are dropped first: replay gives a thread the record does
/// not name that same priority, and a knock-out that removes a whole thread
/// (once no priority change is left) must not pick one of them, which would
/// change nothing.
fn knock_out_latest(record: &PreemptionRecord) -> Option<PreemptionRecord> {
    let pruned = record.without_default_threads();
    let knocked_out = pruned.with_latest_preempt_removed();
    (knocked_out != pruned).then_some(knocked_out)
}

/// What a partial report says about `--prefix-search`'s `bracket`. Short
/// prefixes miss the target and long ones meet it, so the deciding event
/// lies between the longest prefix known to miss and the shortest known to
/// meet.
fn describe_bracket(bracket: &PrefixBracket) -> String {
    let mut said = match (bracket.misses, bracket.meets) {
        (Some(misses), Some(meets)) => format!(
            "The prefix search's bracket, continuing without chaos {}: replaying the first \
             {misses} of the target schedule's {} events misses the target ({}), and replaying \
             the first {meets} meets it ({}). The deciding event is one of events #{misses} to \
             #{} ({} events), assuming, as the search does, that the outcome changes once along \
             the schedule.\n",
            bracket.continuation,
            bracket.events,
            bracket.misses_run.as_deref().unwrap_or("a trial"),
            bracket.meets_run.as_deref().unwrap_or("a trial"),
            meets - 1,
            meets - misses,
        ),
        (None, Some(meets)) => format!(
            "The prefix search had found that replaying the first {meets} of the target \
             schedule's {} events meets the target ({}), continuing without chaos {}, but no \
             shorter prefix whose trial finished and missed it yet: the deciding event is one \
             of events #0 to #{}.\n",
            bracket.events,
            bracket.meets_run.as_deref().unwrap_or("a trial"),
            bracket.continuation,
            meets - 1,
        ),
        (misses, None) => format!(
            "The prefix search had not yet found a prefix of the target schedule's {} events \
             whose replay meets the target, continuing without chaos {}{}.\n",
            bracket.events,
            bracket.continuation,
            misses.map_or_else(String::new, |misses| format!(
                "; replaying the first {misses} misses it ({})",
                bracket.misses_run.as_deref().unwrap_or("a trial")
            )),
        ),
    };
    if !bracket.unfinished.is_empty() {
        said.push_str(&format!(
            "The trials of the prefixes of {} events in between did not finish (a bound or a \
             refusal stopped them); they are not outcomes.\n",
            bracket
                .unfinished
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    said
}

/// The command line that continues this analysis in `workspace`: this
/// invocation's own, with `--resume=WORKSPACE` in place of any earlier one.
fn resume_command(workspace: &Path) -> String {
    resume_command_with(workspace, None)
}

/// [`resume_command`], with `trial_timeout` in place of any
/// `--trial-timeout` the invocation was given.
fn resume_command_with(workspace: &Path, trial_timeout: Option<&str>) -> String {
    let mut args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let analyze = args.iter().position(|arg| arg == "analyze");
    // Only analyze's own options, before the run arguments.
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut index = analyze.map_or(0, |at| at + 1);
    let mut end = end;
    while index < end {
        let replaced = |option: &str| trial_timeout.is_some() && option == "--trial-timeout";
        if args[index] == "--resume" || replaced(&args[index]) {
            let taken = (index + 2).min(end) - index;
            args.drain(index..index + taken);
            end -= taken;
        } else if args[index].starts_with("--resume=")
            || (trial_timeout.is_some() && args[index].starts_with("--trial-timeout="))
        {
            args.remove(index);
            end -= 1;
        } else {
            index += 1;
        }
    }
    let mut added = vec![format!("--resume={}", workspace.display())];
    added.extend(trial_timeout.map(|bound| format!("--trial-timeout={bound}")));
    let at = analyze.map_or(args.len(), |at| at + 1);
    args.splice(at..at, added);
    shell_words::join(&args)
}

/// The invocation's command line with analyze's own options in `valued`
/// (each as `--option value` or `--option=value`) and the flags in `flags`
/// left out, and `added` put first, as a new analysis.
fn command_replacing(valued: &[&str], flags: &[&str], added: Vec<String>) -> String {
    let mut args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let analyze = args.iter().position(|arg| arg == "analyze");
    // Only analyze's own options, before the run arguments.
    let mut end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut index = analyze.map_or(0, |at| at + 1);
    while index < end {
        let arg = args[index].as_str();
        if valued.contains(&arg) {
            let taken = (index + 2).min(end) - index;
            args.drain(index..index + taken);
            end -= taken;
        } else if flags.contains(&arg)
            || valued
                .iter()
                .any(|option| arg.starts_with(&format!("{option}=")))
        {
            args.remove(index);
            end -= 1;
        } else {
            index += 1;
        }
    }
    let at = analyze.map_or(args.len(), |at| at + 1);
    args.splice(at..at, added);
    shell_words::join(&args)
}

/// The invocation's command line run afresh with `--prefix-search` in place
/// of the default search's options (`--run2-seed`, `--run2-preemptions`,
/// `--run2-schedule`, `--run-needleman`, `--minimize`), which
/// `--prefix-search` does not take, and without `--resume`.
fn prefix_search_command() -> String {
    command_replacing(
        &[
            "--run2-seed",
            "--run2-preemptions",
            "--run2-schedule",
            "--resume",
        ],
        &["--run-needleman", "--minimize", "--prefix-search"],
        vec!["--prefix-search".to_owned()],
    )
}

/// Why an analysis ends with a partial report: a budget ran out, a search
/// trial stayed inconclusive, or no schedule between the default search's
/// pair could be replayed.
#[derive(Debug, Clone)]
enum PartialCause {
    Budget(BudgetExpired),
    Inconclusive(TrialInconclusive),
    Infeasible(NoFeasibleSchedule),
}

impl From<NoFeasibleSchedule> for PartialCause {
    fn from(infeasible: NoFeasibleSchedule) -> Self {
        PartialCause::Infeasible(infeasible)
    }
}

impl From<BudgetExpired> for PartialCause {
    fn from(expired: BudgetExpired) -> Self {
        PartialCause::Budget(expired)
    }
}

impl From<TrialInconclusive> for PartialCause {
    fn from(inconclusive: TrialInconclusive) -> Self {
        PartialCause::Inconclusive(inconclusive)
    }
}

impl PartialCause {
    fn reason(&self) -> PartialReason {
        match self {
            PartialCause::Budget(expired) => expired.kind.into(),
            PartialCause::Inconclusive(_) | PartialCause::Infeasible(_) => {
                PartialReason::TrialInconclusive
            }
        }
    }
}

/// The guest program `program` on the host: as it is when it names a path,
/// else found on `PATH`. `None` when it is not there.
fn find_program(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return program.is_file().then(|| program.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// The default search's interval, kept in the workspace as it narrows: the
/// side that changed is written again, and the progress (`checkpoint`)
/// names both.
struct IntervalFiles {
    paths: [PathBuf; 2],
    /// The digest of the schedule each file holds (`events_digest`).
    written: [Option<String>; 2],
}

/// The sha256 that `IntervalResume` keeps of the schedules `lists`: of each
/// one's length and events as `std::hash::Hash` writes them, streamed
/// rather than collected, since a schedule can hold millions of events.
fn events_digest(lists: &[&[SchedEvent]]) -> String {
    /// Collects what `Hash` writes.
    struct Sink<'a>(&'a mut Vec<u8>);
    impl std::hash::Hasher for Sink<'_> {
        fn write(&mut self, bytes: &[u8]) {
            self.0.extend_from_slice(bytes);
        }
        fn finish(&self) -> u64 {
            0
        }
    }
    struct Stream<'a> {
        lists: std::slice::Iter<'a, &'a [SchedEvent]>,
        events: std::slice::Iter<'a, SchedEvent>,
        buf: Vec<u8>,
        at: usize,
    }
    impl std::io::Read for Stream<'_> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            use std::hash::Hash as _;
            if self.at == self.buf.len() {
                self.buf.clear();
                self.at = 0;
                while self.buf.len() < 1 << 16 {
                    match self.events.next() {
                        Some(event) => event.hash(&mut Sink(&mut self.buf)),
                        None => match self.lists.next() {
                            Some(list) => {
                                list.len().hash(&mut Sink(&mut self.buf));
                                self.events = list.iter();
                            }
                            None => break,
                        },
                    }
                }
            }
            let n = out.len().min(self.buf.len() - self.at);
            out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }
    let stream = Stream {
        lists: lists.iter(),
        events: [].iter(),
        buf: Vec::new(),
        at: 0,
    };
    detcore::Digest::digest_reader(stream)
        .expect("reading from memory does not fail")
        .to_string()
}

/// Where the default search stands, besides its interval: what
/// `IntervalResume` keeps.
#[derive(Clone)]
struct SearchState {
    endpoints: String,
    event_level: bool,
    trials: usize,
    unfinished: usize,
    refused: usize,
}

impl IntervalFiles {
    fn new(workspace: &Path) -> Self {
        Self {
            paths: [
                workspace.join("interval_passing").with_extension(SCHED_EXT),
                workspace.join("interval_failing").with_extension(SCHED_EXT),
            ],
            written: [None, None],
        }
    }

    /// Record the interval `passing`, `failing`, at the start of `pass`
    /// with its edit and swap `distances` and the search's `state`, or
    /// before the first pass. Each file is replaced whole (written aside,
    /// then renamed), and the progress names the interval only after both.
    fn record(
        &mut self,
        opts: &AnalyzeOpts,
        pass: Option<usize>,
        [passing, failing]: [&[SchedEvent]; 2],
        distances: Option<(usize, usize)>,
        state: Option<SearchState>,
    ) {
        let mut hashes = [String::new(), String::new()];
        let mut whole = true;
        for (slot, events) in [passing, failing].into_iter().enumerate() {
            let hash = events_digest(&[events]);
            hashes[slot] = hash.clone();
            if self.written[slot].as_ref() == Some(&hash) {
                continue;
            }
            let path = &self.paths[slot];
            let aside = path.with_extension("partial");
            let written = opts
                .write_trial_schedule(events.to_vec(), &aside)
                .and_then(|()| Ok(std::fs::rename(&aside, path)?));
            match written {
                Ok(()) => self.written[slot] = Some(hash),
                Err(error) => {
                    whole = false;
                    self.written[slot] = None;
                    note(&format!(
                        ":: cannot write the search's interval to {}: {error:#}",
                        path.display()
                    ))
                }
            }
        }
        let common_prefix = passing
            .iter()
            .zip(failing)
            .take_while(|(first, second)| first == second)
            .count();
        opts.progress.interval(BisectInterval {
            pass,
            edit_distance: distances.map(|(edit, _)| edit),
            swap_distance: distances.map(|(_, swap)| swap),
            passing: self.paths[0].clone(),
            passing_events: passing.len(),
            failing: self.paths[1].clone(),
            failing_events: failing.len(),
            common_prefix,
            passing_event: passing.get(common_prefix).map(ToString::to_string),
            failing_event: failing.get(common_prefix).map(ToString::to_string),
            resume: state.filter(|_| whole).map(|state| IntervalResume {
                endpoints: state.endpoints,
                passing_hash: hashes[0].clone(),
                failing_hash: hashes[1].clone(),
                event_level: state.event_level,
                trials: state.trials,
                unfinished: state.unfinished,
                refused: state.refused,
            }),
        });
    }
}

/// How the last trial of each prefix ended, per continuation, and the
/// wall-clock bound it ran under, in seconds.
type Ends = BTreeMap<(usize, MaybeTimeslice), (TrialEnd, Option<u64>)>;

/// `--prefix-search`'s bracket at `continuation` over a schedule of
/// `events` events, from each prefix's last trial (`ends`) and the run that
/// was (`runs`): only trials that finished bound it.
fn prefix_bracket(
    ends: &Ends,
    runs: &BTreeMap<(usize, MaybeTimeslice), String>,
    continuation: MaybeTimeslice,
    events: usize,
) -> PrefixBracket {
    let at = |end: TrialEnd| {
        ends.iter()
            .filter(move |((_, timeslice), (ended, _))| *timeslice == continuation && *ended == end)
            .map(|((prefix, _), _)| *prefix)
    };
    let meets = at(TrialEnd::Met).min();
    let below = meets.unwrap_or(usize::MAX);
    let misses = at(TrialEnd::Missed).filter(|&prefix| prefix < below).max();
    let unfinished = ends
        .iter()
        .filter(|((prefix, timeslice), (end, _))| {
            *timeslice == continuation
                && !end.finished()
                && *prefix < below
                && misses.is_none_or(|misses| *prefix > misses)
        })
        .map(|((prefix, _), _)| *prefix)
        .collect();
    let run = |prefix: Option<usize>| {
        prefix.and_then(|prefix| runs.get(&(prefix, continuation)).cloned())
    };
    PrefixBracket {
        events,
        continuation: describe_continuation(continuation),
        misses,
        misses_run: run(misses),
        meets,
        meets_run: run(meets),
        unfinished,
    }
}

/// The result of `--prefix-search`: replaying the first `decision` events of
/// `schedule` and then scheduling without chaos, with timeslices of
/// `continuation`, misses the target, while replaying event `decision` too
/// meets it.
pub struct PrefixDecision {
    pub schedule: PreemptionRecord,
    pub decision: usize,
    pub continuation: MaybeTimeslice,
    /// [`refused_trials_warning`], if any trial was refused.
    pub refused: Option<String>,
    /// [`unfinished_boundary_warning`], when the last trial of the first
    /// `decision` events did not finish.
    pub unfinished: Option<String>,
    /// Why analyze searched the prefixes instead of bisecting between two
    /// runs, when it says so in the report ([`DivergentTraces`]).
    pub fallback: Option<String>,
}

/// The longest prefix shorter than `passing` whose last trial at
/// `continuation` finished, and how it ended, from each prefix's last trial
/// (`ends`).
fn longest_finished_below(
    ends: &BTreeMap<(usize, MaybeTimeslice), (TrialEnd, Option<u64>)>,
    continuation: MaybeTimeslice,
    passing: usize,
) -> Option<(usize, TrialEnd)> {
    ends.iter()
        .rev()
        .filter(|((prefix, timeslice), _)| *timeslice == continuation && *prefix < passing)
        .map(|(&(prefix, _), &(end, _))| (prefix, end))
        .find(|&(_, end)| end.finished())
}

/// Which flag raises the bound that stopped a trial, as advice: the run
/// arguments' own `--timeout` (`own`, in seconds) where that is the bound
/// trials ran under, `last` (none when `--trial-timeout` is `off`), else
/// `--trial-timeout`.
fn bound_remedy(own: Option<u64>, trial_timeout_off: bool, last: Option<u64>) -> &'static str {
    if own.is_some() && (trial_timeout_off || own <= last) {
        "raise the run arguments' --timeout"
    } else {
        "raise --trial-timeout"
    }
}

/// What a search should say about its result when the last trial of the
/// first `passing` events, the boundary's baseline side, did not finish
/// within `bound` seconds of wall-clock time, the run arguments' own
/// `--timeout`, which analyze cannot give longer (a trial its own bound
/// stopped runs again, [`AnalyzeOpts::decisive_trial`]). `below` is the
/// longest shorter prefix the search replayed whose trial finished, and how
/// it ended. It names only prefixes
/// the search replayed: the search assumes the outcome changes once along
/// the schedule, so it claims nothing about the prefixes in between.
/// `remedy` is [`bound_remedy`]'s advice.
fn unfinished_boundary_warning(
    passing: usize,
    bound: Option<u64>,
    below: Option<(usize, TrialEnd)>,
    remedy: &str,
) -> String {
    let confirmation = ", and no longer bound could be given to confirm it";
    let below = match below {
        Some((prefix, end)) => format!(
            "The longest shorter prefix the search replayed that finished is the prefix of \
             {prefix} events, which {}.",
            describe_trial_end(end, None)
        ),
        None => "No shorter prefix the search replayed finished.".to_owned(),
    };
    format!(
        "replaying the first {passing} events and then scheduling without chaos \
         {}{confirmation}: the guest hangs (for example in a deadlock while other threads keep \
         polling), or needs longer still. A hang is a third outcome, not a run that misses the \
         target, yet the search counted it as not meeting the target: this boundary rests on a \
         wall-clock bound, not on an outcome. Given longer, that trial may meet the target, and \
         an earlier event decide the outcome ({remedy}). {below}",
        describe_trial_end(TrialEnd::Unfinished(Stop::Bound), bound)
    )
}

/// How a trial ended, for a message; `bound` is the wall-clock bound it ran
/// under, in seconds.
fn describe_trial_end(end: TrialEnd, bound: Option<u64>) -> String {
    match end {
        TrialEnd::Met => "met the target".to_owned(),
        TrialEnd::Missed => "missed the target".to_owned(),
        TrialEnd::Unfinished(Stop::Bound) => match bound {
            Some(bound) => format!("did not finish within {bound} s"),
            None => "did not finish".to_owned(),
        },
        TrialEnd::Unfinished(Stop::Backstop) => match bound {
            Some(bound) => format!("did not finish within hermit analyze's bound of {bound} s"),
            None => "did not finish within hermit analyze's bound".to_owned(),
        },
        TrialEnd::Unfinished(Stop::Refusal) => "was refused by Hermit".to_owned(),
    }
}

/// What a search with `refused` trials Hermit refused at run time, all
/// counted as not meeting the target, should say about its result.
fn refused_trials_warning(refused: usize) -> Option<String> {
    (refused > 0).then(|| {
        format!(
            "{refused} trial(s) were refused by Hermit (exit 122) and counted as not meeting \
             the target, so the deciding event may separate the target from a path Hermit does \
             not support rather than from a run that misses the target."
        )
    })
}

/// The coarsest timeslice `--continuation-timeslice=auto` tries, in virtual
/// nanoseconds: `hermit run`'s default `--max-timeslice` (200 ms).
pub const COARSEST_CONTINUATION_TIMESLICE: NonZeroU64 = NonZeroU64::new(200_000_000).unwrap();

/// The continuation timeslices `--continuation-timeslice=auto` tries, in
/// order, for a run whose own maximum is `run`: that, then
/// [`COARSEST_CONTINUATION_TIMESLICE`] if it is coarser. Never a shorter one:
/// a trial's `--max-timeslice` also ends the replayed prefix's slices that
/// its schedule does not end, so a shorter one would change the prefix. A run
/// without PMU timeslices (`None`) continues as it is.
fn continuation_candidates(run: MaybeTimeslice) -> Vec<MaybeTimeslice> {
    let Some(run) = run else {
        return vec![None];
    };
    let mut candidates = vec![Some(run), Some(run.max(COARSEST_CONTINUATION_TIMESLICE))];
    candidates.dedup();
    candidates
}

/// What the endpoint trials of a prefix search showed for one continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Continuation {
    /// Replaying none of the schedule misses the target, and replaying all
    /// of it meets it: a search can find the event in between that decides.
    Usable,
    /// The scheduler alone, replaying none of the schedule, meets the target.
    MeetsTargetAlone,
    /// Replaying the whole schedule finished and missed the target.
    ReplayMisses,
    /// Replaying the whole schedule did not finish (a bound or a refusal
    /// stopped it).
    ReplayUnfinished,
}

/// Check the two endpoint trials of a prefix search over `len` events.
fn check_continuation(
    len: usize,
    mut trial: impl FnMut(usize) -> anyhow::Result<TrialEnd>,
) -> anyhow::Result<Continuation> {
    if trial(0)?.reached_target() {
        return Ok(Continuation::MeetsTargetAlone);
    }
    Ok(match trial(len)? {
        TrialEnd::Met => Continuation::Usable,
        TrialEnd::Missed => Continuation::ReplayMisses,
        TrialEnd::Unfinished(_) => Continuation::ReplayUnfinished,
    })
}

/// Pick the first of `candidates` (continuation timeslices) whose endpoint
/// trials allow a prefix search over `len` events, each candidate's trials
/// run by `trial(prefix, timeslice)`. When none does, the error says what
/// each one showed and what to do instead.
///
/// A candidate one of whose endpoint trials has no outcome (analyze's own
/// bound stopped it twice, [`TrialInconclusive`]) ends the search there,
/// with the next candidate named in the error: moving on to it would let
/// the host's speed choose the timeslice, and with it, possibly, the
/// deciding event.
fn choose_continuation(
    candidates: &[MaybeTimeslice],
    len: usize,
    mut trial: impl FnMut(usize, MaybeTimeslice) -> anyhow::Result<TrialEnd>,
) -> anyhow::Result<MaybeTimeslice> {
    let mut outcomes = Vec::new();
    for (index, &timeslice) in candidates.iter().enumerate() {
        match check_continuation(len, |prefix| trial(prefix, timeslice)) {
            Ok(Continuation::Usable) => return Ok(timeslice),
            Ok(outcome) => {
                eprintln!(
                    ":: Continuing without chaos {}, {}",
                    describe_continuation(timeslice),
                    describe_outcome(outcome)
                );
                outcomes.push((timeslice, outcome));
            }
            Err(error) => {
                let mut stopped = error.downcast::<TrialInconclusive>()?;
                stopped.next_continuation = candidates[index + 1..]
                    .iter()
                    .find_map(|next| next.map(NonZeroU64::get));
                return Err(stopped.into());
            }
        }
    }
    let alone: Vec<String> = outcomes
        .iter()
        .filter(|(_, outcome)| *outcome == Continuation::MeetsTargetAlone)
        .map(|(timeslice, _)| describe_continuation(*timeslice))
        .collect();
    if alone.len() == outcomes.len() {
        bail!(
            "no event of the target schedule decides the outcome: without chaos, the scheduler \
             alone already meets the target criteria {}. The outcome does not depend on a chaos \
             decision, so debug that run directly: the reproducer of the first trial \
             (prefix_round_001.repro in the workspace) runs it. To bisect against a run that \
             misses the target instead, pass --continuation-timeslice with a timeslice at which \
             the scheduler alone misses it, or use the default search with a passing \
             --run2-seed.",
            alone.join(" and ")
        );
    }
    let remedy = if outcomes
        .iter()
        .any(|(_, outcome)| *outcome == Continuation::ReplayUnfinished)
    {
        "A replay that did not finish was ended by a bound (raise --trial-timeout) or refused \
         by Hermit (its reason is in its log)."
    } else {
        "Schedule replay does not reproduce this run where replaying the whole target schedule \
         misses the target; check that the target run replays (--selfcheck)."
    };
    bail!(
        "no continuation allows a prefix search: {}. {remedy} The default search with a \
         passing --run2-seed needs no continuation.",
        outcomes
            .iter()
            .map(|(timeslice, outcome)| format!(
                "{} {}",
                describe_continuation(*timeslice),
                describe_outcome(*outcome)
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

/// "at a 200000000 ns timeslice", or "without PMU timeslices".
fn describe_continuation(timeslice: MaybeTimeslice) -> String {
    match timeslice {
        Some(nanos) => format!("at a {nanos} ns timeslice"),
        None => "without PMU timeslices".to_owned(),
    }
}

fn describe_outcome(outcome: Continuation) -> &'static str {
    match outcome {
        Continuation::Usable => "a prefix search can run",
        Continuation::MeetsTargetAlone => {
            "the scheduler alone, replaying none of the target schedule, meets the target"
        }
        Continuation::ReplayMisses => "replaying the whole target schedule misses the target",
        Continuation::ReplayUnfinished => {
            "replaying the whole target schedule did not finish (a bound or a refusal stopped it)"
        }
    }
}

/// The boundary a prefix search settled on: the prefix of `passing` events
/// does not meet the target, the prefix of `passing + 1` events meets it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Boundary {
    passing: usize,
    /// How the last trial of the prefix of `passing` events ended: it missed
    /// the target, or it did not finish.
    passing_end: TrialEnd,
}

/// The binary search of `--prefix-search` over a schedule of `len` events
/// whose endpoints [`check_continuation`] found usable, the trial of the
/// empty prefix having ended `empty`. Uses about log2(len) trials.
///
/// `trial` runs a prefix's trial to an end that does not depend on when the
/// host's clock ran out: a trial analyze's own wall-clock bound stopped runs
/// again first, and one stopped again ends the search
/// ([`AnalyzeOpts::decisive_trial`]). A trial that the run arguments' own
/// `--timeout` stopped, or that Hermit refused, counts as not meeting the
/// target, since a guest can spin for ever on the baseline side; when the
/// boundary rests on one, the report says so ([`unfinished_boundary_warning`]).
fn search_prefix_boundary(
    len: usize,
    empty: TrialEnd,
    mut trial: impl FnMut(usize) -> anyhow::Result<TrialEnd>,
) -> anyhow::Result<Boundary> {
    let (mut passing, mut passing_end, mut failing) = (0, empty, len);
    while failing - passing > 1 {
        let mid = passing + (failing - passing) / 2;
        let end = trial(mid)?;
        if end.reached_target() {
            failing = mid;
        } else {
            passing = mid;
            passing_end = end;
        }
    }
    Ok(Boundary {
        passing,
        passing_end,
    })
}

/// Why the critical pair a search settled on may not be a race, if there is a reason.
///
/// Swapping two events of one thread is no interleaving any execution can take, so
/// the outcome change at such a pair comes from the replay falling off the schedule.
/// Trials that did not finish (a bound or a refusal ended them) were counted as
/// not meeting the target, which can steer the search onto exactly such a pair.
fn critical_pair_caveat(crit: &CriticalSchedule, unfinished: usize) -> Option<String> {
    let index = crit.critical_event_index;
    let schedule = &crit.failing_schedule;
    let same_thread =
        index > 0 && index < schedule.len() && schedule[index - 1].dettid == schedule[index].dettid;
    if !same_thread {
        return None;
    }
    Some(format!(
        "the critical events #{} and #{index} both belong to thread {}, so swapping them is \
         not an interleaving and this pair is not a race. {unfinished} trial(s) did not finish \
         (a bound or a refusal ended them) and were counted as not meeting the target.",
        index - 1,
        schedule[index].dettid,
    ))
}

/// How long one thread runs from an event on, in one schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slice {
    /// Its events, from the first one on.
    events: usize,
    /// The retired conditional branches among them.
    branches: u64,
    /// The event at which another thread runs next, and that thread; `None`
    /// if none does before the schedule ends.
    next: Option<(usize, DetTid)>,
}

/// A record that replays `continued`, the schedule a prefix trial of the
/// first `len` events of `schedule` realized, under the callback checkpoints
/// of that prefix (a replay records none of its own), unless they no longer
/// fit its events.
fn continued_record(
    schedule: &PreemptionRecord,
    len: usize,
    continued: &[SchedEvent],
) -> PreemptionRecord {
    schedule
        .schedule_prefix(len)
        .with_schedule(continued.to_vec())
        .unwrap_or_else(|| PreemptionRecord::from_sched_events(continued.to_vec()))
}

/// The run of `thread`'s events in `schedule` from event `from` on.
fn slice_from(schedule: &[SchedEvent], from: usize, thread: DetTid) -> Slice {
    let mut slice = Slice {
        events: 0,
        branches: 0,
        next: None,
    };
    for (index, event) in schedule.iter().enumerate().skip(from) {
        if event.dettid != thread {
            slice.next = Some((index, event.dettid));
            break;
        }
        slice.events += 1;
        if event.op == detcore_model::schedule::Op::Branch {
            slice.branches += u64::from(event.count);
        }
    }
    slice
}

/// Whether two stacks have the same frames, whichever threads they belong
/// to.
fn same_frames(first: &PrettyBacktrace, second: &PrettyBacktrace) -> bool {
    first.iter().eq(second.iter())
}

/// The events at which the deciding thread's slice ends in the continuation
/// with the deciding event and in the one without it, when the stacks of the
/// threads that run next (`next_stacks`) tell nothing apart: both captured,
/// with the same frames (often threads just started, at their first
/// instruction). `None` otherwise.
fn slice_end_events(
    with: &Slice,
    without: &Slice,
    next_stacks: [Option<&PrettyBacktrace>; 2],
) -> Option<[usize; 2]> {
    match (next_stacks, with.next, without.next) {
        ([Some(first), Some(second)], Some((with_next, _)), Some((without_next, _)))
            if same_frames(first, second) =>
        {
            Some([with_next - 1, without_next - 1])
        }
        _ => None,
    }
}

/// The header of a `--prefix-search` report whose deciding event `event`,
/// #`decision`, is a timing decision: `with` and `without` are how long its
/// thread then runs with and without that event replayed. `slice_ends`, the
/// last events of those slices, is set when stack1 and stack2 show where the
/// deciding thread stops instead of the thread that runs next.
fn timing_decision_header(
    decision: usize,
    event: &SchedEvent,
    (with, without): (&Slice, &Slice),
    criteria: &str,
    continuation: MaybeTimeslice,
    slice_ends: Option<[usize; 2]>,
) -> String {
    let thread = event.dettid;
    let then = |slice: &Slice, stack: &str| match slice.next {
        Some((index, next)) if slice_ends.is_none() => {
            format!("then thread {next} runs at event #{index} ({stack})")
        }
        Some((index, next)) => format!("then thread {next} runs at event #{index}"),
        None => "and no other thread runs after it".to_owned(),
    };
    let mut header = format!(
        "The scheduling decision at event #{decision} of the target schedule decides the \
         outcome, and it is a timing decision: with or without it, the scheduler runs thread \
         {thread} there, so what decides is how long thread {thread} runs, not which thread \
         runs.\nWith event #{decision} replayed ({event}), thread {thread} runs {} events ({} \
         retired conditional branches) from event #{decision} on, {}, and the run meets the \
         target criteria ({criteria}).\nWithout it, scheduling without chaos {}, thread {thread} \
         runs {} events ({} retired conditional branches), {}, and the run does not.\n",
        with.events,
        with.branches,
        then(with, "stack1"),
        describe_continuation(continuation),
        without.events,
        without.branches,
        then(without, "stack2"),
    );
    if let Some([with_end, without_end]) = slice_ends {
        header.push_str(&format!(
            "The thread that runs next has the same stack in both continuations (often a \
             thread just started), so stack1 and stack2 show where thread {thread} is when its \
             slice ends instead: at event #{with_end} with the event, at event #{without_end} \
             without it.\n"
        ));
    }
    header.push_str(&format!(
        "Thread {thread}'s own stack at event #{decision} is printed first (deciding_event in \
         the report file).\nThe outcome depends on what thread {thread} does before the other \
         thread runs: add synchronization that makes its timing irrelevant, or that enforces \
         the passing order.\n"
    ));
    header
}

/// The header of a `--prefix-search` report whose continuation with the
/// deciding event is missing: the stack-trace replay that records it was
/// stopped by a wall-clock bound (`stopped` says which). The decision stands,
/// but not its kind: without the event the scheduler runs the deciding
/// thread there too (`without` shows), and whether the run with it runs that
/// thread longer, which would make it a timing decision, is what is missing.
fn timing_decision_missing_header(
    decision: usize,
    event: &SchedEvent,
    without: &Slice,
    criteria: &str,
    continuation: MaybeTimeslice,
    stopped: &str,
) -> String {
    let thread = event.dettid;
    let then = match without.next {
        Some((index, next)) => format!("then thread {next} runs at event #{index} (stack2)"),
        None => "and no other thread runs after it".to_owned(),
    };
    format!(
        "The scheduling decision at event #{decision} of the target schedule decides the \
         outcome.\nWithout it, scheduling without chaos {}, the scheduler runs thread {thread} \
         there too, for {} events ({} retired conditional branches), {then}, and the run does \
         not meet the target criteria; replaying event #{decision} too ({event}) meets them \
         ({criteria}).\nWhat the scheduler runs after event #{decision} with the event, and so \
         whether what decides is how long thread {thread} runs or which thread runs there, is \
         missing: {stopped}. stack1, the thread that would run next with the event, is missing \
         with it.\nThread {thread}'s own stack at event #{decision} is printed first \
         (deciding_event in the report file).\n",
        describe_continuation(continuation),
        without.events,
        without.branches,
    )
}

/// Whether `--prefix-search`'s stack-trace replays contradict the search's
/// outcomes: one `with` the deciding event that finished and missed the
/// target, or one `without` it that met it. A replay a bound stopped has no
/// outcome, and contradicts nothing.
fn stack_runs_contradict(with: &[&PrefixStack], without: &[&PrefixStack]) -> bool {
    with.iter().any(|run| run.finished && !run.met) || without.iter().any(|run| run.met)
}

/// What a `--prefix-search` stack-trace replay gave.
struct PrefixStack {
    /// It finished: a stopped replay has no outcome to compare with the
    /// search's.
    finished: bool,
    /// It finished and met the target criteria.
    met: bool,
    /// The stack at the event, if the replay reached it.
    stack: Option<PrettyBacktrace>,
    /// What stopped it, when a wall-clock bound did
    /// ([`AnalyzeOpts::stack_trace_trial`]).
    stopped: Option<String>,
}

impl PrefixStack {
    /// No replay: there was no event to take a stack at.
    fn none() -> Self {
        PrefixStack {
            finished: false,
            met: false,
            stack: None,
            stopped: None,
        }
    }

    /// "`which` is missing: why", when a wall-clock bound stopped the replay
    /// before it took its stack.
    fn missing(&self, which: &str) -> Option<String> {
        match (&self.stack, &self.stopped) {
            (None, Some(stopped)) => Some(format!("{which} is missing: {stopped}.")),
            _ => None,
        }
    }
}

/// What a stack-trace replay of a critical pair gave.
struct StackReplay {
    /// It finished (no bound or refusal stopped it).
    finished: bool,
    /// It finished and met the target criteria.
    met_target: bool,
    /// The stacks at its schedule's events `index - 1` and `index`.
    stacks: [Option<PrettyBacktrace>; 2],
    /// Where it left its schedule before passing event `index`, if it did.
    left_schedule: Option<Departure>,
    /// The threads it ran at events `index - 1` and `index`, if it
    /// recorded them.
    ran: [Option<DetTid>; 2],
}

impl StackReplay {
    /// Whether the replay followed its schedule through event `event`, so
    /// that a stack it took there is that event's.
    fn followed_through(&self, event: usize) -> bool {
        match &self.left_schedule {
            None => true,
            Some(Departure::OtherEvent { index, .. }) => *index > event,
            Some(Departure::Ended { events }) => *events > event,
            Some(Departure::Unknown) => false,
        }
    }
}

/// Where a replay left the schedule it replayed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Departure {
    /// At this event it ran another event than the schedule's: another
    /// thread, operation or instruction pointer, compared as the scheduler
    /// does when it takes a stack (the count and time aside).
    OtherEvent {
        index: usize,
        expected: SchedEvent,
        ran: SchedEvent,
    },
    /// It ended after this many events.
    Ended { events: usize },
    /// It recorded no schedule (a bound or a refusal stopped it).
    Unknown,
}

impl std::fmt::Display for Departure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Departure::OtherEvent {
                index,
                expected,
                ran,
            } => write!(
                f,
                "ran {ran} at event #{index}, where the schedule has {expected}"
            ),
            Departure::Ended { events } => write!(f, "ended after {events} events"),
            Departure::Unknown => write!(
                f,
                "did not finish (a bound or a refusal stopped it), so it recorded no schedule"
            ),
        }
    }
}

/// Whether `ran` is the event `expected`, as the scheduler compares them when
/// it takes a stack: the count and the time aside.
fn same_event(expected: &SchedEvent, ran: &SchedEvent) -> bool {
    expected.dettid == ran.dettid
        && expected.op == ran.op
        && expected.start_rip == ran.start_rip
        && expected.end_rip == ran.end_rip
}

/// The first of the first `until` events of `requested` at which `realized`
/// ran another event, or ended.
fn departure(requested: &[SchedEvent], realized: &[SchedEvent], until: usize) -> Option<Departure> {
    let until = until.min(requested.len());
    for (index, expected) in requested[..until].iter().enumerate() {
        match realized.get(index) {
            None => return Some(Departure::Ended { events: index }),
            Some(ran) if !same_event(expected, ran) => {
                return Some(Departure::OtherEvent {
                    index,
                    expected: expected.clone(),
                    ran: ran.clone(),
                });
            }
            Some(_) => {}
        }
    }
    None
}

/// The thread a stack was taken on, as the guest numbers it (which is the
/// scheduler's numbering under Hermit's thread-id virtualization).
fn stack_thread(stack: &PrettyBacktrace) -> Option<DetTid> {
    let thread = serde_json::to_value(stack)
        .ok()?
        .get("thread_id")?
        .as_i64()?;
    Some(DetTid::from_raw(i32::try_from(thread).ok()?))
}

/// The stacks of the two racing operations, in the passing schedule's order,
/// each with the replay it came from; `threads` are the threads of the
/// passing schedule's events `index - 1` and `index`. Those events are the
/// failing schedule's `index` and `index - 1`, so each stack can come from
/// either replay: preferably one that followed its schedule through that
/// event, else one taken on that operation's thread (the replay ran that
/// thread there, or the stack says so), said to be possibly another
/// occurrence of the operation. A stack taken on another thread is left
/// out: a replay that left its schedule can take one there.
fn merge_stacks(
    baseline: &StackReplay,
    target: &StackReplay,
    index: usize,
    threads: [DetTid; 2],
) -> [Option<(PrettyBacktrace, String)>; 2] {
    [0, 1].map(|slot| {
        let other = 1 - slot;
        let candidates = [
            ("the baseline replay", baseline, slot, index - 1 + slot),
            ("the target replay", target, other, index - 1 + other),
        ];
        candidates
            .iter()
            .find(|(_, replay, stack, event)| {
                replay.stacks[*stack].is_some() && replay.followed_through(*event)
            })
            .map(|(name, replay, stack, _)| {
                (replay.stacks[*stack].clone().unwrap(), (*name).to_owned())
            })
            .or_else(|| {
                candidates.iter().find_map(|(name, replay, stack, _)| {
                    let taken = replay.stacks[*stack].as_ref()?;
                    let on_its_thread = replay.ran[*stack] == Some(threads[slot])
                        || stack_thread(taken) == Some(threads[slot]);
                    on_its_thread.then_some(())?;
                    replay.stacks[*stack].clone().map(|stack| {
                        (
                            stack,
                            format!(
                                "{name}, which left its schedule before this operation, so the \
                                 stack may be another occurrence of it"
                            ),
                        )
                    })
                })
            })
    })
}

/// Why neither replay captured `which` (the stack at the passing schedule's
/// event `event`).
fn missing_stack_reason(
    which: &str,
    (event, target_event): (usize, usize),
    baseline: &StackReplay,
    target: &StackReplay,
) -> String {
    let describe = |replay: &StackReplay, event: usize| match &replay.left_schedule {
        Some(departure) if !replay.followed_through(event) => {
            format!("left its schedule: it {departure}")
        }
        _ => format!(
            "followed its schedule through event #{event}, but left no readable stack there"
        ),
    };
    let unfinished = [baseline, target]
        .iter()
        .any(|replay| replay.left_schedule == Some(Departure::Unknown));
    format!(
        "{which} is not available: no stack-trace replay captured that operation (event #{event} \
         of the passing schedule). The baseline replay {}; the target replay {}. {}",
        describe(baseline, event),
        describe(target, target_event),
        if unfinished {
            "A replay that did not finish was stopped by a bound (raise --trial-timeout) or \
             refused by Hermit (its reason is in its log)."
        } else {
            "Schedule replay did not reproduce these runs; check that they replay (--selfcheck) \
             on a host with an exact branch counter (hermit host-capabilities)."
        },
    )
}

/// The line of `text` that holds `pattern`'s first match.
fn matched_line<'a>(pattern: &Regex, text: &'a str) -> Option<&'a str> {
    let found = pattern.find(text)?;
    let start = text[..found.start()].rfind('\n').map_or(0, |at| at + 1);
    let end = text[found.start()..]
        .find('\n')
        .map_or(text.len(), |at| found.start() + at);
    Some(&text[start..end])
}

/// Whether the replay at workspace root `replay` met the criteria with
/// another failure than the target run at root `target`: whether the line
/// each output pattern (`patterns`, by stream) matches differs between them.
/// Output the patterns do not match may differ with the interleaving and is
/// not compared.
fn changed_output(
    target: &Path,
    replay: &Path,
    patterns: [(&str, Option<&Regex>); 2],
) -> Option<String> {
    patterns.into_iter().find_map(|(stream, pattern)| {
        let pattern = pattern?;
        let read = |root: &Path| {
            std::fs::read(root.with_extension(stream))
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        };
        let (want, got) = (read(target)?, read(replay)?);
        let (want, got) = (matched_line(pattern, &want)?, matched_line(pattern, &got)?);
        (want != got).then(|| {
            format!(
                "the final target schedule meets the target criteria with another failure than \
                 the target run: the {stream} line that --target-{stream} matches is {got:?}, \
                 where the target run's is {want:?}. The critical pair may belong to that other \
                 failure; narrow --target-{stream} to tell them apart."
            )
        })
    })
}

impl AnalyzeOpts {
    pub fn get_tmp(&self) -> anyhow::Result<&Path> {
        if let Some(pb) = &self.tmp_dir {
            Ok(pb.as_path())
        } else {
            bail!("Expected tmp_dir to be set at this point!")
        }
    }

    /// Launch a chaos run searching for a target (e.g. failing) schudule.
    /// Returns Some if a target schedule is found.
    fn launch_search(&self, round: u64, sched_seed: u64) -> Result<Option<RunData>, Error> {
        yellow_msg(&format!(
            "Searching (round {}) for a target execution, chaos --sched-seed={} ",
            round, sched_seed
        ));
        let runname = format!("search_round_{:0wide$}", round, wide = 3);
        let mut rundat = RunData::new_baseline(self, runname)?.with_preemption_recording();
        rundat.runopts.det_opts.det_config.sched_seed = Some(sched_seed);
        if self.imprecise_search {
            rundat.runopts.det_opts.det_config.imprecise_timers = true; // TODO: enable this by default when bugs are fixed.
        }
        if rundat.launch_trial()?.reached_target() {
            eprintln!(
                ":: {}:\n    {}",
                "Target run established by --search. Reproducer"
                    .green()
                    .bold(),
                rundat.to_repro(),
            );
            Ok(Some(rundat))
        } else {
            Ok(None)
        }
    }

    // TODO: REMOVE. Only used by minimize atm.
    //
    /// Launch a run with preempts provided (to replay). No logging. Return true if it matches the
    /// criteria. If provided, additionally record full schedule events from the run to
    /// `record_sched_path`.
    pub(super) fn launch_from_preempts_to_sched(
        &self,
        runname: &str,
        preempts_path: &Path,
        record_sched_path: Option<&Path>,
    ) -> anyhow::Result<(bool, RunOpts)> {
        let mut run = RunData::new_baseline(self, runname.to_string())?
            .with_preempts_path_in(preempts_path.to_path_buf());

        if let Some(path) = record_sched_path {
            run = run.with_schedule_recording_to(path.to_path_buf());
        }
        let is_match = run.launch_trial()?.reached_target();
        Ok((is_match, run.into_runopts()))
    }

    /// Replay `schedule` (written at `schedule_path`) for the stacks of the
    /// critical pair, its events `index - 1` and `index`, recording the
    /// schedule the replay realized. A replay that a bound stops or Hermit
    /// refuses still returns what it captured; one that analyze's own bound
    /// stops runs again once with a longer one first
    /// ([`Self::stack_trace_trial`]).
    fn replay_for_stacks(
        &self,
        runname: &str,
        schedule_path: &Path,
        schedule: &[SchedEvent],
        index: usize,
    ) -> anyhow::Result<StackReplay> {
        let stack_paths = |run: &RunData| {
            let root_path = run.root_path();
            [
                root_path.with_extension("stack1"),
                root_path.with_extension("stack2"),
            ]
        };
        let (end, mut run, _) = self.stack_trace_trial(runname, |name| {
            let mut run = RunData::new_baseline(self, name.to_string())?
                .with_schedule_replay_from(schedule_path.to_path_buf())
                .with_schedule_recording();
            let [first, second] = stack_paths(&run);
            run.runopts.det_opts.det_config.stacktrace_event = vec![
                (index as u64 - 1, Some(first)),
                (index as u64, Some(second)),
            ];
            Ok(run)
        })?;
        let realized = run.sched_path_out().to_path_buf();
        let paths = stack_paths(&run);
        let finished = !matches!(end, TrialEnd::Unfinished(_));
        if !finished {
            eprintln!(
                ":: {} the stack-trace replay {} did not finish ({}); using the stacks it \
                 recorded",
                "WARNING:".yellow().bold(),
                run.name(),
                describe_trial_end(end, run.trial_bound()),
            );
        }
        eprintln!(
            "\n:: {}\n{}",
            if finished {
                "Stack-trace replay finished.  Repro command:"
            } else {
                "Stack-trace replay stopped.  Repro command:"
            }
            .green()
            .bold(),
            run.to_repro()
        );
        let mut stacks = [None, None];
        for (stack, path) in stacks.iter_mut().zip(&paths) {
            let Ok(file) = File::open(path) else {
                continue;
            };
            match serde_json::from_reader(file) {
                Ok(parsed) => *stack = Some(parsed),
                Err(error) => eprintln!(
                    ":: {} cannot read the stack trace in {} ({error}); it is left out",
                    "WARNING:".yellow().bold(),
                    path.display()
                ),
            }
        }
        let (left_schedule, ran) = if realized.exists() {
            let realized = PreemptionReader::new(&realized).load_all().into_global();
            let ran_at = |event: usize| realized.get(event).map(|event| event.dettid);
            (
                departure(schedule, &realized, index + 1),
                [ran_at(index - 1), ran_at(index)],
            )
        } else {
            (Some(Departure::Unknown), [None, None])
        };
        Ok(StackReplay {
            finished,
            met_target: end == TrialEnd::Met,
            stacks,
            left_schedule,
            ran,
        })
    }

    /// One `--prefix-search` trial: replay the first `len` events of `target`,
    /// then let the scheduler go on without chaos, with timeslices of
    /// `continuation`. The trial's record keeps the callback checkpoints
    /// within the prefix ([`PreemptionRecord::schedule_prefix`]), so its
    /// slices end where the recording's did, all but the one running at the
    /// prefix's end: where that slice ends decides what runs next, which is
    /// event `len`'s decision. A trial that replayed it would already make
    /// that decision, and the search would name the event before it, with
    /// the thread that ran up to it on both sides of the pair.
    fn prefix_trial(
        &self,
        runname: &str,
        target: &PreemptionRecord,
        len: usize,
        continuation: MaybeTimeslice,
    ) -> anyhow::Result<RunData> {
        let mut run = RunData::new_baseline(self, runname.to_string())?;
        run.runopts.det_opts.det_config.chaos = false;
        run.runopts.det_opts.det_config.max_timeslice = continuation;
        let path = run.sched_path_in().to_path_buf();
        self.write_trial_record(target.schedule_prefix(len), &path)?;
        Ok(run.with_schedule_replay_from(path))
    }

    /// Launch the search trial `make(name)` creates, and say how it ended
    /// and which run that end came from. A trial that analyze's own
    /// wall-clock bound stopped ([`Stop::Backstop`]) has no outcome: the
    /// host's speed decided when the bound fired. It runs again at once as
    /// `<name>_retry`, with [`CONFIRM_BOUND_FACTOR`] times the bound (within
    /// the run arguments' own `--timeout` and the time budget left), and the
    /// rerun's end counts instead. A rerun that bound stops again ends the
    /// search with [`TrialInconclusive`], which `end_on_budget` reports as a
    /// partial result; `what` says what the trial replays and `prefix` is a
    /// prefix search's prefix, for that report. A stop by the run arguments'
    /// own `--timeout` and a refusal are ends like any other.
    ///
    /// The rerun is named after the trial, so that whether a rerun was
    /// needed does not shift the names, and the journal keys, of the trials
    /// after it.
    pub(super) fn decisive_trial(
        &self,
        name: &str,
        what: &str,
        prefix: Option<usize>,
        make: impl Fn(&str) -> anyhow::Result<RunData>,
    ) -> anyhow::Result<(TrialEnd, RunData)> {
        self.decisive_trial_unless(name, what, prefix, make, |_| false)
    }

    /// [`Self::decisive_trial`], except that a run `settled` says is
    /// settled already, however it ended, is not run again: the bisection's
    /// round that left its spliced schedule has no outcome for that schedule
    /// whether or not it finished, and a rerun would leave it at the same
    /// event.
    pub(super) fn decisive_trial_unless(
        &self,
        name: &str,
        what: &str,
        prefix: Option<usize>,
        make: impl Fn(&str) -> anyhow::Result<RunData>,
        settled: impl Fn(&RunData) -> bool,
    ) -> anyhow::Result<(TrialEnd, RunData)> {
        let mut run = make(name)?;
        self.show_trial_repro(&run);
        let end = run.launch_trial()?;
        if end != TrialEnd::Unfinished(Stop::Backstop) || settled(&run) {
            return Ok((end, run));
        }
        let retry_name = format!("{name}_retry");
        let mut retry = make(&retry_name)?.with_bound_factor(CONFIRM_BOUND_FACTOR);
        let seconds = |bound: Option<u64>| bound.map_or("?".to_owned(), |bound| bound.to_string());
        eprintln!(
            ":: {name} did not finish within hermit analyze's wall-clock bound of {} s. When \
             that bound fires is the host's, so the trial has no outcome; running it again as \
             {retry_name} with a bound of {} s",
            seconds(run.trial_bound()),
            seconds(
                retry
                    .runopts
                    .run_timeout_seconds()
                    .map(std::num::NonZeroU64::get)
            ),
        );
        self.show_trial_repro(&retry);
        let end = retry.launch_trial()?;
        if end == TrialEnd::Unfinished(Stop::Backstop) && !settled(&retry) {
            return Err(TrialInconclusive {
                trial: what.to_owned(),
                prefix,
                runs: [name.to_owned(), retry_name],
                bounds: [run.trial_bound(), retry.trial_bound()],
                next_continuation: None,
            }
            .into());
        }
        Ok((end, retry))
    }

    /// With `--verbose`, the command that reproduces trial `run`.
    fn show_trial_repro(&self, run: &RunData) {
        if self.verbose {
            eprintln!(
                ":: {}\n    {}",
                format!("Trial {}, repro command:", run.name())
                    .yellow()
                    .bold(),
                run.to_repro()
            );
        }
    }

    /// `--prefix-search`: find the shortest prefix of the target schedule that,
    /// replayed and then continued without chaos, still meets the target
    /// criteria. Returns the index of that prefix's last event.
    pub fn phase5_prefix_search(
        &mut self,
        target_record: &PreemptionRecord,
    ) -> anyhow::Result<PrefixDecision> {
        let target = target_record.schedevents();
        let candidates = match self.continuation_timeslice {
            ContinuationTimeslice::Auto => continuation_candidates(self.trial_max_timeslice()),
            ContinuationTimeslice::Nanos(nanos) => vec![Some(nanos)],
        };
        self.progress.phase(Phase::PrefixSearch);
        let mut trial = 0;
        let mut refused = 0;
        let ends = std::cell::RefCell::new(Ends::new());
        // The run of each prefix's last trial, per continuation.
        let mut runs = BTreeMap::new();
        let bound_of = |len: usize, continuation: MaybeTimeslice| {
            ends.borrow()
                .get(&(len, continuation))
                .and_then(|&(_, bound)| bound)
        };
        let mut run_trial =
            |len: usize, continuation: MaybeTimeslice| -> anyhow::Result<TrialEnd> {
                trial += 1;
                let (end, run) = self.decisive_trial(
                    &format!("prefix_round_{trial:03}"),
                    &format!(
                        "replaying the first {len} events and then scheduling without chaos {}",
                        describe_continuation(continuation)
                    ),
                    Some(len),
                    |name| self.prefix_trial(name, target_record, len, continuation),
                )?;
                let runname = run.name().to_owned();
                let bound = run
                    .stopped_by()
                    .or_else(|| run.runopts.run_timeout().map(|bound| bound.as_secs()));
                refused += usize::from(end == TrialEnd::Unfinished(Stop::Refusal));
                eprintln!(
                    ":: Prefix of {len} of {} events => {}",
                    target.len(),
                    match end {
                        TrialEnd::Met => format!("target condition ({})", self.display_criteria()),
                        TrialEnd::Missed => "baseline condition".to_string(),
                        TrialEnd::Unfinished(_) => format!(
                            "no outcome: it {}, counted as not meeting the target",
                            describe_trial_end(end, bound)
                        ),
                    }
                );
                ends.borrow_mut().insert((len, continuation), (end, bound));
                runs.insert((len, continuation), runname);
                self.progress.bracket(prefix_bracket(
                    &ends.borrow(),
                    &runs,
                    continuation,
                    target.len(),
                ));
                Ok(end)
            };

        let continuation = choose_continuation(&candidates, target.len(), &mut run_trial)?;
        eprintln!(
            ":: Searching the prefixes continued without chaos {}",
            describe_continuation(continuation)
        );
        let empty = ends.borrow()[&(0, continuation)].0;
        let Boundary {
            passing,
            passing_end,
        } = search_prefix_boundary(target.len(), empty, |len| run_trial(len, continuation))?;
        let failing = passing + 1;
        // On a host whose branch counter is not exact, two replays of one
        // prefix can differ, so the boundary is checked once more. A trial
        // the bound stopped has no outcome to repeat, so such a baseline side
        // is not run again, and a check the bound stops shows a trial close
        // to the bound, not another outcome.
        let mut rechecks = vec![(failing, TrialEnd::Met)];
        if passing_end != TrialEnd::Unfinished(Stop::Bound) {
            rechecks.insert(0, (passing, passing_end));
        }
        let (mut changed, mut unfinished_again) = (Vec::new(), Vec::new());
        for (len, first) in rechecks {
            let first_bound = bound_of(len, continuation);
            // A recheck only warns: one that analyze's bound stopped twice is
            // a trial close to the bound, not a reason to drop the decision.
            // `decisive_trial` never ends a trial as stopped by analyze's
            // bound (it reruns it, then errs), so this mapping is the only
            // source of the `Stop::Backstop` the warning below tests for.
            let (again, again_bound) = match run_trial(len, continuation) {
                Err(error) if error.downcast_ref::<TrialInconclusive>().is_some() => {
                    let stopped = error.downcast::<TrialInconclusive>()?;
                    (TrialEnd::Unfinished(Stop::Backstop), stopped.bounds[1])
                }
                again => (again?, bound_of(len, continuation)),
            };
            if again == first {
                continue;
            }
            let what = format!(
                "the prefix of {len} events {} and then {}",
                describe_trial_end(first, first_bound),
                describe_trial_end(again, again_bound)
            );
            if matches!(again, TrialEnd::Unfinished(Stop::Bound | Stop::Backstop)) {
                unfinished_again.push(what);
            } else if again.reached_target() != first.reached_target() {
                changed.push(what);
                // As before: one other outcome is enough to warn.
                break;
            }
        }
        if !changed.is_empty() {
            eprintln!(
                ":: {} replaying the prefixes of {passing} and {failing} events again gave other \
                 outcomes ({}); the decision below may not repeat",
                "WARNING:".yellow().bold(),
                changed.join("; ")
            );
        }
        if !unfinished_again.is_empty() {
            eprintln!(
                ":: {} {}: these trials run close to their wall-clock bound, so the search may \
                 have counted trials that meet the target given longer as not meeting it, and an \
                 earlier event decide the outcome ({})",
                "WARNING:".yellow().bold(),
                unfinished_again.join("; "),
                bound_remedy(
                    self.run_args_timeout(),
                    self.trial_timeout == TrialTimeout::Off,
                    self.effective_trial_bound(),
                )
            );
        }
        let refused = refused_trials_warning(refused);
        if let Some(warning) = &refused {
            eprintln!(":: {} {warning}", "WARNING:".yellow().bold());
        }
        let unfinished = (passing_end == TrialEnd::Unfinished(Stop::Bound)).then(|| {
            let below = longest_finished_below(&ends.borrow(), continuation, passing);
            let bound = bound_of(passing, continuation);
            let remedy = bound_remedy(
                self.run_args_timeout(),
                self.trial_timeout == TrialTimeout::Off,
                bound,
            );
            unfinished_boundary_warning(passing, bound, below, remedy)
        });
        if let Some(warning) = &unfinished {
            eprintln!(":: {} {warning}", "WARNING:".yellow().bold());
        }
        eprintln!(
            ":: The scheduling decision at event #{passing} of {} decides the outcome: {}",
            target.len(),
            target[passing]
        );
        Ok(PrefixDecision {
            schedule: target_record.clone(),
            decision: passing,
            refused,
            unfinished,
            continuation,
            fallback: None,
        })
    }

    /// Replay the first `len` events of `schedule` as a prefix trial and
    /// return whether it met the target with the stack at schedule position
    /// `position`: a replayed event, or, at `len`, the first event the
    /// scheduler ran after the prefix.
    fn prefix_stack(
        &self,
        runname: &str,
        schedule: &PreemptionRecord,
        len: usize,
        position: usize,
        continuation: MaybeTimeslice,
    ) -> anyhow::Result<PrefixStack> {
        let (end, run, stopped) = self.stack_trace_trial(runname, |name| {
            let mut run = self.prefix_trial(name, schedule, len, continuation)?;
            let stack_path = run.root_path().with_extension("stack");
            run.runopts.det_opts.det_config.stacktrace_event =
                vec![(position as u64, Some(stack_path))];
            Ok(run)
        })?;
        eprintln!(
            ":: {}\n{}",
            "Stack-trace run completed.  Repro command:".green().bold(),
            run.to_repro()
        );
        let stack_path = run.root_path().with_extension("stack");
        let stack = match File::open(&stack_path) {
            Ok(file) => Some(serde_json::from_reader(file).with_context(|| {
                format!("cannot parse the stack trace in {}", stack_path.display())
            })?),
            Err(_) => None,
        };
        Ok(PrefixStack {
            finished: !matches!(end, TrialEnd::Unfinished(_)),
            met: end.reached_target(),
            stack,
            stopped,
        })
    }

    /// Launch the stack-trace replay `make(name)` creates. It decides
    /// nothing (the search has decided), but a wall-clock bound stops it at a
    /// time the host chooses, so a stop must not change the kind of answer
    /// either. One that analyze's own bound stopped runs again once as
    /// `<name>_retry` with [`CONFIRM_BOUND_FACTOR`] times the bound, within
    /// the run arguments' own `--timeout`; a stop by that `--timeout` cannot
    /// be given longer. Returns how the last run ended, that run, and, when
    /// a wall-clock bound stopped it, what to say in place of what it would
    /// have shown.
    fn stack_trace_trial(
        &self,
        name: &str,
        make: impl Fn(&str) -> anyhow::Result<RunData>,
    ) -> anyhow::Result<(TrialEnd, RunData, Option<String>)> {
        let seconds = |bound: Option<u64>| bound.map_or("?".to_owned(), |bound| bound.to_string());
        let mut run = make(name)?;
        self.show_trial_repro(&run);
        let mut end = run.launch_trial()?;
        if end == TrialEnd::Unfinished(Stop::Backstop) {
            let retry_name = format!("{name}_retry");
            let retry = make(&retry_name)?.with_bound_factor(CONFIRM_BOUND_FACTOR);
            eprintln!(
                ":: the stack-trace replay {name} did not finish within hermit analyze's \
                 wall-clock bound of {} s; running it again as {retry_name} with a bound of {} s",
                seconds(run.trial_bound()),
                seconds(
                    retry
                        .runopts
                        .run_timeout_seconds()
                        .map(std::num::NonZeroU64::get)
                ),
            );
            run = retry;
            self.show_trial_repro(&run);
            end = run.launch_trial()?;
        }
        let stopped = match end {
            TrialEnd::Unfinished(Stop::Backstop) => Some(format!(
                "the stack-trace replay {} stopped at hermit analyze's wall-clock bound ({} s)",
                run.name(),
                seconds(run.trial_bound())
            )),
            TrialEnd::Unfinished(Stop::Bound) => Some(format!(
                "the stack-trace replay {} stopped at --timeout={}",
                run.name(),
                seconds(
                    run.runopts
                        .run_timeout_seconds()
                        .map(std::num::NonZeroU64::get)
                )
            )),
            _ => None,
        };
        if let Some(stopped) = &stopped {
            eprintln!(
                ":: {} {stopped}: what it would have shown is missing from the report, and the \
                 decision stands",
                "NOTE:".yellow().bold()
            );
        }
        Ok((end, run, stopped))
    }

    /// The schedule a prefix trial of the first `len` events of `schedule`
    /// realized: the replayed events, then the events the scheduler ran after
    /// them, the file it is recorded in, and what stopped the trial when a
    /// wall-clock bound did ([`AnalyzeOpts::stack_trace_trial`]). Empty if
    /// the trial wrote no schedule, or was stopped.
    fn prefix_continuation(
        &self,
        runname: &str,
        schedule: &PreemptionRecord,
        len: usize,
        continuation: MaybeTimeslice,
    ) -> anyhow::Result<(Vec<SchedEvent>, PathBuf, Option<String>)> {
        let (_, mut run, stopped) = self.stack_trace_trial(runname, |name| {
            Ok(self
                .prefix_trial(name, schedule, len, continuation)?
                .with_schedule_recording())
        })?;
        let path = run.sched_path_out().to_path_buf();
        let realized = if stopped.is_none() && path.exists() {
            PreemptionReader::new(&path).load_all().into_global()
        } else {
            Vec::new()
        };
        Ok((realized, path, stopped))
    }

    /// The report of `--prefix-search`: the stacks of the two threads that
    /// could run at the deciding event.
    pub fn phase6_record_prefix_outputs(&self, found: PrefixDecision) -> anyhow::Result<Report> {
        let PrefixDecision {
            schedule,
            decision,
            refused,
            unfinished,
            continuation,
            fallback,
        } = found;
        self.progress.phase(Phase::StackTraces);
        self.progress.decided(
            format!(
                "event #{decision} of the target schedule decides the outcome ({})",
                schedule.schedevents()[decision]
            ),
            DecidedEvents::Deciding(decision),
        );
        let notes: Vec<String> = [fallback, refused, unfinished.clone()]
            .into_iter()
            .flatten()
            .collect();
        let target = self.prefix_stack(
            "final_target_for_stacktraces",
            &schedule,
            decision + 1,
            decision,
            continuation,
        )?;
        // A stack-trace replay a wall-clock bound stopped leaves its part of
        // the report missing, and says so; it never changes the decision.
        let mut missing = Vec::new();
        // The thread the scheduler runs at event #decision once the prefix
        // ends. `--stacktrace-event` names a position of the replayed schedule
        // only, so the prefix trial without event #decision runs first with its
        // schedule recorded, and the stack is taken from a replay of that
        // schedule's first decision + 1 events.
        let (continued, continued_path, continued_stopped) =
            self.prefix_continuation("final_baseline_schedule", &schedule, decision, continuation)?;
        if continued
            .get(decision)
            .is_some_and(|event| event.dettid == schedule.schedevents()[decision].dettid)
            && let Some(report) = self.report_timing_decision(
                &schedule,
                decision,
                continuation,
                &target,
                (&continued, &continued_path),
                &notes,
            )?
        {
            return Ok(report);
        }
        if target.stack.is_none()
            && let Some(stopped) = &target.stopped
        {
            missing.push(format!("stack1 is missing: {stopped}."));
        }
        let hang = unfinished.is_some() && continued.is_empty();
        let baseline = if continued.len() > decision {
            let baseline = continued_record(&schedule, decision, &continued);
            let baseline = self.prefix_stack(
                "final_baseline_for_stacktraces",
                &baseline,
                decision + 1,
                decision,
                continuation,
            )?;
            if baseline.stack.is_none()
                && let Some(stopped) = &baseline.stopped
            {
                missing.push(format!("stack2 is missing: {stopped}."));
            }
            Some(baseline)
        } else if hang {
            eprintln!(
                ":: {} the run without event #{decision} did not finish and recorded no \
                 schedule, so stack2 is empty",
                "Note:".yellow().bold(),
            );
            None
        } else if let Some(stopped) = &continued_stopped {
            missing.push(format!(
                "which thread runs at event #{decision} without the event, and so stack2, is \
                 missing: {stopped}."
            ));
            None
        } else {
            eprintln!(
                ":: {} the run without event #{decision} ended after {} events, so no thread \
                 runs there instead and stack2 is empty",
                "WARNING:".yellow().bold(),
                continued.len()
            );
            None
        };
        // Only finished replays have outcomes to compare with the search's.
        let baseline_met = baseline.as_ref().is_some_and(|run| run.met);
        if stack_runs_contradict(&[&target], &baseline.iter().collect::<Vec<_>>()) {
            eprintln!(
                ":: {} the stack-trace runs did not repeat the search's outcomes (target met: {}, \
                 baseline met: {}); read the stacks with care",
                "WARNING:".yellow().bold(),
                target.met,
                baseline_met
            );
        }
        let (chosen, instead) = (target.stack, baseline.and_then(|run| run.stack));

        let misses = if unfinished.is_some() {
            "does not finish, which the search counts as not meeting the target criteria (a hang, \
             see the note below)"
        } else {
            "does not meet the target criteria"
        };
        let mut header = format!(
            "The scheduling decision at event #{decision} of the target schedule decides the \
             outcome.\nReplaying the first {decision} events and then scheduling without chaos \
             {} {misses}; replaying event #{decision} too ({}) meets them ({}).\n",
            describe_continuation(continuation),
            schedule.schedevents()[decision],
            self.display_criteria()
        );
        if hang {
            header.push_str(
                "stack1 is the thread the target schedule runs at that point; stack2 is empty: the \
                 run without the event did not finish and recorded no schedule.\nWithout the event \
                 the guest may wait for something that never comes: look for the wait it hangs in \
                 (a lock or a join that nothing releases), which the event avoids.\n",
            );
        } else if continued_stopped.is_some() {
            header.push_str(
                "stack1 is the thread the target schedule runs at that point; the thread the \
                 scheduler runs there without the event is missing (see below).\n",
            );
        } else {
            header.push_str(&stack_pair_sentences(
                schedule.schedevents()[decision].dettid,
                continued.get(decision).map(|event| event.dettid),
            ));
        }
        for note in missing.iter().chain(&notes) {
            header.push_str(&format!("Note: {note}\n"));
        }
        let context = Self::generate_context(
            schedule.schedevents(),
            decision.max(1),
            self.execution_context,
        );

        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        if let Some(warning) = &self.branch_counter_warning {
            println!("{warning}");
        }
        println!("{}", header);
        Self::print_stack("stack1", &chosen);
        Self::print_stack("stack2", &instead);
        println!("\n{}", context);

        Ok(Report {
            header,
            additional_context: context,
            baseline_run: false,
            critical_event1: ReportCriticalEvent {
                event_index: decision,
                stack: chosen,
            },
            critical_event2: ReportCriticalEvent {
                event_index: decision,
                stack: instead,
            },
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event: None,
            partial: None,
        })
    }

    /// The report of `--prefix-search` when the scheduler runs the same
    /// thread at the deciding event without it (`continued`, recorded at
    /// `continued_path`, is the schedule without it) as the target schedule
    /// does: the decision is how long that thread runs, so the report shows
    /// the thread that runs next in each continuation, and the deciding
    /// thread's own stack (from `target`, a trial that met the target or
    /// not) separately. `None` when the continuation with the event does not
    /// bear that out (it does not run the thread there, or the thread runs
    /// exactly as long in both): the usual report follows. A continuation
    /// with the event that a wall-clock bound stopped leaves that side
    /// missing ([`Self::report_timing_decision_without_its_continuation`]),
    /// never another kind of report.
    fn report_timing_decision(
        &self,
        schedule: &PreemptionRecord,
        decision: usize,
        continuation: MaybeTimeslice,
        target: &PrefixStack,
        (continued, continued_path): (&[SchedEvent], &Path),
        notes: &[String],
    ) -> anyhow::Result<Option<Report>> {
        let events = schedule.schedevents();
        let thread = events[decision].dettid;
        let deciding = &target.stack;
        let (target_continued, target_continued_path, target_stopped) = self.prefix_continuation(
            "final_target_schedule",
            schedule,
            decision + 1,
            continuation,
        )?;
        let without = slice_from(continued, decision, thread);
        if let Some(stopped) = &target_stopped {
            return self
                .report_timing_decision_without_its_continuation(
                    schedule,
                    decision,
                    continuation,
                    (target, stopped),
                    (continued, continued_path),
                    notes,
                )
                .map(Some);
        }
        let with = slice_from(&target_continued, decision, thread);
        if with.events == 0 || (with.events, with.branches) == (without.events, without.branches) {
            eprintln!(
                ":: {} without event #{decision} the scheduler runs thread {thread} there too, \
                 but the run with it {}; reporting the two threads at that event instead",
                "WARNING:".yellow().bold(),
                if with.events == 0 {
                    "does not run it there (its continuation was not recorded, or left the \
                     schedule)"
                } else {
                    "runs it for exactly as long"
                }
            );
            return Ok(None);
        }
        // Each continuation replays under the checkpoints of the prefix it
        // continued.
        let with_record = continued_record(schedule, decision + 1, &target_continued);
        let without_record = continued_record(schedule, decision, continued);
        let next_stack = |runname: &str, variant: &PreemptionRecord, slice: &Slice| match slice.next
        {
            Some((index, _)) => self.prefix_stack(runname, variant, index + 1, index, continuation),
            None => Ok(PrefixStack::none()),
        };
        let next_target = next_stack("final_target_next_for_stacktraces", &with_record, &with)?;
        let next_baseline = next_stack(
            "final_baseline_next_for_stacktraces",
            &without_record,
            &without,
        )?;
        let mut missing: Vec<String> = [
            next_target.missing("stack1"),
            next_baseline.missing("stack2"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if stack_runs_contradict(&[target, &next_target], &[&next_baseline]) {
            eprintln!(
                ":: {} the stack-trace runs did not repeat the search's outcomes; read the \
                 stacks with care",
                "WARNING:".yellow().bold(),
            );
        }
        let next_index = |variant: &[SchedEvent], slice: &Slice| {
            slice.next.map_or(variant.len(), |(index, _)| index)
        };
        let mut critical = [
            (next_index(&target_continued, &with), next_target.stack),
            (next_index(continued, &without), next_baseline.stack),
        ];
        // When the threads that run next have the same stack in both
        // continuations, those stacks tell nothing apart; where the deciding
        // thread stops does. Its stacks replace them only when both were
        // captured.
        let mut slice_ends = slice_end_events(
            &with,
            &without,
            [critical[0].1.as_ref(), critical[1].1.as_ref()],
        );
        if let Some(ends) = slice_ends {
            let stack_at = |runname: &str, variant: &PreemptionRecord, end: usize| {
                if end == decision {
                    // A slice of one event ends where it starts.
                    Ok(deciding.clone())
                } else {
                    // Without both, the stacks of the threads that run next
                    // stay: a stopped replay here leaves nothing missing.
                    self.prefix_stack(runname, variant, end + 1, end, continuation)
                        .map(|replay| replay.stack)
                }
            };
            let with_end = stack_at(
                "final_target_slice_end_for_stacktraces",
                &with_record,
                ends[0],
            )?;
            let without_end = stack_at(
                "final_baseline_slice_end_for_stacktraces",
                &without_record,
                ends[1],
            )?;
            match (with_end, without_end) {
                (Some(with_end), Some(without_end)) => {
                    critical = [(ends[0], Some(with_end)), (ends[1], Some(without_end))];
                }
                _ => slice_ends = None,
            }
        }
        let [(event1, stack1), (event2, stack2)] = critical;
        let mut header = timing_decision_header(
            decision,
            &events[decision],
            (&with, &without),
            &self.display_criteria(),
            continuation,
            slice_ends,
        );
        if slice_ends.is_some()
            && let (Some(first), Some(second)) = (&stack1, &stack2)
            && same_frames(first, second)
        {
            header.push_str(&format!(
                "Thread {thread} has the same stack at both events: what differs is how far it \
                 gets before the other thread runs, not where it stops.\n"
            ));
        }
        let name = |path: &Path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        header.push_str(&format!(
            "Event numbers after #{decision} are positions in each continuation's recorded \
             schedule, in the workspace: {} with the event, {} without it.\n",
            name(&target_continued_path),
            name(continued_path),
        ));
        missing.extend(notes.iter().cloned());
        for note in &missing {
            header.push_str(&format!("Note: {note}\n"));
        }
        let context = format!(
            "With event #{decision} replayed:\n{}\nWithout it:\n{}",
            Self::generate_context(
                &target_continued,
                with.next.map_or(decision.max(1), |(index, _)| index),
                self.execution_context
            ),
            Self::generate_context(
                continued,
                without.next.map_or(decision.max(1), |(index, _)| index),
                self.execution_context
            ),
        );

        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        if let Some(warning) = &self.branch_counter_warning {
            println!("{warning}");
        }
        println!("{}", header);
        println!("Thread {thread} at the deciding event #{decision}:");
        Self::print_stack("the deciding thread's stack", deciding);
        Self::print_stack("stack1", &stack1);
        Self::print_stack("stack2", &stack2);
        println!("\n{}", context);

        Ok(Some(Report {
            header,
            additional_context: context,
            baseline_run: false,
            critical_event1: ReportCriticalEvent {
                event_index: event1,
                stack: stack1,
            },
            critical_event2: ReportCriticalEvent {
                event_index: event2,
                stack: stack2,
            },
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event: Some(ReportCriticalEvent {
                event_index: decision,
                stack: deciding.clone(),
            }),
            partial: None,
        }))
    }

    /// [`Self::report_timing_decision`] when the replay that records the
    /// continuation with the event was stopped by a wall-clock bound
    /// (`stopped` says which). The decision stands, but not its kind: the
    /// scheduler runs the deciding thread at that event without the event
    /// too (`continued`, recorded at `continued_path`), and whether the run
    /// with the event runs it longer, which would make it a timing decision,
    /// is what is missing ([`timing_decision_missing_header`]), with stack1.
    /// The side without the event is shown.
    fn report_timing_decision_without_its_continuation(
        &self,
        schedule: &PreemptionRecord,
        decision: usize,
        continuation: MaybeTimeslice,
        (target, stopped): (&PrefixStack, &str),
        (continued, continued_path): (&[SchedEvent], &Path),
        notes: &[String],
    ) -> anyhow::Result<Report> {
        let events = schedule.schedevents();
        let thread = events[decision].dettid;
        let without = &slice_from(continued, decision, thread);
        let next_baseline = match without.next {
            Some((index, _)) => self.prefix_stack(
                "final_baseline_next_for_stacktraces",
                &continued_record(schedule, decision, continued),
                index + 1,
                index,
                continuation,
            )?,
            None => PrefixStack::none(),
        };
        if stack_runs_contradict(&[target], &[&next_baseline]) {
            eprintln!(
                ":: {} the stack-trace runs did not repeat the search's outcomes; read the \
                 stacks with care",
                "WARNING:".yellow().bold(),
            );
        }
        let mut header = timing_decision_missing_header(
            decision,
            &events[decision],
            without,
            &self.display_criteria(),
            continuation,
            stopped,
        );
        header.push_str(&format!(
            "Event numbers after #{decision} are positions in the recorded schedule of the \
             continuation without it, in the workspace: {}.\n",
            continued_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
        for note in next_baseline.missing("stack2").iter().chain(notes) {
            header.push_str(&format!("Note: {note}\n"));
        }
        let next = without.next.map(|(index, _)| index);
        let context = format!(
            "Without event #{decision}:\n{}",
            Self::generate_context(
                continued,
                next.unwrap_or(decision.max(1)),
                self.execution_context
            ),
        );

        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        println!("{}", header);
        println!("Thread {thread} at the deciding event #{decision}:");
        Self::print_stack("the deciding thread's stack", &target.stack);
        Self::print_stack("stack1", &None);
        Self::print_stack("stack2", &next_baseline.stack);
        println!("\n{}", context);

        Ok(Report {
            header,
            additional_context: context,
            baseline_run: false,
            critical_event1: ReportCriticalEvent {
                event_index: decision,
                stack: None,
            },
            critical_event2: ReportCriticalEvent {
                event_index: next.unwrap_or(continued.len()),
                stack: next_baseline.stack,
            },
            deciding_event: Some(ReportCriticalEvent {
                event_index: decision,
                stack: target.stack.clone(),
            }),
            partial: None,
        })
    }

    /// It's weird if no filter is specified.
    fn has_filters(&self) -> bool {
        self.target_stdout.is_some()
            || self.target_stderr.is_some()
            || self.target_exit_code != ExitStatusConstraint::Any
    }

    /// Is the criteria the most common setting of target = failure = nonzero exit.
    fn is_vanilla_criteria(&self) -> bool {
        self.target_stdout.is_none()
            && self.target_stderr.is_none()
            && self.target_exit_code == ExitStatusConstraint::NonZero
    }

    /// Write analyze's final on-target and baseline schedules, stamped
    /// ([`Self::stamp_trial_record`]), and say where.
    pub fn write_final_schedules(
        &self,
        failing: Vec<SchedEvent>,
        failing_path: &Path,
        passing: Vec<SchedEvent>,
        passing_path: &Path,
    ) {
        self.write_trial_schedule(failing, failing_path).unwrap();
        eprintln!(
            "Wrote final on-target ({}) schedule to {}",
            self.display_criteria(),
            failing_path.display()
        );
        self.write_trial_schedule(passing, passing_path).unwrap();
        eprintln!(
            "Wrote final baseline (off-target) schedule to {}",
            passing_path.display()
        );
    }

    fn display_criteria(&self) -> String {
        let mut strs: Vec<String> = Vec::new();
        match &self.target_exit_code {
            ExitStatusConstraint::Exact(c) => {
                strs.push(format!("exit code={}", c));
            }
            ExitStatusConstraint::NonZero => {
                strs.push("nonzero exit".to_string());
            }
            ExitStatusConstraint::Any => {}
        }
        if self.target_stdout.is_some() {
            strs.push("matching stdout".to_string());
        }
        if self.target_stderr.is_some() {
            strs.push("matching stderr".to_string());
        }
        strs.join(", ")
    }

    /// Create the workspace in `--tmp-dir` and make it `tmp_dir`, where every
    /// run puts its files. The returned guard removes or keeps it, as
    /// `retention` says, when it is dropped.
    fn phase0_initialize(&mut self, retention: Retention) -> anyhow::Result<Workspace> {
        let workspace = Workspace::create(self.tmp_dir.as_deref(), retention)?;
        eprintln!(
            ":: Temp workspace: {}{}",
            workspace.path().display(),
            match retention {
                Retention::Remove => " (removed when analyze exits; --keep-tmp keeps it)",
                Retention::Keep { .. } => "",
            }
        );
        self.tmp_dir = Some(workspace.path().to_path_buf());
        Ok(workspace)
    }

    /// Write the analysis's checkpoint into the new workspace and open its
    /// journal and progress there (`checkpoint`). Without --analyze-seed,
    /// the seed of analyze's own random choices is drawn here, once, and
    /// recorded, so that a resume makes the same choices.
    fn start_checkpoint(&mut self) -> anyhow::Result<()> {
        if self.analyze_seed.is_none() {
            self.analyze_seed = Some(rand::random());
            self.analyze_seed_drawn = true;
        }
        let workspace = self.get_tmp()?.to_path_buf();
        Checkpoint {
            version: CHECKPOINT_VERSION,
            hermit_version: crate::version::Version::get().to_owned(),
            identity: self.identity()?,
        }
        .write(&workspace)?;
        self.journal = Journal::open(&workspace)?;
        self.progress.write_into(&workspace);
        Ok(())
    }

    /// `--resume`: adopt the workspace `resumed` and its journal, after
    /// checking that this invocation analyzes what the recorded one did. A
    /// difference refuses (exit 122) and names each one.
    fn phase0_resume(&mut self, resumed: &Path) -> anyhow::Result<Workspace> {
        let refuse =
            |message: String| anyhow::Error::new(crate::container::PolicyRefusal).context(message);
        let checkpoint = Checkpoint::read(resumed).map_err(|error| {
            refuse(format!(
                "{error:#}. To analyze anyway, run the same command without --resume: that \
                 starts a new analysis (in a new workspace)."
            ))
        })?;
        let recorded = &checkpoint.identity;
        if self.analyze_seed.is_none() {
            self.analyze_seed = Some(recorded.analyze_seed);
        }
        let now = self.identity()?;
        let differences = recorded.differences(&now);
        if !differences.is_empty() {
            return Err(refuse(format!(
                "hermit analyze cannot resume the analysis in {}: it was of something else, so the \
                 outcomes its journal records do not apply. What differs:\n  - {}\nTo continue \
                 anyway, run the same command without --resume: that starts a new analysis (in a \
                 new workspace) and leaves this one as it is.",
                resumed.display(),
                differences.join("\n  - ")
            )));
        }
        let workspace = Workspace::adopt(resumed, self.workspace_retention())?;
        self.tmp_dir = Some(workspace.path().to_path_buf());
        self.journal = Journal::open(workspace.path())?;
        self.resumed_progress = Progress::read(workspace.path());
        if let Some(earlier) = &self.resumed_progress {
            self.progress.carry(earlier);
        }
        self.progress.write_into(workspace.path());
        eprintln!(
            ":: Resuming the analysis in {} (started by {}): {} runs are journaled, and each \
             takes its recorded outcome instead of running again.",
            workspace.path().display(),
            checkpoint.hermit_version,
            self.journal.len()
        );
        Ok(workspace)
    }

    /// Why the workspace stays, whatever its retention says, after the
    /// analysis ended with `result`: a partial report gave the command that
    /// continues it, or a resumed analysis did not finish, and its adopted
    /// workspace holds an earlier invocation's journaled runs.
    fn keep_workspace_because(&self, result: &anyhow::Result<ExitStatus>) -> Option<&'static str> {
        if self.partial_report_written {
            Some("a budget ended the analysis; --resume continues it, as the report above says")
        } else if self.resume.is_some() && result.is_err() {
            Some(
                "a resumed analysis that does not finish keeps its workspace and journal; \
                 --resume continues it again",
            )
        } else {
            None
        }
    }

    /// What this analysis is of: what a resume must find unchanged.
    fn identity(&self) -> anyhow::Result<Identity> {
        let hermit = FileIdentity {
            path: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe")),
            ..FileIdentity::of(Path::new("/proc/self/exe"))?
        };
        let runopts = RunData::raw_runopts(self);
        let program = find_program(runopts.program())
            .map(|path| FileIdentity::of(&path))
            .transpose()?;
        let unset = || "unset".to_owned();
        let target = vec![
            (
                "--target-stdout".to_owned(),
                self.target_stdout
                    .as_ref()
                    .map_or_else(unset, |regex| regex.as_str().to_owned()),
            ),
            (
                "--target-stderr".to_owned(),
                self.target_stderr
                    .as_ref()
                    .map_or_else(unset, |regex| regex.as_str().to_owned()),
            ),
            (
                "--target-exit-code".to_owned(),
                format!("{:?}", self.target_exit_code),
            ),
        ];
        let search = [
            ("--search", self.search.to_string()),
            ("--prefix-search", self.prefix_search.to_string()),
            ("--minimize", self.minimize.to_string()),
            ("--run-needleman", self.run_needleman.to_string()),
            ("--imprecise-search", self.imprecise_search.to_string()),
            ("--selfcheck", self.selfcheck.to_string()),
            ("--run1-seed", format!("{:?}", self.run1_seed)),
            ("--run2-seed", format!("{:?}", self.run2_seed)),
            ("--jitter-dist", format!("{:?}", self.jitter_dist)),
            (
                "--continuation-timeslice",
                format!("{:?}", self.continuation_timeslice),
            ),
            // Not --trial-timeout: a search trial its bound stops has no
            // outcome (`decisive_trial`), so it decides only when analyze
            // stops, and a resume may give stopped trials more time.
            ("--max-log-bytes", format!("{:?}", self.max_log_bytes)),
            ("--backend", format!("{:?}", self.backend)),
        ]
        .into_iter()
        .map(|(option, value)| (option.to_owned(), value))
        .collect();
        let mut records = Vec::new();
        for (option, path) in [
            ("--run1-preemptions", &self.run1_preemptions),
            ("--run2-preemptions", &self.run2_preemptions),
            ("--run1-schedule", &self.run1_schedule),
            ("--run2-schedule", &self.run2_schedule),
        ] {
            if let Some(path) = path {
                records.push((option.to_owned(), FileIdentity::of(path)?));
            }
        }
        Ok(Identity {
            hermit,
            program,
            run_args: self.run_arg.iter().chain(&self.run_args).cloned().collect(),
            target,
            search,
            records,
            analyze_seed: self
                .analyze_seed
                .context("the analyze seed is set before the identity is taken")?,
        })
    }

    /// What becomes of analyze's workspace: kept with `--keep-tmp`, and with
    /// `--verbose`, whose per-run logs are in it for the user to read;
    /// removed otherwise.
    fn workspace_retention(&self) -> Retention {
        if self.keep_tmp {
            Retention::Keep {
                because: Some("--keep-tmp"),
            }
        } else if self.verbose {
            Retention::Keep {
                because: Some("--verbose"),
            }
        } else {
            Retention::Remove
        }
    }

    /// Create our workspace and verify the input run matches the criteria, or find one that does.
    /// Returns the results of the target run.
    fn phase1_establish_target_run(&mut self) -> Result<RunData, Error> {
        let mut run1data = RunData::new_run1_target(self, "run1_target".to_string())?;
        eprintln!(
            // `to_repro` is already a complete `hermit ... run ...` command.
            ":: {} {}",
            "Studying target execution: ".yellow().bold(),
            run1data.to_repro()
        );

        if let Some(p) = &self.run1_preemptions {
            let preempts_path = run1data.preempts_path_out();
            // Copy preempts to the output location as though they were recorded by this run:
            std::fs::copy(p, preempts_path).expect("copy file to succeed");
            if self.trial_timeout == TrialTimeout::Auto {
                eprintln!(
                    ":: --run1-preemptions runs no target run to measure, so runs are not \
                     bounded until analyze replays the record to verify it (not every search \
                     does); pass --trial-timeout=SECONDS to bound every run."
                );
            }
            // Careful, returning a NON-launched RunData just to contain the output path.
            return Ok(run1data);
        }

        // Translate the seed into a set of preemptions we can work from.
        let mut run1data = run1data.with_preemption_recording();
        run1data.launch()?;
        self.record_endpoint(&run1data);

        if !run1data.is_a_match() {
            if self.search {
                eprintln!(
                    ":: {}",
                    "First run did not match target criteria; now searching for a matching run..."
                        .red()
                        .bold()
                );
                self.progress.phase(Phase::SearchForTarget);
                self.do_search()
            } else {
                bail!("FAILED. The run did not match the target criteria. Try --search.");
            }
        } else if self.has_filters() {
            eprintln!(
                ":: {}",
                format!(
                    "First run matched target criteria ({}).",
                    self.display_criteria()
                )
                .green()
                .bold(),
            );
            Ok(run1data)
        } else {
            eprintln!(":: {}", "WARNING: run without any --filter arguments, so accepting ALL runs. This is probably not what you wanted.".red().bold());
            Ok(run1data)
        }
    }

    /// Reduce the set of preemptions needed to match the criteria.
    ///
    /// Takes the input (non-minimized) preemptions as a file path and returns the minimized
    /// preemptions as a data structure in memory.
    ///
    /// # Returns
    /// - The on-target run with minimized schedule.
    fn phase2_minimize(
        &self,
        global: &GlobalOpts,
        mut last_run: RunData,
    ) -> anyhow::Result<RunData> {
        if self.minimize {
            // In this scenario we need to work with preemptions.
            self.progress.phase(Phase::Minimize);

            let preempts_path = last_run.preempts_path_out();
            let (min_pr, min_pr_path, min_log_path) = self.minimize(preempts_path, global)?;
            eprintln!(
                ":: {}\n {}",
                "Successfully minimized to these critical interventions:"
                    .green()
                    .bold(),
                truncated(1000, serde_json::to_string(&min_pr).unwrap())
            );

            // TEMP: construct a RunData post-facto, until we finish the minimize overhaul:
            let runname = "minimized".to_string();
            let min_run = RunData::from_minimize_output(
                self,
                runname,
                last_run.runopts.clone(),
                min_pr,
                min_pr_path,
                min_log_path,
            );
            Ok(min_run)
        } else {
            // In this scenario we only care about event traces, and never really need to work with
            // preemptions.  Still, we'll need to do another run to record the trace.
            Ok(last_run)
        }
    }

    /// A weaker log difference that does not expect certain lines to be conserved in preemption replay.
    fn log_diff_preemption_replay(
        &self,
        global: &GlobalOpts,
        run1_log_path: &Path,
        run2_log_path: &Path,
    ) -> ExitStatus {
        if self.verbose {
            yellow_msg("[comparing] with log-diff command:");
            eprintln!(
                "    hermit log-diff --record-envelope=all-records-v1 \
                 --ignore-lines=CHAOSRAND {} {}",
                run1_log_path.display(),
                run2_log_path.display(),
            );
        }
        let mut ldopts = LogDiffCLIOpts::new(run1_log_path, run2_log_path);
        ldopts.more.ignore_lines = vec!["CHAOSRAND".to_string(), "SCHEDRAND".to_string()];
        ldopts.main(global)
    }

    /// Optionally do an extra run to verify that preemptions replay and yield the exact same
    /// execution.a
    pub fn phase3_strict_preempt_replay_check(
        &self,
        global: &GlobalOpts,
        min_run: &mut RunData,
    ) -> Result<(), Error> {
        let min_log_path = &min_run.log_path().unwrap().to_path_buf();
        let min_preempts_path = min_run.preempts_path_out().to_path_buf();

        if self.selfcheck {
            self.progress.phase(Phase::Selfcheck);
            yellow_msg("[selfcheck] Verifying target run preserved under preemption-replay");

            let runname = "run1b_selfcheck";
            let mut run1b = RunData::new_run1_replay(self, runname.to_string(), &min_run.runopts)?
                .with_preempts_path_in(min_preempts_path.clone())
                .with_preemption_recording();
            eprintln!("    {}", run1b.to_repro());

            run1b.launch()?;
            yellow_msg("[selfcheck] Comparing output from additional run (run1 vs run1b)");
            let run1b_log_path = run1b.log_path().unwrap();
            let status = self.log_diff_preemption_replay(global, min_log_path, run1b_log_path);

            if !run1b.is_a_match() {
                bail!("First run matched criteria but second run did not.");
            }
            if !status.success() {
                bail!(
                    "Log differences found, aborting because --selfcheck requires perfect reproducibility of the target run!"
                )
            }
            let run1b_preempts_path = run1b.preempts_path_out();
            if !preempt_files_equal(&min_preempts_path, run1b_preempts_path) {
                bail!(
                    "The preemptions recorded by the additional run did not match the preemptions replayed (no fixed point): {} vs {}",
                    min_preempts_path.display(),
                    run1b_preempts_path.display(),
                );
            }

            eprintln!(
                ":: {}",
                "Identical executions confirmed between target run and its preemption-based replay."
                    .green()
                    .bold()
            );
        }
        Ok(())
    }

    /// Once we have the target MATCHING run in hand (usually crashing/failing), we need to
    /// determine which baseline, non-matching run to use. Then we need to extract the schedule from
    /// it.
    ///
    /// Returns a path to a file containing recorded schedule events for the baseline run.
    pub fn phase4_choose_baseline_sched_events(
        &mut self,
        matching_pr: PreemptionRecord,
    ) -> anyhow::Result<RunData> {
        self.progress.phase(Phase::Baseline);
        if self.minimize {
            // Enforced by the clap conflicts_with annotations:
            assert!(self.run2_seed.is_none());
            assert!(self.run2_preemptions.is_none());
            assert!(self.run2_schedule.is_none());

            // If we're minimizing, then we know that ALL interventions in the schedule are critical.
            // Thus omitting any of them is sufficient to exit the target schedule space.
            // Omitting the last one should yield the lowest distance match/non-match schedule pair.
            knock_out_until_missed(matching_pr, |ix, knocked_out| {
                let runname = format!("run2_baseline_try{:03}", ix);
                let (still_matching, newrun) =
                    self.save_nearby_non_matching_sched_events(&runname, knocked_out)?;
                Ok((!still_matching).then_some(newrun))
            })
        } else {
            if let Some(path) = &self.run2_schedule {
                if self.selfcheck {
                    // TODO: Don't trust the recorded schedule is a baseline, replay and check it.
                }
                let runname = "from_existing_run2_schedule".to_string();
                let runopts = RunData::new_baseline(self, "run2_baseline".to_string())?
                    .with_preemption_recording()
                    .into_runopts();
                let fakerun = RunData::from_schedule_trace(self, runname, runopts, path.clone());
                return Ok(fakerun);
            }
            let from_where = if self.run2_seed.is_some() {
                "--run2-seed"
            } else if self.run2_preemptions.is_some() {
                "--run2-preemptions"
            } else {
                "non-chaos (0 extra preemptions) run"
            };
            // Like a search trial, a baseline run that analyze's own bound
            // stopped has no outcome, so whether analyze bisects or searches
            // within the target schedule does not hang on the host's speed:
            // it runs again with a longer bound (`decisive_trial`).
            let (end, mut newrun) = self.decisive_trial(
                "run2_baseline",
                &format!("the baseline run ({from_where})"),
                None,
                |name| {
                    let mut newrun =
                        RunData::new_baseline(self, name.to_owned())?.with_preemption_recording();
                    // Tweak the runopts according to several different scenarios:
                    let ro = &mut newrun.runopts;
                    if let Some(seed) = self.run2_seed {
                        // Replay from seed to record schedule.
                        ro.det_opts.det_config.seed = seed;
                    } else if let Some(path) = &self.run2_preemptions {
                        // Replay from preemption record to record schedule.
                        ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
                        // TODO: if the file already contains a schedule, we don't need to
                        // rerun it. Unless selfcheck is specified.
                    } else {
                        // Otherwise we just assume that a *baseline* (non-chaos) run will do
                        // the trick.
                        ro.det_opts.det_config.chaos = false;
                    }
                    Ok(newrun)
                },
            )?;
            if !end.finished() {
                // Without a baseline the user chose, analyze searches within
                // the target schedule instead; one the user chose is not
                // replaced behind their back.
                if self.run2_seed.is_none() && self.run2_preemptions.is_none() {
                    return Err(BaselineUnfinished(from_where).into());
                }
                bail!(
                    "the baseline run ({from_where}) did not finish (a bound or a refusal \
                     stopped it; see above). Pass a larger --trial-timeout, or another baseline; \
                     without one, analyze bisects against the run without chaos, or searches \
                     within the target schedule when that does not finish either."
                );
            }
            self.record_endpoint(&newrun);

            eprintln!(
                ":: Recorded schedule from {} as baseline run ({})",
                from_where,
                newrun
                    .sched_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy(),
            );
            if newrun.is_a_match() {
                bail!(
                    "Expectations not met... baseline run matched target criteria when it should not."
                )
            } else {
                eprintln!("Good: baseline run does not match criteria (e.g. pass not fail).");
                Ok(newrun)
            }
        }
    }

    /// Perform the binary search through schedule-space, identifying critical events.
    pub fn phase5_bisect_traces(
        &self,
        target: &[SchedEvent],
        baseline: &[SchedEvent],
    ) -> anyhow::Result<CriticalSchedule> {
        self.progress.phase(Phase::Bisect);
        let endpoints = events_digest(&[baseline, target]);
        let resumed = self.resumable_interval(&endpoints);
        let resume = resumed.as_ref().map(|(resume, ..)| resume);
        let trials = Cell::new(resume.map_or(0, |resume| resume.trials));
        let unfinished = Cell::new(resume.map_or(0, |resume| resume.unfinished));
        let refused = Cell::new(resume.map_or(0, |resume| resume.refused));

        // Each call is one trial. A trial that cannot finish (for example, one
        // that --max-log-bytes ended) returns its error unchanged, and the
        // search stops there. A trial that the run arguments' --timeout ended
        // or that Hermit refused is the exception: a spliced schedule can
        // leave the guest waiting for an event that never comes, or take it
        // down a path Hermit does not support, so it did not reach the
        // target, and the search goes on. One that analyze's own bound
        // (--trial-timeout) stopped has no outcome and runs again with a
        // longer one (`decisive_trial`).
        let test_fn = |sched: &[SchedEvent]| -> TrialOutcome {
            trials.set(trials.get() + 1);
            let i = trials.get();
            let runname = format!("bisect_round_{:0wide$}", i, wide = 3);
            let (end, mut newrun) = self.decisive_trial_unless(
                &runname,
                &format!(
                    "replaying a schedule of {} events that the default search spliced",
                    sched.len()
                ),
                None,
                |name| {
                    let mut run = RunData::new_baseline(self, name.to_owned())?
                        .with_schedule_replay() // Default input path
                        .with_schedule_recording(); // Default output path
                    // Prepare the next synthetic schedule on disk:
                    self.write_trial_schedule(sched.to_owned(), run.sched_path_in())?;
                    Ok(run)
                },
                // A round that left its schedule is not run again when a
                // bound stopped it: it would leave it at the same event.
                |run| run.departure().is_some(),
            )?;
            // A replay that could not follow the spliced schedule ran
            // another one, whatever its outcome, finished or stopped: it has
            // none for this schedule, and the search tries another.
            if let Some(departure) = newrun.departure() {
                return Err(InfeasibleSchedule {
                    run: newrun.name().to_owned(),
                    event: departure.event,
                    expected: departure.expected.as_raw(),
                    running: departure.running.as_raw(),
                    passed: match end {
                        TrialEnd::Unfinished(_) => true,
                        end => !end.reached_target(),
                    },
                }
                .into());
            }

            let is_match = match end {
                TrialEnd::Unfinished(stop) => {
                    unfinished.set(unfinished.get() + 1);
                    refused.set(refused.get() + usize::from(stop == Stop::Refusal));
                    return Ok((true, sched.to_owned()));
                }
                end => end.reached_target(),
            };
            if is_match {
                eprintln!(" => Target condition ({})", self.display_criteria());
            } else {
                eprintln!(" => Baseline condition (usually absence of crash)");
            }

            let sched_out = PreemptionReader::new(newrun.sched_path_out()).load_all();
            Ok((!is_match, sched_out.into_global()))
        };

        // Only an analysis that journals keeps its interval (`hermit bisect`
        // keeps every trial's schedules anyway).
        let mut interval = if self.journal.is_on() {
            Some(IntervalFiles::new(self.get_tmp()?))
        } else {
            None
        };
        if let Some(interval) = &mut interval
            && resumed.is_none()
        {
            interval.record(self, None, [baseline, target], None, None);
        }
        // TODO: have search_for_critical_schedule borrow only.
        let (start, baseline, target) = match resumed {
            Some((resume, pass, passing, failing)) => (
                SearchStart::Interval {
                    pass,
                    event_level: resume.event_level,
                },
                passing,
                failing,
            ),
            None => (SearchStart::Endpoints, baseline.to_vec(), target.to_vec()),
        };
        let mut cfg = Config {
            verbose: self.verbose,
            needleman_search: self.run_needleman,
            ..Default::default()
        };
        if let Some(dist) = self.jitter_dist {
            cfg.max_jitter_editdist = dist;
            cfg.max_jitter_swapdist = dist * dist;
        }
        let crit = search_for_critical_schedule_observed(
            test_fn,
            baseline,
            target,
            &cfg,
            start,
            &mut |at| {
                if let Some(interval) = &mut interval {
                    let state = SearchState {
                        endpoints: endpoints.clone(),
                        event_level: at.event_level,
                        trials: trials.get(),
                        unfinished: unfinished.get(),
                        refused: refused.get(),
                    };
                    interval.record(
                        self,
                        Some(at.pass),
                        [at.passing, at.failing],
                        Some((at.edit_distance, at.swap_distance)),
                        Some(state),
                    );
                }
            },
        )?;
        eprintln!(
            "Critical event of final on-target schedule is {}",
            crit.critical_event_index
        );
        if let Some(caveat) = critical_pair_caveat(&crit, unfinished.get()) {
            eprintln!(":: {} {caveat}", "WARNING:".yellow().bold());
        }
        if let Some(warning) = refused_trials_warning(refused.get()) {
            eprintln!(":: {} {warning}", "WARNING:".yellow().bold());
        }
        Ok(crit)
    }

    /// `--resume`: the interval the earlier invocation's search had reached
    /// between `endpoints` (`events_digest` of the baseline and target
    /// schedules), with its pass and its two schedules, if the progress
    /// names one whose files are whole. The search restarts there: its
    /// trials keep no schedules, so the journal alone could not bring it
    /// back to that pass. With no such interval it starts over, and the
    /// journal answers what it can.
    fn resumable_interval(
        &self,
        endpoints: &str,
    ) -> Option<(IntervalResume, usize, Vec<SchedEvent>, Vec<SchedEvent>)> {
        let interval = self.resumed_progress.as_ref()?.interval.as_ref()?;
        let (Some(pass), Some(resume)) = (interval.pass, interval.resume.clone()) else {
            return None;
        };
        let load = |path: &Path, hash: &str| {
            let events = PreemptionReader::new(path).load_all().into_global();
            (events_digest(&[&events]) == hash).then_some(events)
        };
        let loaded = (resume.endpoints == endpoints)
            .then(|| {
                Some((
                    load(&interval.passing, &resume.passing_hash)?,
                    load(&interval.failing, &resume.failing_hash)?,
                ))
            })
            .flatten();
        let Some((passing, failing)) = loaded else {
            eprintln!(
                ":: The earlier invocation's search interval (pass {pass}) is not this search's, \
                 or its schedules are not whole; the search starts from its endpoints."
            );
            return None;
        };
        eprintln!(
            ":: Resuming the search at pass {pass}, from the interval the earlier invocation \
             reached ({} and {} events, {}); its {} trials do not run again.",
            passing.len(),
            failing.len(),
            interval.edit_distance.map_or_else(
                || "distance not measured".to_owned(),
                |edit| format!("edit distance {edit}")
            ),
            resume.trials,
        );
        Some((resume, pass, passing, failing))
    }

    /// Bisect a known passing/failing schedule pair and report the critical event ordering.
    pub fn bisect_schedule_pair(
        &mut self,
        good: Vec<SchedEvent>,
        bad: Vec<SchedEvent>,
    ) -> anyhow::Result<ExitStatus> {
        // Bisect keeps its workspace, and says where when it exits.
        let _workspace = self.phase0_initialize(Retention::Keep { because: None })?;
        eprintln!(
            ":: Beginning schedule bisection using good/bad endpoints ({} / {} events)",
            good.len(),
            bad.len(),
        );

        let critical = self.phase5_bisect_traces(&bad, &good)?;
        let index = critical.critical_event_index;
        if index == 0 || index >= critical.failing_schedule.len() {
            bail!("schedule search returned an invalid critical event index {index}");
        }

        println!("\n---------------- hermit bisect result ----------------");
        println!("Schedule divergence localized at bad event #{index}:");
        println!(
            "  preceding event #{}: {}",
            index - 1,
            critical.failing_schedule[index - 1]
        );
        println!(
            "  critical event  #{index}: {}",
            critical.failing_schedule[index]
        );
        println!("Swapping these adjacent events reproduces the good outcome.");

        let report = self.phase6_record_outputs(critical, None)?;
        if let Some(path) = &self.report_file {
            let text = serde_json::to_string_pretty(&report)?;
            std::fs::write(path, text)?;
            eprintln!(":: Final bisection report written to {}", path.display());
        }

        Ok(ExitStatus::SUCCESS)
    }

    fn generate_context(
        sched: &[SchedEvent],
        critical_event_index: usize,
        window_size: usize,
    ) -> String {
        let mut additional_context = "Execution context for the critical events (*):\n".to_string();
        {
            let start = critical_event_index - 1;
            let start = start - cmp::min(window_size, start);
            let end = cmp::min(critical_event_index + window_size + 1, sched.len());

            for (ix, item) in sched.iter().enumerate().take(end).skip(start) {
                additional_context.push_str(&format!(
                    "  {} {}\n",
                    if ix == critical_event_index || ix == critical_event_index - 1 {
                        "*"
                    } else {
                        " "
                    },
                    item
                ));
            }
        }
        additional_context
    }

    /// Record the schedules on disk as reproducers and report stack-traces of
    /// critical events. `target_run` is the workspace root of the run whose
    /// schedule was the search's target endpoint, if one ran.
    pub fn phase6_record_outputs(
        &self,
        crit: CriticalSchedule,
        target_run: Option<&Path>,
    ) -> Result<Report, Error> {
        let tmp_dir = self.get_tmp()?;
        let CriticalSchedule {
            failing_schedule,
            passing_schedule,
            critical_event_index,
        } = crit;
        self.progress.phase(Phase::StackTraces);
        self.progress.decided(
            format!(
                "events #{} and #{critical_event_index} of the failing schedule are the critical \
                 pair, in {}",
                critical_event_index - 1,
                self.get_tmp()?
                    .join("final_target_for_stacktraces")
                    .with_extension(SCHED_EXT)
                    .display()
            ),
            DecidedEvents::CriticalPair(critical_event_index - 1, critical_event_index),
        );

        let runname1 = "final_target_for_stacktraces";
        let runname2 = "final_baseline_for_stacktraces";
        let final_failing_path = tmp_dir.join(runname1).with_extension(SCHED_EXT);
        let final_passing_path = tmp_dir.join(runname2).with_extension(SCHED_EXT);

        let passing_context = Self::generate_context(
            &passing_schedule,
            critical_event_index,
            self.execution_context,
        );
        let failing_context = Self::generate_context(
            &failing_schedule,
            critical_event_index,
            self.execution_context,
        );

        self.write_final_schedules(
            failing_schedule.clone(),
            &final_failing_path,
            passing_schedule.clone(),
            &final_passing_path,
        );

        let mut header = String::new();
        header.push_str("These two operations, on different threads, are RACING with eachother.\n");
        header.push_str(&format!(
            "The order of events (#{} and #{} in the schedule) determines the program outcome.\n",
            critical_event_index - 1,
            critical_event_index,
        ));
        header.push_str(
            "You must add synchronization to prevent these operations from racing, or give them a different order.\n",
        );

        if self.is_vanilla_criteria() {
            header.push_str(
                "Attached are the stacktraces in the PASSING order, but flipping the order of the two events makes the program FAIL (nonzero exit).\n",
            );
        } else {
            header.push_str(&format!(
                "Attached are the stacktraces in the baseline order, while flipping the order makes the program exhibit the target criteria ({}).\n",
                self.display_criteria()
            ));
        }

        // Both schedules are replayed: the passing schedule's events
        // #index - 1 and #index are the failing schedule's #index and
        // #index - 1, so each racing operation's stack can come from either
        // replay when the other one leaves its schedule first.
        let baseline = self.replay_for_stacks(
            runname2,
            &final_passing_path,
            &passing_schedule,
            critical_event_index,
        )?;
        let target = self.replay_for_stacks(
            runname1,
            &final_failing_path,
            &failing_schedule,
            critical_event_index,
        )?;
        if (baseline.finished && baseline.met_target) || (target.finished && !target.met_target) {
            eprintln!(
                ":: {} the stack-trace replays did not repeat the search's outcomes (baseline \
                 met the target: {}, target met it: {}); read the stacks with care",
                "WARNING:".yellow().bold(),
                baseline.met_target,
                target.met_target,
            );
        }
        if target.met_target
            && let Some(change) = target_run.and_then(|run| {
                changed_output(
                    run,
                    &tmp_dir.join(runname1),
                    [
                        ("stdout", self.target_stdout.as_ref()),
                        ("stderr", self.target_stderr.as_ref()),
                    ],
                )
            })
        {
            eprintln!(":: {} {change}", "WARNING:".yellow().bold());
            header.push_str(&format!("Note: {change}\n"));
        }
        let [stack1, stack2] = merge_stacks(
            &baseline,
            &target,
            critical_event_index,
            [
                passing_schedule[critical_event_index - 1].dettid,
                passing_schedule[critical_event_index].dettid,
            ],
        );
        for (which, stack, event) in [
            ("stack1", &stack1, critical_event_index - 1),
            ("stack2", &stack2, critical_event_index),
        ] {
            match stack {
                Some((_, source)) => header.push_str(&format!("{which} is from {source}.\n")),
                None => {
                    let target_event = 2 * critical_event_index - 1 - event;
                    let reason =
                        missing_stack_reason(which, (event, target_event), &baseline, &target);
                    eprintln!(":: {} {reason}", "WARNING:".yellow().bold());
                    header.push_str(&format!("{reason}\n"));
                }
            }
        }
        let (stack1, stack2) = (
            stack1.map(|(stack, _)| stack),
            stack2.map(|(stack, _)| stack),
        );

        // Also print to the screen, the branch-counter caveat first: the
        // report must not read as reproducible where its trials may not
        // have been.
        println!(
            "\n------------------------------ hermit analyze report ------------------------------"
        );
        if let Some(warning) = &self.branch_counter_warning {
            println!("{warning}");
        }
        println!("{}", header);
        Self::print_stack("stack1", &stack1);
        Self::print_stack("stack2", &stack2);
        println!("\n{}", passing_context);

        if self.verbose {
            println!(
                "\n----------------------- stacks from final on-target run -----------------------"
            );
            Self::print_stack(
                &format!("event #{} (stack2's operation)", critical_event_index - 1),
                &target.stacks[0],
            );
            Self::print_stack(
                &format!("event #{critical_event_index} (stack1's operation)"),
                &target.stacks[1],
            );
            println!("\n{}", failing_context);
        }

        Ok(Report {
            header,
            additional_context: passing_context,
            baseline_run: true,
            critical_event1: ReportCriticalEvent {
                event_index: critical_event_index - 1,
                stack: stack1,
            },
            critical_event2: ReportCriticalEvent {
                event_index: critical_event_index,
                stack: stack2,
            },
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event: None,
            partial: None,
        })
    }

    fn print_stack(which: &str, m_stack: &Option<PrettyBacktrace>) {
        if let Some(s) = m_stack {
            println!("{}", s);
        } else {
            println!(" ({} not available) ", which);
        }
    }

    /// Everything `main` does before phase 0 creates the workspace: a
    /// refusal here starts nothing.
    fn prepare(&mut self, global: &GlobalOpts) -> anyhow::Result<()> {
        self.apply_global(global);
        self.refuse_unsupervised_log_cap()?;
        self.refuse_options_trials_do_not_apply()?;
        self.refuse_outputs_trials_overwrite()?;
        self.refuse_unqualified_trial_timeout()?;
        self.refuse_unenforceable_trial_timeout()?;
        self.refuse_unsound_continuation_timeslice()?;
        self.install_trial_pmu_config()?;
        self.admit_branch_counter(&mut crate::tracing::write_stderr_diagnostic)?;
        // `--run1-schedule` and `--run2-schedule` parse but have no
        // implementation. Refuse them before any workspace or trial, rather
        // than panicking here.
        let unimplemented: Vec<&str> = [
            (self.run1_schedule.is_some(), "--run1-schedule"),
            (self.run2_schedule.is_some(), "--run2-schedule"),
        ]
        .into_iter()
        .filter_map(|(given, option)| given.then_some(option))
        .collect();
        if !unimplemented.is_empty() {
            return Err(
                anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                    "hermit analyze does not implement {}. Identify a run by a preemption record \
                 (--run1-preemptions or --run2-preemptions, written by `hermit run \
                 --record-preemptions-to`) or by a seed (--run1-seed or --run2-seed).",
                    unimplemented.join(" or ")
                )),
            );
        }
        // The trials that replay these records start from the epoch they were
        // recorded under (https://github.com/rrnewton/hermit/issues/3870).
        let run1 = self.run1_preemptions.clone();
        let run2 = self.run2_preemptions.clone();
        let recordings: Vec<(&str, &Path)> = [
            ("--run1-preemptions", run1.as_deref()),
            ("--run2-preemptions", run2.as_deref()),
        ]
        .into_iter()
        .filter_map(|(option, path)| Some((option, path?)))
        .collect();
        self.adopt_recorded_epoch(&recordings)?;
        // Every trial charges expired timed waits as the target did.
        self.adopt_expired_wait_charge(run1.as_deref())?;
        self.refuse_recordings_under_another_pipe_wake_latency(&recordings)
    }

    /// Take the settings `analyze` inherits from the global options: every
    /// trial runs on the globally selected backend
    /// (`hermit --backend <BACKEND> analyze ...`) and charges the invocation's
    /// shared `--max-log-bytes` budget.
    pub fn apply_global(&mut self, global: &GlobalOpts) {
        self.backend = global.backend;
        self.max_log_bytes = global.max_log_bytes;
        self.log_budget = global.log_budget();
    }

    pub fn main(&mut self, global: &GlobalOpts) -> anyhow::Result<ExitStatus> {
        // The budget counts from here, before anything that takes time.
        self.budget = Budget::new(self.time_budget, self.trial_budget);
        self.prepare(global)?;
        // From here SIGTERM, SIGINT and SIGHUP end the run in progress and
        // the analysis, which then removes what it created like any other
        // ending, and the process dies of the signal (`raise_or_exit`).
        let stop_signals = StopSignals::install()?;
        let mut workspace = match self.resume.clone() {
            Some(resumed) => self.phase0_resume(&resumed)?,
            None => {
                let workspace = self.phase0_initialize(self.workspace_retention())?;
                self.start_checkpoint()?;
                workspace
            }
        };
        let result = self.analyze(global);
        if let Some(because) = self.keep_workspace_because(&result) {
            workspace.keep(because);
        }
        drop(workspace);
        // Restored before the last check, so a signal from here on has its
        // default action rather than being recorded too late to act on.
        drop(stop_signals);
        let Err(Stopped(signal)) = stop::check() else {
            return result;
        };
        match result {
            Err(error) if error.downcast_ref::<Stopped>().is_none() => note(&format!(
                ":: hermit analyze stopped on {signal}, after this error: {error:#}"
            )),
            _ => note(&format!(":: hermit analyze stopped on {signal}.")),
        }
        Ok(ExitStatus::Signaled(signal, false))
    }

    /// The analysis, in the workspace phase 0 created; a partial report
    /// (exit 120) when a budget ends it first.
    fn analyze(&mut self, global: &GlobalOpts) -> anyhow::Result<ExitStatus> {
        let result = self.analyze_steps(global);
        self.end_on_budget(result)
    }

    /// `result`, or the partial report when a budget ended the analysis or a
    /// search trial stayed inconclusive.
    fn end_on_budget(&mut self, result: anyhow::Result<ExitStatus>) -> anyhow::Result<ExitStatus> {
        match result {
            Err(error) => match error.downcast::<BudgetExpired>() {
                Ok(expired) => self.report_partial(expired),
                Err(error) => match error.downcast::<TrialInconclusive>() {
                    Ok(inconclusive) => self.report_partial(inconclusive),
                    Err(error) => match error.downcast::<NoFeasibleSchedule>() {
                        Ok(infeasible) => self.report_partial(infeasible),
                        Err(error) => Err(error),
                    },
                },
            },
            done => done,
        }
    }

    /// The report of an analysis a budget or an inconclusive search trial
    /// ended: why and where it stopped, its best answer so far from the
    /// progress (`checkpoint`), the stacks of the default search's pair when
    /// two replays fit the grace period, and how to continue. Exits
    /// [`hermit::HERMIT_ANALYZE_PARTIAL_EXIT`].
    fn report_partial(&mut self, cause: impl Into<PartialCause>) -> anyhow::Result<ExitStatus> {
        let cause = cause.into();
        let progress = self.progress.snapshot();
        let workspace = self.get_tmp()?.to_path_buf();
        let budget = self.budget.clone();
        let elapsed = budget.elapsed();
        let in_workspace = |path: &Path| {
            path.strip_prefix(&workspace)
                .unwrap_or(path)
                .display()
                .to_string()
        };

        let mut header = match &cause {
            PartialCause::Budget(expired) => format!(
                "PARTIAL RESULT. hermit analyze reached its {}{} after {} s, while {}, so what \
                 follows is its best answer so far, not the critical events.\n",
                expired.kind.option(),
                match expired.kind {
                    BudgetKind::TimeBudget =>
                        budget
                            .time_limit()
                            .map_or_else(String::new, |limit| format!(
                                " ({})",
                                budget::describe(limit)
                            )),
                    BudgetKind::TrialBudget => budget
                        .trial_limit()
                        .map_or_else(String::new, |limit| format!(" ({limit} runs)")),
                },
                elapsed.as_secs(),
                progress.phase.describe(),
            ),
            PartialCause::Inconclusive(inconclusive) => format!(
                "PARTIAL RESULT. hermit analyze stopped after {} s, while {}, because a trial \
                 has no outcome: {inconclusive}. What follows is its best answer so far, not \
                 the critical events.\n",
                elapsed.as_secs(),
                progress.phase.describe(),
            ),
            PartialCause::Infeasible(infeasible) => format!(
                "PARTIAL RESULT. hermit analyze stopped after {} s, while {}, because {infeasible} \
                 What follows is its best answer so far, not the critical events.\n",
                elapsed.as_secs(),
                progress.phase.describe(),
            ),
        };
        if let PartialCause::Budget(BudgetExpired { cut: Some(run), .. }) = &cause {
            header.push_str(&format!(
                "The budget cut {run} short; its outcome is not counted, and a resumed analysis \
                 runs it again.\n"
            ));
        }
        let mut replay_commands = Vec::new();
        let mut critical = [None, None];
        let mut stacks =
            "not applicable: the default search had no pair of schedules yet".to_owned();
        // The events the report's structured fields name: the critical pair's
        // and the deciding event's indices.
        let mut critical_indices = progress
            .interval
            .as_ref()
            .map_or([0, 0], |interval| [interval.common_prefix; 2]);
        let mut deciding_event = None;
        if let Some(decided) = &progress.decided {
            header.push_str(&format!(
                "The search had finished: {decided}. Its stack-trace runs did not finish within \
                 the budget; --resume takes them.\n"
            ));
            // A budget hit after the search decided still reports the
            // decision, in the fields a complete report puts it in; only the
            // stacks are missing.
            stacks = "not taken: the budget ran out during the stack-trace runs; --resume takes \
                      them"
                .to_owned();
            match progress.decision {
                Some(DecidedEvents::Deciding(decision)) => {
                    critical_indices = [decision; 2];
                    deciding_event = Some(ReportCriticalEvent {
                        event_index: decision,
                        stack: None,
                    });
                }
                Some(DecidedEvents::CriticalPair(first, second)) => {
                    critical_indices = [first, second];
                }
                None => {}
            }
        } else if let Some(interval) = &progress.interval
            && progress.phase == Phase::Bisect
        {
            header.push_str(&format!(
                "The closest pair the default search found{}: a schedule whose replay misses the \
                 target ({}, {} events) and one whose replay meets it ({}, {} events){}. ",
                match interval.pass {
                    Some(pass) => format!(" in {} passes", pass),
                    None => " (its endpoint runs; the search had not replayed them yet)".to_owned(),
                },
                in_workspace(&interval.passing),
                interval.passing_events,
                in_workspace(&interval.failing),
                interval.failing_events,
                match (interval.edit_distance, interval.swap_distance) {
                    (Some(edit), Some(swap)) => {
                        format!(", at edit distance {edit} and swap distance {swap}")
                    }
                    _ => String::new(),
                },
            ));
            header.push_str(&format!(
                "They agree on their first {} events, so the deciding event is event #{} or a \
                 later one: {}.\n",
                interval.common_prefix,
                interval.common_prefix,
                match (&interval.passing_event, &interval.failing_event) {
                    (Some(passing), Some(failing)) => format!(
                        "there the passing schedule runs {passing}, and the failing one {failing}"
                    ),
                    _ => "there one of them has already ended".to_owned(),
                }
            ));
            for (which, path) in [
                ("passing", &interval.passing),
                ("failing", &interval.failing),
            ] {
                // Each its own name, so that the two can run at once.
                replay_commands.push(
                    RunData::new_baseline(self, format!("interval_{which}_replay"))?
                        .with_schedule_replay_from(path.clone())
                        .to_repro(),
                );
            }
            let taken;
            (critical, taken) = self.partial_stacks(interval);
            stacks = taken;
            header.push_str(&format!("Stack traces: {stacks}.\n"));
        } else if let Some(bracket) = &progress.bracket
            && progress.phase == Phase::PrefixSearch
        {
            stacks = "not taken: a prefix search's bracket names a range of events".to_owned();
            header.push_str(&describe_bracket(bracket));
            for run in [&bracket.misses_run, &bracket.meets_run]
                .into_iter()
                .flatten()
            {
                if let Ok(command) =
                    std::fs::read_to_string(workspace.join(run).with_extension("repro"))
                {
                    replay_commands.push(command.trim_end().to_owned());
                }
            }
        } else {
            header.push_str(
                "The analysis had not reached its search yet, so there is no pair of schedules or \
                 bracket to report.\n",
            );
        }
        let runs_from_journal = self.journal.answered();
        header.push_str(&format!(
            "This invocation started {} runs{}.\n",
            budget.trials_started(),
            if runs_from_journal > 0 {
                format!(" and took {runs_from_journal} from the journal of an earlier one")
            } else {
                String::new()
            }
        ));
        let resume_command = match &cause {
            PartialCause::Budget(_) => {
                let command = resume_command(&workspace);
                header.push_str(&format!(
                    "The workspace is kept: {}\nContinue the analysis with (add a budget to \
                     bound it again):\n  {command}\n",
                    workspace.display()
                ));
                command
            }
            PartialCause::Inconclusive(inconclusive) => {
                let longer = inconclusive.bounds[1].map_or("off".to_owned(), |bound| {
                    bound
                        .saturating_mul(u64::from(CONFIRM_BOUND_FACTOR))
                        .to_string()
                });
                let command = resume_command_with(&workspace, Some(&longer));
                header.push_str(&format!(
                    "The workspace is kept: {}\nContinue the analysis, giving that trial more \
                     time (a longer --trial-timeout, or --trial-timeout=off to wait for it \
                     however long it takes), with:\n  {command}\nThe runs that finished are \
                     taken from the journal. A --timeout in the run arguments instead ends such \
                     a trial as not meeting the target, as a bound you chose.\n",
                    workspace.display()
                ));
                if let Some(next) = inconclusive.next_continuation {
                    header.push_str(&format!(
                        "Or search at the next, coarser continuation instead, in a new analysis \
                         (the continuation is part of what a resume must keep):\n  {}\nanalyze \
                         does not move on to it by itself: on a slower host it would then \
                         answer at another timeslice.\n",
                        command_replacing(
                            &["--resume", "--continuation-timeslice"],
                            &[],
                            vec![format!("--continuation-timeslice={next}")],
                        )
                    ));
                }
                command
            }
            PartialCause::Infeasible(_) => {
                let command = prefix_search_command();
                header.push_str(&format!(
                    "The workspace is kept: {}\nA resumed analysis would replay the same \
                     schedules. Search within the failing schedule instead with:\n  {command}\n",
                    workspace.display()
                ));
                command
            }
        };

        let [critical1, critical2] = critical;
        let report = Report {
            header: header.clone(),
            additional_context: String::new(),
            baseline_run: true,
            critical_event1: ReportCriticalEvent {
                event_index: critical_indices[0],
                stack: critical1,
            },
            critical_event2: ReportCriticalEvent {
                event_index: critical_indices[1],
                stack: critical2,
            },
            branch_counter_warning: self.branch_counter_warning.clone(),
            deciding_event,
            partial: Some(PartialReport {
                reason: cause.reason(),
                phase: progress.phase,
                elapsed_seconds: elapsed.as_secs(),
                time_budget_seconds: budget.time_limit().map(|limit| limit.as_secs()),
                runs_started: budget.trials_started(),
                trial_budget: budget.trial_limit(),
                runs_from_journal,
                cut_run: match &cause {
                    PartialCause::Budget(expired) => expired.cut.clone(),
                    PartialCause::Inconclusive(_) | PartialCause::Infeasible(_) => None,
                },
                inconclusive: match &cause {
                    PartialCause::Inconclusive(inconclusive) => Some(inconclusive.clone()),
                    PartialCause::Budget(_) | PartialCause::Infeasible(_) => None,
                },
                infeasible: match &cause {
                    PartialCause::Infeasible(infeasible) => Some(infeasible.clone()),
                    PartialCause::Budget(_) | PartialCause::Inconclusive(_) => None,
                },
                interval: progress.interval.clone(),
                bracket: progress.bracket.clone(),
                decided: progress.decided.clone(),
                stacks,
                replay_commands,
                workspace: workspace.clone(),
                resume_command,
            }),
        };
        println!(
            "\n------------------------- hermit analyze report (PARTIAL) -------------------------"
        );
        if let Some(warning) = &self.branch_counter_warning {
            println!("{warning}");
        }
        println!("{header}");
        if let Some(stack) = &report.critical_event1.stack {
            println!(
                "The passing schedule's thread at event #{}:",
                report.critical_event1.event_index
            );
            println!("{stack}");
        }
        if let Some(stack) = &report.critical_event2.stack {
            println!(
                "The failing schedule's thread at event #{}:",
                report.critical_event2.event_index
            );
            println!("{stack}");
        }
        note(&format!(
            "HERMIT_ANALYZE_PARTIAL class={} phase={} workspace={}",
            cause.reason().class(),
            serde_json::to_value(progress.phase)
                .ok()
                .and_then(|phase| phase.as_str().map(str::to_owned))
                .unwrap_or_default(),
            workspace.display()
        ));
        if let Some(path) = &self.report_file {
            std::fs::write(path, serde_json::to_string(&report)?)
                .with_context(|| format!("cannot write the report to {}", path.display()))?;
            eprintln!(
                ":: {}\n {}",
                "Partial analysis report written to:".yellow().bold(),
                path.display()
            );
        }
        self.partial_report_written = true;
        Ok(ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT))
    }

    /// The stacks of the threads the two schedules of the default search's
    /// `interval` run at the first event at which they differ, and what the
    /// report says about them. They are taken only when two replays as slow
    /// as the slowest endpoint run fit the grace period after the budget ran
    /// out ([`Budget::stack_grace`]), and each replay is bounded by what is
    /// left of it.
    fn partial_stacks(&self, interval: &BisectInterval) -> ([Option<PrettyBacktrace>; 2], String) {
        let grace = self.budget.stack_grace();
        let fits = self
            .endpoints
            .slowest
            .is_some_and(|slowest| slowest * 2 <= grace);
        if !fits {
            return (
                [None, None],
                format!(
                    "not taken: two replays as slow as the slowest endpoint run ({}) do not fit \
                     the grace period of {} s after the budget ran out (min(120 s, 5% of \
                     --time-budget)); the replay commands below take them",
                    self.endpoints
                        .slowest
                        .map_or("not measured".to_owned(), |slowest| format!(
                            "{} s",
                            slowest.as_secs()
                        )),
                    grace.as_secs()
                ),
            );
        }
        self.budget.open_grace(grace);
        let mut stacks = [None, None];
        let mut missing = Vec::new();
        for (slot, (which, path)) in [
            ("passing", &interval.passing),
            ("failing", &interval.failing),
        ]
        .into_iter()
        .enumerate()
        {
            let taken = (|| -> anyhow::Result<Option<PrettyBacktrace>> {
                let mut run = RunData::new_baseline(self, format!("partial_{which}_stack"))?
                    .with_schedule_replay_from(path.clone());
                let stack_path = run.root_path().with_extension("stack");
                run.runopts.det_opts.det_config.stacktrace_event =
                    vec![(interval.common_prefix as u64, Some(stack_path.clone()))];
                match run.launch() {
                    Ok(()) => {}
                    Err(error) if error.downcast_ref::<RunTimeoutMarker>().is_some() => {}
                    Err(error) => return Err(error),
                }
                Ok(File::open(&stack_path)
                    .ok()
                    .and_then(|file| serde_json::from_reader(file).ok()))
            })();
            match taken {
                Ok(Some(stack)) => stacks[slot] = Some(stack),
                Ok(None) => missing.push(format!("the {which} replay left no stack there")),
                Err(error) if error.downcast_ref::<BudgetExpired>().is_some() => {
                    missing.push(format!(
                        "the {which} replay did not fit the grace period of {} s",
                        grace.as_secs()
                    ));
                }
                Err(error) => missing.push(format!("the {which} replay failed: {error:#}")),
            }
        }
        self.budget.close_grace();
        let said = if missing.is_empty() {
            format!(
                "taken from replays of both schedules within the grace period of {} s",
                grace.as_secs()
            )
        } else {
            format!("partly or not taken: {}", missing.join("; "))
        };
        (stacks, said)
    }

    /// The analysis, in the workspace phase 0 created.
    fn analyze_steps(&mut self, global: &GlobalOpts) -> anyhow::Result<ExitStatus> {
        self.progress.phase(Phase::TargetRun);
        {
            let dummy = RunData::new_baseline(self, "dummy".to_string())?;
            if !dummy.runopts.det_opts.det_config.chaos {
                eprintln!(
                    ":: {} You may want to turn it on explicitly, along with a --max-timeslice that works well for this program.",
                    "WARNING: implicitly activating --chaos.".yellow().bold()
                );
            }
        }

        let run1data = self.phase1_establish_target_run()?;

        let mut min_run = self.phase2_minimize(global, run1data)?;
        self.phase3_strict_preempt_replay_check(global, &mut min_run)?;

        // The record the target endpoint replays, and the one --minimize's
        // baseline knocks a preemption out of, is the record that met the
        // target: a normalized copy renumbers its priorities across the
        // scheduler's fixed ones (the default priority, among others), and
        // can run another schedule.
        let (normalized_path, normalized) = min_run.write_normalized_preempts();
        eprintln!(
            ":: {}\n {}",
            format!(
                "Normalized for reading ({}; analyze uses {}), that preemption record \
                 becomes:",
                normalized_path.file_name().unwrap().to_string_lossy(),
                min_run
                    .preempts_path_out()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            )
            .green()
            .bold(),
            truncated(
                1000,
                serde_json::to_string_pretty(&normalized.clone_preemptions_only()).unwrap()
            )
        );
        let target_preempts = min_run.preempts_out().clone();

        // One endpoint of the bisection search:
        let mut target_endpoint_run = self.save_final_target_sched_events(min_run)?;

        if self.prefix_search {
            return self.prefix_search_and_report(&mut target_endpoint_run, None);
        }

        // The other endpoint of the bisection search:
        // What we thought was the final_pr can change here:
        let baseline_endpoint_run = match self.phase4_choose_baseline_sched_events(target_preempts)
        {
            Ok(run) => run,
            // A baseline run that did not finish, or no knock-out left,
            // gives no schedule to bisect against; the decision is still
            // within the target schedule, which a prefix search needs
            // alone.
            Err(error)
                if error.downcast_ref::<BaselineUnfinished>().is_some()
                    || error.downcast_ref::<NothingToKnockOut>().is_some() =>
            {
                let hint = if error.downcast_ref::<BaselineUnfinished>().is_some() {
                    "; a larger --trial-timeout may let the baseline finish"
                } else {
                    ""
                };
                eprintln!(
                    ":: {} {error}\n:: Searching within the target schedule instead \
                         (--prefix-search){hint}.",
                    "No baseline:".yellow().bold(),
                );
                return self.prefix_search_and_report(&mut target_endpoint_run, None);
            }
            Err(error) => return Err(error),
        };

        self.bisect_endpoints_and_report(target_endpoint_run, baseline_endpoint_run)
    }

    /// The default search between the target and baseline endpoint runs, and
    /// its report; the target's prefix search instead when the two runs'
    /// per-thread traces diverge ([`DivergentTraces`]) or no single swap
    /// separates them.
    fn bisect_endpoints_and_report(
        &mut self,
        mut target_endpoint_run: RunData,
        mut baseline_endpoint_run: RunData,
    ) -> anyhow::Result<ExitStatus> {
        let baseline_sched_name = baseline_endpoint_run.sched_out_file_name();
        let target_sched_name = target_endpoint_run.sched_out_file_name();
        let target = target_endpoint_run.sched_out();
        let baseline = baseline_endpoint_run.sched_out();

        eprintln!(
            ":: {} (event lengths {} / {}): {} {}",
            "Beginning bisection using endpoints".yellow().bold(),
            baseline.len(),
            target.len(),
            baseline_sched_name,
            target_sched_name,
        );
        // Splices of the two runs' orders keep each thread's events in
        // program order only when the runs' per-thread traces agree; where
        // they diverge, every spliced round may leave its schedule, and the
        // decision is still within the failing schedule.
        let divergence = trace_divergence(baseline, target);
        if divergence.diverges() {
            let why = DivergentTraces(divergence);
            eprintln!(
                ":: {} {why}\n:: Searching within the failing schedule instead \
                 (--prefix-search).",
                "No splice:".yellow().bold(),
            );
            return self.prefix_search_and_report(
                &mut target_endpoint_run,
                Some(format!(
                    "the default search did not bisect between the two runs: {why}. analyze \
                     searched within the failing schedule instead (--prefix-search)."
                )),
            );
        }
        let crit_sched = match self.phase5_bisect_traces(target, baseline) {
            Ok(crit_sched) => crit_sched,
            // The two endpoints differ in events only the failing run executed:
            // the decision is still within the failing schedule, and a prefix
            // search needs no baseline to find it.
            Err(error) if error.downcast_ref::<NoSingleSwap>().is_some() => {
                eprintln!(
                    ":: {} {error:#}\n:: Searching within the failing schedule instead \
                     (--prefix-search).",
                    "No single swap separates the endpoints:".yellow().bold(),
                );
                return self.prefix_search_and_report(&mut target_endpoint_run, None);
            }
            Err(error) => return Err(error),
        };

        let target_run = target_endpoint_run.root_path();
        let report = self.phase6_record_outputs(crit_sched, Some(&target_run))?;
        self.write_report_and_exit(report)
    }

    /// `--prefix-search` over the target endpoint's schedule, and its report,
    /// which notes `fallback`, why analyze searched the prefixes, if given.
    fn prefix_search_and_report(
        &mut self,
        target_endpoint_run: &mut RunData,
        fallback: Option<String>,
    ) -> anyhow::Result<ExitStatus> {
        let target = target_endpoint_run.preempts_out().clone();
        eprintln!(
            ":: {} ({} events): {}",
            "Beginning prefix search within the target schedule"
                .yellow()
                .bold(),
            target.schedevents().len(),
            target_endpoint_run.sched_out_file_name(),
        );
        let decision = PrefixDecision {
            fallback,
            ..self.phase5_prefix_search(&target)?
        };
        let report = self.phase6_record_prefix_outputs(decision)?;
        self.write_report_and_exit(report)
    }

    /// Write `report` to `--report-file`, if given, and return the exit status
    /// of a successful analysis.
    fn write_report_and_exit(&self, report: Report) -> anyhow::Result<ExitStatus> {
        if let Some(path) = &self.report_file {
            let txt = serde_json::to_string(&report).unwrap();
            std::fs::write(path, txt).expect("Unable to write report file");
            eprintln!(
                ":: {}\n {}",
                "Final analysis report written to:".green().bold(),
                path.display()
            );
        }
        self.success_exit_code
            .map_or(Ok(ExitStatus::SUCCESS), |exit_code| {
                Ok(ExitStatus::Exited(exit_code))
            })
    }

    /// This extra run, to record the schedule, thus converting Preemptions to a full Schedule
    /// would be unnecessary if we recorded that each time as we minimize.
    ///
    /// Argument: the last on-target run that was preemption based.
    fn save_final_target_sched_events(&mut self, mut last_run: RunData) -> anyhow::Result<RunData> {
        let pr = last_run.preempts_out();
        if pr.contains_schedevents() {
            let path = last_run.preempts_path_out();
            // This run already has sched events recorded... no extra run necessary.
            yellow_msg(&format!(
                "Using the last run as the target endpoint ({} contains sched events)",
                path.file_name().unwrap().to_string_lossy()
            ));
            Ok(last_run)
        } else {
            // Verify that the new preemption record does in fact now cause a matching execution,
            // and rerecord during this verification with full recording that include sched events
            self.progress.phase(Phase::TargetSchedule);
            yellow_msg(
                "Verify target endpoint preemption record causes criteria to hold and record sched events",
            );
            let runname = "verify_target_endpoint";
            let mut newrun = RunData::new(self, runname.to_string(), last_run.runopts.clone())
                .with_preempts_path_in(last_run.preempts_path_out().to_path_buf())
                .with_schedule_recording();
            newrun.launch()?;
            // With --run1-preemptions this is the first run of the target.
            self.record_endpoint(&newrun);
            eprintln!("    {}", newrun.to_repro());
            if !newrun.is_a_match() {
                bail!("Final preemption record still does not match target criteria");
            }
            Ok(newrun)
        }
    }

    // Returns the record, with one knockout, if it still satisfies the criteria that we want it not to.
    fn save_nearby_non_matching_sched_events(
        &self,
        runname: &str,
        non_matching_preempts: &PreemptionRecord,
    ) -> anyhow::Result<(bool, RunData)> {
        // Given preemptions that hermit analyze has determined are critical to match the criteria
        // (most commonly, a failing execution), removing the last critical preemption
        // ([`knock_out_latest`]) should cause the minimal execution change to now no longer match
        // the criteria (most commonly, an execution that now passes).

        // Validate the preemption record
        if let Err(e) = non_matching_preempts.validate() {
            bail!(
                "Hermit analyzer produced corrupt nearby non-matching preemption record, cannot proceed.\n\
                Error: {}\n\n\
                Corrupt record: {}",
                e,
                non_matching_preempts,
            );
        }

        let mut newrun = RunData::new_baseline(self, runname.to_string())?
            .with_preempts_in(non_matching_preempts.clone())
            .with_schedule_recording();

        // Verify that the new preemption record does in fact now cause a non-matching execution,
        // and rerecord during this verification with full recording that include sched events
        yellow_msg(
            "Verify preemption record *without* latest critical preempt causes criteria non-match. Also record sched events.",
        );
        if !newrun.launch_trial()?.finished() {
            return Err(BaselineUnfinished("--minimize's knock-out of its last preemption").into());
        }
        eprintln!("    {}", newrun.to_repro());
        if newrun.is_a_match() {
            eprintln!(
                "{}",
                ":: New preemption record still matches criteria! Attempting further knockouts.."
                    .red()
                    .bold(),
            );
            Ok((true, newrun))
        } else {
            Ok((false, newrun))
        }
    }

    /// Search for a target run. Return the run when found.
    fn do_search(&self) -> Result<RunData, Error> {
        self.search_rounds(|round, sched_seed| self.launch_search(round, sched_seed))
    }

    /// The `--search` loop: one chaos trial per round, each with the next
    /// schedule seed from the search RNG, until a trial matches. A trial that
    /// cannot finish (for example, one that `--max-log-bytes` ended) ends the
    /// search with its error unchanged, so main still reports it by its class.
    fn search_rounds(
        &self,
        mut launch: impl FnMut(u64, u64) -> Result<Option<RunData>, Error>,
    ) -> Result<RunData, Error> {
        let search_seed = match self.analyze_seed {
            Some(seed) if !self.analyze_seed_drawn => seed,
            drawn => {
                let seed = drawn.unwrap_or_else(rand::random);
                yellow_msg(&format!(
                    "WARNING: performing --search with system randomness, use --analyze-seed={} to repro.",
                    seed
                ));
                seed
            }
        };
        yellow_msg(&format!("Failure search using RNG seed {}", search_seed));
        let mut rng = Pcg64Mcg::seed_from_u64(search_seed);

        let mut round = 0;
        loop {
            let sched_seed = rng.random();
            if let Some(mut rundat) = launch(round, sched_seed)? {
                if self.verbose {
                    let preempts = rundat.preempts_path_out();
                    let init_schedule: PreemptionRecord =
                        PreemptionReader::new(preempts).load_all();
                    eprintln!(
                        ":: {}:\nSchedule:\n {}",
                        "Search successfully found a failing run with schedule:"
                            .green()
                            .bold(),
                        truncated(
                            1000,
                            serde_json::to_string(&init_schedule.clone_preemptions_only()).unwrap()
                        ),
                    );
                }
                return Ok(rundat);
            }
            round += 1;
        }
    }

    /// Does the run meet the criteria we are looking for (e.g. a particular error message).
    /// With `--verbose`, says which criteria it misses.
    pub fn output_matches(&self, out: &Output) -> bool {
        let misses = self.criteria_misses(out);
        if self.verbose {
            for miss in &misses {
                eprintln!("{miss}");
            }
        }
        misses.is_empty()
    }

    /// The target criteria `out` does not meet. (`RunData::launch` prints
    /// the guest's output under `--verbose`.)
    fn criteria_misses(&self, out: &Output) -> Vec<String> {
        let mut misses = Vec::new();
        for (stream, pattern, text) in [
            ("stdout", &self.target_stdout, &out.stdout),
            ("stderr", &self.target_stderr, &out.stderr),
        ] {
            if let Some(pattern) = pattern
                && !pattern.is_match(&String::from_utf8_lossy(text))
            {
                misses.push(format!("Mismatch for {stream} pattern {pattern}"));
            }
        }
        if !self.target_exit_code.is_match(out.status) {
            misses.push(format!(
                "  Exit code {} is not what we're looking for.",
                out.status.into_raw()
            ));
        }
        misses
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser as _;

    use super::*;
    use crate::container::LogCapExceeded;
    use crate::container::PolicyRefusal;

    fn critical(tids: &[i32], index: usize) -> CriticalSchedule {
        let schedule: Vec<SchedEvent> = tids
            .iter()
            .map(|&tid| SchedEvent::branches(detcore::types::DetTid::from_raw(tid), 1))
            .collect();
        CriticalSchedule {
            failing_schedule: schedule.clone(),
            passing_schedule: schedule,
            critical_event_index: index,
        }
    }

    /// A `--prefix-search` trial replays the target record's first events
    /// together with the callback checkpoints of the slices those events
    /// end, so its slices end where the recording's did; a checkpoint past
    /// the prefix is dropped, and so is the one at its end while an event
    /// follows: that slice end is the next event's decision.
    #[test]
    fn a_prefix_trial_keeps_the_target_checkpoints_within_its_prefix() {
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = format!("--tmp-dir={}", parent.path().display());
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", tmp_dir.as_str(), "--", "/bin/true"]).unwrap();
        let _workspace = options.phase0_initialize(Retention::Remove).unwrap();
        let branches = |tid: i32, count: u32| {
            serde_json::json!({
                "dettid": tid, "op": "Branch", "count": count,
                "start_rip": null, "end_rip": null, "end_time": null
            })
        };
        let checkpoint = |row: usize, progress: u64, time: u64| {
            serde_json::json!({
                "history_index": row,
                "origin": {
                    "callback": "PmuTimer", "site": "Prehook", "progress": progress,
                    "last_op": "Branch", "time": time, "rcbs": progress
                }
            })
        };
        let target: PreemptionRecord = serde_json::from_value(serde_json::json!({
            "per_thread": {
                "3": {
                    "final_prio": 1000,
                    "prio_changes": [[100, 1000], [300, 1000]],
                    "preemption_rcbs": [10, 15]
                },
                "5": {"final_prio": 1000, "prio_changes": [[200, 1000]], "preemption_rcbs": [4]}
            },
            "global": [branches(3, 10), branches(5, 4), branches(3, 5)],
            "callback_checkpoints": {
                "3": [checkpoint(0, 10, 100), checkpoint(1, 15, 300)],
                "5": [checkpoint(0, 4, 200)]
            }
        }))
        .unwrap();
        target.validate().unwrap();
        for (len, kept) in [(0, 0), (1, 0), (2, 1), (3, 3)] {
            let mut run = options
                .prefix_trial("prefix_round", &target, len, None)
                .unwrap();
            let written: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(run.sched_path_in()).unwrap())
                    .unwrap();
            assert_eq!(written["global"].as_array().unwrap().len(), len);
            let points: usize = ["3", "5"]
                .iter()
                .map(|tid| {
                    written["callback_checkpoints"][tid]
                        .as_array()
                        .map_or(0, |points| points.len())
                })
                .sum();
            assert_eq!(points, kept, "prefix of {len}: {written}");
            PreemptionReader::new(run.sched_path_in())
                .load_all()
                .validate()
                .unwrap();
        }
    }

    /// The sentences a `--prefix-search` report gives its stacks present two
    /// threads at the deciding event as the pair whose order decides the
    /// outcome, and one thread, which the scheduler runs there without the
    /// event too, as that one thread: never as a race with itself.
    #[test]
    fn a_prefix_report_says_when_both_stacks_are_one_thread() {
        let (reader, binder) = (
            detcore::types::DetTid::from_raw(7),
            detcore::types::DetTid::from_raw(9),
        );
        let pair = stack_pair_sentences(binder, Some(reader));
        assert!(
            pair.contains("stack2 is the thread that runs there instead"),
            "{pair}"
        );
        assert!(pair.contains("which of the two runs first"), "{pair}");

        let one = stack_pair_sentences(reader, Some(reader));
        assert!(one.contains("both belong to thread 7"), "{one}");
        assert!(one.contains("not two threads racing"), "{one}");
        for race in ["runs there instead", "which of the two runs first"] {
            assert!(!one.contains(race), "{race:?} in\n{one}");
        }

        // A run without the event that ended before it keeps the usual
        // sentences: its stack2 is empty, and a warning says why.
        assert_eq!(stack_pair_sentences(reader, None), pair);
    }

    /// `--tmp-dir` names the directory the workspace is created in, and is
    /// created if it is missing. The workspace goes when analyze is done
    /// with it, unless `--keep-tmp` or `--verbose` keeps it (not
    /// `--selfcheck`, though it writes run logs there too); the `--tmp-dir`
    /// stays.
    #[test]
    fn the_workspace_is_created_in_the_tmp_dir() {
        for (option, kept) in [
            (None, false),
            (Some("--keep-tmp"), true),
            (Some("--verbose"), true),
            (Some("--selfcheck"), false),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let requested = parent.path().join("not-yet-created");
            let tmp_dir = format!("--tmp-dir={}", requested.display());
            let mut argv = vec!["analyze", tmp_dir.as_str(), "--", "/bin/true"];
            argv.splice(1..1, option);
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let retention = options.workspace_retention();
            assert_eq!(
                retention,
                if kept {
                    Retention::Keep { because: option }
                } else {
                    Retention::Remove
                },
                "{option:?}"
            );
            let guard = options.phase0_initialize(retention).unwrap();
            let workspace = options.get_tmp().unwrap().to_path_buf();
            assert_eq!(workspace.parent(), Some(requested.as_path()));
            assert!(workspace.is_dir(), "{}", workspace.display());
            drop(guard);
            assert_eq!(
                workspace.exists(),
                kept,
                "{option:?}: {}",
                workspace.display()
            );
            assert!(requested.is_dir(), "{}", requested.display());
        }
    }

    /// The prefix search finds the event at which the outcome flips, wherever
    /// it lies, in about log2(len) trials once its endpoints differ.
    #[test]
    fn the_prefix_search_finds_the_deciding_event() {
        for (len, decision) in [
            (1, 0),
            (2, 1),
            (37_710, 0),
            (37_710, 33_225),
            (37_710, 37_709),
        ] {
            let mut trials = 0;
            let found = search_prefix_boundary(len, TrialEnd::Missed, |prefix| {
                trials += 1;
                Ok(if prefix > decision {
                    TrialEnd::Met
                } else {
                    TrialEnd::Missed
                })
            })
            .unwrap();
            assert_eq!(
                found,
                Boundary {
                    passing: decision,
                    passing_end: TrialEnd::Missed
                },
                "len {len}"
            );
            assert!(
                trials <= (len as f64).log2().ceil() as usize,
                "{trials} trials"
            );
        }
    }

    /// When the run arguments' own `--timeout` stopped the boundary's trial,
    /// the report calls it a hang, a third outcome rather than a run that misses,
    /// says that the boundary rests on a wall-clock bound, and names the
    /// longest shorter prefix the search replayed that finished, or that none
    /// did. Before, the progress line and the header called such a trial the
    /// baseline condition.
    #[test]
    fn a_boundary_trial_still_stopped_is_reported_as_a_hang() {
        let note = unfinished_boundary_warning(
            35_631,
            Some(60),
            Some((9_000, TrialEnd::Missed)),
            "raise --trial-timeout",
        );
        for expected in [
            "replaying the first 35631 events and then scheduling without chaos did not finish \
             within 60 s, and no longer bound could be given to confirm it",
            "A hang is a third outcome, not a run that misses the target",
            "this boundary rests on a wall-clock bound, not on an outcome",
            "(raise --trial-timeout)",
            "the prefix of 9000 events, which missed the target.",
        ] {
            assert!(note.contains(expected), "no {expected:?} in {note}");
        }
        let unconfirmed =
            unfinished_boundary_warning(1, Some(120), None, "raise the run arguments' --timeout");
        for expected in [
            "did not finish within 120 s, and no longer bound could be given to confirm it",
            "(raise the run arguments' --timeout)",
            "No shorter prefix the search replayed finished.",
        ] {
            assert!(
                unconfirmed.contains(expected),
                "no {expected:?} in {unconfirmed}"
            );
        }
    }

    /// The longest shorter prefix the hang note names finished: a stopped or
    /// a refused trial does not count, nor a prefix at or past the boundary,
    /// nor another continuation's trial, and a prefix's last trial (a
    /// confirmation, say) is the one that counts.
    #[test]
    fn the_longest_finished_prefix_below_the_boundary_is_named() {
        let ns = NonZeroU64::new(100_000);
        let other = NonZeroU64::new(200_000_000);
        let bound = TrialEnd::Unfinished(Stop::Bound);
        let refusal = TrialEnd::Unfinished(Stop::Refusal);
        let ends = BTreeMap::from([
            ((0, ns), (TrialEnd::Missed, Some(60))),
            ((10, ns), (TrialEnd::Missed, Some(240))),
            ((20, ns), (refusal, Some(60))),
            ((25, other), (TrialEnd::Missed, Some(60))),
            ((30, ns), (bound, Some(60))),
            ((40, ns), (bound, Some(240))),
            ((41, ns), (TrialEnd::Met, Some(60))),
        ]);
        assert_eq!(
            longest_finished_below(&ends, ns, 40),
            Some((10, TrialEnd::Missed))
        );
        assert_eq!(
            longest_finished_below(&ends, ns, 10),
            Some((0, TrialEnd::Missed))
        );
        assert_eq!(longest_finished_below(&ends, ns, 0), None);
        assert_eq!(
            longest_finished_below(&ends, other, 40),
            Some((25, TrialEnd::Missed))
        );
    }

    /// `auto` continues at the run's own timeslice, then at hermit's default
    /// 200 ms when that is coarser, never at a shorter one; a run without PMU
    /// timeslices continues as it is.
    #[test]
    fn auto_continuations_only_get_coarser() {
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        assert_eq!(continuation_candidates(ns(200_000_000)), [ns(200_000_000)]);
        assert_eq!(
            continuation_candidates(ns(1_000)),
            [ns(1_000), ns(200_000_000)]
        );
        assert_eq!(continuation_candidates(ns(500_000_000)), [ns(500_000_000)]);
        assert_eq!(continuation_candidates(None), [None]);
    }

    /// The search continues at the first timeslice whose endpoint trials
    /// differ: one at which the scheduler alone meets the target, or the
    /// whole schedule's replay misses it or does not finish, is skipped. When
    /// every timeslice fails, the error names each, says what it showed, and
    /// what to do.
    #[test]
    fn the_prefix_search_continues_at_the_first_usable_timeslice() {
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        let candidates = [ns(1_000), ns(200_000_000)];
        let end = |met: bool| if met { TrialEnd::Met } else { TrialEnd::Missed };
        // At 1000 ns the scheduler alone already meets the target (a race
        // found only at a short timeslice); at 200 ms it does not.
        let mut trials = Vec::new();
        let chosen = choose_continuation(&candidates, 50, |prefix, timeslice| {
            trials.push((prefix, timeslice));
            Ok(end(prefix > 0 || timeslice == ns(1_000)))
        })
        .unwrap();
        assert_eq!(chosen, ns(200_000_000));
        assert_eq!(
            trials,
            [(0, ns(1_000)), (0, ns(200_000_000)), (50, ns(200_000_000))]
        );
        assert_eq!(
            choose_continuation(&candidates, 50, |prefix, _| Ok(end(prefix > 0))).unwrap(),
            ns(1_000),
            "the first usable one wins"
        );

        let alone = choose_continuation(&candidates, 50, |_, _| Ok(TrialEnd::Met)).unwrap_err();
        let message = alone.to_string();
        assert!(
            message.contains("the scheduler alone already meets the target criteria at a 1000 ns timeslice and at a 200000000 ns timeslice"),
            "{message}"
        );
        assert!(
            message.contains("prefix_round_001.repro") && message.contains("--run2-seed"),
            "{message}"
        );
        let replay = choose_continuation(&candidates, 50, |_, _| Ok(TrialEnd::Missed)).unwrap_err();
        assert!(
            replay.to_string().contains(
                "at a 1000 ns timeslice replaying the whole target schedule misses the target"
            ),
            "{replay}"
        );
        assert!(replay.to_string().contains("--selfcheck"), "{replay}");
        let unfinished = choose_continuation(&candidates, 50, |prefix, _| {
            Ok(if prefix == 0 {
                TrialEnd::Missed
            } else {
                TrialEnd::Unfinished(Stop::Bound)
            })
        })
        .unwrap_err();
        assert!(
            unfinished
                .to_string()
                .contains("did not finish (a bound or a refusal stopped it)")
                && unfinished.to_string().contains("raise --trial-timeout"),
            "{unfinished}"
        );
    }

    /// Only a criterion the run misses is reported. Before, `--verbose` printed "Mismatch for stderr pattern"
    /// for every run, right before saying that the stderr matched.
    #[test]
    fn only_the_criteria_a_run_misses_are_reported() {
        let options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--target-stderr=double-close",
            "--target-stdout=^$",
            "--",
            "/bin/true",
        ])
        .unwrap();
        let output = |stdout: &str, stderr: &str, code: i32| Output {
            status: ExitStatus::Exited(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        };
        let met = output("", "fdsan: double-close of fd 5\n", 1);
        assert!(options.criteria_misses(&met).is_empty());
        assert!(options.output_matches(&met));

        let missed = options.criteria_misses(&output("ok\n", "all good\n", 0));
        assert_eq!(missed.len(), 3, "{missed:?}");
        assert_eq!(missed[0], "Mismatch for stdout pattern ^$");
        assert_eq!(missed[1], "Mismatch for stderr pattern double-close");
        assert!(missed[2].contains("Exit code 0 is not what we're looking for"));
    }

    fn events(tids: &[i32]) -> Vec<SchedEvent> {
        tids.iter()
            .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 10))
            .collect()
    }

    /// A replay leaves its schedule at the first event that differs from the
    /// schedule's as the scheduler compares them when it takes a stack
    /// (another thread, operation or instruction pointer, but not another
    /// count), or where it ends; only the events up to the critical pair
    /// count.
    #[test]
    fn a_replay_leaves_its_schedule_at_the_first_other_event() {
        let requested = events(&[3, 5, 3, 5, 7]);
        assert_eq!(departure(&requested, &events(&[3, 5, 3, 5, 7]), 5), None);
        let mut other_count = events(&[3, 5, 3, 5, 7]);
        other_count[2].count = 99;
        assert_eq!(departure(&requested, &other_count, 5), None);
        let realized = events(&[3, 5, 9, 5, 7]);
        assert_eq!(
            departure(&requested, &realized, 5),
            Some(Departure::OtherEvent {
                index: 2,
                expected: requested[2].clone(),
                ran: realized[2].clone(),
            })
        );
        let mut other_op = requested.clone();
        other_op[1] = SchedEvent::syscall(
            DetTid::from_raw(5),
            reverie::syscalls::Sysno::write,
            detcore_model::schedule::SyscallPhase::Posthook,
        );
        assert!(matches!(
            departure(&requested, &other_op, 5),
            Some(Departure::OtherEvent { index: 1, .. })
        ));
        assert_eq!(departure(&requested, &events(&[3, 5, 3, 5, 9]), 4), None);
        assert_eq!(
            departure(&requested, &events(&[3, 5]), 5),
            Some(Departure::Ended { events: 2 })
        );
    }

    /// A stack either replay captured is reported, in the passing order, with
    /// the replay it came from: the target replay's events #index - 1 and
    /// #index are the passing schedule's #index and #index - 1. A replay that
    /// followed its schedule through the operation is preferred; a stack from
    /// one that did not is kept only if that replay ran the operation's
    /// thread there, and is said to be possibly another occurrence. One
    /// neither gives says where each replay left its schedule. Before, a
    /// baseline replay that left its schedule early reported both stacks as
    /// unavailable, without a reason.
    #[test]
    fn each_racing_stack_comes_from_a_replay_that_reached_it() {
        let tid = DetTid::from_raw;
        let on = |thread: i32, name: &str| -> PrettyBacktrace {
            serde_json::from_value(serde_json::json!({
                "thread_id": thread, "thread_name": name, "frames": []
            }))
            .unwrap()
        };
        let replay = |finished, stacks, left_schedule, ran| StackReplay {
            finished,
            met_target: false,
            stacks,
            left_schedule,
            ran,
        };
        // The passing schedule runs thread 5's read at #1093, then thread 7's
        // close at #1094; the failing one the other way round.
        let threads = [tid(5), tid(7)];
        let left = Some(Departure::OtherEvent {
            index: 735,
            expected: SchedEvent::branches(tid(5), 10),
            ran: SchedEvent::branches(tid(9), 10),
        });
        let baseline = replay(
            true,
            [None, None],
            left.clone(),
            [Some(tid(9)), Some(tid(9))],
        );
        let target = replay(
            true,
            [Some(on(7, "close")), None],
            None,
            [Some(tid(7)), Some(tid(5))],
        );
        let [first, second] = merge_stacks(&baseline, &target, 1094, threads);
        assert_eq!(first, None);
        assert_eq!(
            second,
            Some((on(7, "close"), "the target replay".to_owned())),
            "the target's first is the passing second"
        );

        let ended = StackReplay {
            left_schedule: Some(Departure::Ended { events: 1093 }),
            ..target
        };
        let reason = missing_stack_reason("stack1", (1093, 1094), &baseline, &ended);
        assert!(
            reason.contains("The baseline replay left its schedule: it ran (tid9 cnt=10 Branch) at event #735, where the schedule has (tid5 cnt=10 Branch); the target replay left its schedule: it ended after 1093 events."),
            "{reason}"
        );
        assert!(reason.contains("--selfcheck"), "{reason}");
        let followed_without = replay(true, [None, None], None, [Some(tid(7)), Some(tid(5))]);
        let reason = missing_stack_reason("stack1", (1093, 1094), &baseline, &followed_without);
        assert!(
            reason.contains("the target replay followed its schedule through event #1094, but left no readable stack there"),
            "{reason}"
        );
        let stopped = replay(false, [None, None], Some(Departure::Unknown), [None, None]);
        let reason = missing_stack_reason("stack1", (1093, 1094), &baseline, &stopped);
        assert!(
            reason.contains("did not finish") && reason.contains("--trial-timeout"),
            "{reason}"
        );

        // The baseline replay took its stacks but left its schedule first:
        // the target replay, which followed its own, is preferred.
        let early = replay(
            true,
            [Some(on(5, "early-read")), Some(on(9, "early-write"))],
            left,
            [Some(tid(5)), Some(tid(9))],
        );
        let followed = replay(
            true,
            [Some(on(7, "close")), Some(on(5, "read"))],
            None,
            [Some(tid(7)), Some(tid(5))],
        );
        assert_eq!(
            merge_stacks(&early, &followed, 1094, threads),
            [
                Some((on(5, "read"), "the target replay".to_owned())),
                Some((on(7, "close"), "the target replay".to_owned())),
            ]
        );
        // With no replay that followed its schedule, a stack taken on the
        // operation's thread is kept with a caveat; one taken on another
        // thread (thread 9 where the schedule has thread 7) is left out.
        let [read, close] = merge_stacks(&early, &stopped, 1094, threads);
        let (read, source) = read.unwrap();
        assert_eq!(read, on(5, "early-read"));
        assert!(source.contains("may be another occurrence"), "{source}");
        assert_eq!(close, None);
        // A stopped replay recorded no schedule; its stack still counts when
        // it was taken on the operation's thread.
        let stopped_with = replay(
            false,
            [Some(on(7, "close")), Some(on(9, "other"))],
            Some(Departure::Unknown),
            [None, None],
        );
        let [read, close] = merge_stacks(&baseline, &stopped_with, 1094, threads);
        assert_eq!(read, None);
        assert_eq!(close.unwrap().0, on(7, "close"));

        let both = replay(
            true,
            [Some(on(5, "read")), Some(on(7, "close"))],
            None,
            [Some(tid(5)), Some(tid(7))],
        );
        assert_eq!(
            merge_stacks(&both, &followed, 1094, threads).map(|stack| stack.unwrap().1),
            ["the baseline replay", "the baseline replay"],
            "the baseline replay's own stacks come first"
        );
    }

    /// The final target schedule meets the criteria with another failure
    /// when the line its output pattern matches differs from the target
    /// run's; output the pattern does not match, which can differ with the
    /// interleaving, and line endings around the match are not compared.
    #[test]
    fn a_target_replay_with_another_failure_than_the_target_run_is_named() {
        let workspace = tempfile::tempdir().unwrap();
        let (target, replay) = (
            workspace.path().join("run1_target"),
            workspace.path().join("final_target_for_stacktraces"),
        );
        let pattern = Regex::new("fdsan: ").unwrap();
        let compare = |target_stderr: &str, replay_stderr: &str| {
            std::fs::write(target.with_extension("stderr"), target_stderr).unwrap();
            std::fs::write(replay.with_extension("stderr"), replay_stderr).unwrap();
            changed_output(
                &target,
                &replay,
                [("stdout", None), ("stderr", Some(&pattern))],
            )
        };
        assert_eq!(
            compare(
                "start\nfdsan: double-close\n",
                "other start\nfdsan: double-close"
            ),
            None
        );
        assert_eq!(compare("x\n\n", "x\n"), None, "no match, no comparison");
        let change = compare(
            "start\nfdsan: double-close\n",
            "start\nfdsan: failed to exchange ownership\n",
        )
        .unwrap();
        assert!(
            change.contains("the stderr line that --target-stderr matches is \"fdsan: failed to exchange ownership\", where the target run's is \"fdsan: double-close\""),
            "{change}"
        );
        assert!(change.contains("narrow --target-stderr"), "{change}");
        assert_eq!(
            changed_output(
                &workspace.path().join("absent"),
                &replay,
                [("stdout", None), ("stderr", Some(&pattern))],
            ),
            None,
            "no target run output (hermit bisect): no comparison"
        );
    }

    /// `--minimize`'s baseline knock-out removes a real intervention each
    /// time: the latest priority change, then whole threads, never a thread
    /// left at the default priority (which would change nothing), and gives
    /// up when none is left. Before, the records later knock-outs start
    /// from (recorded runs) named every thread, and once no priority change
    /// was left a knock-out could remove one of those, changing nothing.
    #[test]
    fn the_baseline_knock_out_removes_a_real_intervention_until_none_is_left() {
        use std::collections::BTreeMap;

        use detcore::types::LogicalTime;
        let at = LogicalTime::from_nanos;
        let record = PreemptionRecord::from_vecs(&BTreeMap::from([
            (DetTid::from_raw(3), vec![(at(0), 4923), (at(500), 1200)]),
            (DetTid::from_raw(5), vec![(at(0), 352)]),
            (DetTid::from_raw(9), vec![]),
        ]));
        let threads = |record: &PreemptionRecord| -> Vec<u32> {
            record
                .extract_all()
                .keys()
                .map(|tid| tid.as_raw() as u32)
                .collect()
        };
        let first = knock_out_latest(&record).unwrap();
        assert_eq!(threads(&first), [3, 5], "thread 9 had nothing to knock out");
        assert_ne!(
            first,
            record.without_default_threads(),
            "the knock-out changed the record"
        );
        let mut knocked = first;
        let mut steps = 1;
        while let Some(next) = knock_out_latest(&knocked) {
            assert!(!threads(&next).contains(&9));
            knocked = next;
            steps += 1;
            assert!(steps < 10, "the knock-outs do not end");
        }
        assert_eq!(
            threads(&knocked.without_default_threads()),
            Vec::<u32>::new()
        );
    }

    /// `--minimize`'s baseline search knocks out one more intervention of the
    /// record it last replayed each time, until a replay misses the target or
    /// none is left; it never starts again from a run's own recording, which
    /// holds more priority changes than it replayed. Before, a target whose
    /// replays kept meeting the target knocked out chaos's own changes
    /// without end.
    #[test]
    fn the_baseline_search_knocks_out_the_replayed_record_until_a_replay_misses() {
        use std::collections::BTreeMap;

        use detcore::types::LogicalTime;
        let at = LogicalTime::from_nanos;
        let record = PreemptionRecord::from_vecs(&BTreeMap::from([
            (DetTid::from_raw(3), vec![(at(0), 4923), (at(500), 1200)]),
            (DetTid::from_raw(5), vec![(at(0), 352)]),
        ]));
        let mut replayed = Vec::new();
        let missed = knock_out_until_missed(record.clone(), |attempt, knocked_out| {
            replayed.push(knocked_out.clone());
            Ok((attempt == 2).then_some(attempt))
        })
        .unwrap();
        assert_eq!(missed, 2);
        assert_eq!(replayed[0], knock_out_latest(&record).unwrap());
        assert_eq!(replayed[1], knock_out_latest(&replayed[0]).unwrap());

        let mut attempts = 0;
        let error = knock_out_until_missed(record, |_, _| {
            attempts += 1;
            Ok(None::<()>)
        })
        .unwrap_err();
        assert!(
            error.downcast_ref::<NothingToKnockOut>().is_some(),
            "{error:#}"
        );
        assert!(
            attempts == 3,
            "{attempts} knock-outs for three interventions"
        );
    }

    /// A report written before the deciding event was reported still reads.
    #[test]
    fn a_report_without_a_deciding_event_reads() {
        let report: Report = serde_json::from_value(serde_json::json!({
            "header": "h",
            "additional_context": "c",
            "baseline_run": true,
            "critical_event1": {"event_index": 1, "stack": null},
            "critical_event2": {"event_index": 2, "stack": null},
        }))
        .unwrap();
        assert_eq!(report.deciding_event, None);
        assert!(
            !serde_json::to_string(&report)
                .unwrap()
                .contains("deciding_event")
        );
    }

    /// A timing decision: the deciding thread runs on in both continuations,
    /// and the report names how long, and which thread runs next in each.
    #[test]
    fn a_timing_decision_names_the_thread_that_runs_next_in_each_continuation() {
        let with = events(&[5, 3, 3, 3, 7, 5]);
        let without = events(&[5, 3, 3, 5, 7]);
        let thread = DetTid::from_raw(3);
        let with = slice_from(&with, 1, thread);
        let without = slice_from(&without, 1, thread);
        assert_eq!(
            with,
            Slice {
                events: 3,
                branches: 30,
                next: Some((4, DetTid::from_raw(7)))
            }
        );
        assert_eq!(without.next, Some((3, DetTid::from_raw(5))));
        assert_eq!(slice_from(&events(&[3, 3]), 0, thread).next, None);

        let header = timing_decision_header(
            1,
            &SchedEvent::branches(thread, 10),
            (&with, &without),
            "matching stderr",
            NonZeroU64::new(200_000_000),
            None,
        );
        for expected in [
            "it is a timing decision: with or without it, the scheduler runs thread 3 there",
            "thread 3 runs 3 events (30 retired conditional branches) from event #1 on, then thread 7 runs at event #4 (stack1), and the run meets the target criteria (matching stderr)",
            "Without it, scheduling without chaos at a 200000000 ns timeslice, thread 3 runs 2 events (20 retired conditional branches), then thread 5 runs at event #3 (stack2), and the run does not",
        ] {
            assert!(header.contains(expected), "no {expected:?} in\n{header}");
        }

        // The thread that runs next is where it was in both: stack1 and
        // stack2 show where thread 3 stops instead, and the next threads are
        // named without them.
        let header = timing_decision_header(
            1,
            &SchedEvent::branches(thread, 10),
            (&with, &without),
            "matching stderr",
            NonZeroU64::new(200_000_000),
            Some([3, 2]),
        );
        for expected in [
            "from event #1 on, then thread 7 runs at event #4, and the run meets",
            "then thread 5 runs at event #3, and the run does not",
            "so stack1 and stack2 show where thread 3 is when its slice ends instead: at event #3 with the event, at event #2 without it.",
        ] {
            assert!(header.contains(expected), "no {expected:?} in\n{header}");
        }
        assert!(!header.contains("(stack1)"), "{header}");
    }

    /// The deciding thread's slice ends replace the next threads' stacks only
    /// when those have the same frames (whichever threads they are) and both
    /// were captured.
    #[test]
    fn slice_ends_are_reported_only_when_the_next_stacks_tell_nothing_apart() {
        let stack = |thread: i32, ips: &[u64]| -> PrettyBacktrace {
            let frames: Vec<_> = ips
                .iter()
                .map(|ip| {
                    serde_json::json!({
                        "frame": {"ip": ip, "is_signal": false},
                        "symbol": null,
                        "locations": []
                    })
                })
                .collect();
            serde_json::from_value(serde_json::json!({
                "thread_id": thread, "thread_name": null, "frames": frames
            }))
            .unwrap()
        };
        let thread = DetTid::from_raw(3);
        let with = slice_from(&events(&[5, 3, 3, 3, 7, 5]), 1, thread);
        let without = slice_from(&events(&[5, 3, 3, 5, 7]), 1, thread);
        let started = [0x10, 0x20];
        // Two different threads, both just started.
        let (seven, five) = (stack(7, &started), stack(5, &started));
        assert_eq!(
            slice_end_events(&with, &without, [Some(&seven), Some(&five)]),
            Some([3, 2])
        );
        let elsewhere = stack(5, &[0x30, 0x20]);
        assert_eq!(
            slice_end_events(&with, &without, [Some(&seven), Some(&elsewhere)]),
            None
        );
        assert_eq!(
            slice_end_events(&with, &without, [Some(&seven), None]),
            None
        );
        let alone = slice_from(&events(&[5, 3, 3]), 1, thread);
        assert_eq!(
            slice_end_events(&alone, &without, [Some(&seven), Some(&five)]),
            None
        );
    }

    #[test]
    fn a_same_thread_critical_pair_is_flagged_and_a_cross_thread_one_is_not() {
        let caveat = critical_pair_caveat(&critical(&[3, 3, 5], 1), 2)
            .expect("two events of thread 3 cannot be reordered");
        assert!(caveat.contains("both belong to thread 3"), "{caveat}");
        assert!(caveat.contains("2 trial(s) did not finish"), "{caveat}");

        assert_eq!(
            critical_pair_caveat(&critical(&[3, 3, 5], 2), 0),
            None,
            "3 then 5 is a race"
        );
    }

    /// A search round whose trial `--max-log-bytes` ended stops the `--search`
    /// loop and returns that error, still a `LogCapExceeded`, so main reports
    /// it as the cap (123). Before, the loop panicked on any round's error.
    #[test]
    fn a_failing_search_round_ends_the_search_with_its_error() {
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        for failing_round in 1..=4 {
            let mut rounds = 0;
            let result = options.search_rounds(|round, _sched_seed| {
                assert_eq!(round, rounds, "rounds are numbered from 0");
                rounds += 1;
                if rounds == failing_round {
                    return Err(Error::new(LogCapExceeded));
                }
                // An earlier round finished and did not match.
                Ok(None)
            });
            let Err(error) = result else {
                panic!("round {failing_round} failed, yet the search returned a run")
            };
            assert!(
                error.downcast_ref::<LogCapExceeded>().is_some(),
                "round {failing_round}: the error lost its type: {error:#}"
            );
            assert_eq!(
                rounds, failing_round,
                "the search ran more rounds after round {failing_round} failed"
            );
        }
    }

    const RECORDED: &str = "2000-12-31T23:59:59.123456789Z";
    const RECORDED_RFC3339: &str = "2000-12-31T23:59:59.123456789+00:00";
    const OTHER: &str = "2026-01-01T00:00:00Z";

    /// A preemption record written to `dir` under `name`, with the given
    /// recorded epoch.
    fn record_with_epoch(dir: &Path, name: &str, epoch: Option<&str>) -> PathBuf {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("test-resources/flaky_cas_sequence_schedules-passing.json");
        let mut record: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(source).unwrap()).unwrap();
        if let Some(epoch) = epoch {
            record["epoch"] = serde_json::Value::from(epoch);
        }
        let path = dir.join(format!("{name}.json"));
        std::fs::write(&path, record.to_string()).unwrap();
        path
    }

    /// Through `prepare`, which `main` runs before phase 0: the trial
    /// configuration `hermit analyze` would use for these records, or its
    /// refusal.
    fn prepared_analysis(
        dir: &Path,
        run1: Option<&str>,
        run2: Option<&str>,
        run_args: &[&str],
    ) -> anyhow::Result<AnalyzeOpts> {
        let run1 = format!(
            "--run1-preemptions={}",
            record_with_epoch(dir, "run1", run1).display()
        );
        let run2 = run2.map(|epoch| {
            format!(
                "--run2-preemptions={}",
                record_with_epoch(dir, "run2", Some(epoch)).display()
            )
        });
        let mut argv = vec!["hermit", "analyze", run1.as_str()];
        argv.extend(run2.as_deref());
        argv.push("--");
        argv.extend(run_args);
        argv.push("/bin/true");
        let args = crate::Args::try_parse_from(&argv).unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.prepare(&args.global)?;
        options.tmp_dir = Some(dir.to_path_buf());
        Ok(*options)
    }

    /// Analyze runs every trial, the baseline it records without `--chaos`
    /// included, under the `--expired-wait-charge` of its target: the run
    /// arguments' own, or else `--run1-preemptions`'s rule, or else `each`
    /// for the chaos target it records itself. The value is put in the run
    /// arguments, every record analyze writes carries it, and a
    /// `--run2-preemptions` record under the other rule is refused.
    #[test]
    fn analyze_runs_every_trial_under_the_targets_expired_wait_charge() {
        use detcore_model::config::ExpiredWaitCharge;
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, once: bool| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if once {
                value["expired_wait_runs_charged_once"] = serde_json::Value::from(true);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: Option<&Path>, run2: Option<&Path>, run_args: &[&str]| {
            let run1 = run1.map(|path| format!("--run1-preemptions={}", path.display()));
            let run2 = run2.map(|path| format!("--run2-preemptions={}", path.display()));
            let mut argv = vec!["hermit", "analyze"];
            argv.extend(run1.as_deref());
            argv.extend(run2.as_deref());
            argv.push("--");
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let each = record("each", false);
        let once = record("once", true);
        let written = |options: &AnalyzeOpts| {
            let (failing, passing, trial) = (
                dir.path().join("final-failing.json"),
                dir.path().join("final-passing.json"),
                dir.path().join("trial.json"),
            );
            options.write_final_schedules(Vec::new(), &failing, Vec::new(), &passing);
            options.write_trial_schedule(Vec::new(), &trial).unwrap();
            [failing, passing, trial].map(|path| {
                detcore::preemptions::read_recorded_expired_wait_runs_charged_once(&path).unwrap()
            })
        };
        for (run1, run2, run_args, expected) in [
            (None, None, vec![], ExpiredWaitCharge::Each),
            (
                None,
                None,
                vec!["--expired-wait-charge=once"],
                ExpiredWaitCharge::Once,
            ),
            (Some(&each), None, vec![], ExpiredWaitCharge::Each),
            (Some(&once), None, vec![], ExpiredWaitCharge::Once),
            (Some(&once), Some(&once), vec![], ExpiredWaitCharge::Once),
            (Some(&each), Some(&each), vec![], ExpiredWaitCharge::Each),
        ] {
            let case = format!("{run1:?} {run2:?} {run_args:?}");
            let options = prepare(
                run1.map(|p| p.as_path()),
                run2.map(|p| p.as_path()),
                &run_args,
            )
            .unwrap_or_else(|error| panic!("{case}: {error:#}"));
            assert_eq!(options.trial_expired_wait_charge(), expected, "{case}");
            assert_eq!(
                written(&options),
                [expected == ExpiredWaitCharge::Once; 3],
                "{case}"
            );
        }
        // Without --run1-preemptions the trials use `each`, so a plain
        // (`once`) --run2-preemptions baseline is refused.
        assert_eq!(
            prepare(None, Some(&each), &[])
                .unwrap()
                .trial_expired_wait_charge(),
            ExpiredWaitCharge::Each
        );
        assert!(prepare(None, Some(&once), &[]).is_err());
        for (run1, run2, run_args) in [
            (&once, Some(&each), vec![]),
            (&each, Some(&once), vec![]),
            (&once, None, vec!["--expired-wait-charge=each"]),
        ] {
            let case = format!("{run1:?} {run2:?} {run_args:?}");
            let error = prepare(Some(run1), run2.map(|p| p.as_path()), &run_args).expect_err(&case);
            assert!(
                format!("{error:#}").contains("--expired-wait-charge"),
                "{case}: {error:#}"
            );
        }
    }

    /// Analyze refuses, before any trial, a `--run1-preemptions` record made
    /// under another `--pipe-wake-latency` than the trials' own, or one its
    /// run arguments replay; its final schedules carry the trials' value
    /// (https://github.com/rrnewton/hermit/issues/4380).
    #[test]
    fn analyze_refuses_records_under_another_pipe_wake_latency() {
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, latency: Option<u64>| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if let Some(latency) = latency {
                value["pipe_wake_latency"] = serde_json::Value::from(latency);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: &Path, run_args: &[&str]| {
            let run1 = format!("--run1-preemptions={}", run1.display());
            let mut argv = vec!["hermit", "analyze", run1.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let off = record("off", None);
        let zero = record("zero", Some(0));
        let options = prepare(&zero, &["--pipe-wake-latency=0"]).unwrap();
        assert_eq!(options.trial_pipe_wake_latency(), Some(0));
        // Every record analyze writes carries the trials' latency: the final
        // schedules, each trial's schedule, and minimize's records.
        let written = |options: &AnalyzeOpts| {
            let (failing, passing, trial) = (
                dir.path().join("final-failing.json"),
                dir.path().join("final-passing.json"),
                dir.path().join("trial.json"),
            );
            options.write_final_schedules(Vec::new(), &failing, Vec::new(), &passing);
            options.write_trial_schedule(Vec::new(), &trial).unwrap();
            [failing, passing, trial]
                .map(|path| detcore::preemptions::read_recorded_pipe_wake_latency(&path).unwrap())
        };
        assert_eq!(written(&options), [Some(0); 3]);
        assert_eq!(
            options
                .stamp_trial_record(PreemptionRecord::from_vecs(&Default::default()))
                .pipe_wake_latency(),
            Some(0)
        );
        let options_off = prepare(&off, &[]).unwrap();
        assert_eq!(written(&options_off), [None; 3]);
        let replay_flag = format!("--replay-preemptions-from={}", zero.display());
        for (run1, run_args, named) in [
            (&zero, vec![], "--run1-preemptions"),
            (&off, vec!["--pipe-wake-latency=0"], "--run1-preemptions"),
            (
                &off,
                vec![replay_flag.as_str()],
                "--replay-preemptions-from",
            ),
        ] {
            let error = prepare(run1, &run_args).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            let message = format!("{error:#}");
            assert!(
                message.contains(named) && message.contains("cannot reproduce the run"),
                "{run_args:?}: {message}"
            );
        }
    }

    /// Every record analyze writes states the trials' signal-state copy
    /// setting, and a recording made under the other setting is refused
    /// before any trial; one that states no setting is accepted.
    #[test]
    fn analyze_stamps_and_checks_the_signal_state_copy_setting() {
        let dir = tempfile::tempdir().unwrap();
        let record = |name: &str, copy: Option<bool>| {
            let path = record_with_epoch(dir.path(), name, None);
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            if let Some(copy) = copy {
                value["signal_state_copy"] = serde_json::Value::from(copy);
            }
            std::fs::write(&path, value.to_string()).unwrap();
            path
        };
        let prepare = |run1: &Path, run_args: &[&str]| {
            let run1 = format!("--run1-preemptions={}", run1.display());
            let mut argv = vec!["hermit", "analyze", run1.as_str(), "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.prepare(&args.global).map(|_| options)
        };
        let unsaid = record("unsaid", None);
        let on = record("on", Some(true));
        let off = record("off", Some(false));
        let written = |options: &AnalyzeOpts| {
            let trial = dir.path().join("copy-trial.json");
            options.write_trial_schedule(Vec::new(), &trial).unwrap();
            detcore::preemptions::read_recorded_signal_state_copy(&trial).unwrap()
        };
        let default = prepare(&on, &[]).unwrap();
        assert!(default.trial_signal_state_copy());
        assert_eq!(written(&default), Some(true));
        let copy_off = prepare(&off, &["--no-signal-state-copy"]).unwrap();
        assert_eq!(written(&copy_off), Some(false));
        assert!(prepare(&unsaid, &[]).is_ok());
        assert!(prepare(&unsaid, &["--no-signal-state-copy"]).is_ok());
        for (run1, run_args) in [(&off, vec![]), (&on, vec!["--no-signal-state-copy"])] {
            let error = prepare(run1, &run_args).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            assert!(
                format!("{error:#}").contains("signal-state copy"),
                "{error:#}"
            );
        }
    }

    /// Runs `test` (this module's test of that name) in two child processes,
    /// once with `HERMIT_EPOCH` unset and once set to [`OTHER`], and returns
    /// `None`. Inside such a child it returns the arm, `"unset"` or `"set"`.
    /// `HERMIT_EPOCH` counts as an explicit epoch, so the validation's
    /// forwarded value cannot decide the checks.
    fn epoch_environment_arm(test: &str) -> Option<String> {
        const CHILD: &str = "HERMIT_ANALYZE_EPOCH_TEST_CHILD";
        if let Some(arm) = std::env::var_os(CHILD) {
            return Some(arm.into_string().unwrap());
        }
        let name = format!("{}::{test}", module_path!().split_once("::").unwrap().1);
        for arm in ["unset", "set"] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child
                .args(["--exact", &name, "--nocapture"])
                .env(CHILD, arm);
            if arm == "set" {
                child.env("HERMIT_EPOCH", OTHER);
            } else {
                child.env_remove("HERMIT_EPOCH");
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{arm}: {output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{arm}: the child ran no test: {output:?}"
            );
        }
        None
    }

    fn refused(error: anyhow::Error, expected: &str) {
        assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
        assert!(error.to_string().contains(expected), "{error:#}");
    }

    /// The trial that replays `--run1-preemptions` starts from the epoch the
    /// record was made under, as `hermit run` replaying it does
    /// (https://github.com/rrnewton/hermit/issues/3870). Before, it started
    /// from the run arguments' epoch, the default when none was given. The
    /// checks run in a child process whose environment fixes `HERMIT_EPOCH`,
    /// which counts as an explicit epoch: once unset, and once set to an epoch
    /// other than the recorded one.
    #[test]
    fn analyze_replays_a_record_from_the_epoch_it_was_recorded_under() {
        let Some(arm) =
            epoch_environment_arm("analyze_replays_a_record_from_the_epoch_it_was_recorded_under")
        else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        if arm == "set" {
            refused(
                prepared_analysis(dir, Some(RECORDED), None, &[]).unwrap_err(),
                &format!(
                    "the explicit virtual-time epoch 2026-01-01T00:00:00+00:00 (from --epoch or \
                     HERMIT_EPOCH) differs from the epoch {RECORDED_RFC3339} that \
                     --run1-preemptions"
                ),
            );
            return;
        }

        // A record without an epoch keeps the default.
        let analysis = prepared_analysis(dir, None, None, &[]).unwrap();
        // No `--epoch` is added; `--expired-wait-charge` always is.
        assert_eq!(analysis.run_arg, ["--expired-wait-charge=each"]);
        assert_eq!(analysis.trial_epoch_for_test(), "2026-01-01T00:00:00+00:00");

        // The recorded epoch reaches the trials, from either record.
        let analysis = prepared_analysis(dir, Some(RECORDED), None, &[]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);
        let analysis = prepared_analysis(dir, None, Some(RECORDED), &[]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // An explicit epoch equal to the recorded one adds no second --epoch.
        let analysis =
            prepared_analysis(dir, Some(RECORDED), None, &["--epoch", RECORDED]).unwrap();
        // No `--epoch` is added; `--expired-wait-charge` always is.
        assert_eq!(analysis.run_arg, ["--expired-wait-charge=each"]);
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // A different explicit epoch is refused, naming the remedy.
        refused(
            prepared_analysis(dir, Some(RECORDED), None, &["--epoch", OTHER]).unwrap_err(),
            &format!("pass --epoch={RECORDED_RFC3339}"),
        );

        // Two records under different epochs are refused, naming both.
        refused(
            prepared_analysis(dir, Some(RECORDED), Some(OTHER), &[]).unwrap_err(),
            &format!(
                "was recorded under the virtual-time epoch {RECORDED_RFC3339} and \
                 --run2-preemptions"
            ),
        );
    }

    /// A network trace the run arguments replay (`--replay-networking`)
    /// releases its inputs at absolute virtual times measured from its own
    /// epoch, and every trial replays it. The trials start from that epoch,
    /// and a trace that disagrees with a preemption record or an explicit
    /// epoch is refused before any trial, as `hermit run` refuses it
    /// (https://github.com/rrnewton/hermit/issues/3875). Before, the trials
    /// replayed it from the run arguments' epoch.
    #[test]
    fn analyze_replays_a_network_trace_from_the_epoch_it_was_recorded_under() {
        let Some(arm) = epoch_environment_arm(
            "analyze_replays_a_network_trace_from_the_epoch_it_was_recorded_under",
        ) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let trace = |name: &str, epoch: &str| {
            let path = dir.join(format!("{name}.trace"));
            detcore_model::network_engine::NetworkEngine::new_record(epoch.parse().unwrap())
                .finish()
                .unwrap()
                .write_framed(std::fs::File::create(&path).unwrap())
                .unwrap();
            format!("--replay-networking={}", path.display())
        };
        let recorded = trace("recorded", RECORDED);
        if arm == "set" {
            refused(
                prepared_analysis(dir, None, None, &[&recorded]).unwrap_err(),
                &format!(
                    "the explicit virtual-time epoch 2026-01-01T00:00:00+00:00 (from --epoch or \
                     HERMIT_EPOCH) differs from the epoch {RECORDED_RFC3339} that \
                     --replay-networking"
                ),
            );
            return;
        }

        // The trace alone sets the trials' epoch.
        let analysis = prepared_analysis(dir, None, None, &[&recorded]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // With a preemption record from the same epoch it is accepted.
        let analysis = prepared_analysis(dir, Some(RECORDED), None, &[&recorded]).unwrap();
        assert_eq!(analysis.trial_epoch_for_test(), RECORDED_RFC3339);

        // A trace from another epoch than the record is refused, naming both.
        let other = trace("other", OTHER);
        refused(
            prepared_analysis(dir, Some(RECORDED), None, &[&other]).unwrap_err(),
            &format!(
                "was recorded under the virtual-time epoch {RECORDED_RFC3339} and \
                 --replay-networking"
            ),
        );

        // An explicit epoch other than the trace's is refused.
        refused(
            prepared_analysis(dir, None, None, &[&recorded, "--epoch", OTHER]).unwrap_err(),
            &format!("pass --epoch={RECORDED_RFC3339}"),
        );
    }

    /// Through `main`: `--run1-schedule` and `--run2-schedule` parse but have
    /// no implementation, so `hermit analyze` refuses them before any
    /// workspace or trial, naming the options that work. Before, `main`
    /// reached `unimplemented!()` and the process panicked (exit 101).
    #[test]
    fn analyze_refuses_the_unimplemented_schedule_inputs_before_any_trial() {
        for (options, named) in [
            (
                &["--run1-schedule=/nonexistent/run1.json"][..],
                "--run1-schedule",
            ),
            (
                &["--run2-schedule=/nonexistent/run2.json"][..],
                "--run2-schedule",
            ),
            (
                &[
                    "--run1-schedule=/nonexistent/run1.json",
                    "--run2-schedule=/nonexistent/run2.json",
                ][..],
                "--run1-schedule or --run2-schedule",
            ),
        ] {
            let mut argv = vec!["hermit", "analyze"];
            argv.extend(options);
            argv.extend(["--", "/bin/true"]);
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Analyze(mut analyze) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            let error = analyze.main(&args.global).unwrap_err();
            refused(
                error,
                &format!(
                    "hermit analyze does not implement {named}. Identify a run by a preemption \
                     record (--run1-preemptions or --run2-preemptions"
                ),
            );
            assert!(
                analyze.tmp_dir.is_none(),
                "{options:?}: a workspace was created before the refusal"
            );
        }
    }

    /// A prefix search's trials as `RunData::launch` runs them: a trial the
    /// journal holds takes its recorded outcome; any other first asks the
    /// budget, then runs (here: `outcome`) and is journaled. Each trial's end
    /// and run go into `ends` and `runs`, as `phase5_prefix_search` keeps
    /// them for the bracket.
    struct JournaledPrefixTrials {
        journal: BTreeMap<usize, TrialEnd>,
        ends: Ends,
        runs: BTreeMap<(usize, MaybeTimeslice), String>,
    }

    impl JournaledPrefixTrials {
        fn new() -> Self {
            Self {
                journal: BTreeMap::new(),
                ends: Ends::new(),
                runs: BTreeMap::new(),
            }
        }

        fn trial(
            &mut self,
            budget: &Budget,
            prefix: usize,
            outcome: impl Fn(usize) -> TrialEnd,
        ) -> anyhow::Result<TrialEnd> {
            let end = match self.journal.get(&prefix) {
                Some(&end) => end,
                None => {
                    budget.start_trial()?;
                    let end = outcome(prefix);
                    self.journal.insert(prefix, end);
                    end
                }
            };
            self.ends.insert((prefix, None), (end, None));
            self.runs
                .insert((prefix, None), format!("prefix_round_{prefix}"));
            Ok(end)
        }
    }

    /// A budget that ends a prefix search leaves a bracket whose lower prefix
    /// misses the target and whose upper prefix meets it, with the deciding
    /// event between them. Resumed with the journal of the trials it ran, the
    /// search runs only the trials it had not, and finds the event the
    /// uninterrupted search finds.
    #[test]
    fn a_budget_ends_the_prefix_search_at_a_bracket_and_a_resume_finds_the_same_event() {
        use crate::analyze::budget::TimeBudget;
        use crate::analyze::budget::TrialBudget;
        let (len, decision) = (37_710, 28_055);
        let outcome = |prefix: usize| {
            if prefix > decision {
                TrialEnd::Met
            } else {
                TrialEnd::Missed
            }
        };
        let mut uninterrupted_trials = 0;
        let uninterrupted = search_prefix_boundary(len, TrialEnd::Missed, |prefix| {
            uninterrupted_trials += 1;
            Ok(outcome(prefix))
        })
        .unwrap();

        let allowed = 7;
        let budget = Budget::new(
            TimeBudget::Off,
            TrialBudget::Limit(NonZeroU64::new(allowed).unwrap()),
        );
        let mut trials = JournaledPrefixTrials::new();
        let stopped = search_prefix_boundary(len, TrialEnd::Missed, |prefix| {
            trials.trial(&budget, prefix, outcome)
        })
        .unwrap_err();
        let expired = stopped.downcast_ref::<BudgetExpired>().unwrap();
        assert_eq!(expired.kind, BudgetKind::TrialBudget);
        let bracket = prefix_bracket(&trials.ends, &trials.runs, None, len);
        let (misses, meets) = (bracket.misses.unwrap(), bracket.meets.unwrap());
        assert_eq!(outcome(misses), TrialEnd::Missed);
        assert_eq!(outcome(meets), TrialEnd::Met);
        assert!(misses <= decision && decision < meets, "{bracket:?}");
        assert_eq!(bracket.meets_run, Some(format!("prefix_round_{meets}")));
        assert!(bracket.unfinished.is_empty());

        let resumed_budget = Budget::default();
        let resumed = search_prefix_boundary(len, TrialEnd::Missed, |prefix| {
            trials.trial(&resumed_budget, prefix, outcome)
        })
        .unwrap();
        assert_eq!(resumed, uninterrupted);
        assert_eq!(
            allowed + resumed_budget.trials_started(),
            uninterrupted_trials as u64
        );
    }

    /// A time budget ends the search the same way: no trial starts once the
    /// time is spent, whatever the trial budget.
    #[test]
    fn a_time_budget_ends_the_prefix_search_once_the_time_is_spent() {
        use std::sync::atomic::AtomicU64;
        use std::sync::atomic::Ordering;

        use crate::analyze::budget::TimeBudget;
        use crate::analyze::budget::TrialBudget;
        let seconds = std::sync::Arc::new(AtomicU64::new(0));
        let clock = std::sync::Arc::clone(&seconds);
        let budget = Budget::with_clock(
            TimeBudget::Limit(std::time::Duration::from_secs(600)),
            TrialBudget::Off,
            std::sync::Arc::new(move || {
                std::time::Duration::from_secs(clock.load(Ordering::SeqCst))
            }),
        );
        let mut trials = JournaledPrefixTrials::new();
        let stopped = search_prefix_boundary(1000, TrialEnd::Missed, |prefix| {
            // Every trial takes 250 s of the 600.
            let end = trials.trial(&budget, prefix, |prefix| {
                if prefix > 400 {
                    TrialEnd::Met
                } else {
                    TrialEnd::Missed
                }
            });
            seconds.fetch_add(250, Ordering::SeqCst);
            end
        })
        .unwrap_err();
        assert_eq!(
            stopped.downcast_ref::<BudgetExpired>().unwrap().kind,
            BudgetKind::TimeBudget
        );
        assert_eq!(budget.trials_started(), 3);
        let bracket = prefix_bracket(&trials.ends, &trials.runs, None, 1000);
        assert_eq!((bracket.misses, bracket.meets), (Some(375), Some(500)));
    }

    /// Only trials that finished bound the bracket: a stopped one in between
    /// is named as no outcome.
    #[test]
    fn a_bracket_rests_on_finished_trials() {
        let mut ends = Ends::new();
        let mut runs = BTreeMap::new();
        for (prefix, end) in [
            (0, TrialEnd::Missed),
            (10, TrialEnd::Missed),
            (20, TrialEnd::Unfinished(Stop::Bound)),
            (30, TrialEnd::Met),
            (40, TrialEnd::Met),
        ] {
            ends.insert((prefix, None), (end, None));
            runs.insert((prefix, None), format!("prefix_round_{prefix:03}"));
        }
        let bracket = prefix_bracket(&ends, &runs, None, 50);
        assert_eq!(bracket.misses, Some(10));
        assert_eq!(bracket.meets, Some(30));
        assert_eq!(bracket.unfinished, [20]);
        let said = describe_bracket(&bracket);
        assert!(
            said.contains(
                "replaying the first 10 of the target schedule's 50 events misses the target \
                 (prefix_round_010), and replaying the first 30 meets it (prefix_round_030). The \
                 deciding event is one of events #10 to #29 (20 events)"
            ),
            "{said}"
        );
        assert!(
            said.contains("prefixes of 20 events in between did not finish"),
            "{said}"
        );
        // Before any trial met the target there is no upper end.
        ends.retain(|_, (end, _)| *end != TrialEnd::Met);
        let open = prefix_bracket(&ends, &runs, None, 50);
        assert_eq!((open.misses, open.meets), (Some(10), None));
        assert!(
            describe_bracket(&open).contains("had not yet found a prefix"),
            "{}",
            describe_bracket(&open)
        );
    }

    /// The options of an analysis of `/bin/true` in a fresh workspace under
    /// `parent`, with `extra` options.
    fn analysis_in(parent: &Path, extra: &[&str]) -> (AnalyzeOpts, Workspace) {
        let tmp_dir = format!("--tmp-dir={}", parent.display());
        let mut argv = vec!["analyze", tmp_dir.as_str()];
        argv.extend(extra);
        argv.extend(["--", "--chaos", "/bin/true"]);
        let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
        let workspace = options.phase0_initialize(Retention::Remove).unwrap();
        (options, workspace)
    }

    /// A stack-trace replay never changes what the report decides. One that
    /// analyze's own bound stops runs again once with 4 times the bound, and
    /// a rerun that finishes gives the report an uninterrupted run gives.
    /// Stopped again, or stopped by the run arguments' own `--timeout` (which
    /// cannot be given longer, so there is no rerun), it leaves its part of
    /// the report missing and says why, and names the same event without
    /// claiming either kind.
    #[test]
    fn a_stopped_stack_trace_replay_leaves_its_stacks_missing_not_the_decision() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let tid = DetTid::from_raw;
        let decision = 4;
        let target_events: Vec<SchedEvent> = [3, 5, 3, 5, 3, 5, 5, 5]
            .iter()
            .map(|&thread| SchedEvent::branches(tid(thread), 1))
            .collect();
        // What the scheduler runs after a prefix of `len` events: thread 3
        // runs on for one event without the deciding event, three with it.
        let realized = move |replayed: &[SchedEvent]| -> Vec<SchedEvent> {
            let tail: &[i32] = if replayed.len() > decision {
                &[3, 3, 5, 5]
            } else {
                &[3, 5, 5, 5]
            };
            let mut realized = replayed.to_vec();
            if replayed.len() == decision || replayed.len() == decision + 1 {
                realized.extend(
                    tail.iter()
                        .map(|&thread| SchedEvent::branches(tid(thread), 1)),
                );
            }
            realized
        };
        let runner = move |stops: fn(&str) -> bool, ran: Arc<Mutex<Vec<String>>>| {
            crate::analyze::rundata::TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                ran.lock().unwrap().push(run.name().to_owned());
                if stops(run.name()) {
                    return Err(anyhow::Error::new(crate::container::RunTimeoutMarker));
                }
                let input = run.sched_path_in().to_path_buf();
                let replayed = PreemptionReader::new(&input).load_all().into_global();
                let realized = realized(&replayed);
                for (position, path) in run.runopts.det_opts.det_config.stacktrace_event.clone() {
                    // A frame per position, so that no two stacks match.
                    let thread = realized[position as usize].dettid.as_raw();
                    std::fs::write(
                        path.unwrap(),
                        serde_json::json!({
                            "thread_id": thread, "thread_name": null, "frames": [{
                                "frame": {"ip": position, "is_signal": false},
                                "symbol": null,
                                "locations": []
                            }]
                        })
                        .to_string(),
                    )
                    .unwrap();
                }
                let output = run.sched_path_out().to_path_buf();
                PreemptionRecord::from_sched_events(realized)
                    .write_to_disk(&output)
                    .unwrap();
                run.finish_for_test(replayed.len() > decision, std::time::Duration::from_secs(1));
                Ok(())
            })))
        };
        let parent = tempfile::tempdir().unwrap();
        let report_of = |label: &str, run_timeout: Option<&str>, stops: fn(&str) -> bool| {
            let tmp_dir = format!("--tmp-dir={}", parent.path().join(label).display());
            let mut argv = vec![
                "analyze",
                tmp_dir.as_str(),
                "--target-stdout=FAILED",
                "--prefix-search",
                "--trial-timeout=10",
                "--",
                "--chaos",
            ];
            argv.extend(run_timeout);
            argv.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let workspace = options.phase0_initialize(Retention::Remove).unwrap();
            let ran = Arc::new(Mutex::new(Vec::new()));
            options.trial_runner = runner(stops, ran.clone());
            let report = options
                .phase6_record_prefix_outputs(PrefixDecision {
                    schedule: PreemptionRecord::from_sched_events(target_events.clone()),
                    decision,
                    continuation: None,
                    refused: None,
                    unfinished: None,
                    fallback: None,
                })
                .unwrap();
            let ran = ran.lock().unwrap().clone();
            drop(workspace);
            (report, ran)
        };
        let timing = |report: &Report| {
            report.header.contains(
                "it is a timing decision: with or without it, the scheduler runs thread 3",
            )
        };
        let thread_of = |event: &ReportCriticalEvent| event.stack.as_ref().and_then(stack_thread);

        let (whole, _) = report_of("whole", None, |_| false);
        assert!(timing(&whole), "{}", whole.header);
        assert_eq!(
            whole.deciding_event.as_ref().map(|e| e.event_index),
            Some(decision)
        );
        assert_eq!(thread_of(&whole.critical_event1), Some(tid(5)));
        assert_eq!(thread_of(&whole.critical_event2), Some(tid(5)));

        // Stopped once by analyze's bound: the rerun gives the same report.
        let (rerun, ran) = report_of("rerun", None, |name| name == "final_target_schedule");
        assert!(
            ran.contains(&"final_target_schedule_retry".to_owned()),
            "{ran:?}"
        );
        assert!(timing(&rerun), "{}", rerun.header);
        assert_eq!(
            [
                rerun.critical_event1.event_index,
                rerun.critical_event2.event_index
            ],
            [
                whole.critical_event1.event_index,
                whole.critical_event2.event_index
            ]
        );
        assert_eq!(thread_of(&rerun.critical_event1), Some(tid(5)));

        // Stopped twice: the same event, its kind not claimed, the
        // continuation with the event missing.
        let (missing, _) = report_of("missing", None, |name| {
            name.starts_with("final_target_schedule")
        });
        assert!(!timing(&missing), "{}", missing.header);
        assert!(
            missing.header.starts_with(&format!(
                "The scheduling decision at event #{decision} of the target schedule decides \
                 the outcome.\n"
            )) && missing.header.contains(
                "is missing: the stack-trace replay final_target_schedule_retry stopped at \
                 hermit analyze's wall-clock bound (40 s)"
            ),
            "{}",
            missing.header
        );
        assert_eq!(
            missing.deciding_event.as_ref().map(|e| e.event_index),
            Some(decision)
        );
        assert!(missing.critical_event1.stack.is_none());
        assert_eq!(
            missing.critical_event2.event_index,
            whole.critical_event2.event_index
        );
        assert_eq!(thread_of(&missing.critical_event2), Some(tid(5)));

        // Stopped by the run arguments' own --timeout: no rerun.
        let (own, ran) = report_of("own", Some("--timeout=5"), |name| {
            name.starts_with("final_target_schedule")
        });
        assert!(!ran.iter().any(|name| name.ends_with("_retry")), "{ran:?}");
        assert!(!timing(&own), "{}", own.header);
        assert!(
            own.header.contains(
                "is missing: the stack-trace replay final_target_schedule stopped at --timeout=5"
            ),
            "{}",
            own.header
        );

        // The continuation without the event stopped: the same event, the
        // kind of answer not claimed, and why stack2 is missing.
        let (without, _) = report_of("without", None, |name| {
            name.starts_with("final_baseline_schedule")
        });
        assert!(!timing(&without), "{}", without.header);
        assert!(
            without.header.contains(&format!(
                "The scheduling decision at event #{decision} of the target schedule decides the \
                 outcome."
            )) && without.header.contains(&format!(
                "Note: which thread runs at event #{decision} without the event, and so stack2, \
                 is missing: the stack-trace replay final_baseline_schedule_retry stopped"
            )) && !without.header.contains("ended after"),
            "{}",
            without.header
        );
        assert_eq!(without.critical_event1.event_index, decision);
        assert_eq!(thread_of(&without.critical_event1), Some(tid(3)));
        assert!(without.critical_event2.stack.is_none());
    }

    /// The warning that the stack-trace replays did not repeat the search's
    /// outcomes reads only replays that finished: one with the event that a
    /// bound stopped contradicts nothing, one that finished and missed does,
    /// and so does one without the event that met the target.
    #[test]
    fn only_finished_stack_trace_replays_contradict_the_search() {
        let replay = |finished: bool, met: bool| PrefixStack {
            finished,
            met,
            stack: None,
            stopped: (!finished).then(|| "stopped".to_owned()),
        };
        let stopped = replay(false, false);
        let met = replay(true, true);
        let missed = replay(true, false);
        assert!(!stack_runs_contradict(&[&stopped], &[&missed]));
        assert!(!stack_runs_contradict(&[&met, &stopped], &[&stopped]));
        assert!(stack_runs_contradict(&[&missed], &[]));
        assert!(stack_runs_contradict(&[&met], &[&met]));
        assert!(!stack_runs_contradict(&[&met], &[&missed, &stopped]));
    }

    /// A partial report says that it is partial, why, which phase was
    /// running, its bracket, and the command that resumes the analysis; it
    /// exits 120 and keeps the workspace.
    #[test]
    fn a_partial_report_says_why_where_and_how_to_resume() {
        let parent = tempfile::tempdir().unwrap();
        let report_file = parent.path().join("report.json");
        let report = format!("--report-file={}", report_file.display());
        let (mut options, _workspace) =
            analysis_in(parent.path(), &[report.as_str(), "--trial-budget=1"]);
        options.budget = Budget::new(options.time_budget, options.trial_budget);
        options.budget.start_trial().unwrap();
        let expired = options.budget.start_trial().unwrap_err();
        options.progress.phase(Phase::PrefixSearch);
        let mut ends = Ends::new();
        ends.insert((100, None), (TrialEnd::Missed, None));
        ends.insert((200, None), (TrialEnd::Met, None));
        let runs = BTreeMap::from([
            ((100, None), "prefix_round_004".to_owned()),
            ((200, None), "prefix_round_003".to_owned()),
        ]);
        let bracket = prefix_bracket(&ends, &runs, None, 300);
        options.progress.bracket(bracket.clone());

        let status = options.report_partial(expired).unwrap();
        assert_eq!(
            status,
            ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
        );
        assert!(options.partial_report_written);
        let written: Report =
            serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
        let partial = written.partial.unwrap();
        assert_eq!(partial.reason, PartialReason::TrialBudget);
        assert_eq!(partial.phase, Phase::PrefixSearch);
        assert_eq!(partial.bracket, Some(bracket));
        assert_eq!(partial.runs_started, 1);
        assert_eq!(partial.trial_budget, Some(1));
        assert_eq!(&partial.workspace, options.tmp_dir.as_ref().unwrap());
        assert!(
            partial
                .resume_command
                .contains(&format!("--resume={}", partial.workspace.display())),
            "{}",
            partial.resume_command
        );
        for said in [
            "PARTIAL RESULT. hermit analyze reached its --trial-budget (1 runs)",
            "while searching the target schedule's prefixes (--prefix-search)",
            "The deciding event is one of events #100 to #199 (100 events)",
            "The workspace is kept",
        ] {
            assert!(
                written.header.contains(said),
                "{said:?} not in {}",
                written.header
            );
        }
    }

    /// A resumed analysis restarts its search from the interval the earlier
    /// invocation recorded, with its trial count and warnings, when that
    /// interval is of the same endpoints and its two files hold what it
    /// names; otherwise the search starts from its endpoints.
    #[test]
    fn a_resume_restarts_the_search_only_from_a_whole_interval_of_its_endpoints() {
        let parent = tempfile::tempdir().unwrap();
        let (mut options, _workspace) = analysis_in(parent.path(), &[]);
        options.progress.phase(Phase::Bisect);
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
                .collect()
        };
        let (baseline, target) = (events(&[3, 3, 5, 5]), events(&[5, 5, 3, 3]));
        let endpoints = events_digest(&[&baseline, &target]);
        let (passing, failing) = (events(&[3, 5, 3, 5]), events(&[3, 5, 5, 3]));
        let mut interval = IntervalFiles::new(options.tmp_dir.as_ref().unwrap());
        interval.record(
            &options,
            Some(3),
            [&passing, &failing],
            Some((2, 1)),
            Some(SearchState {
                endpoints: endpoints.clone(),
                event_level: true,
                trials: 7,
                unfinished: 1,
                refused: 0,
            }),
        );
        options.resumed_progress = Some(options.progress.snapshot());

        let (resume, pass, resumed_passing, resumed_failing) =
            options.resumable_interval(&endpoints).unwrap();
        assert_eq!(pass, 3);
        assert_eq!(
            (resume.trials, resume.unfinished, resume.refused),
            (7, 1, 0)
        );
        assert!(resume.event_level);
        assert_eq!(resumed_passing, passing);
        assert_eq!(resumed_failing, failing);

        // A search between other endpoints starts over.
        assert!(options.resumable_interval("another digest").is_none());
        // So does one whose file no longer holds what the interval names.
        let recorded = options.progress.snapshot().interval.unwrap();
        options
            .write_trial_schedule(events(&[5, 3, 3, 5]), &recorded.passing)
            .unwrap();
        assert!(options.resumable_interval(&endpoints).is_none());
        // And one recorded before the search's first pass, which has no state.
        interval.record(&options, None, [&baseline, &target], None, None);
        options.resumed_progress = Some(options.progress.snapshot());
        assert!(options.resumable_interval(&endpoints).is_none());
    }

    /// A budget hit after the search decided, during the stack-trace runs,
    /// still reports the decision in the report's structured fields: the
    /// deciding event for `--prefix-search`, and the critical pair for the
    /// default search, with no stacks, and exits 120. Before, the decision
    /// was only text, and the event indices were 0 on the prefix path.
    #[test]
    fn a_budget_hit_after_the_decision_reports_it() {
        for (decided, events, critical, deciding) in [
            (
                "event #41 of the target schedule decides the outcome (tid 3)",
                DecidedEvents::Deciding(41),
                [41, 41],
                Some(41),
            ),
            (
                "events #6 and #7 of the failing schedule are the critical pair",
                DecidedEvents::CriticalPair(6, 7),
                [6, 7],
                None,
            ),
        ] {
            let parent = tempfile::tempdir().unwrap();
            let report_file = parent.path().join("report.json");
            let report = format!("--report-file={}", report_file.display());
            let (mut options, _workspace) =
                analysis_in(parent.path(), &[report.as_str(), "--time-budget=30m"]);
            options.budget = Budget::new(options.time_budget, options.trial_budget);
            options.progress.phase(Phase::StackTraces);
            options.progress.decided(decided.to_owned(), events);
            let expired = BudgetExpired {
                kind: BudgetKind::TimeBudget,
                cut: Some("final_target_for_stacktraces".to_owned()),
            };

            let status = options.report_partial(expired).unwrap();
            assert_eq!(
                status,
                ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
            );
            let written: Report =
                serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
            assert_eq!(
                [
                    written.critical_event1.event_index,
                    written.critical_event2.event_index
                ],
                critical,
                "{events:?}"
            );
            assert_eq!(
                written
                    .deciding_event
                    .as_ref()
                    .map(|event| event.event_index),
                deciding,
                "{events:?}"
            );
            assert!(written.critical_event1.stack.is_none());
            assert!(written.critical_event2.stack.is_none());
            assert!(
                written
                    .deciding_event
                    .as_ref()
                    .is_none_or(|event| event.stack.is_none())
            );
            let partial = written.partial.unwrap();
            assert_eq!(partial.phase, Phase::StackTraces);
            assert_eq!(partial.decided.as_deref(), Some(decided));
            assert_eq!(
                partial.stacks,
                "not taken: the budget ran out during the stack-trace runs; --resume takes them"
            );
            assert!(
                written
                    .header
                    .contains(&format!("The search had finished: {decided}.")),
                "{}",
                written.header
            );
        }
    }

    /// The same, through the real cut path: a prefix search with a trial
    /// runner in place of the guest and `--trial-budget` set to the trials the
    /// search takes, so the budget runs out at the first stack-trace run
    /// (`final_target_for_stacktraces`), and `end_on_budget` reports it as
    /// `analyze` does. The report carries the decision as above. Resumed
    /// through the journal, the analysis answers the search's trials from it
    /// and finishes with the same deciding event.
    #[test]
    fn a_trial_budget_that_ends_at_the_stack_trace_runs_reports_the_decision() {
        use std::sync::Arc;
        use std::sync::Mutex;

        use crate::analyze::rundata::TrialRunner;
        let (len, deciding) = (200, 137);
        let target = PreemptionRecord::from_sched_events(
            (0..len)
                .map(|i| SchedEvent::branches(DetTid::from_raw(3 + (i % 2)), 1))
                .collect(),
        );
        // A trial meets the target when its prefix holds the deciding event.
        let runner = |names: Arc<Mutex<Vec<String>>>| {
            TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                names.lock().unwrap().push(run.name().to_owned());
                let replayed = run
                    .runopts
                    .det_opts
                    .det_config
                    .replay_schedule_from
                    .clone()
                    .unwrap();
                let prefix = PreemptionReader::new(&replayed)
                    .load_all()
                    .schedevents()
                    .len();
                run.finish_for_test(prefix > deciding, std::time::Duration::from_secs(1));
                Ok(())
            })))
        };
        let search = ["--target-stdout=FAILED", "--prefix-search"];

        // How many trials the search takes, uninterrupted.
        let parent = tempfile::tempdir().unwrap();
        let counted = Arc::new(Mutex::new(Vec::new()));
        let (mut uninterrupted, _uninterrupted_workspace) =
            analysis_in(&parent.path().join("uninterrupted"), &search);
        uninterrupted.start_checkpoint().unwrap();
        uninterrupted.trial_runner = runner(counted.clone());
        assert_eq!(
            uninterrupted
                .phase5_prefix_search(&target)
                .unwrap()
                .decision,
            deciding
        );
        let search_trials = counted.lock().unwrap().len();

        let report_file = parent.path().join("report.json");
        let report = format!("--report-file={}", report_file.display());
        let budget = format!("--trial-budget={search_trials}");
        let mut args = search.to_vec();
        args.extend([report.as_str(), budget.as_str()]);
        let (mut options, mut workspace) = analysis_in(&parent.path().join("first"), &args);
        workspace.keep("the resumed analysis owns it");
        options.start_checkpoint().unwrap();
        options.budget = Budget::new(options.time_budget, options.trial_budget);
        let ran = Arc::new(Mutex::new(Vec::new()));
        options.trial_runner = runner(ran.clone());
        let result = options
            .phase5_prefix_search(&target)
            .and_then(|decision| options.phase6_record_prefix_outputs(decision))
            .map(|_| ExitStatus::SUCCESS);
        let status = options.end_on_budget(result).unwrap();
        assert_eq!(
            status,
            ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
        );
        // The search ran to its end; the stack-trace run never started.
        assert_eq!(ran.lock().unwrap().len(), search_trials);
        assert!(
            !ran.lock()
                .unwrap()
                .iter()
                .any(|name| name == "final_target_for_stacktraces")
        );
        let written: Report =
            serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
        assert_eq!(
            written
                .deciding_event
                .as_ref()
                .map(|event| event.event_index),
            Some(deciding)
        );
        assert_eq!(
            [
                written.critical_event1.event_index,
                written.critical_event2.event_index
            ],
            [deciding, deciding]
        );
        assert!(written.critical_event1.stack.is_none());
        assert!(written.critical_event2.stack.is_none());
        assert!(
            written
                .deciding_event
                .as_ref()
                .is_some_and(|event| event.stack.is_none())
        );
        let partial = written.partial.unwrap();
        assert_eq!(partial.reason, PartialReason::TrialBudget);
        assert_eq!(partial.phase, Phase::StackTraces);
        assert!(
            partial.decided.as_deref().is_some_and(|decided| decided
                .starts_with(&format!("event #{deciding} of the target schedule decides"))),
            "{:?}",
            partial.decided
        );
        assert_eq!(
            partial.stacks,
            "not taken: the budget ran out during the stack-trace runs; --resume takes them"
        );

        // Resumed, the search's trials come from the journal and only the
        // stack-trace runs run.
        let path = workspace.path().to_path_buf();
        let resume = format!("--resume={}", path.display());
        let mut argv = vec!["analyze", resume.as_str()];
        argv.extend(search);
        argv.extend(["--", "--chaos", "/bin/true"]);
        let mut resumed = AnalyzeOpts::try_parse_from(argv).unwrap();
        let resumed_workspace = resumed.phase0_resume(&path).unwrap();
        let resumed_ran = Arc::new(Mutex::new(Vec::new()));
        resumed.trial_runner = runner(resumed_ran.clone());
        let decision = resumed.phase5_prefix_search(&target).unwrap();
        assert_eq!(resumed.journal.answered() as usize, search_trials);
        assert!(resumed_ran.lock().unwrap().is_empty());
        let complete = resumed.phase6_record_prefix_outputs(decision).unwrap();
        assert!(complete.partial.is_none());
        assert_eq!(complete.critical_event1.event_index, deciding);
        assert_eq!(
            resumed_ran.lock().unwrap().first().map(String::as_str),
            Some("final_target_for_stacktraces")
        );
        drop(resumed_workspace);
        drop(workspace);
    }

    /// The runs a test's trial runner started: each one's name and
    /// `--timeout`.
    type RanTrials = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<u64>)>>>;

    /// A prefix search's trial runner: a trial meets the target when its
    /// prefix holds event `deciding`, and `stopped(run, prefix)` says when
    /// analyze's wall-clock bound stops it instead. Records each run's name
    /// and `--timeout`.
    fn backstop_runner(
        deciding: usize,
        stopped: impl Fn(&RunData, usize) -> bool + Send + Sync + 'static,
        ran: RanTrials,
    ) -> crate::analyze::rundata::TrialRunner {
        crate::analyze::rundata::TrialRunner(Some(std::sync::Arc::new(move |run: &mut RunData| {
            ran.lock().unwrap().push((
                run.name().to_owned(),
                run.runopts
                    .run_timeout_seconds()
                    .map(std::num::NonZeroU64::get),
            ));
            let replayed = run
                .runopts
                .det_opts
                .det_config
                .replay_schedule_from
                .clone()
                .unwrap();
            let prefix = PreemptionReader::new(&replayed)
                .load_all()
                .schedevents()
                .len();
            if stopped(run, prefix) {
                return Err(anyhow::Error::new(crate::container::RunTimeoutMarker));
            }
            run.finish_for_test(prefix > deciding, std::time::Duration::from_secs(1));
            Ok(())
        })))
    }

    fn two_thread_schedule(len: usize) -> PreemptionRecord {
        PreemptionRecord::from_sched_events(
            (0..len)
                .map(|i| SchedEvent::branches(DetTid::from_raw(3 + (i % 2) as i32), 1))
                .collect(),
        )
    }

    /// A prefix trial that analyze's own wall-clock bound stopped has no
    /// outcome: it runs again at once with 4 times the bound, under its own
    /// name plus `_retry`, and the rerun's end counts. With the first run of
    /// every trial stopped, the search decides the same event as one in
    /// which every trial finished, in the same number of trials.
    #[test]
    fn a_trial_the_backstop_stopped_runs_again_and_does_not_change_the_answer() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let (len, deciding) = (200, 137);
        let target = two_thread_schedule(len);
        let search = [
            "--target-stdout=FAILED",
            "--prefix-search",
            "--trial-timeout=10",
        ];

        let parent = tempfile::tempdir().unwrap();
        let (mut finishing, _first) = analysis_in(&parent.path().join("finishing"), &search);
        let finished_runs = Arc::new(Mutex::new(Vec::new()));
        finishing.trial_runner = backstop_runner(deciding, |_, _| false, finished_runs.clone());
        let expected = finishing.phase5_prefix_search(&target).unwrap();
        assert_eq!(expected.decision, deciding);

        let (mut stopping, _second) = analysis_in(&parent.path().join("stopping"), &search);
        let stopped_runs = Arc::new(Mutex::new(Vec::new()));
        stopping.trial_runner = backstop_runner(
            deciding,
            |run, _| !run.name().ends_with("_retry"),
            stopped_runs.clone(),
        );
        let decided = stopping.phase5_prefix_search(&target).unwrap();
        assert_eq!(decided.decision, expected.decision);
        assert!(decided.unfinished.is_none());

        let finished_runs = finished_runs.lock().unwrap();
        let stopped_runs = stopped_runs.lock().unwrap();
        assert_eq!(stopped_runs.len(), 2 * finished_runs.len());
        for (pair, (name, bound)) in stopped_runs.chunks(2).zip(finished_runs.iter()) {
            assert_eq!(pair[0], (name.clone(), *bound));
            assert_eq!(*bound, Some(10));
            assert_eq!(pair[1], (format!("{name}_retry"), Some(40)));
        }
    }

    /// A prefix trial stopped again when it runs with the longer bound ends
    /// the search with a partial report (exit 120): its reason is
    /// `trial-inconclusive`, it names the prefix, both runs and their
    /// bounds, says the guest hangs or needs longer still, keeps the bracket
    /// found so far, and gives the command that continues with a longer
    /// `--trial-timeout`.
    #[test]
    fn a_trial_stopped_again_ends_the_search_with_a_partial_report() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let (len, deciding) = (200, 137);
        let target = two_thread_schedule(len);
        let parent = tempfile::tempdir().unwrap();
        let report_file = parent.path().join("report.json");
        let report = format!("--report-file={}", report_file.display());
        let (mut options, _workspace) = analysis_in(
            &parent.path().join("analysis"),
            &[
                "--target-stdout=FAILED",
                "--prefix-search",
                "--trial-timeout=10",
                report.as_str(),
            ],
        );
        let ran = Arc::new(Mutex::new(Vec::new()));
        // The empty prefix misses, the whole schedule meets, and the first
        // search trial, of the prefix of 100 events, never ends.
        options.trial_runner = backstop_runner(deciding, |_, prefix| prefix == 100, ran.clone());
        let result = options
            .phase5_prefix_search(&target)
            .map(|_| ExitStatus::SUCCESS);
        let status = options.end_on_budget(result).unwrap();
        assert_eq!(
            status,
            ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
        );
        assert!(options.partial_report_written);
        assert_eq!(
            ran.lock()
                .unwrap()
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            [
                "prefix_round_001",
                "prefix_round_002",
                "prefix_round_003",
                "prefix_round_003_retry"
            ]
        );
        let written: Report =
            serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
        let partial = written.partial.unwrap();
        assert_eq!(partial.reason, PartialReason::TrialInconclusive);
        assert_eq!(partial.reason.class(), "trial-inconclusive");
        assert_eq!(partial.phase, Phase::PrefixSearch);
        assert_eq!(partial.cut_run, None);
        let inconclusive = partial.inconclusive.unwrap();
        assert_eq!(inconclusive.prefix, Some(100));
        assert_eq!(
            inconclusive.runs,
            [
                "prefix_round_003".to_owned(),
                "prefix_round_003_retry".to_owned()
            ]
        );
        assert_eq!(inconclusive.bounds, [Some(10), Some(40)]);
        let bracket = partial.bracket.unwrap();
        assert_eq!((bracket.misses, bracket.meets), (Some(0), Some(len)));
        assert_eq!(bracket.misses_run.as_deref(), Some("prefix_round_001"));
        for said in [
            "because a trial has no outcome: replaying the first 100 events and then scheduling \
             without chaos",
            "(prefix_round_003) did not finish within hermit analyze's wall-clock bound of 10 s, \
             nor within 40 s when it ran again (prefix_round_003_retry): the guest hangs (for \
             example in a deadlock while other threads keep polling), or needs longer still. A \
             hang is a third outcome, not a run that misses the target",
            "--trial-timeout=160",
            "--trial-timeout=off",
        ] {
            assert!(written.header.contains(said), "{said}\n{}", written.header);
        }
        assert!(partial.resume_command.contains("--trial-timeout=160"));
    }

    /// A trial that the run arguments' own `--timeout` stopped is not
    /// rerun: that bound is the user's, and the trial counts as not meeting
    /// the target, as before. Here it is a trial that would have met the
    /// target, so the boundary settles on it, and the report says that the
    /// boundary rests on a wall-clock bound and that the guest hangs.
    #[test]
    fn a_stop_by_the_run_arguments_timeout_still_counts_as_not_meeting_the_target() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let (len, deciding) = (200, 137);
        let target = two_thread_schedule(len);
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = format!("--tmp-dir={}", parent.path().display());
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            tmp_dir.as_str(),
            "--target-stdout=FAILED",
            "--prefix-search",
            "--trial-timeout=off",
            "--",
            "--chaos",
            "--timeout=5",
            "/bin/true",
        ])
        .unwrap();
        let _workspace = options.phase0_initialize(Retention::Remove).unwrap();
        let ran = Arc::new(Mutex::new(Vec::new()));
        options.trial_runner = backstop_runner(deciding, |_, prefix| prefix == 150, ran.clone());
        let decided = options.phase5_prefix_search(&target).unwrap();
        assert_eq!(decided.decision, 150);
        assert!(
            ran.lock()
                .unwrap()
                .iter()
                .all(|(name, bound)| !name.ends_with("_retry") && *bound == Some(5))
        );
        let unfinished = decided.unfinished.unwrap();
        assert!(
            unfinished.contains("the guest hangs")
                && unfinished.contains("this boundary rests on a wall-clock bound"),
            "{unfinished}"
        );
    }

    /// A continuation one of whose endpoint trials analyze's bound stopped
    /// twice has no outcome there, and the search ends with that trial,
    /// naming the next, coarser continuation, which it does not try: on a
    /// slower host, moving on would answer at another timeslice. Through the
    /// search, the analysis ends with a partial report (exit 120) whose
    /// advice gives both the longer bound and that continuation.
    #[test]
    fn a_continuation_with_an_inconclusive_endpoint_trial_ends_the_search() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let ns = |nanos: u64| NonZeroU64::new(nanos);
        let candidates = [ns(1_000), ns(200_000_000)];
        let inconclusive = |next_continuation| TrialInconclusive {
            trial: "replaying the first 50 events and then scheduling without chaos at a 1000 ns \
                    timeslice"
                .to_owned(),
            prefix: Some(50),
            runs: [
                "prefix_round_002".to_owned(),
                "prefix_round_002_retry".to_owned(),
            ],
            bounds: [Some(240), Some(960)],
            next_continuation,
        };
        let tried = std::cell::RefCell::new(Vec::new());
        let error = choose_continuation(&candidates, 50, |prefix, timeslice| {
            tried.borrow_mut().push((prefix, timeslice));
            if timeslice == ns(1_000) && prefix == 50 {
                return Err(inconclusive(None).into());
            }
            Ok(if prefix > 0 {
                TrialEnd::Met
            } else {
                TrialEnd::Missed
            })
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<TrialInconclusive>(),
            Some(&inconclusive(Some(200_000_000)))
        );
        assert_eq!(tried.into_inner(), [(0, ns(1_000)), (50, ns(1_000))]);
        // The last candidate has no next one.
        let error = choose_continuation(&candidates, 50, |prefix, timeslice| {
            match (timeslice == ns(1_000), prefix) {
                (true, 50) => Ok(TrialEnd::Missed),
                (false, 50) => Err(inconclusive(None).into()),
                _ => Ok(TrialEnd::Missed),
            }
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<TrialInconclusive>(),
            Some(&inconclusive(None))
        );

        // Through the analysis: at the run's own 1000 ns timeslice the whole
        // schedule's replay never finishes.
        let len = 200;
        let target = two_thread_schedule(len);
        let parent = tempfile::tempdir().unwrap();
        let tmp_dir = format!("--tmp-dir={}", parent.path().display());
        let report_file = parent.path().join("report.json");
        let report = format!("--report-file={}", report_file.display());
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            tmp_dir.as_str(),
            report.as_str(),
            "--target-stdout=FAILED",
            "--prefix-search",
            "--trial-timeout=10",
            "--",
            "--chaos",
            "--max-timeslice=1000",
            "/bin/true",
        ])
        .unwrap();
        let _workspace = options.phase0_initialize(Retention::Remove).unwrap();
        let ran = Arc::new(Mutex::new(Vec::new()));
        options.trial_runner = backstop_runner(
            137,
            move |run, prefix| {
                prefix == len && run.runopts.det_opts.det_config.max_timeslice == ns(1_000)
            },
            ran.clone(),
        );
        let searched = options
            .phase5_prefix_search(&target)
            .map(|_| ExitStatus::SUCCESS);
        let status = options.end_on_budget(searched).unwrap();
        assert_eq!(
            status,
            ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
        );
        assert!(
            ran.lock()
                .unwrap()
                .iter()
                .all(|(_, bound)| *bound == Some(10) || *bound == Some(40)),
        );
        assert_eq!(ran.lock().unwrap().len(), 3, "{:?}", ran.lock().unwrap());
        let written: Report =
            serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
        let partial = written.partial.unwrap();
        assert_eq!(partial.reason, PartialReason::TrialInconclusive);
        assert_eq!(
            partial.inconclusive.unwrap().next_continuation,
            Some(200_000_000)
        );
        assert!(
            written.header.contains("--trial-timeout=160")
                && written
                    .header
                    .contains("Or search at the next, coarser continuation instead")
                && written
                    .header
                    .contains("--continuation-timeslice=200000000"),
            "{}",
            written.header
        );
    }

    /// The baseline run follows the same rule: one that analyze's bound
    /// stopped runs again with 4 times the bound and is the baseline when it
    /// finishes. Stopped again, it ends the analysis as inconclusive rather
    /// than sending it to the prefix search, which is the path a host's speed
    /// would otherwise choose.
    #[test]
    fn a_baseline_run_the_backstop_stopped_runs_again() {
        use std::sync::Arc;
        use std::sync::Mutex;
        for stop_again in [false, true] {
            let parent = tempfile::tempdir().unwrap();
            let (mut options, _workspace) = analysis_in(
                parent.path(),
                &["--target-stdout=FAILED", "--trial-timeout=10"],
            );
            let ran: RanTrials = Arc::new(Mutex::new(Vec::new()));
            let log = ran.clone();
            options.trial_runner =
                crate::analyze::rundata::TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                    log.lock().unwrap().push((
                        run.name().to_owned(),
                        run.runopts
                            .run_timeout_seconds()
                            .map(std::num::NonZeroU64::get),
                    ));
                    if run.name() == "run2_baseline"
                        || (stop_again && run.name() == "run2_baseline_retry")
                    {
                        return Err(anyhow::Error::new(crate::container::RunTimeoutMarker));
                    }
                    run.finish_for_test(false, std::time::Duration::from_secs(1));
                    Ok(())
                })));
            let chosen = options
                .phase4_choose_baseline_sched_events(PreemptionRecord::from_sched_events(vec![]));
            assert_eq!(
                *ran.lock().unwrap(),
                [
                    ("run2_baseline".to_owned(), Some(10)),
                    ("run2_baseline_retry".to_owned(), Some(40))
                ]
            );
            if stop_again {
                let error = chosen.err().unwrap();
                let inconclusive = error.downcast_ref::<TrialInconclusive>().unwrap();
                assert_eq!(
                    inconclusive.trial,
                    "the baseline run (non-chaos (0 extra preemptions) run)"
                );
                assert_eq!(inconclusive.bounds, [Some(10), Some(40)]);
                assert!(error.downcast_ref::<BaselineUnfinished>().is_none());
            } else {
                assert_eq!(chosen.unwrap().name(), "run2_baseline_retry");
            }
        }
    }

    /// The default search's trials follow the same rule: with the first run
    /// of every trial stopped by analyze's bound, each runs again with 4
    /// times it, and the search settles on the same critical pair as one in
    /// which every trial finished.
    #[test]
    fn a_bisection_trial_the_backstop_stopped_runs_again_and_does_not_change_the_pair() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
                .collect()
        };
        // Thread 5 running before thread 3 finishes meets the target.
        let target = events(&[3, 3, 5, 3, 5, 3]);
        let baseline = events(&[3, 3, 3, 3, 5, 5]);
        let runner = |stop_first: bool, ran: RanTrials| {
            crate::analyze::rundata::TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                ran.lock().unwrap().push((
                    run.name().to_owned(),
                    run.runopts
                        .run_timeout_seconds()
                        .map(std::num::NonZeroU64::get),
                ));
                if stop_first && !run.name().ends_with("_retry") {
                    return Err(anyhow::Error::new(crate::container::RunTimeoutMarker));
                }
                let input = run.sched_path_in().to_path_buf();
                let schedule = PreemptionReader::new(&input).load_all().into_global();
                let last_of_3 = schedule
                    .iter()
                    .rposition(|event| event.dettid == DetTid::from_raw(3));
                let first_of_5 = schedule
                    .iter()
                    .position(|event| event.dettid == DetTid::from_raw(5));
                let met =
                    matches!((first_of_5, last_of_3), (Some(five), Some(three)) if five < three);
                let output = run.sched_path_out().to_path_buf();
                std::fs::copy(&input, &output).unwrap();
                run.finish_for_test(met, std::time::Duration::from_secs(1));
                Ok(())
            })))
        };
        let parent = tempfile::tempdir().unwrap();
        let mut found = Vec::new();
        let mut counts = Vec::new();
        for stop_first in [false, true] {
            let (mut options, _workspace) = analysis_in(
                &parent.path().join(stop_first.to_string()),
                &["--target-stdout=FAILED", "--trial-timeout=10"],
            );
            let ran = Arc::new(Mutex::new(Vec::new()));
            options.trial_runner = runner(stop_first, ran.clone());
            let crit = options.phase5_bisect_traces(&target, &baseline).unwrap();
            found.push((
                crit.critical_event_index,
                crit.failing_schedule.clone(),
                crit.passing_schedule.clone(),
            ));
            let ran = ran.lock().unwrap();
            if stop_first {
                for pair in ran.chunks(2) {
                    assert_eq!(pair[0].1, Some(10));
                    assert_eq!(pair[1], (format!("{}_retry", pair[0].0), Some(40)));
                }
            }
            counts.push(ran.len());
        }
        assert_eq!(found[0], found[1]);
        assert_eq!(counts[1], 2 * counts[0]);
    }

    /// A round that leaves its schedule and is then stopped (by analyze's
    /// bound, by the run arguments' own `--timeout`, or by a refusal) has no
    /// outcome either: it is not run again, the search tries another
    /// schedule between the pair, and it settles on the pair a search in
    /// which every round is followed finds. The departure is known without
    /// the run's summary (`departure_note`).
    #[test]
    fn a_round_that_left_its_schedule_and_then_stopped_never_decides() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
                .collect()
        };
        // Thread 5 running before thread 3 finishes meets the target.
        let target = events(&[3, 3, 5, 3, 3, 5, 3, 3]);
        let baseline = events(&[3, 3, 3, 3, 3, 3, 5, 5]);
        let departure = detcore_model::summary::ScheduleDeparture {
            event: 2,
            expected: DetTid::from_raw(5),
            running: DetTid::from_raw(3),
        };
        // The first spliced round leaves its schedule and then ends with
        // `stop()`; every other round is followed.
        let runner = |stop: Option<fn() -> anyhow::Error>, ran: Arc<Mutex<Vec<String>>>| {
            crate::analyze::rundata::TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                ran.lock().unwrap().push(run.name().to_owned());
                if let Some(stop) = stop
                    && run.name().starts_with("bisect_round_003")
                {
                    run.depart_for_test(departure);
                    return Err(stop());
                }
                let input = run.sched_path_in().to_path_buf();
                let schedule = PreemptionReader::new(&input).load_all().into_global();
                let last_of_3 = schedule
                    .iter()
                    .rposition(|event| event.dettid == DetTid::from_raw(3));
                let first_of_5 = schedule
                    .iter()
                    .position(|event| event.dettid == DetTid::from_raw(5));
                let met =
                    matches!((first_of_5, last_of_3), (Some(five), Some(three)) if five < three);
                std::fs::copy(&input, run.sched_path_out()).unwrap();
                run.finish_for_test(met, std::time::Duration::from_secs(1));
                Ok(())
            })))
        };
        let parent = tempfile::tempdir().unwrap();
        let bisect = |label: &str, extra: &[&str], stop: Option<fn() -> anyhow::Error>| {
            let tmp_dir = format!("--tmp-dir={}", parent.path().join(label).display());
            let mut argv = vec!["analyze", tmp_dir.as_str(), "--target-stdout=FAILED"];
            argv.extend(extra);
            argv.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let workspace = options.phase0_initialize(Retention::Remove).unwrap();
            let ran = Arc::new(Mutex::new(Vec::new()));
            options.trial_runner = runner(stop, ran.clone());
            let crit = options.phase5_bisect_traces(&target, &baseline).unwrap();
            drop(workspace);
            let ran = ran.lock().unwrap().clone();
            (crit, ran)
        };
        let (followed, _) = bisect("followed", &["--", "--chaos"], None);
        for (label, extra, stop) in [
            (
                "backstop",
                &["--trial-timeout=10", "--", "--chaos"][..],
                (|| anyhow::Error::new(crate::container::RunTimeoutMarker))
                    as fn() -> anyhow::Error,
            ),
            ("own-timeout", &["--", "--chaos", "--timeout=5"][..], || {
                anyhow::Error::new(crate::container::RunTimeoutMarker)
            }),
            ("refused", &["--", "--chaos"][..], || {
                anyhow::Error::new(crate::container::PolicyRefusal)
            }),
        ] {
            let (found, ran) = bisect(label, extra, Some(stop));
            assert!(
                ran.contains(&"bisect_round_003".to_owned()),
                "{label}: {ran:?}"
            );
            assert!(
                !ran.iter().any(|name| name.ends_with("_retry")),
                "{label}: a round that left its schedule ran again: {ran:?}"
            );
            assert_eq!(
                (
                    found.critical_event_index,
                    &found.failing_schedule,
                    &found.passing_schedule
                ),
                (
                    followed.critical_event_index,
                    &followed.failing_schedule,
                    &followed.passing_schedule
                ),
                "{label}"
            );
        }
    }

    /// Endpoints whose per-thread traces diverge are not bisected: the
    /// analysis runs the target's prefix search instead, starts no bisection
    /// round, and its report says why.
    #[test]
    fn diverging_endpoints_go_to_the_prefix_search() {
        use std::sync::Arc;
        use std::sync::Mutex;

        use detcore::types::SyscallPhase;
        use reverie::syscalls::Sysno;
        let t = DetTid::from_raw;
        let syscall = |sysno, phase| SchedEvent::syscall(t(3), sysno, phase);
        let read = [
            syscall(Sysno::read, SyscallPhase::Prehook),
            syscall(Sysno::read, SyscallPhase::Posthook),
        ];
        let write = [
            syscall(Sysno::write, SyscallPhase::Prehook),
            syscall(Sysno::write, SyscallPhase::Posthook),
        ];
        let target: Vec<SchedEvent> = (0..10)
            .flat_map(|_| [read.as_slice(), &[SchedEvent::branches(t(5), 1)], &write].concat())
            .collect();
        // The baseline run made thread 3's first write before its first read.
        let mut baseline = target.clone();
        baseline.splice(
            ..5,
            [
                write[0].clone(),
                write[1].clone(),
                SchedEvent::branches(t(5), 1),
                read[0].clone(),
                read[1].clone(),
            ],
        );
        assert!(crate::schedule_search::trace_divergence(&baseline, &target).diverges());

        let parent = tempfile::tempdir().unwrap();
        let report_file = parent.path().join("report.json");
        let report = format!("--report-file={}", report_file.display());
        let (mut options, _workspace) = analysis_in(
            &parent.path().join("analysis"),
            &["--target-stdout=FAILED", &report],
        );
        let ran = Arc::new(Mutex::new(Vec::new()));
        options.trial_runner = backstop_runner(20, |_, _| false, ran.clone());
        let endpoint = |name: &str, events: &[SchedEvent]| {
            let mut run = RunData::new_baseline(&options, name.to_owned()).unwrap();
            PreemptionRecord::from_sched_events(events.to_vec())
                .write_to_disk(run.sched_path_out())
                .unwrap();
            run
        };
        let target_run = endpoint("run1_target", &target);
        let baseline_run = endpoint("run2_baseline", &baseline);
        let status = options
            .bisect_endpoints_and_report(target_run, baseline_run)
            .unwrap();
        assert_eq!(status, ExitStatus::SUCCESS);
        let ran = ran.lock().unwrap();
        assert!(
            !ran.iter().any(|(name, _)| name.starts_with("bisect_round")),
            "{ran:?}"
        );
        assert!(
            ran.iter().any(|(name, _)| name.starts_with("prefix_round")),
            "{ran:?}"
        );
        let written: Report =
            serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
        assert!(
            written.header.contains(
                "Note: the default search did not bisect between the two runs: the two runs' \
                 per-thread traces diverge, so splices are not feasible"
            ),
            "{}",
            written.header
        );
    }

    /// When analyze searches the prefixes because the two runs' per-thread
    /// traces diverge, its report says so, with what it counted.
    #[test]
    fn a_prefix_search_after_divergent_traces_says_why() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let parent = tempfile::tempdir().unwrap();
        let (mut options, _workspace) = analysis_in(
            parent.path(),
            &["--target-stdout=FAILED", "--prefix-search"],
        );
        options.trial_runner = backstop_runner(137, |_, _| false, Arc::new(Mutex::new(Vec::new())));
        let why =
            crate::schedule_search::DivergentTraces(crate::schedule_search::TraceDivergence {
                reordered: 81,
                split_syscalls: 1,
            });
        let decision = PrefixDecision {
            fallback: Some(format!(
                "the default search did not bisect between the two runs: {why}."
            )),
            ..options
                .phase5_prefix_search(&two_thread_schedule(200))
                .unwrap()
        };
        assert_eq!(decision.decision, 137);
        let report = options.phase6_record_prefix_outputs(decision).unwrap();
        assert!(
            report.header.contains(
                "Note: the default search did not bisect between the two runs: the two runs' \
                 per-thread traces diverge, so splices are not feasible: matching the two \
                 schedules' events would move 81 of them"
            ),
            "{}",
            report.header
        );
    }

    /// A bisection round whose replay could not follow its spliced schedule
    /// has no outcome: the search tries another schedule between the pair and
    /// settles on the same critical pair as a search in which every round is
    /// followed. When no schedule between the pair can be followed, the
    /// analysis ends with a partial report (exit 120, `trial-inconclusive`)
    /// that lists the rounds and gives the `--prefix-search` command.
    #[test]
    fn a_bisection_round_that_left_its_schedule_never_decides() {
        use std::sync::Arc;
        use std::sync::Mutex;
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
                .collect()
        };
        // Thread 5 running before thread 3 finishes meets the target.
        let target = events(&[3, 3, 5, 3, 3, 5, 3, 3]);
        let baseline = events(&[3, 3, 3, 3, 3, 3, 5, 5]);
        let departure = detcore_model::summary::ScheduleDeparture {
            event: 2,
            expected: DetTid::from_raw(5),
            running: DetTid::from_raw(3),
        };
        // `departs(name)`: whether the round of that name cannot follow its
        // schedule.
        let runner = |departs: fn(&str) -> bool, ran: Arc<Mutex<Vec<String>>>| {
            crate::analyze::rundata::TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                ran.lock().unwrap().push(run.name().to_owned());
                let input = run.sched_path_in().to_path_buf();
                let schedule = PreemptionReader::new(&input).load_all().into_global();
                let last_of_3 = schedule
                    .iter()
                    .rposition(|event| event.dettid == DetTid::from_raw(3));
                let first_of_5 = schedule
                    .iter()
                    .position(|event| event.dettid == DetTid::from_raw(5));
                let met =
                    matches!((first_of_5, last_of_3), (Some(five), Some(three)) if five < three);
                let output = run.sched_path_out().to_path_buf();
                std::fs::copy(&input, &output).unwrap();
                // A run that left its schedule finishes with some outcome;
                // the opposite one shows that it is not read.
                run.finish_for_test(
                    met != departs(run.name()),
                    std::time::Duration::from_secs(1),
                );
                if departs(run.name()) {
                    run.depart_for_test(departure);
                }
                Ok(())
            })))
        };
        let parent = tempfile::tempdir().unwrap();
        let bisect = |label: &str, departs: fn(&str) -> bool| {
            let report_file = parent.path().join(format!("{label}.json"));
            let report = format!("--report-file={}", report_file.display());
            let (mut options, workspace) = analysis_in(
                &parent.path().join(label),
                &["--target-stdout=FAILED", report.as_str()],
            );
            let ran = Arc::new(Mutex::new(Vec::new()));
            options.trial_runner = runner(departs, ran.clone());
            let crit = options.phase5_bisect_traces(&target, &baseline);
            (options, workspace, crit, ran, report_file)
        };

        let (_, _w1, followed, followed_ran, _) = bisect("followed", |_| false);
        let followed = followed.unwrap();
        // The first spliced round, the first pass's midpoint, cannot be
        // followed.
        let (_, _w2, found, ran, _) = bisect("departed", |name| name == "bisect_round_003");
        let found = found.unwrap();
        assert_eq!(found.critical_event_index, followed.critical_event_index);
        assert_eq!(found.failing_schedule, followed.failing_schedule);
        assert_eq!(found.passing_schedule, followed.passing_schedule);
        let ran = ran.lock().unwrap();
        assert!(ran.contains(&"bisect_round_003".to_owned()));
        assert!(followed_ran.lock().unwrap().len() >= 4 && ran.len() >= 4);
        drop(ran);

        // No spliced round can be followed.
        let (mut options, _w3, stopped, ran, report_file) = bisect("none", |name| {
            name != "bisect_round_001" && name != "bisect_round_002"
        });
        let status = options
            .end_on_budget(stopped.map(|_| ExitStatus::SUCCESS))
            .unwrap();
        assert_eq!(
            status,
            ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
        );
        let written: Report =
            serde_json::from_str(&std::fs::read_to_string(&report_file).unwrap()).unwrap();
        let partial = written.partial.unwrap();
        assert_eq!(partial.reason, PartialReason::TrialInconclusive);
        assert_eq!(partial.phase, Phase::Bisect);
        let infeasible = partial.infeasible.unwrap();
        assert_eq!(infeasible.pass, 0);
        assert_eq!(infeasible.tried.len() + 2, ran.lock().unwrap().len());
        assert_eq!(
            infeasible.tried[0],
            InfeasibleSchedule {
                run: "bisect_round_003".to_owned(),
                event: 2,
                expected: 5,
                running: 3,
                passed: false,
            }
        );
        assert!(
            written
                .header
                .contains("because no schedule between the closest pair could be replayed")
                && written
                    .header
                    .contains("A resumed analysis would replay the same schedules"),
            "{}",
            written.header
        );
        assert!(partial.resume_command.contains("--prefix-search"));
    }

    /// The default search's partial report names its pair of schedules, the
    /// first event at which they differ and the commands that replay them,
    /// and says when the stack traces did not fit the grace period.
    #[test]
    fn a_partial_bisection_names_its_pair_and_why_it_took_no_stacks() {
        let parent = tempfile::tempdir().unwrap();
        let (mut options, _workspace) = analysis_in(parent.path(), &["--time-budget=30m"]);
        options.budget = Budget::new(options.time_budget, options.trial_budget);
        options
            .endpoints
            .record(std::time::Duration::from_secs(400), None);
        options.progress.phase(Phase::Bisect);
        let events = |tids: &[i32]| -> Vec<SchedEvent> {
            tids.iter()
                .map(|&tid| SchedEvent::branches(DetTid::from_raw(tid), 1))
                .collect()
        };
        let mut interval = IntervalFiles::new(options.tmp_dir.as_ref().unwrap());
        interval.record(
            &options,
            Some(3),
            [&events(&[3, 3, 5, 3]), &events(&[3, 3, 3, 5])],
            Some((2, 1)),
            None,
        );
        let recorded = options.progress.snapshot().interval.unwrap();
        assert_eq!(recorded.common_prefix, 2);
        assert!(recorded.passing.is_file() && recorded.failing.is_file());

        let expired = BudgetExpired {
            kind: BudgetKind::TimeBudget,
            cut: Some("bisect_round_009".to_owned()),
        };
        assert_eq!(
            options.report_partial(expired).unwrap(),
            ExitStatus::Exited(hermit::HERMIT_ANALYZE_PARTIAL_EXIT)
        );
        let progress = options.progress.snapshot();
        assert_eq!(progress.interval, Some(recorded));
        // Two replays of a 400 s endpoint do not fit min(120 s, 5% of 30 min).
        let partial_stacks = options
            .partial_stacks(progress.interval.as_ref().unwrap())
            .1;
        assert!(
            partial_stacks.starts_with(
                "not taken: two replays as slow as the slowest endpoint run (400 s) do not fit \
                 the grace period of 90 s"
            ),
            "{partial_stacks}"
        );
    }

    /// A resume refuses an analysis of something else, names what differs,
    /// and says how to start over; the same analysis resumes, with the seed
    /// the first invocation drew.
    #[test]
    fn a_resume_refuses_another_analysis_and_says_how_to_start_over() {
        let parent = tempfile::tempdir().unwrap();
        let (mut first, mut workspace) = analysis_in(
            parent.path(),
            &["--target-stdout=FAILED", "--prefix-search"],
        );
        workspace.keep("the resumed analysis owns it");
        first.start_checkpoint().unwrap();
        let drawn = first.analyze_seed.unwrap();
        let path = workspace.path().to_path_buf();
        let resume = format!("--resume={}", path.display());

        for (options, differs) in [
            (
                vec!["--target-stdout=PASSED", "--prefix-search"],
                "the target --target-stdout: recorded FAILED, now PASSED",
            ),
            (
                vec!["--target-stdout=FAILED"],
                "the search option --prefix-search: recorded true, now false",
            ),
        ] {
            let mut argv = vec!["analyze", resume.as_str()];
            argv.extend(options);
            argv.extend(["--", "--chaos", "/bin/true"]);
            let mut again = AnalyzeOpts::try_parse_from(argv).unwrap();
            let error = again.phase0_resume(&path).unwrap_err();
            assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
            let message = format!("{error:#}");
            assert!(message.contains(differs), "{message}");
            assert!(
                message.contains("To continue anyway, run the same command without --resume"),
                "{message}"
            );
        }
        // --trial-timeout decides only when a search trial is stopped, so a
        // resume may give stopped trials more time.
        let mut argv = vec!["analyze", resume.as_str()];
        argv.extend([
            "--target-stdout=FAILED",
            "--prefix-search",
            "--trial-timeout=600",
        ]);
        argv.extend(["--", "--chaos", "/bin/true"]);
        let mut same = AnalyzeOpts::try_parse_from(argv).unwrap();
        let resumed = same.phase0_resume(&path).unwrap();
        assert_eq!(same.analyze_seed, Some(drawn));
        assert_eq!(same.tmp_dir.as_deref(), Some(path.as_path()));
        assert!(same.journal.is_on());
        drop(resumed);
        drop(workspace);
    }

    /// A resumed analysis that errors or is stopped keeps the workspace it
    /// adopted, with the journal of the runs before it; one that finishes
    /// leaves it to its retention, like a new analysis.
    #[test]
    fn a_resumed_analysis_keeps_its_workspace_unless_it_finishes() {
        let parent = tempfile::tempdir().unwrap();
        let (mut options, _workspace) = analysis_in(parent.path(), &[]);
        let failed: anyhow::Result<ExitStatus> = Err(anyhow::anyhow!("interrupted"));
        let finished: anyhow::Result<ExitStatus> = Ok(ExitStatus::SUCCESS);
        assert_eq!(options.keep_workspace_because(&failed), None);
        options.resume = Some(parent.path().to_path_buf());
        assert!(
            options
                .keep_workspace_because(&failed)
                .unwrap()
                .contains("a resumed analysis that does not finish keeps its workspace")
        );
        assert_eq!(options.keep_workspace_because(&finished), None);
        options.partial_report_written = true;
        assert!(
            options
                .keep_workspace_because(&finished)
                .unwrap()
                .contains("a budget ended the analysis")
        );
    }

    /// A prefix search that a trial budget ends, resumed through
    /// `phase0_resume`, `RunData::launch` and the journal, with a trial runner
    /// in place of the guest: the resumed search answers the trials the first
    /// invocation ran from the journal, runs only the others, and decides
    /// what an uninterrupted search decides.
    #[test]
    fn a_prefix_search_resumed_through_the_journal_decides_as_an_uninterrupted_one() {
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use crate::analyze::rundata::TrialRunner;
        let (len, deciding) = (600, 437);
        let target = PreemptionRecord::from_sched_events(
            (0..len)
                .map(|i| SchedEvent::branches(DetTid::from_raw(3 + (i % 2)), 1))
                .collect(),
        );
        // A trial meets the target when its prefix holds the deciding event.
        let runner = |count: Arc<AtomicUsize>| {
            TrialRunner(Some(Arc::new(move |run: &mut RunData| {
                count.fetch_add(1, Ordering::SeqCst);
                let replayed = run
                    .runopts
                    .det_opts
                    .det_config
                    .replay_schedule_from
                    .clone()
                    .unwrap();
                let prefix = PreemptionReader::new(&replayed)
                    .load_all()
                    .schedevents()
                    .len();
                run.finish_for_test(prefix > deciding, std::time::Duration::from_secs(1));
                Ok(())
            })))
        };
        let search = ["--target-stdout=FAILED", "--prefix-search"];

        let parent = tempfile::tempdir().unwrap();
        let uninterrupted_runs = Arc::new(AtomicUsize::new(0));
        let (mut uninterrupted, _uninterrupted_workspace) =
            analysis_in(&parent.path().join("uninterrupted"), &search);
        uninterrupted.start_checkpoint().unwrap();
        uninterrupted.trial_runner = runner(uninterrupted_runs.clone());
        let expected = uninterrupted.phase5_prefix_search(&target).unwrap();
        let all = uninterrupted_runs.load(Ordering::SeqCst);
        assert!(all > 4, "{all} trials");

        let stopped_runs = Arc::new(AtomicUsize::new(0));
        let mut first_args = search.to_vec();
        first_args.push("--trial-budget=4");
        let (mut first, mut workspace) = analysis_in(&parent.path().join("first"), &first_args);
        workspace.keep("the resumed analysis owns it");
        first.start_checkpoint().unwrap();
        first.budget = Budget::new(first.time_budget, first.trial_budget);
        first.trial_runner = runner(stopped_runs.clone());
        let Err(error) = first.phase5_prefix_search(&target) else {
            panic!("the trial budget did not end the search");
        };
        assert!(error.downcast_ref::<BudgetExpired>().is_some(), "{error:#}");
        assert_eq!(stopped_runs.load(Ordering::SeqCst), 4);

        let path = workspace.path().to_path_buf();
        let resume = format!("--resume={}", path.display());
        let mut argv = vec!["analyze", resume.as_str()];
        argv.extend(search);
        argv.extend(["--", "--chaos", "/bin/true"]);
        let mut resumed = AnalyzeOpts::try_parse_from(argv).unwrap();
        let resumed_workspace = resumed.phase0_resume(&path).unwrap();
        let resumed_runs = Arc::new(AtomicUsize::new(0));
        resumed.trial_runner = runner(resumed_runs.clone());
        let decided = resumed.phase5_prefix_search(&target).unwrap();
        assert_eq!(decided.decision, expected.decision);
        assert_eq!(decided.decision, deciding);
        assert_eq!(resumed.journal.answered(), 4);
        assert_eq!(resumed_runs.load(Ordering::SeqCst), all - 4);
        drop(resumed_workspace);
        drop(workspace);
    }
}
