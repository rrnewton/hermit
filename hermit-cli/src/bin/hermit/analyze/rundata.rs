/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Everything to do with the configuration, inputs, and outputs of a single run: a single point in
//! the search space that `hermit analyze` must navigate.

use std::fs::File;
use std::io::Write;
use std::num::NonZeroU64;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::bail;
use clap::CommandFactory;
use clap::FromArgMatches;
use clap::parser::ValueSource;
use colored::Colorize;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use detcore_model::config::ExpiredWaitCharge;
use hermit::process::Bind;
use reverie::process::Output;
use tracing::metadata::LevelFilter;

use crate::analyze::budget::BudgetExpired;
use crate::analyze::checkpoint::Journal;
use crate::analyze::checkpoint::JournalEntry;
use crate::analyze::checkpoint::RecordedEnd;
use crate::analyze::checkpoint::RecordedFile;
use crate::analyze::consts::*;
use crate::analyze::stop;
use crate::analyze::types::AnalyzeOpts;
use crate::analyze::types::ContinuationTimeslice;
use crate::analyze::types::TRIAL_LOG_FACTOR;
use crate::analyze::types::TRIAL_LOG_FLOOR;
use crate::analyze::types::TRIAL_TIMEOUT_FACTOR;
use crate::analyze::types::TRIAL_TIMEOUT_FLOOR;
use crate::analyze::types::TrialTimeout;
use crate::analyze::verdict::TimingSetting;
use crate::container::RunTimeoutMarker;
use crate::global_opts::GlobalOpts;
use crate::run::RunOpts;

/// What runs a trial's guest in place of [`RunData::run_now`]. Production
/// never sets one: it is a seam for tests, which drive analyze's searches,
/// the journal and resume through [`RunData::launch`] without a guest.
pub type TrialFn = dyn Fn(&mut RunData) -> anyhow::Result<()> + Send + Sync;

/// [`AnalyzeOpts::trial_runner`]: unset, or a [`TrialFn`].
#[derive(Clone, Default)]
pub struct TrialRunner(pub Option<std::sync::Arc<TrialFn>>);

impl std::fmt::Debug for TrialRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() {
            "TrialRunner(set)"
        } else {
            "TrialRunner(unset)"
        })
    }
}

/// A single run plus the results of the run, either in memory or on disk.
pub struct RunData {
    /// A unique name for this run.
    runname: String,
    /// An immutable snapshot of the options.
    analyze_opts: AnalyzeOpts, // Could use an Rc to share 1 copy.

    pub runopts: RunOpts, // TEMP: make private.

    preempts_path_in: Option<PathBuf>,
    sched_path_in: Option<PathBuf>,
    sched_path_out: Option<PathBuf>,

    /// How much logging to enable in the guest.
    log_level: LevelFilter,
    /// Where to write the guest log too (otherwise stderr).
    log_path: Option<PathBuf>,

    /// The input preemptions, if it has been read to memory.
    in_mem_preempts_in: Option<PreemptionRecord>,
    in_mem_sched_out: Option<PreemptionRecord>,

    is_a_match: Option<bool>,

    /// The `--trial-timeout` bound this run took, when it is shorter than the
    /// run arguments' own `--timeout` ([`AnalyzeOpts::trial_bound`]).
    trial_bound: Option<NonZeroU64>,
    /// How long the run took, once it finished.
    wall: Option<Duration>,
    /// The log file the run wrote, once it launched (`--verbose` or
    /// `--selfcheck`).
    logged_to: Option<PathBuf>,
    /// The size of that log as the journal recorded it, for a run the
    /// journal answered.
    journaled_log_bytes: Option<u64>,
}

impl RunData {
    pub fn root_path(&self) -> PathBuf {
        let tmp_dir = self.analyze_opts.tmp_dir.as_ref().unwrap();
        tmp_dir.join(&self.runname)
    }

    fn out_path(&self) -> PathBuf {
        let tmp_dir = self.analyze_opts.tmp_dir.as_ref().unwrap();
        tmp_dir.join(self.runname.clone() + "_out")
    }

    /// Where analyze writes a preemption record for this run to replay
    /// ([`Self::with_preempts_in`]): the workspace, under the run's name.
    /// Never a path from the run arguments: a `--replay-preemptions-from`
    /// there names the user's own record, which a trial must not overwrite.
    pub fn preempts_path_in(&mut self) -> &Path {
        if self.preempts_path_in.is_none() {
            self.preempts_path_in = Some(self.root_path().with_extension(PREEMPTS_EXT));
        }
        self.preempts_path_in.as_ref().unwrap()
    }

    pub fn preempts_path_out(&mut self) -> &Path {
        // TODO: split these apart:
        self.sched_path_out()
    }

    // Return a reference to the in-memory preemption record, reading it from disk if it isn't read
    // already. Errors if the file doesn't exist.
    pub fn preempts_out(&mut self) -> &PreemptionRecord {
        if self.in_mem_sched_out.is_none() {
            let path = self.sched_path_out();
            let record = PreemptionReader::new(path).load_all();
            // Trials spliced from this schedule make its jumps of global time.
            self.analyze_opts
                .time_skip_sources
                .lock()
                .unwrap()
                .remember(&record);
            self.in_mem_sched_out = Some(record);
        }
        self.in_mem_sched_out.as_ref().unwrap()
    }

    pub fn sched_path_out(&mut self) -> &Path {
        if self.sched_path_out.is_none() {
            let path = if let Some(p) = &self.runopts.det_opts.det_config.record_preemptions_to {
                p.to_owned()
            } else {
                self.out_path().with_extension(PREEMPTS_EXT)
            };
            self.sched_path_out = Some(path);
        }
        self.sched_path_out.as_ref().unwrap()
    }

    /// Where analyze writes a schedule for this run to replay (a bisection
    /// trial's, [`Self::with_schedule_replay`]): the workspace, under the
    /// run's name. Never a path from the run arguments: a
    /// `--replay-schedule-from` there names the user's own schedule, which a
    /// trial must not overwrite.
    pub fn sched_path_in(&mut self) -> &Path {
        if self.sched_path_in.is_none() {
            self.sched_path_in = Some(self.root_path().with_extension(PREEMPTS_EXT));
        }
        self.sched_path_in.as_ref().unwrap()
    }

    /// Convenience function
    pub fn sched_out_file_name(&mut self) -> String {
        self.sched_path_out()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    pub fn sched_out(&mut self) -> anyhow::Result<&Vec<SchedEvent>> {
        self.require_complete_output()?;
        Ok(self.preempts_out().schedevents())
    }

    /// Known omissions cannot become an analysis endpoint or selfcheck proof.
    pub fn require_complete_output(&mut self) -> anyhow::Result<()> {
        let path = self.sched_path_out().to_path_buf();
        Self::require_complete_record(self.preempts_out(), &path)
    }

    pub(super) fn require_complete_record(
        record: &PreemptionRecord,
        path: &Path,
    ) -> anyhow::Result<()> {
        record.require_faithful_replay().map_err(|error| {
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "analysis cannot use {} as completed schedule evidence: {error:#}",
                path.display()
            ))
        })
    }

    /// Only set after launch.
    pub fn log_path(&mut self) -> Option<&PathBuf> {
        if self.has_launched() {
            if self.log_path.is_none() {
                self.log_path = Some(self.root_path().with_extension(LOG_EXT))
            }
            self.log_path.as_ref()
        } else {
            None
        }
    }

    /// Only set after launch.
    pub fn is_a_match(&self) -> bool {
        self.is_a_match.expect("only called after launch method")
    }

    pub fn has_launched(&self) -> bool {
        self.is_a_match.is_some()
    }

    /// For a test that drives a phase with a launch it models: the completed
    /// observation `launch` makes, whether the run met the target, and the
    /// schedule it recorded.
    #[cfg(test)]
    pub(super) fn complete_for_test(
        &mut self,
        met: bool,
        recorded: &[SchedEvent],
    ) -> anyhow::Result<()> {
        self.is_a_match = Some(met);
        PreemptionRecord::from_sched_events(recorded.to_vec())
            .write_to_disk(self.sched_path_out())
            .map_err(anyhow::Error::msg)
    }

    /// Write a normalized copy of the output preemption record, for reading
    /// only, beside it, and return its path and contents. The run keeps its
    /// own record: normalization renumbers priorities across the scheduler's
    /// fixed ones, so the copy can replay another schedule.
    pub fn write_normalized_preempts(&mut self) -> (PathBuf, PreemptionRecord) {
        assert!(self.has_launched());
        let normalized_path = self.preempts_path_out().with_extension("normalized");
        let normalized = self.preempts_out().normalize();
        normalized
            .write_to_disk(&normalized_path)
            .expect("write of preempts file to succeed");
        (normalized_path, normalized)
    }

    /// The global options a trial run is launched with. Trials go through
    /// `RunOpts::run`, which dispatches on the trial `RunOpts`' own backend (set
    /// from `hermit --backend <BACKEND> analyze` by `get_raw_runopts`); this
    /// copy keeps the trial's global options consistent with it.
    fn trial_global_opts(&self) -> GlobalOpts {
        let log_path = self.root_path().with_extension(LOG_EXT);
        GlobalOpts {
            log: Some(self.log_level),
            log_file: if self.analyze_opts.verbose || self.analyze_opts.selfcheck {
                Some(log_path)
            } else {
                None
            },
            backend: self.analyze_opts.backend,
            // A run analyze bounds in time also gets the log bound.
            log_file_limit: self
                .analyze_opts
                .trial_bound(&self.runopts)
                .and(self.analyze_opts.trial_log_limit()),
            // Every trial charges the invocation's ONE budget, as verify's
            // per-run logs do. Without --verbose/--selfcheck a trial traces to
            // hermit's own stderr -- the public stream the cap exists to bound.
            max_log_bytes: self.analyze_opts.max_log_bytes,
            log_budget: self.analyze_opts.log_budget.clone(),
            log_file_handle: None,
            unsafe_ignore_host_seccomp: false,
        }
    }

    /// Execute the run. (Including setting up logging and temp dir binding.)
    ///
    /// Once a stop signal has arrived (`stop`), no run starts, and the run it
    /// ended reports the stop instead of its own outcome.
    ///
    /// With the journal on (`checkpoint`), a run the journal already holds,
    /// with the files it wrote still there, takes its recorded outcome
    /// instead of running, and a run that finished, or that its own bound or
    /// a refusal stopped, is journaled before its outcome is read. Once a
    /// budget has run out no run starts (`BudgetExpired`). A run gets at most
    /// the time the budget has left as its `--timeout`; one that this
    /// shortened bound ends is cut: its outcome came from the budget, not the
    /// guest, so it is not journaled and the analysis stops
    /// (`BudgetExpired`), to run it again when resumed.
    pub fn launch(&mut self) -> anyhow::Result<()> {
        stop::check()?;
        let journal = self.analyze_opts.journal.clone();
        let key = if journal.is_on() {
            Some(self.journal_key())
        } else {
            None
        };
        if let Some(key) = &key
            && let Some(entry) = journal.lookup(key)
            && let Some(answered) = self.answer_from_journal(&entry)?
        {
            journal.note_answered();
            return answered;
        }
        let own_timeout = self.runopts.run_timeout_seconds();
        let shortened = self.bound_by_budget()?;
        let launched = match self.analyze_opts.trial_runner.0.clone() {
            Some(run) => run(self),
            None => self.run_now(own_timeout),
        };
        self.runopts.set_run_timeout(own_timeout);
        if shortened
            && launched
                .as_ref()
                .is_err_and(|error| error.downcast_ref::<RunTimeoutMarker>().is_some())
        {
            return Err(self.analyze_opts.budget.cut(&self.runname).into());
        }
        if let Some(key) = key {
            self.journal_outcome(&journal, key, &launched)?;
        }
        launched
    }

    /// Start the budget's count of this run and shorten its `--timeout` to
    /// the time left; whether it shortened it. `Err` once a budget has run
    /// out. A backend on which analyze cannot bound a run with `--timeout`
    /// gets no shorter bound: the budget then stops analyze between runs.
    fn bound_by_budget(&mut self) -> Result<bool, BudgetExpired> {
        let Some(left) = self.analyze_opts.budget.start_trial()? else {
            return Ok(false);
        };
        if !self.runopts.accepts_run_timeout()
            || self
                .runopts
                .run_timeout_seconds()
                .is_some_and(|own| own <= left)
        {
            return Ok(false);
        }
        self.runopts.bound_run_timeout(left);
        Ok(true)
    }

    /// What names this run in the journal: its name, its `hermit run`
    /// command (with its own `--timeout`, never the budget's), and the
    /// contents of the schedule or record it replays.
    fn journal_key(&self) -> String {
        let mut material = format!("{}\n{}\n", self.runname, self.to_repro());
        let config = &self.runopts.det_opts.det_config;
        for input in [
            &config.replay_schedule_from,
            &config.replay_preemptions_from,
        ]
        .into_iter()
        .flatten()
        {
            let digest = detcore::Digest::digest_path(input)
                .map_or_else(|_| "unreadable".to_owned(), |digest| digest.to_string());
            material.push_str(&format!("{}={digest}\n", input.display()));
        }
        detcore::Digest::new(material.as_bytes()).to_string()
    }

    /// Take `entry`'s outcome instead of running, as `launch` returns it:
    /// `None` when a file the run wrote is gone or changed, and the run must
    /// run again. A recording that analyze removed is restored from the
    /// schedule it replayed when the two hold the same events.
    fn answer_from_journal(
        &mut self,
        entry: &JournalEntry,
    ) -> anyhow::Result<Option<anyhow::Result<()>>> {
        let workspace = self.analyze_opts.get_tmp()?.to_path_buf();
        let mut restored = None;
        if let Some((recorded, replayed)) = &entry.recording_replays_input {
            let (recorded_path, replayed_path) =
                (workspace.join(recorded), workspace.join(replayed));
            if !recorded_path.exists() && replayed_path.exists() {
                std::fs::copy(&replayed_path, &recorded_path).with_context(|| {
                    format!(
                        "cannot restore {} from {}",
                        recorded_path.display(),
                        replayed_path.display()
                    )
                })?;
                restored = Some(recorded);
            }
        }
        for output in &entry.outputs {
            if restored == Some(&output.path) {
                continue;
            }
            match detcore::Digest::digest_path(workspace.join(&output.path)) {
                Ok(digest) if digest.to_string() == output.sha256 => {}
                _ => return Ok(None),
            }
        }
        self.logged_to = self.trial_global_opts().log_file;
        self.journaled_log_bytes = entry.log_bytes;
        eprintln!(
            ":: {} answered from the journal of an earlier invocation: {}",
            self.runname,
            match entry.end {
                RecordedEnd::Finished { matched: true } => "met the target",
                RecordedEnd::Finished { matched: false } => "missed the target",
                RecordedEnd::Timeout => "its bound stopped it",
                RecordedEnd::Refusal => "Hermit refused it",
            }
        );
        Ok(Some(match entry.end {
            RecordedEnd::Finished { matched } => {
                self.is_a_match = Some(matched);
                self.wall = entry.wall_ms.map(Duration::from_millis);
                Ok(())
            }
            RecordedEnd::Timeout => {
                Err(self.name_the_trial_bound(anyhow::Error::new(RunTimeoutMarker)))
            }
            RecordedEnd::Refusal => Err(anyhow::Error::new(crate::container::PolicyRefusal)),
        }))
    }

    /// Journal the run that `launched` as it did, keyed `key`. A run that
    /// ended any other way than finishing, its own bound or a refusal is not
    /// journaled: it ends the analysis, and a resume runs it again.
    fn journal_outcome(
        &mut self,
        journal: &Journal,
        key: String,
        launched: &anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let end = match launched {
            Ok(()) => RecordedEnd::Finished {
                matched: self.is_a_match(),
            },
            Err(error) if error.downcast_ref::<RunTimeoutMarker>().is_some() => {
                RecordedEnd::Timeout
            }
            Err(error) if guest_reached_a_refusal(error) => RecordedEnd::Refusal,
            Err(_) => return Ok(()),
        };
        let workspace = self.analyze_opts.get_tmp()?.to_path_buf();
        let relative = |path: &Path| path.strip_prefix(&workspace).unwrap_or(path).to_path_buf();
        let root = self.root_path();
        let config = &self.runopts.det_opts.det_config;
        // Not its log: a trial log can be hundreds of megabytes, and nothing
        // decides from it; its size is in `log_bytes`.
        let mut written = vec![root.with_extension("stdout"), root.with_extension("stderr")];
        written.extend(config.record_preemptions_to.clone());
        written.extend(
            config
                .stacktrace_event
                .iter()
                .filter_map(|(_, path)| path.clone()),
        );
        let mut outputs = Vec::new();
        for path in written {
            if let Ok(digest) = detcore::Digest::digest_path(&path) {
                outputs.push(RecordedFile {
                    path: relative(&path),
                    sha256: digest.to_string(),
                });
            }
        }
        let recording_replays_input =
            match (&config.record_preemptions_to, &config.replay_schedule_from) {
                (Some(recorded), Some(replayed)) if recorded.exists() && replayed.exists() => {
                    let events = |path: &Path| PreemptionReader::new(path).load_all();
                    (events(recorded).schedevents() == events(replayed).schedevents())
                        .then(|| (relative(recorded), relative(replayed)))
                }
                _ => None,
            };
        journal.record(JournalEntry {
            run: self.runname.clone(),
            key,
            end,
            wall_ms: self.wall.map(|wall| wall.as_millis() as u64),
            log_bytes: self
                .logged_to
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len()),
            outputs,
            recording_replays_input,
        })
    }

    /// Run it: the part of `launch` that starts the guest. `own_timeout` is
    /// the run's `--timeout` before the budget shortened it, which its
    /// reproducer carries.
    fn run_now(&mut self, own_timeout: Option<NonZeroU64>) -> anyhow::Result<()> {
        stop::check()?;
        let root = self.root_path();
        let gopts = self.trial_global_opts();
        // Open it HERE, on the host, for the same reason `main` does: `launch` runs
        // before the container exists, and opening later would resolve this path in
        // the guest namespace. Analyze's log lands under the run's own directory, so
        // it is only exposed when that directory is itself inside the container's
        // /tmp -- but the fix is the same and costs nothing.
        let mut gopts = gopts;
        gopts.open_log_file()?;
        self.logged_to = gopts.log_file.clone();
        let final_record_path = self
            .runopts
            .det_opts
            .det_config
            .record_preemptions_to
            .take();

        // Some last-minute sleight-of-hand to record to a temporary location instead.
        if final_record_path.is_some() {
            let temp_path = root.with_extension(PREEMPTS_EXT_PRESTRIPPED);
            self.runopts.det_opts.det_config.record_preemptions_to = Some(temp_path);
        }

        self.runopts.validate_args()?;

        self.write_repro(own_timeout)?;
        self.runopts.write_debug_options_dump()?;

        let started = Instant::now();
        let ran = self.runopts.run(&gopts, true);
        stop::check()?;
        let (_, output, terminal_outcome) = ran.map_err(|error| {
            self.run_error(self.name_the_trial_bound(error), gopts.log_file.as_deref())
        })?;
        let output: Output = output.context("expected captured output")?;

        File::create(root.with_extension("stdout"))
            .unwrap()
            .write_all(&output.stdout)
            .unwrap();
        File::create(root.with_extension("stderr"))
            .unwrap()
            .write_all(&output.stderr)
            .unwrap();

        let temp_path = self
            .runopts
            .det_opts
            .det_config
            .record_preemptions_to
            .take();
        if let Some(temp_path) = temp_path {
            Self::post_process_events_file(&temp_path, final_record_path.as_ref().unwrap())?;
            // Restore the setting:
            self.runopts.det_opts.det_config.record_preemptions_to = final_record_path;
        }

        self.is_a_match = Some(self.analyze_opts.trial_matches(&output, terminal_outcome)?);
        // Only a completed, successfully recorded run contributes a measurement.
        self.wall = Some(started.elapsed());

        if self.analyze_opts.verbose {
            println!(
                "Guest stdout:\n{}",
                String::from_utf8(output.stdout).unwrap()
            );
            println!(
                "Guest stderr:\n{}",
                String::from_utf8(output.stderr).unwrap()
            );
        }
        Ok(())
    }

    /// Write the run's reproducer, `<run>.repro`, with `own_timeout`, its
    /// `--timeout` before the budget shortened it: a command to replay it
    /// does not carry the time an invocation happened to have left.
    fn write_repro(&mut self, own_timeout: Option<NonZeroU64>) -> anyhow::Result<()> {
        let bounded = self.runopts.run_timeout_seconds();
        self.runopts.set_run_timeout(own_timeout);
        let repro = self.to_repro();
        self.runopts.set_run_timeout(bounded);
        std::fs::write(self.root_path().with_extension("repro"), repro + "\n")?;
        Ok(())
    }

    /// Bound the run's wall-clock time ([`AnalyzeOpts::trial_bound`]) by the
    /// endpoint runs measured before it was created.
    fn apply_trial_bound(&mut self) {
        let Some(bound) = self.analyze_opts.trial_bound(&self.runopts) else {
            return;
        };
        if self
            .runopts
            .run_timeout()
            .is_none_or(|own| own.as_secs() > bound.get())
        {
            self.trial_bound = Some(bound);
        }
        self.runopts.bound_run_timeout(bound);
    }

    /// A run that `--trial-timeout`'s bound ended says so, and how to change
    /// the bound: the error itself names only the run's `--timeout`, which
    /// the user did not pass.
    fn name_the_trial_bound(&self, error: anyhow::Error) -> anyhow::Error {
        match self.trial_bound {
            Some(bound) if error.downcast_ref::<RunTimeoutMarker>().is_some() => {
                error.context(format!(
                    "{} reached hermit analyze's bound of {bound} s per run \
                     (--trial-timeout=SECONDS raises it, --trial-timeout=off lifts it); \
                     no program outcome was established",
                    self.runname
                ))
            }
            _ => error,
        }
    }

    /// How long the run took and how large its log file grew, once it
    /// finished: what an endpoint run contributes to the trials' bounds.
    pub fn measurement(&self) -> Option<(Duration, Option<u64>)> {
        if !self.has_launched() {
            return None;
        }
        let wall = self.wall?;
        let log_bytes = self.journaled_log_bytes.or_else(|| {
            self.logged_to
                .as_ref()
                .and_then(|path| std::fs::metadata(path).ok())
                .map(|metadata| metadata.len())
        });
        Some((wall, log_bytes))
    }

    /// A test's trial runner ([`TrialRunner`]) ends a run as `run_now`
    /// would: its output written, whether it `matched` and its `wall` time.
    #[cfg(test)]
    pub(crate) fn finish_for_test(&mut self, matched: bool, wall: Duration) {
        let root = self.root_path();
        let said = if matched { "FAILED\n" } else { "PASSED\n" };
        std::fs::write(root.with_extension("stdout"), said).unwrap();
        std::fs::write(root.with_extension("stderr"), "").unwrap();
        self.is_a_match = Some(matched);
        self.wall = Some(wall);
    }

    /// This run's name in the workspace.
    pub fn name(&self) -> &str {
        &self.runname
    }

    /// Keep every failure typed. A policy refusal has no completed guest
    /// outcome, whether its reason arrived as context or was logged separately.
    fn run_error(&self, error: anyhow::Error, log: Option<&Path>) -> anyhow::Error {
        if error
            .downcast_ref::<crate::container::PolicyRefusal>()
            .is_none()
        {
            return error;
        }
        let diagnostics = log.map_or_else(
            || "Hermit's stderr above".to_owned(),
            |path| format!("the trial log {}", path.display()),
        );
        error.context(format!(
            "Hermit refused analysis run {}; no program outcome was established. \
             Inspect the refusal below and {diagnostics}; use a supported operation or \
             configuration before retrying.",
            self.runname,
        ))
    }

    /// Any post-processing that needs to be applied to a recorded file of SchedEvents
    pub fn post_process_events_file(sched_path: &Path, dest: &Path) -> anyhow::Result<()> {
        let mut recording = detcore::preemptions::boundary_recording::Recording::read(sched_path)?;
        recording.require_faithful_replay().map_err(|error| {
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "analysis trial at {} omitted a replay change point: {error:#}; \
                 the raw diagnostic record is retained and cannot certify an outcome",
                sched_path.display()
            ))
        })?;
        recording.strip_times();
        recording.write(dest)
    }

    fn log_level(aopts: &AnalyzeOpts) -> LevelFilter {
        let mut lvl = if let Some(l) = &aopts.guest_log {
            *l
        } else {
            LevelFilter::WARN
        };

        // Ensure a minum level for these functionalities:
        if aopts.verbose || aopts.selfcheck && lvl < LevelFilter::DEBUG {
            if aopts.guest_log.is_some() {
                eprintln!(
                    "WARNING: manually set log lvl to {} but need DEBUG for selfcheck/verbose functionality",
                    lvl
                );
            }
            lvl = LevelFilter::DEBUG;
        }

        lvl
    }

    /// A run with `runopts`, bounded by the endpoint runs measured so far
    /// ([`AnalyzeOpts::trial_bound`]).
    pub fn new(aopts: &AnalyzeOpts, runname: String, runopts: RunOpts) -> Self {
        let mut run = Self::new_unbounded(aopts, runname, runopts);
        run.apply_trial_bound();
        run
    }

    /// A run with `runopts` that no `--trial-timeout` bounds: the target run.
    fn new_unbounded(aopts: &AnalyzeOpts, runname: String, runopts: RunOpts) -> Self {
        let log_level: LevelFilter = Self::log_level(aopts);
        let mut rd = RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: None,
            log_path: None,
            in_mem_preempts_in: None,
            in_mem_sched_out: None,
            is_a_match: None,
            log_level,
            trial_bound: None,
            wall: None,
            logged_to: None,
            journaled_log_bytes: None,
        };

        // By default we save the config for every run.
        let conf_file = rd.root_path().with_extension("config");
        rd.runopts.save_config = Some(conf_file);

        let summary_file = rd.root_path().with_extension("summary");
        rd.runopts.summary_json = Some(summary_file);

        rd
    }

    /// Create a new run with the baseline RunOpts created from the AnalyzeOpts
    pub fn new_baseline(aopts: &AnalyzeOpts, runname: String) -> anyhow::Result<Self> {
        let ro = Self::get_base_runopts(aopts)?;
        Ok(Self::new(aopts, runname, ro))
    }

    /// Create the initial target attempt without an analyze trial bound.
    /// The run arguments' own timeout still applies.
    pub fn new_run1_target(aopts: &AnalyzeOpts, runname: String) -> anyhow::Result<Self> {
        let ro = Self::get_run1_runopts(aopts)?;
        Ok(Self::new_unbounded(aopts, runname, ro))
    }

    /// Replay with the selected run's seeds and the original replay policy.
    pub fn new_run1_replay(
        aopts: &AnalyzeOpts,
        runname: String,
        selected: &RunOpts,
    ) -> anyhow::Result<Self> {
        let mut replay = Self::new_run1_target(aopts, runname)?;
        let target = &selected.det_opts.det_config;
        let config = &mut replay.runopts.det_opts.det_config;
        // Search chooses an effective scheduler seed independently of the
        // initial command line. Preserve all selected seed inputs, without
        // copying search-only overrides such as --imprecise-search into replay.
        config.seed = target.seed;
        config.rng_seed = target.rng_seed;
        config.fuzz_seed = target.fuzz_seed;
        config.sched_seed = target.sched_seed;
        replay.apply_trial_bound();
        Ok(replay)
    }

    /// The baseline RunOpts based on user flags plus some sanitation/validation.
    fn get_base_runopts(aopts: &AnalyzeOpts) -> anyhow::Result<RunOpts> {
        let mut ro = Self::get_raw_runopts(aopts);
        if ro.no_sequentialize_threads {
            bail!(
                "Error, cannot search through executions with --no-sequentialize-threads.  Determinism required.",
            )
        }

        // We could add a flag for analyze-without chaos, but it's a rare use case that isn't
        // usefully supported now anyway.  Exploring with RNG alone doesn't make sense, but we may
        // want to make it possible to do analyze with the stick random scheduler instead of the
        // one.
        ro.det_opts.det_config.chaos = true;

        // Trials may differ only in their schedule. `--seed` also seeds what the guest reads
        // as randomness (getrandom, /dev/urandom) and syscall fuzzing, so a run2 seed or a
        // schedule replay at the default seed would hand the guest other inputs, and no
        // schedule could reproduce the target. Pin both to the target's seed.
        let config = &mut ro.det_opts.det_config;
        let target_seed = aopts.run1_seed.unwrap_or(config.seed);
        config.rng_seed.get_or_insert(target_seed);
        config.fuzz_seed.get_or_insert(target_seed);

        ro.validate_args()?;
        assert!(ro.det_opts.det_config.sequentialize_threads);
        if aopts.run1_seed.is_some() && !ro.det_opts.det_config.chaos {
            eprintln!(
                "{}",
                "WARNING: --chaos not in supplied hermit run args, but --run1-seed is.  Usually this is an error."
                    .bold()
                    .red()
            )
        }
        Self::runopts_add_binds(aopts, &mut ro)?;

        Ok(ro)
    }

    /// Trials write their records into the workspace from inside the
    /// container, whose `/tmp` is private: a workspace under host `/tmp` is
    /// bound into it. One elsewhere (`--tmp-dir`) is visible as it is, and a
    /// bind of it would only make every trial warn that the bind has no
    /// effect (`hermit run` honors binds under `/tmp` alone). Where the
    /// workspace is is decided on its canonical path, so a relative path or a
    /// symlink into `/tmp` is bound, and one under `/tmp` that leads out of it
    /// is not.
    fn runopts_add_binds(aopts: &AnalyzeOpts, runopts: &mut RunOpts) -> anyhow::Result<()> {
        let workspace = aopts.get_tmp()?;
        let workspace = std::fs::canonicalize(workspace)
            .or_else(|_| std::path::absolute(workspace))
            .map_err(|error| {
                anyhow::anyhow!(
                    "cannot resolve the workspace {}: {error}",
                    workspace.display()
                )
            })?;
        if workspace.starts_with("/tmp") {
            runopts
                .bind
                .push(Bind::from_str(workspace.to_str().unwrap())?);
        }
        runopts.validate_args()?;
        Ok(())
    }

    /// The run arguments as `hermit run` parses them, before analyze changes
    /// anything.
    pub fn raw_runopts(aopts: &AnalyzeOpts) -> RunOpts {
        Self::get_raw_runopts(aopts)
    }

    /// The raw, unvarnished, RunOpts.
    fn get_raw_runopts(aopts: &AnalyzeOpts) -> RunOpts {
        // Bogus arg 0 for CLI argument parsing:
        let mut run_cmd: Vec<String> = vec!["hermit-run".to_string()];

        for arg in &aopts.run_arg {
            run_cmd.push(arg.to_string());
        }
        for arg in &aopts.run_args {
            run_cmd.push(arg.to_string());
        }
        let matches = RunOpts::command()
            .try_get_matches_from(run_cmd.iter())
            .unwrap_or_else(|error| {
                if crate::misplaced_backend_argument(&error).is_some() {
                    // `--backend` is global. Point at the spelling that selects the
                    // trials' backend instead of letting clap suggest the unrelated
                    // `--backend-engagement-json`. clap reports only the flag name,
                    // so read the value from the run arguments themselves.
                    // The first `--backend` is the one clap refused; later tokens may
                    // be the guest's own arguments.
                    let value = run_cmd
                        .iter()
                        .enumerate()
                        .find_map(|(index, arg)| {
                            if arg == "--backend" {
                                Some(run_cmd.get(index + 1).map(String::as_str).unwrap_or(""))
                            } else {
                                arg.strip_prefix("--backend=")
                            }
                        })
                        .filter(|value| !value.is_empty() && !value.starts_with('-'))
                        .unwrap_or("<BACKEND>");
                    clap::Error::raw(
                        clap::error::ErrorKind::UnknownArgument,
                        format!(
                            "`--backend` is not a `run` option, so it cannot be passed through \
                         the run arguments of `hermit analyze` or `hermit bisect`. It is a \
                         global option: select the backend for every trial with `hermit \
                         --backend={value} analyze ...` or `hermit --backend={value} bisect \
                         ...`.\n"
                        ),
                    )
                    .exit()
                }
                error.exit()
            });
        let mut runopts = RunOpts::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
        runopts.capture_max_timeslice_source(&matches);
        // Apply the global backend before `get_base_runopts` validates these
        // options, so backend-specific validation sees the backend the trials
        // will run on.
        runopts.set_backend(aopts.backend);
        runopts.set_inexact_branch_counter(aopts.inexact_branch_counter);
        runopts
    }

    /// Extract the (initial) RunOpts for target/run1 that are implied by all of hermit analyze's arguments.
    fn get_run1_runopts(aopts: &AnalyzeOpts) -> anyhow::Result<RunOpts> {
        let mut ro = Self::get_base_runopts(aopts)?;

        // If there was a --sched-seed specified in run_args, it is overridden by this setting:
        if let Some(seed) = aopts.run1_seed {
            ro.det_opts.det_config.seed = seed;
        } else if let Some(path) = &aopts.run1_preemptions {
            ro.det_opts.det_config.replay_preemptions_from = Some(path.clone());
        }
        Ok(ro)
    }

    /// A temporary constructor method until minimize overhaul is complete and it returns a RunData directly.
    pub fn from_minimize_output(
        aopts: &AnalyzeOpts,
        runname: String,
        runopts: RunOpts,
        in_mem_preempts: PreemptionRecord,
        preempts_path: PathBuf,
        log_path: PathBuf,
    ) -> Self {
        let log_level: LevelFilter = Self::log_level(aopts);
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: Some(preempts_path),
            log_path: Some(log_path),
            log_level,
            in_mem_sched_out: Some(in_mem_preempts),
            in_mem_preempts_in: None,
            // Invariant: minimize should always return an on-target configuration:
            is_a_match: Some(true),
            trial_bound: None,
            wall: None,
            logged_to: None,
            journaled_log_bytes: None,
        }
    }

    /// Another fake run that stores a result without actually launching anything.
    pub fn from_schedule_trace(
        aopts: &AnalyzeOpts,
        runname: String,
        runopts: RunOpts,
        sched_path: PathBuf,
    ) -> Self {
        let log_level: LevelFilter = Self::log_level(aopts);
        RunData {
            runname,
            analyze_opts: aopts.clone(),
            runopts,
            preempts_path_in: None,
            sched_path_in: None,
            sched_path_out: Some(sched_path),
            log_path: None,
            log_level,
            in_mem_sched_out: None,
            in_mem_preempts_in: None,
            // Don't claim that it was run:
            is_a_match: None,
            trial_bound: None,
            wall: None,
            logged_to: None,
            journaled_log_bytes: None,
        }
    }

    /// Replay the preemption record at `path`, a recording: a replay that
    /// reaches a recorded slice end already passed stops, as any replay
    /// that diverges does.
    pub fn with_preempts_path_in(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.replay_preemptions_from = Some(path);
        self
    }

    /// Replay the preemption record at `path`, one analyze edited
    /// (minimization and the baseline search knock preemptions out of a
    /// recording) or one a replay of such a record met the target with. A
    /// thread can then reach recorded slices that have already ended, which
    /// the replay skips (`--replay-edited-preemptions`).
    pub fn with_edited_preempts_path_in(self, path: PathBuf) -> Self {
        let mut run = self.with_preempts_path_in(path);
        run.runopts.det_opts.det_config.replay_edited_preemptions = true;
        run
    }

    /// Replay `pr`, a record analyze edited
    /// ([`Self::with_edited_preempts_path_in`]).
    pub fn with_preempts_in(mut self, pr: PreemptionRecord) -> Self {
        let path = self.preempts_path_in().to_path_buf();
        pr.write_to_disk(&path)
            .expect("write of preempts file to succeed");
        self.in_mem_preempts_in = Some(pr);
        self.with_edited_preempts_path_in(path)
    }

    pub fn with_preemption_recording(self) -> Self {
        let path = self.out_path().with_extension(PREEMPTS_EXT);
        self.with_preemption_recording_to(path)
    }

    pub fn with_preemption_recording_to(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.record_preemptions_to = Some(path);
        self
    }

    // TODO: separate from preemption recording
    pub fn with_schedule_recording(self) -> Self {
        self.with_preemption_recording()
    }

    // TODO: separate from preemption recording
    pub fn with_schedule_recording_to(self, path: PathBuf) -> Self {
        self.with_preemption_recording_to(path)
    }

    /// Replay from the default location, as returned by sched_path_in
    pub fn with_schedule_replay(mut self) -> Self {
        let path = self.sched_path_in().to_owned();
        self.with_schedule_replay_from(path)
    }

    pub fn with_schedule_replay_from(mut self, path: PathBuf) -> Self {
        self.runopts.det_opts.det_config.replay_schedule_from = Some(path);
        self
    }

    /// Record an access trace beside the recorded schedule, leaving out
    /// records that end before event `from` and stopping after
    /// `max_records` records; with the trace's path, or the reason `hermit
    /// run` would refuse it here (a backend or mode it does not support), in
    /// which case the run goes without it.
    pub fn with_access_trace(
        mut self,
        from: u64,
        max_records: u64,
    ) -> (Self, Result<PathBuf, String>) {
        let path = self.root_path().with_extension(ACCESS_TRACE_EXT);
        let config = &mut self.runopts.det_opts.det_config;
        config.access_trace = Some(path.clone());
        config.access_trace_from = from;
        config.access_trace_max_records = Some(max_records);
        let refusal = self.runopts.clone().validate_args().err();
        let mut without = self.runopts.clone();
        without.det_opts.det_config.access_trace = None;
        without.det_opts.det_config.access_trace_from = 0;
        without.det_opts.det_config.access_trace_max_records = None;
        match refusal {
            // A run refused without the trace too fails for its own reason.
            Some(error) if without.clone().validate_args().is_ok() => {
                self.runopts = without;
                (self, Err(format!("{error:#}")))
            }
            _ => (self, Ok(path)),
        }
    }

    /// The run's reproducer: a shell line that changes to the directory the
    /// trial ran in, then runs [`Self::repro_command`].
    ///
    /// A trial without `--workdir` gives its guest Hermit's working
    /// directory, which is analyze's own; a run of the bare command from
    /// another directory would start its guest there instead
    /// (https://github.com/rrnewton/hermit/issues/4812). The `cd` keeps that
    /// inherited directory rather than passing it as `--workdir`, which
    /// would name a different one: `--workdir` resolves after the guest's
    /// mounts, so a directory under the guest's private `/tmp` does not
    /// exist there. The `cd` also holds Hermit's own relative paths (an
    /// `--image` or `--bind` source) to the trial's, so it is written even
    /// with `--workdir`. Only when analyze's directory cannot be named
    /// (removed, or not UTF-8) is the line the bare command.
    pub fn to_repro(&self) -> String {
        let dir = std::env::current_dir().ok();
        reproducer_line(dir.as_deref().and_then(Path::to_str), self.repro_command())
    }

    /// The `hermit ... run ...` command that reproduces this run, without
    /// the working directory it needs ([`Self::to_repro`]).
    pub fn repro_command(&self) -> String {
        let logging = if let Some(path) = &self.log_path {
            format!(" --log=debug --log-file={}", path.display())
        } else {
            "".to_string()
        };
        // let logging = if self.analyze_opts.verbose || self.analyze_opts.selfcheck {
        //     let path = self.log_path().unwrap();
        //     format!(" --log=debug --log-file={}", path.display())
        // } else {
        //     "".to_string()
        // };
        format!(
            "hermit{}{} run {}",
            logging,
            self.runopts.global_backend_arg(),
            self.runopts
        )
    }

    pub fn into_runopts(self) -> RunOpts {
        self.runopts
    }
}

/// `cd <dir> && <command>` with `dir` quoted for the shell, or the bare
/// command when there is no directory to name ([`RunData::to_repro`]).
fn reproducer_line(dir: Option<&str>, command: String) -> String {
    match dir {
        Some(dir) => format!("cd {} && {command}", shell_words::quote(dir)),
        None => command,
    }
}

impl AnalyzeOpts {
    /// The wall-clock bound, in whole seconds, on a run with `trial`'s
    /// options (`--trial-timeout`): none where `--timeout` cannot be
    /// enforced, and with `auto` max(60 s, 10 times the slowest endpoint run
    /// measured so far), none before one was.
    pub fn trial_bound(&self, trial: &RunOpts) -> Option<NonZeroU64> {
        if !trial.accepts_run_timeout() {
            return None;
        }
        match self.trial_timeout {
            TrialTimeout::Off => None,
            TrialTimeout::Seconds(seconds) => Some(seconds),
            TrialTimeout::Auto => {
                let bound = self
                    .endpoints
                    .slowest?
                    .saturating_mul(TRIAL_TIMEOUT_FACTOR)
                    .max(TRIAL_TIMEOUT_FLOOR);
                NonZeroU64::new(
                    bound
                        .as_secs()
                        .saturating_add(u64::from(bound.subsec_nanos() > 0)),
                )
            }
        }
    }

    /// The cap on a trial's log file while trials are bounded: max(64 MiB,
    /// 10 times the largest endpoint log), once an endpoint run has written
    /// one.
    pub fn trial_log_limit(&self) -> Option<u64> {
        if self.trial_timeout == TrialTimeout::Off {
            return None;
        }
        let largest = self.endpoints.largest_log?;
        Some(
            largest
                .saturating_mul(TRIAL_LOG_FACTOR)
                .max(TRIAL_LOG_FLOOR),
        )
    }

    /// Record a finished endpoint `run`, and say what bound the runs after it
    /// now get, when that changed.
    pub fn record_endpoint(&mut self, run: &RunData) {
        let Some((wall, log_bytes)) = run.measurement() else {
            return;
        };
        let trial = RunData::get_raw_runopts(self);
        let first = self.endpoints.slowest.is_none();
        let before = self.trial_bound(&trial);
        if !self.endpoints.record(wall, log_bytes) || self.trial_timeout != TrialTimeout::Auto {
            return;
        }
        let Some(bound) = self.trial_bound(&trial) else {
            if !first {
                return;
            }
            eprintln!(
                ":: {} the runs' backend does not enforce hermit run --timeout, so analyze \
                 does not bound them (--trial-timeout). Bound the whole analysis from outside \
                 instead, for example with timeout(1).",
                "WARNING:".yellow().bold()
            );
            return;
        };
        if before == Some(bound) {
            return;
        }
        let shorter = trial
            .run_timeout()
            .filter(|own| own.as_secs() < bound.get())
            .map_or(String::new(), |own| {
                format!(
                    "; the run arguments' shorter --timeout={} applies instead",
                    own.as_secs()
                )
            });
        eprintln!(
            ":: Runs from here on are bounded at {bound} s of wall-clock time: max(60 s, 10 x \
             the slowest endpoint run, {} at {:.1} s) (--trial-timeout{shorter}).",
            run.name(),
            wall.as_secs_f64(),
        );
    }

    /// Refuses (exit 122) an explicit `--trial-timeout=SECONDS` where the
    /// trials' backend does not enforce `hermit run --timeout`
    /// (`RunOpts::ensure_timeout_supported`), before any workspace or trial
    /// exists: the bound could not be kept.
    pub fn refuse_unenforceable_trial_timeout(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        match self.trial_timeout {
            TrialTimeout::Seconds(seconds) if !trial.accepts_run_timeout() => Err(
                anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                    "--trial-timeout={seconds} cannot be enforced: the trials run on a backend \
                     on which `hermit run --timeout` is not qualified. Use --trial-timeout=off \
                     and an outer bound (see docs/TIMEOUT_LADDER.md)."
                )),
            ),
            _ => Ok(()),
        }
    }

    /// Refuses `--max-log-bytes` (exit 122) where the cap's `_exit(123)` could
    /// leave a trial's guest running (`RunOpts::log_cap_refusal`). `run` makes
    /// this check in `RunOpts::main`, but trials go straight to `RunOpts::run`,
    /// so `analyze` and `bisect` make it here, before any workspace or trial
    /// exists.
    pub fn refuse_unsupervised_log_cap(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        trial.refuse_unsupervised_log_cap(self.max_log_bytes)
    }

    /// Refuses `--strict` in the run arguments (exit 122) where the trials'
    /// backend reads its virtual clock from a retired-branch counter that
    /// fails Reverie's validation, as `hermit run --strict` does in
    /// `RunOpts::main`. Trials go straight to `RunOpts::run`, so `analyze` and
    /// `bisect` make the check here, before any workspace or trial exists.
    /// Without it a trial on a miscounting counter can disagree with another
    /// for a hardware reason and be reported as a race
    /// (https://github.com/rrnewton/hermit/issues/3810). Unlike `run`, a trial
    /// with `--namespace-only` is not exempt: trials never reach the
    /// namespace-only launcher, so they still run on ptrace.
    pub fn refuse_strict_with_inexact_branch_counter(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).refuse_strict_trial_with_inexact_branch_counter()
    }

    /// The warning for trials that go on without `--strict` on a
    /// retired-branch counter that fails Reverie's validation, wherever
    /// [`Self::refuse_strict_with_inexact_branch_counter`] refuses `--strict`
    /// (`RunOpts::trial_inexact_branch_counter_warning`): a trial can then
    /// disagree with another for a hardware reason, and a seed, preemption
    /// record or schedule that analyze or bisect reports need not reproduce
    /// its outcome.
    pub fn trial_branch_counter_warning(&self) -> Option<String> {
        RunData::get_raw_runopts(self).trial_inexact_branch_counter_warning()
    }

    /// The branch-counter check `analyze` and `bisect` make before any
    /// workspace or trial, as `RunOpts::admit_branch_counter` does for a run:
    /// [`Self::refuse_strict_with_inexact_branch_counter`], then
    /// [`Self::trial_branch_counter_warning`] passed to `warn` (stderr in
    /// `main`), once for all the trials, and kept in
    /// `AnalyzeOpts::branch_counter_warning` for the final report.
    pub fn admit_branch_counter(&mut self, warn: &mut dyn FnMut(&str)) -> anyhow::Result<()> {
        self.refuse_strict_with_inexact_branch_counter()?;
        let warning = self.trial_branch_counter_warning();
        if let Some(warning) = &warning {
            warn(warning);
        }
        self.branch_counter_warning = warning.map(|line| line.trim_end().to_owned());
        Ok(())
    }

    /// The notice `hermit run` writes for a small `--max-timeslice`
    /// (`RunOpts::small_max_timeslice_notice`), for the run arguments every
    /// trial shares. Trials go straight to `RunOpts::run`, so `analyze` and
    /// `bisect` write it once here, after the trials' PMU configuration is
    /// installed, rather than once per trial. `skid_margin` is normally
    /// `run::installed_skid_margin`.
    pub fn small_trial_max_timeslice_notice(&self, skid_margin: fn() -> u64) -> Option<String> {
        RunData::get_raw_runopts(self).small_trial_max_timeslice_notice(skid_margin)
    }

    /// Installs the trials' `--skid-margin`, as `RunOpts::main` does for a
    /// run: Reverie's PMU configuration is per process, and every trial of
    /// one `analyze` or `bisect` runs in this process with the same run
    /// arguments. It must precede the strict counter refusal, whose
    /// validation reads that configuration. Before this, trials accepted
    /// `--skid-margin`, ran with the default margin, and printed a
    /// reproducer that applied it.
    pub fn install_trial_pmu_config(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).install_pmu_config()
    }

    /// Replay the given records from the virtual-time epoch they were
    /// recorded under, as `hermit run` does when it replays one
    /// (`RunOpts::adopt_replayed_schedule_epoch`): every trial gets that epoch
    /// as `--epoch`. A record's times are absolute virtual times measured from
    /// its own epoch, and trials start through `RunOpts::run`, which does not
    /// reconcile epochs, so without this they would start from the run
    /// arguments' epoch instead.
    ///
    /// A network trace that the run arguments replay (`--replay-networking`)
    /// is reconciled with them: its inputs are released at absolute virtual
    /// times too (https://github.com/rrnewton/hermit/issues/3875).
    ///
    /// Records stored under two different epochs, or an explicit epoch
    /// (`--epoch` or `HERMIT_EPOCH`) that differs from theirs, are refused
    /// before any trial. Records written before epochs were stored carry none
    /// and change nothing. Each recording is named by its option, such as
    /// `--good`, and its path.
    pub fn adopt_recorded_epoch(&mut self, recordings: &[(&str, &Path)]) -> anyhow::Result<()> {
        let run_cmd = std::iter::once("hermit-run").chain(
            self.run_arg
                .iter()
                .chain(&self.run_args)
                .map(String::as_str),
        );
        let matches = RunOpts::command()
            .try_get_matches_from(run_cmd)
            .context("cannot parse the trials' run arguments")?;
        let parsed = RunOpts::from_arg_matches(&matches)?;
        let mut epochs = Vec::new();
        for (option, path) in recordings {
            let epoch =
                detcore::preemptions::read_recorded_epoch(path).map_err(anyhow::Error::msg)?;
            epochs.push((format!("{option} {}", path.display()), epoch));
        }
        // A network trace given in the run arguments releases its inputs at
        // absolute virtual times too, and every trial replays it.
        if let Some(path) = parsed.replay_networking_trace() {
            let file = File::open(path)
                .with_context(|| format!("cannot open network trace {}", path.display()))?;
            let trace = detcore_model::network_trace::NetworkTraceV2::read_framed(
                std::io::BufReader::new(file),
            )
            .with_context(|| format!("{} is not a valid network trace", path.display()))?;
            epochs.push((
                format!("--replay-networking {}", path.display()),
                Some(trace.epoch),
            ));
        }
        let mut recorded = None;
        let mut sources: Vec<String> = Vec::new();
        for (source, epoch) in epochs {
            let Some(epoch) = epoch else {
                continue;
            };
            match recorded {
                None => recorded = Some(epoch),
                Some(first) if first != epoch => {
                    return Err(anyhow::Error::new(crate::container::PolicyRefusal).context(
                        format!(
                            "{} was recorded under the virtual-time epoch {} and {source} \
                             under {}. Each record's times are measured from its own epoch, \
                             so no single replay epoch reproduces both. Record both with the \
                             same --epoch.",
                            sources[0],
                            first.to_rfc3339(),
                            epoch.to_rfc3339(),
                        ),
                    ));
                }
                Some(_) => {}
            }
            sources.push(source);
        }
        let Some(recorded) = recorded else {
            return Ok(());
        };
        let recorded_text = recorded.to_rfc3339();
        if matches.value_source("epoch") == Some(ValueSource::DefaultValue) {
            self.run_arg.push(format!("--epoch={recorded_text}"));
            return Ok(());
        }
        let explicit = parsed.det_opts.det_config.epoch;
        if explicit == recorded {
            return Ok(());
        }
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "the explicit virtual-time epoch {} (from --epoch or HERMIT_EPOCH) differs from \
                 the epoch {recorded_text} that {} {} recorded under. A record's times are \
                 absolute virtual times measured from its own epoch, so replaying it from \
                 another epoch cannot reproduce the run. Omit --epoch to replay from the \
                 recorded epoch, or pass --epoch={recorded_text}.",
                explicit.to_rfc3339(),
                sources.join(" and "),
                if sources.len() == 1 { "was" } else { "were" },
            )),
        )
    }

    /// Run every trial under the `--max-timeslice` the recordings state their
    /// run drew under `--chaos-timeslice-draw=run`
    /// (`PreemptionRecord::chaos_timeslice`), as `adopt_recorded_epoch` runs
    /// them from the recorded epoch. A trial replays schedules analyze writes,
    /// which state no timeslice, so a replay that took it from its record
    /// would not find it there: the timeslice is passed as an explicit
    /// `--max-timeslice` instead.
    ///
    /// The run arguments may not draw a timeslice per run
    /// (`--chaos-timeslice-draw=run`): each trial would draw its own from its
    /// own seed, and the trials must share one. A per-slice range
    /// (`--chaos-timeslice-range` alone) is fine: every trial gets its MAX. Recordings that state different timeslices, or an explicit
    /// `--max-timeslice` that differs from theirs, are refused before any
    /// trial.
    pub fn adopt_recorded_timeslice(&mut self, recordings: &[(&str, &Path)]) -> anyhow::Result<()> {
        let run_cmd = std::iter::once("hermit-run").chain(
            self.run_arg
                .iter()
                .chain(&self.run_args)
                .map(String::as_str),
        );
        let matches = RunOpts::command()
            .try_get_matches_from(run_cmd)
            .context("cannot parse the trials' run arguments")?;
        let parsed = RunOpts::from_arg_matches(&matches)?;
        let refuse = |message: String| {
            Err(anyhow::Error::new(crate::container::PolicyRefusal).context(message))
        };
        let config = &parsed.det_opts.det_config;
        if config.chaos_timeslice_draw == detcore_model::config::ChaosTimesliceDraw::Run {
            let range = config
                .chaos_timeslice_range
                .unwrap_or(detcore_model::config::RECOMMENDED_CHAOS_TIMESLICE_RANGE);
            return refuse(format!(
                "`hermit analyze` and `hermit bisect` cannot take --chaos-timeslice-range={range} \
                 --chaos-timeslice-draw=run in their run arguments: each trial would draw another \
                 --max-timeslice from its own seed, and every trial must run under one. Pass the \
                 timeslice of the run being analyzed instead, --max-timeslice=<ns>, as its \
                 --summary-json reports it (preemption.max_timeslice_ns); a recording of it given \
                 here supplies it."
            ));
        }
        let mut recorded: Option<(u64, String)> = None;
        for (option, path) in recordings {
            let Some(timeslice) = detcore::preemptions::read_recorded_chaos_timeslice(path)
                .map_err(anyhow::Error::msg)?
            else {
                continue;
            };
            let source = format!("{option} {}", path.display());
            match &recorded {
                Some((first, first_source)) if *first != timeslice => {
                    return refuse(format!(
                        "{first_source} was recorded under a drawn --max-timeslice of {first} ns \
                         and {source} under {timeslice} ns. Every trial runs under one \
                         timeslice, so no single one replays both. Record both under the same \
                         --max-timeslice."
                    ));
                }
                Some(_) => {}
                None => recorded = Some((timeslice, source)),
            }
        }
        let Some((timeslice, source)) = recorded else {
            return Ok(());
        };
        if config.chaos_timeslice_range.is_some() {
            // Every trial replays a record or runs under the range's MAX; a
            // drawn timeslice cannot also be given beside the range.
            return refuse(format!(
                "{source} was recorded under a --max-timeslice of {timeslice} ns drawn by \
                 --chaos-timeslice-draw=run, and the run arguments give --chaos-timeslice-range, \
                 which would run the trials under another. Remove --chaos-timeslice-range from \
                 the run arguments to run them under the recorded timeslice."
            ));
        }
        if matches.value_source("max_timeslice") == Some(ValueSource::DefaultValue) {
            self.run_arg.push(format!("--max-timeslice={timeslice}"));
            return Ok(());
        }
        let explicit = config.max_timeslice;
        if explicit.map(std::num::NonZeroU64::get) == Some(timeslice) {
            return Ok(());
        }
        refuse(format!(
            "the run arguments' --max-timeslice ({}) differs from the {timeslice} ns that {source} \
             was recorded under, drawn by --chaos-timeslice-draw=run. Omit --max-timeslice to run \
             the trials under the recorded one, or pass --max-timeslice={timeslice}.",
            explicit.map_or_else(|| "disabled".to_owned(), |maximum| format!("{maximum} ns"))
        ))
    }

    /// Refuses (exit 122) a `--continuation-timeslice` that would change the
    /// replayed prefix, before any workspace or trial: one shorter than the
    /// run's `--max-timeslice`, which also ends the prefix's slices that its
    /// schedule does not end, or one for a run without PMU timeslices, whose
    /// clock it would change.
    pub fn refuse_unsound_continuation_timeslice(&self) -> anyhow::Result<()> {
        let ContinuationTimeslice::Nanos(nanos) = self.continuation_timeslice else {
            return Ok(());
        };
        let reason = match self.trial_max_timeslice() {
            None => "the run arguments disable PMU timeslices, and a continuation timeslice \
                     would turn them on for the replayed prefix too"
                .to_owned(),
            Some(run) if nanos < run => format!(
                "it is shorter than the run's --max-timeslice={run}, and the trials' timeslice \
                 also ends the replayed prefix's slices that its schedule does not end, so it \
                 would change the prefix"
            ),
            Some(_) => return Ok(()),
        };
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "--continuation-timeslice={nanos} is refused: {reason}. Omit it (auto), or pass \
                 a timeslice at least as long as the run's own."
            )),
        )
    }

    /// The trials' `--max-timeslice`, from their run arguments (`None` for
    /// disabled).
    pub fn trial_max_timeslice(&self) -> detcore_model::config::MaybeTimeslice {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .max_timeslice
    }

    /// The trials' `--pipe-wake-latency`, from their run arguments (`None`
    /// for off).
    pub fn trial_pipe_wake_latency(&self) -> Option<u64> {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .pipe_wake_latency
    }

    /// The timing settings every trial keeps from the run arguments: the
    /// recording's timing semantics, under which a prefix search's
    /// scheduler-alone continuation runs too, each with the other value a
    /// run can take. `--pipe-wake-latency` is also stamped on each record;
    /// `--scheduler-turn-cost` changes how far virtual time moves per
    /// scheduler turn. Settings that change no thread's timing (the
    /// signal-state copy) are left out.
    pub fn continuation_timing(&self) -> Vec<TimingSetting> {
        let latency = self.trial_pipe_wake_latency().map(|nanos| TimingSetting {
            option: format!("--pipe-wake-latency={nanos}"),
            alternative: "no --pipe-wake-latency".to_owned(),
            drops: "--pipe-wake-latency",
            adds: None,
        });
        // A turn cost equal to the default charge times nothing differently.
        let turn_cost = self
            .trial_scheduler_turn_cost()
            .filter(|cost| cost.get() as f64 != detcore_model::time::NANOS_PER_SCHED)
            .map(|cost| TimingSetting {
                option: format!("--scheduler-turn-cost={cost}"),
                alternative: "no --scheduler-turn-cost".to_owned(),
                drops: "--scheduler-turn-cost",
                adds: None,
            });
        latency.into_iter().chain(turn_cost).collect()
    }

    /// The trials' `--scheduler-turn-cost`, from their run arguments (`None`
    /// for the default charge).
    pub fn trial_scheduler_turn_cost(&self) -> Option<std::num::NonZeroU64> {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .scheduler_turn_cost
    }

    /// Whether the trials keep the signal-state copy (off with
    /// `--no-signal-state-copy` in their run arguments).
    pub fn trial_signal_state_copy(&self) -> bool {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .signal_state_copy
    }

    /// The trials' `--expired-wait-charge`, from their run arguments, where
    /// [`Self::adopt_expired_wait_charge`] puts it; `each` before then, the
    /// rule of the `--chaos` runs analyze makes.
    pub fn trial_expired_wait_charge(&self) -> ExpiredWaitCharge {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .expired_wait_charge
            .unwrap_or(ExpiredWaitCharge::Each)
    }

    /// Run every trial under one `--expired-wait-charge`: the run arguments'
    /// own, or else the rule of the `target` recording (`--run1-preemptions`
    /// for analyze, `--bad` for bisect), or else `each`, the rule of the
    /// `--chaos` target run analyze makes. A search mixes the target's
    /// schedule with a baseline's recorded without `--chaos`, so the two must
    /// not charge expired waits differently, and a failure is searched under
    /// the rule that found it. The value is added to the run arguments, as
    /// [`Self::adopt_recorded_epoch`] adds the epoch, so trial commands and
    /// reproducers carry it; recordings under another rule are then refused
    /// ([`Self::refuse_recordings_under_another_pipe_wake_latency`]).
    pub fn adopt_expired_wait_charge(&mut self, target: Option<&Path>) -> anyhow::Result<()> {
        if RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .expired_wait_charge
            .is_some()
        {
            return Ok(());
        }
        let once = match target {
            Some(path) => detcore::preemptions::read_recorded_expired_wait_runs_charged_once(path)
                .map_err(anyhow::Error::msg)?,
            None => false,
        };
        self.run_arg.push(format!(
            "--expired-wait-charge={}",
            ExpiredWaitCharge::from_charged_once(once).as_str()
        ));
        Ok(())
    }

    /// `record` stamped with the trials' `--pipe-wake-latency`, signal-state
    /// copy setting and `--expired-wait-charge`, as every record analyze
    /// writes must be: a record that omitted one would be accepted when
    /// replayed under another value
    /// (<https://github.com/rrnewton/hermit/issues/4380>).
    pub fn stamp_trial_record(&self, record: PreemptionRecord) -> PreemptionRecord {
        record
            .with_pipe_wake_latency(self.trial_pipe_wake_latency())
            .with_signal_state_copy(Some(self.trial_signal_state_copy()))
            .with_expired_wait_runs_charged_once(
                self.trial_expired_wait_charge() == ExpiredWaitCharge::Once,
            )
            .with_cap_preemption_charge(self.trial_cap_preemption_charge())
    }

    /// Whether the trials cap preemption charges (`--cap-preemption-charge`
    /// in their run arguments).
    pub fn trial_cap_preemption_charge(&self) -> bool {
        RunData::get_raw_runopts(self)
            .det_opts
            .det_config
            .cap_preemption_charge
    }

    /// Write the schedule `events` for a trial to replay, stamped
    /// ([`Self::stamp_trial_record`]). The events alone carry no callback
    /// checkpoints: a schedule analyze edits (a swap, a reordering) has no
    /// recorded slice ends to keep.
    pub fn write_trial_schedule(&self, events: Vec<SchedEvent>, path: &Path) -> anyhow::Result<()> {
        self.write_trial_record(PreemptionRecord::from_sched_events(events), path)
    }

    /// Write `record` for a trial to replay, stamped
    /// ([`Self::stamp_trial_record`]). The record carries the jumps of global
    /// time of the recorded schedule whose event prefix its events repeat
    /// furthest (`crate::time_skip_carry`), so its replay jumps where that
    /// recording did. All sources must share one timing format; mixing
    /// formats cannot preserve their different switch-in charging rules.
    /// An incompatible format or invalid record is refused before writing.
    pub fn write_trial_record(&self, record: PreemptionRecord, path: &Path) -> anyhow::Result<()> {
        let (skips, format, identity) = {
            let sources = self.time_skip_sources.lock().unwrap();
            (
                sources.carried(record.schedevents()),
                sources.trial_format()?,
                sources.trial_identity_metadata(record.schedevents())?,
            )
        };
        let (guest_ids, reserved_input) = identity.unwrap_or_else(|| {
            (
                record.guest_ids().unwrap_or_default(),
                record.reserved_input().clone(),
            )
        });
        if matches!(
            record.guest_ids(),
            Some(detcore_model::pid::GuestIds::Chosen { .. })
        ) {
            record
                .reserved_input()
                .require_same_for_chosen(&reserved_input)?;
        }
        let record = self.stamp_trial_record(
            record
                .with_format(format)
                .with_time_skips(skips)
                .with_guest_ids(guest_ids)
                .with_reserved_input(reserved_input),
        );
        record.validate().map_err(anyhow::Error::msg)?;
        record.write_to_disk(path).map_err(anyhow::Error::msg)
    }

    /// Refuse, before any trial, recordings made under another
    /// `--pipe-wake-latency` than the trials' own, including a different
    /// latency (https://github.com/rrnewton/hermit/issues/4380). Trials start
    /// through `RunOpts::run`, which skips `hermit run`'s own check, so the
    /// check is made here, as `adopt_recorded_epoch` makes the epoch's. Besides
    /// the named `recordings`, a record the run arguments replay
    /// (`--replay-schedule-from`, `--replay-preemptions-from`) is checked.
    /// A record without the field was made with the option off. The same
    /// recordings are checked for the signal-state copy setting
    /// (`--no-signal-state-copy`), which a record without that field does not
    /// state, so it is accepted under either, and for the trials'
    /// `--expired-wait-charge` ([`Self::adopt_expired_wait_charge`]), which a
    /// record without that field states as `each`.
    pub fn refuse_recordings_under_another_pipe_wake_latency(
        &self,
        recordings: &[(&str, &Path)],
    ) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        let config = &trial.det_opts.det_config;
        let in_run_arguments = [
            (
                "--replay-schedule-from",
                config.replay_schedule_from.as_deref(),
            ),
            (
                "--replay-preemptions-from",
                config.replay_preemptions_from.as_deref(),
            ),
        ];
        for (option, path) in recordings.iter().copied().chain(
            in_run_arguments
                .into_iter()
                .filter_map(|(option, path)| Some((option, path?))),
        ) {
            detcore::preemptions::boundary_recording::Recording::read(path)?
                .require_faithful_replay()
                .map_err(|error| {
                    anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                        "{option} {} cannot supply analysis evidence: {error:#}",
                        path.display()
                    ))
                })?;
            let recorded = detcore::preemptions::read_recorded_pipe_wake_latency(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::pipe_wake_latency_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.pipe_wake_latency,
            ) {
                return Err(anyhow::Error::new(crate::container::PolicyRefusal).context(refusal));
            }
            let recorded = detcore::preemptions::read_recorded_signal_state_copy(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::signal_state_copy_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.signal_state_copy,
            ) {
                return Err(anyhow::Error::new(crate::container::PolicyRefusal).context(refusal));
            }
            let recorded = detcore::preemptions::read_recorded_expired_wait_runs_charged_once(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::expired_wait_charge_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                Some(self.trial_expired_wait_charge()),
                true,
            ) {
                return Err(anyhow::Error::new(crate::container::PolicyRefusal).context(refusal));
            }
            let recorded = detcore::preemptions::read_recorded_cap_preemption_charge(path)
                .map_err(anyhow::Error::msg)?;
            if let Some(refusal) = detcore::preemptions::cap_preemption_charge_mismatch(
                &format!("{option} {}", path.display()),
                recorded,
                config.cap_preemption_charge,
            ) {
                return Err(anyhow::Error::new(crate::container::PolicyRefusal).context(refusal));
            }
        }
        Ok(())
    }

    /// The virtual-time epoch the target trial (run 1) starts from, in
    /// RFC 3339. It replays `--run1-preemptions` when that is given.
    #[cfg(test)]
    pub(crate) fn trial_epoch_for_test(&self) -> String {
        RunData::new_run1_target(self, "epoch-probe".to_owned())
            .unwrap()
            .runopts
            .det_opts
            .det_config
            .epoch
            .to_rfc3339()
    }

    /// Refuses (exit 122) `--save-config` and `--summary-json` in the run
    /// arguments, before any workspace, schedule read or trial exists. Every
    /// trial writes its configuration and summary into the workspace under
    /// its own name (`RunData::new` replaces both paths), so the requested
    /// files would never be written. `--record-networking` is refused too:
    /// it would put every trial on the host network and have every trial
    /// write the same trace. So are the options `RunOpts::options_trials_refuse`
    /// lists: host-network and namespace-less routes, and output paths every
    /// trial would share.
    pub fn refuse_outputs_trials_overwrite(&self) -> anyhow::Result<()> {
        let trial = RunData::get_raw_runopts(self);
        // A recording runs on the host network (`RunOpts::validate_args`), so
        // the trials would stop being reproducible, and every trial would
        // write the same trace path.
        if let Some(path) = trial.record_networking_trace() {
            return Err(
                anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                    "`hermit analyze` and `hermit bisect` refuse --record-networking={} in the \
                     run arguments: it puts every trial on the host network, so the trials \
                     are no longer reproducible, and every trial writes that same trace, so \
                     only the last one would remain. Record the network once with `hermit run \
                     --record-networking`, then pass the trace to the trials with \
                     --replay-networking.",
                    path.display()
                )),
            );
        }
        let refused = trial.options_trials_refuse();
        if !refused.is_empty() {
            return Err(
                anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                    "`hermit analyze` and `hermit bisect` refuse {} in the run arguments: {}. \
                     Remove {} from the run arguments.",
                    refused
                        .iter()
                        .map(|(option, _)| *option)
                        .collect::<Vec<_>>()
                        .join(" and "),
                    refused
                        .iter()
                        .map(|(option, reason)| format!("with {option}, {reason}"))
                        .collect::<Vec<_>>()
                        .join("; "),
                    if refused.len() == 1 { "it" } else { "them" },
                )),
            );
        }
        let overwritten: Vec<&str> = [
            (trial.save_config.is_some(), "--save-config"),
            (trial.summary_json.is_some(), "--summary-json"),
        ]
        .into_iter()
        .filter_map(|(set, option)| set.then_some(option))
        .collect();
        if overwritten.is_empty() {
            return Ok(());
        }
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "`hermit analyze` and `hermit bisect` write each trial's configuration and summary \
             into their workspace, so {} would never be written. Remove {} from the run \
             arguments; the workspace keeps both files for every trial.",
                overwritten.join(" and "),
                if overwritten.len() == 1 { "it" } else { "them" },
            )),
        )
    }

    /// Refuses `--timeout` (exit 122) on a trial backend where `hermit run`
    /// refuses it (`RunOpts::ensure_timeout_supported`), before any workspace,
    /// schedule read or trial exists. Trials never pass through
    /// `RunOpts::main`, which makes that check for a run, so a KVM trial
    /// accepted the bound that its printed `hermit --backend=kvm run
    /// --timeout=N` reproducer is refused for.
    pub fn refuse_unqualified_trial_timeout(&self) -> anyhow::Result<()> {
        RunData::get_raw_runopts(self).ensure_timeout_supported()
    }

    /// Refuses (exit 122) run arguments that a trial would accept and run
    /// without (`RunOpts::options_only_main_applies`), before any workspace,
    /// schedule read or trial exists. Accepting them would run every trial
    /// without them and print a reproducer that applies them, so the
    /// reproducer would not reproduce the trial.
    pub fn refuse_options_trials_do_not_apply(&self) -> anyhow::Result<()> {
        let dropped = RunData::get_raw_runopts(self).options_only_main_applies();
        if dropped.is_empty() {
            return Ok(());
        }
        Err(
            anyhow::Error::new(crate::container::PolicyRefusal).context(format!(
                "`hermit analyze` and `hermit bisect` cannot apply {} to their trials: each trial \
             starts through `RunOpts::run`, and only `hermit run` itself applies {}. Remove {} \
             from the run arguments, or run the program with `hermit run`.",
                dropped.join(", "),
                if dropped.len() == 1 { "it" } else { "them" },
                if dropped.len() == 1 { "it" } else { "them" },
            )),
        )
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    /// The records analyze edited (minimization's and the baseline search's)
    /// replay with `--replay-edited-preemptions`, and so do the repro
    /// commands it prints for them; a recording replays strictly. Before, a
    /// minimization trial whose knock-out let a thread start a slice past
    /// its recorded end stopped the whole analysis with Detcore's "Cannot
    /// set end of timeslice" panic (exit 101).
    #[test]
    fn analyze_replays_only_its_edited_preemption_records_as_edited() {
        let args = crate::Args::try_parse_from(["hermit", "analyze", "--", "/bin/true"]).unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("not analyze")
        };
        options.apply_global(&args.global);
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let record = workspace.path().join("round_001.preempts");
        let edited = RunData::new_baseline(&options, "round_001".to_string())
            .unwrap()
            .with_edited_preempts_path_in(record.clone());
        assert!(edited.runopts.det_opts.det_config.replay_edited_preemptions);
        let repro = edited.to_repro();
        assert!(repro.contains(" --replay-edited-preemptions"), "{repro}");
        // A record analyze builds in memory (the baseline search's
        // knock-outs) is written to the workspace and replayed as edited.
        let knocked_out = RunData::new_baseline(&options, "round_002".to_string())
            .unwrap()
            .with_preempts_in(PreemptionRecord::from_vecs(
                &std::collections::BTreeMap::new(),
            ));
        let config = &knocked_out.runopts.det_opts.det_config;
        assert!(config.replay_edited_preemptions);
        assert!(config.replay_preemptions_from.as_ref().unwrap().is_file());
        let recording = RunData::new_baseline(&options, "verify".to_string())
            .unwrap()
            .with_preempts_path_in(record);
        assert!(
            !recording
                .runopts
                .det_opts
                .det_config
                .replay_edited_preemptions
        );
        assert!(
            !recording.to_repro().contains("--replay-edited-preemptions"),
            "{}",
            recording.to_repro()
        );
    }

    /// The actual writer preserves one format's jumps/charging and refuses
    /// mixed formats before touching its destination, even for an exact prefix.
    #[test]
    fn trial_record_writing_preserves_one_timing_format_and_refuses_mixed_sources() {
        use detcore::preemptions::TimeSkip;
        use detcore::preemptions::TimeSkipReason;
        use detcore::types::DetTid;
        use detcore::types::LogicalTime;

        let events = vec![SchedEvent::branches(DetTid::from_raw(3), 1)];
        let record = |format| {
            let record = PreemptionRecord::from_sched_events(events.clone()).with_format(format);
            if format.is_some() {
                record.with_time_skips(vec![TimeSkip {
                    event: 1,
                    target: LogicalTime::from_nanos(1000),
                    reason: TimeSkipReason::IdleGuestPollers,
                }])
            } else {
                record
            }
        };
        let options = || {
            let args =
                crate::Args::try_parse_from(["hermit", "analyze", "--", "/bin/true"]).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("trial.json");
        for format in [None, Some(2), Some(3)] {
            let options = options();
            let source = record(format);
            options.time_skip_sources.lock().unwrap().remember(&source);
            options
                .write_trial_schedule(events.clone(), &output)
                .unwrap();
            let written = PreemptionReader::try_new(&output).unwrap().into_inner();
            assert_eq!(written.format_version(), format);
            assert_eq!(written.time_skips(), source.time_skips());
            assert_eq!(written.charges_replayed_switch_ins(), format == Some(3));
        }
        for other in [None, Some(2)] {
            for formats in [[Some(3), other], [other, Some(3)]] {
                let options = options();
                for format in formats {
                    options
                        .time_skip_sources
                        .lock()
                        .unwrap()
                        .remember(&record(format));
                }
                std::fs::write(&output, b"existing output must survive refusal").unwrap();
                let error = options
                    .write_trial_schedule(events.clone(), &output)
                    .unwrap_err();
                let message = error.to_string();
                assert!(message.contains("different timing formats"), "{message}");
                assert!(message.contains("record both inputs"), "{message}");
                assert_eq!(
                    std::fs::read(&output).unwrap(),
                    b"existing output must survive refusal"
                );
            }
        }
    }

    /// Analyze and bisect run every trial under the timeslice a recording says
    /// its run drew (`--chaos-timeslice-draw=run`): an explicit
    /// `--max-timeslice` passed to the trials, an equal explicit one kept, a
    /// different one refused. Run arguments that draw per run are refused,
    /// and a per-slice range beside a drawn recording is refused too.
    #[test]
    fn trials_run_under_the_timeslice_a_recording_drew() {
        let options = |run_args: &[&str]| {
            let mut argv: Vec<String> = vec!["hermit".into(), "analyze".into()];
            argv.extend(run_args.iter().map(|arg| format!("--run-arg={arg}")));
            argv.extend(["--".into(), "/bin/true".into()]);
            let args = crate::Args::try_parse_from(&argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            *options
        };
        let dir = tempfile::tempdir().unwrap();
        let drawn = dir.path().join("drawn.json");
        detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_chaos_timeslice(Some(4321))
            .write_to_disk(&drawn)
            .unwrap();
        let other = dir.path().join("other.json");
        detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .with_chaos_timeslice(Some(9999))
            .write_to_disk(&other)
            .unwrap();
        let plain = dir.path().join("plain.json");
        detcore::preemptions::PreemptionRecord::from_sched_events(Vec::new())
            .write_to_disk(&plain)
            .unwrap();
        let recordings = [("--run1-preemptions", drawn.as_path())];

        let mut adopting = options(&[]);
        adopting.adopt_recorded_timeslice(&recordings).unwrap();
        assert!(
            adopting
                .run_arg
                .iter()
                .any(|arg| arg == "--max-timeslice=4321"),
            "{:?}",
            adopting.run_arg
        );
        let mut equal = options(&["--max-timeslice=4321"]);
        equal.adopt_recorded_timeslice(&recordings).unwrap();
        assert_eq!(
            equal.run_arg,
            ["--max-timeslice=4321"],
            "added a second one"
        );
        let mut unmarked = options(&[]);
        unmarked
            .adopt_recorded_timeslice(&[("--run1-preemptions", plain.as_path())])
            .unwrap();
        assert!(
            unmarked
                .run_arg
                .iter()
                .all(|arg| !arg.starts_with("--max-timeslice"))
        );
        // A per-slice range alone is fine without a drawn recording.
        let mut slices = options(&["--chaos", "--chaos-timeslice-range=1000..2000"]);
        slices
            .adopt_recorded_timeslice(&[("--run1-preemptions", plain.as_path())])
            .unwrap();

        for (run_args, recordings, expected) in [
            (
                &["--chaos", "--chaos-timeslice-draw=run"][..],
                &[][..],
                "every trial must run under one",
            ),
            (
                &["--max-timeslice=5000"][..],
                &recordings[..],
                "pass --max-timeslice=4321",
            ),
            (
                &[],
                &[
                    ("--run1-preemptions", drawn.as_path()),
                    ("--run2-preemptions", other.as_path()),
                ][..],
                "Record both under the same",
            ),
            (
                &[
                    "--chaos",
                    "--chaos-timeslice-range=1000..2000",
                    "--chaos-timeslice-draw=run",
                ],
                &[],
                "every trial must run under one",
            ),
            (
                &["--chaos", "--chaos-timeslice-range=1000..2000"],
                &recordings[..],
                "Remove --chaos-timeslice-range",
            ),
        ] {
            let error = options(run_args)
                .adopt_recorded_timeslice(recordings)
                .unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
    }

    /// Round-2 review of https://github.com/rrnewton/hermit/pull/3686, finding
    /// 2: the refusal `RunOpts::main` makes never sees an analyze or bisect
    /// trial, which goes straight to `RunOpts::run`. Those commands admit
    /// ptrace and KVM, and `--no-namespace`, passed through the run arguments,
    /// is refused with either (round-4c review, finding 1).
    #[test]
    fn log_cap_refusal_reaches_analyze_and_bisect_trials() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let kvm = "--backend=kvm and --no-namespace";
        for (refused, named) in [
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "--backend=kvm",
                    "analyze",
                    "--run-arg=--no-namespace",
                    "--",
                    "/bin/true",
                ][..],
                kvm,
            ),
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "--backend=kvm",
                    "analyze",
                    "--",
                    "--no-namespace",
                    "/bin/true",
                ][..],
                kvm,
            ),
            // The default (ptrace) backend: its tracer sets PTRACE_O_EXITKILL
            // only after the trial's guest exists.
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "analyze",
                    "--run-arg=--no-namespace",
                    "--",
                    "/bin/true",
                ][..],
                "--no-namespace",
            ),
            (
                &[
                    "hermit",
                    "--max-log-bytes=4K",
                    "analyze",
                    "--",
                    "--no-namespace",
                    "/bin/true",
                ][..],
                "--no-namespace",
            ),
        ] {
            let error = options(refused)
                .refuse_unsupervised_log_cap()
                .expect_err("the trial's guest could outlive hermit");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{refused:?}: {error:#}"
            );
            assert!(
                error.to_string().starts_with(&format!(
                    "--max-log-bytes cannot be enforced with {named}: "
                )),
                "{refused:?}: {error:#}"
            );
        }
        for accepted in [
            &[
                "hermit",
                "--backend=kvm",
                "analyze",
                "--run-arg=--no-namespace",
                "--",
                "/bin/true",
            ][..],
            &[
                "hermit",
                "--max-log-bytes=4K",
                "--backend=kvm",
                "analyze",
                "--",
                "/bin/true",
            ][..],
            &[
                "hermit",
                "analyze",
                "--run-arg=--no-namespace",
                "--",
                "/bin/true",
            ][..],
            &["hermit", "--max-log-bytes=4K", "analyze", "--", "/bin/true"][..],
        ] {
            options(accepted)
                .refuse_unsupervised_log_cap()
                .unwrap_or_else(|error| panic!("{accepted:?}: {error:#}"));
        }
    }

    /// https://github.com/rrnewton/hermit/issues/3810: `hermit run --strict`
    /// refuses an inexact retired-branch counter in `RunOpts::main`, which
    /// analyze and bisect trials never reach, so the trial options make the
    /// same refusal, with the same conditions: `--strict` in either spelling of
    /// the run arguments, a backend that reads that counter, and a failed
    /// validation.
    #[test]
    fn strict_trials_refuse_an_inexact_branch_counter() {
        let options = |argv: &[&str], probe: fn() -> Option<String>| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options.inexact_branch_counter = probe;
            options
        };
        let inexact = || Some("SpecLockMap is enabled".to_string());
        for refused in [
            &["hermit", "analyze", "--run-arg=--strict", "--", "/bin/true"][..],
            &["hermit", "analyze", "--", "--strict", "/bin/true"][..],
            &[
                "hermit",
                "--backend=ptrace",
                "analyze",
                "--",
                "--strict",
                "/bin/true",
            ][..],
            // `RunOpts::run` ignores `--namespace-only`, so these trials still
            // run on ptrace and its counter (rel-041 review of
            // https://github.com/rrnewton/hermit/pull/3834).
            &[
                "hermit",
                "analyze",
                "--",
                "--strict",
                "--namespace-only",
                "/bin/true",
            ][..],
            &[
                "hermit",
                "analyze",
                "--run-arg=--strict",
                "--run-arg=--lite",
                "--",
                "/bin/true",
            ][..],
        ] {
            let error = options(refused, inexact)
                .refuse_strict_with_inexact_branch_counter()
                .expect_err("a strict trial on an inexact counter");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{refused:?}: {error:#}"
            );
            let message = error.to_string();
            for expected in ["--strict", "`ptrace` backend", "SpecLockMap is enabled"] {
                assert!(
                    message.contains(expected),
                    "{refused:?}: {expected:?}: {message}"
                );
            }
        }
        let unreachable: fn() -> Option<String> = || panic!("the counter is not consulted");
        for (accepted, probe) in [
            (&["hermit", "analyze", "--", "/bin/true"][..], unreachable),
            (
                &[
                    "hermit",
                    "--backend=kvm",
                    "analyze",
                    "--",
                    "--strict",
                    "/bin/true",
                ][..],
                unreachable,
            ),
            (
                &["hermit", "analyze", "--", "--strict", "/bin/true"][..],
                || None,
            ),
        ] {
            options(accepted, probe)
                .refuse_strict_with_inexact_branch_counter()
                .unwrap_or_else(|error| panic!("{accepted:?}: {error:#}"));
        }
    }

    /// Without `--strict`, analyze and bisect trials on a counter that fails
    /// Reverie's validation go on, and `admit_branch_counter` writes one
    /// warning line for all of them and keeps it, without its newline, for the
    /// final report (`branch_counter_warning`). With `--strict` they are
    /// refused and nothing is written. A counter that passed, a backend that
    /// does not read it, or trials without a timeslice write nothing: the
    /// warning goes exactly where the `--strict` refusal would.
    #[test]
    fn trials_without_strict_warn_once_of_an_inexact_branch_counter() {
        let options = |argv: &[&str], probe: fn() -> Option<String>| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options.inexact_branch_counter = probe;
            options
        };
        let admit = |options: &mut AnalyzeOpts| {
            let mut lines = Vec::new();
            let result =
                options.admit_branch_counter(&mut |line: &str| lines.push(line.to_owned()));
            (result, lines)
        };
        let inexact: fn() -> Option<String> = || Some("SpecLockMap is enabled".to_string());
        let warning = crate::run::warning_for_an_inexact_branch_counter(
            crate::run::CounterReader::Trials,
            inexact,
        )
        .unwrap();
        for warned in [
            &["hermit", "analyze", "--", "/bin/true"][..],
            &["hermit", "--backend=ptrace", "analyze", "--", "/bin/true"][..],
        ] {
            let mut options = options(warned, inexact);
            let (result, lines) = admit(&mut options);
            assert!(result.is_ok(), "{warned:?}: {result:?}");
            assert_eq!(lines, std::slice::from_ref(&warning), "{warned:?}");
            assert_eq!(
                options.branch_counter_warning.as_deref(),
                Some(warning.trim_end()),
                "{warned:?}"
            );
        }

        let mut strict = options(
            &["hermit", "analyze", "--", "--strict", "/bin/true"],
            inexact,
        );
        let (result, lines) = admit(&mut strict);
        assert!(
            result.is_err_and(|error| {
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some()
            }),
            "--strict refuses"
        );
        assert!(lines.is_empty(), "{lines:?}");
        assert_eq!(strict.branch_counter_warning, None);

        let unreachable: fn() -> Option<String> = || panic!("the counter is not consulted");
        for (silent, probe) in [
            (
                &["hermit", "analyze", "--", "/bin/true"][..],
                (|| None) as fn() -> _,
            ),
            (
                &["hermit", "--backend=kvm", "analyze", "--", "/bin/true"][..],
                unreachable,
            ),
            (
                &[
                    "hermit",
                    "analyze",
                    "--",
                    "--max-timeslice=disabled",
                    "/bin/true",
                ][..],
                unreachable,
            ),
        ] {
            let mut options = options(silent, probe);
            let (result, lines) = admit(&mut options);
            assert!(
                result.is_ok() && lines.is_empty(),
                "{silent:?}: {result:?} {lines:?}"
            );
            assert_eq!(options.branch_counter_warning, None, "{silent:?}");
        }
    }

    /// Trials start through `RunOpts::run`, so run options that only
    /// `RunOpts::main` applies are refused, each named, instead of being
    /// dropped from every trial while the printed reproducer keeps them.
    /// Options the trials do apply are admitted.
    #[test]
    fn trials_refuse_run_options_only_main_applies() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv)
                .unwrap_or_else(|error| panic!("{argv:?} should parse: {error}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        for (run_args, named) in [
            (&["--namespace-only"][..], &["--namespace-only"][..]),
            (&["--lite"][..], &["--namespace-only"][..]),
            (&["--verify"][..], &["--verify"][..]),
            (&["--verify", "--verify-strict"][..], &["--verify"][..]),
            (&["--verify-run=vr"][..], &["--verify-run"][..]),
            (
                &["--backend-engagement-json=b.json"][..],
                &["--backend-engagement-json"][..],
            ),
            (
                &["--verify", "--happens-before=hb.txt"][..],
                &["--verify", "--happens-before"][..],
            ),
            (
                &["--cpu-profile", "x86-64-v3"][..],
                &["--cpu-profile x86-64-v3"][..],
            ),
        ] {
            let mut argv = vec!["hermit", "analyze", "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            let error = options(&argv)
                .refuse_options_trials_do_not_apply()
                .expect_err("a dropped run option");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{argv:?}: {error:#}"
            );
            let message = error.to_string();
            for option in named {
                assert!(message.contains(option), "{argv:?}: {option}: {message}");
            }
        }
        for run_args in [
            &[][..],
            &["--strict"][..],
            &["--no-namespace"][..],
            &["--timeout=5"][..],
            &["--skid-margin=500"][..],
            &["--summary"][..],
        ] {
            let mut argv = vec!["hermit", "analyze", "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            options(&argv)
                .refuse_options_trials_do_not_apply()
                .unwrap_or_else(|error| panic!("{argv:?}: {error:#}"));
        }
    }

    /// rel-041's review of https://github.com/rrnewton/hermit/pull/3836: an
    /// explicit `--save-config` or `--summary-json` is refused, each named,
    /// because every trial writes both into the workspace under its own
    /// name. Without them a trial still gets those workspace paths.
    #[test]
    fn trials_refuse_outputs_they_overwrite() {
        let options = |run_args: &[&str]| {
            let mut argv = vec!["analyze", "--"];
            argv.extend(run_args);
            argv.push("/bin/true");
            AnalyzeOpts::try_parse_from(&argv).unwrap()
        };
        for (run_args, named) in [
            (
                &["--save-config=requested.config"][..],
                &["--save-config"][..],
            ),
            (
                &["--summary-json=requested.summary"][..],
                &["--summary-json"][..],
            ),
            (
                &["--save-config=c", "--summary-json=s"][..],
                &["--save-config", "--summary-json"][..],
            ),
        ] {
            let error = options(run_args)
                .refuse_outputs_trials_overwrite()
                .expect_err("an overwritten output");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{run_args:?}: {error:#}"
            );
            let message = error.to_string();
            for option in named {
                assert!(
                    message.contains(option),
                    "{run_args:?}: {option}: {message}"
                );
            }
        }
        let mut admitted = options(&[]);
        admitted.refuse_outputs_trials_overwrite().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        admitted.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&admitted, "outputs".to_owned()).unwrap();
        for path in [&run.runopts.save_config, &run.runopts.summary_json] {
            let path = path.as_ref().expect("the trial writes it");
            assert!(path.starts_with(workspace.path()), "{}", path.display());
        }
        // A network recording would put every trial on the host network.
        let error = options(&["--record-networking=requested.trace"])
            .refuse_outputs_trials_overwrite()
            .expect_err("a network recording");
        assert!(
            error
                .downcast_ref::<crate::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        let message = error.to_string();
        assert!(
            message.contains("refuse --record-networking=requested.trace")
                && message.contains("host network")
                && message.contains("--replay-networking"),
            "{message}"
        );
        // Host-network and namespace-less routes, and output paths every
        // trial would share (`RunOpts::options_trials_refuse`).
        for (run_arg, named) in [
            ("--strace-only", "--strace-only"),
            ("--no-sequentialize-threads", "--no-sequentialize-threads"),
            ("--network=host", "--network=host"),
            ("--no-namespace", "--no-namespace"),
            ("--gdbserver", "--gdbserver"),
            ("--dap-listen=127.0.0.1:0", "--dap-listen"),
            (
                "--record-preemptions-to=requested.preempts",
                "--record-preemptions-to",
            ),
            (
                "--stacktrace-event=5,requested.stack",
                "--stacktrace-event with a path",
            ),
            (
                "--preemption-stacktrace-log-file=requested.log",
                "--preemption-stacktrace-log-file",
            ),
            (
                "--record-access-trace-to=requested.access",
                "--record-access-trace-to",
            ),
        ] {
            let error = options(&[run_arg])
                .refuse_outputs_trials_overwrite()
                .expect_err("an option the trials cannot take");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{run_arg}: {error:#}"
            );
            let message = error.to_string();
            assert!(
                message.contains(&format!("refuse {named} in the run arguments"))
                    && message.contains(&format!("with {named}, ")),
                "{run_arg}: {message}"
            );
        }
        // A stack-trace event without a path prints to stderr, and local
        // networking is the default: both stay admitted.
        for admitted in ["--stacktrace-event=5", "--network=local"] {
            options(&[admitted])
                .refuse_outputs_trials_overwrite()
                .unwrap_or_else(|error| panic!("{admitted}: {error:#}"));
        }
    }

    /// rel-041's review of https://github.com/rrnewton/hermit/pull/3836:
    /// `--timeout` on a trial backend where `hermit run` refuses it (KVM) is
    /// refused the same way, while the ptrace trial keeps it.
    #[test]
    fn trials_refuse_a_timeout_their_backend_cannot_enforce() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let error = options(&[
            "hermit",
            "--backend=kvm",
            "analyze",
            "--",
            "--timeout=3",
            "/bin/true",
        ])
        .refuse_unqualified_trial_timeout()
        .expect_err("KVM does not enforce --timeout");
        assert!(
            error
                .downcast_ref::<crate::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("--timeout is not qualified"),
            "{error:#}"
        );
        for admitted in [
            &["hermit", "analyze", "--", "--timeout=3", "/bin/true"][..],
            &[
                "hermit",
                "--backend=ptrace",
                "analyze",
                "--",
                "--timeout=3",
                "/bin/true",
            ][..],
            &["hermit", "--backend=kvm", "analyze", "--", "/bin/true"][..],
        ] {
            options(admitted)
                .refuse_unqualified_trial_timeout()
                .unwrap_or_else(|error| panic!("{admitted:?}: {error:#}"));
        }
    }

    /// Through `main`: the dropped-option, overwritten-output and
    /// unqualified-timeout refusals are in analyze's preflight, before the
    /// workspace and every trial. `--run1-schedule` stops a `main` that passed
    /// them, with its own refusal right after that preflight, so each case
    /// asserts which refusal fired and that it was not that one (see the
    /// strict counter test below).
    #[test]
    fn trials_refuse_run_options_only_main_applies_before_their_workspace() {
        for (backend, extra, named) in [
            (None, "--namespace-only", "--namespace-only"),
            (None, "--lite", "--namespace-only"),
            (None, "--verify", "--verify"),
            (None, "--save-config=requested.config", "--save-config"),
            (None, "--summary-json=requested.summary", "--summary-json"),
            (
                Some("--backend=kvm"),
                "--timeout=3",
                "--timeout is not qualified",
            ),
            (
                None,
                "--record-networking=requested.trace",
                "refuse --record-networking=requested.trace",
            ),
            (
                None,
                "--strace-only",
                "refuse --strace-only in the run arguments",
            ),
            (
                None,
                "--no-sequentialize-threads",
                "refuse --no-sequentialize-threads in the run arguments",
            ),
            (
                None,
                "--network=host",
                "refuse --network=host in the run arguments",
            ),
            (
                None,
                "--no-namespace",
                "refuse --no-namespace in the run arguments",
            ),
            (
                None,
                "--gdbserver",
                "refuse --gdbserver in the run arguments",
            ),
            (
                None,
                "--dap-listen=127.0.0.1:0",
                "refuse --dap-listen in the run arguments",
            ),
            (
                None,
                "--record-preemptions-to=requested.preempts",
                "refuse --record-preemptions-to in the run arguments",
            ),
            (
                None,
                "--stacktrace-event=5,requested.stack",
                "refuse --stacktrace-event with a path in the run arguments",
            ),
            (
                None,
                "--preemption-stacktrace-log-file=requested.log",
                "refuse --preemption-stacktrace-log-file in the run arguments",
            ),
        ] {
            let mut argv = vec!["hermit"];
            argv.extend(backend);
            argv.extend([
                "analyze",
                "--run1-schedule=/nonexistent/schedule.json",
                "--",
                extra,
                "/bin/true",
            ]);
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            let error = options.main(&args.global).unwrap_err();
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{argv:?}: {error:#}"
            );
            let message = error.to_string();
            assert!(message.contains(named), "{argv:?}: {error:#}");
            assert!(
                !message.contains("does not implement"),
                "{argv:?}: main passed the preflight refusal: {error:#}"
            );
        }
    }

    /// Analyze's trials apply `--skid-margin`: its preflight records it as the
    /// skid margin of Reverie's per-process PMU configuration, so a later
    /// margin is refused. `main` refuses the unimplemented `--run1-schedule`
    /// right after that preflight, which stops it before any workspace. That
    /// configuration is per-process, so the test runs in a process of its own.
    #[test]
    fn analyze_installs_the_trials_skid_margin() {
        if crate::test_isolation::rerun_alone() {
            return;
        }
        let argv = [
            "hermit",
            "analyze",
            "--run1-schedule=/nonexistent/schedule.json",
            "--",
            "--skid-margin=4321",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        // The refusal that follows the preflight, not an earlier one.
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("hermit analyze does not implement --run1-schedule"),
            "main stopped before reaching the --run1-schedule refusal: {error:#}"
        );
        // A second margin is refused once one is recorded. Neither recording
        // reads the host's CPU, so this holds on a host Reverie has no PMU
        // profile for, such as a GitHub-hosted runner's Emerald Rapids.
        assert!(
            reverie_ptrace::set_skid_margin_override(1).is_err(),
            "analyze did not install the trials' --skid-margin"
        );
    }

    /// The printed reproducer carries `--timeout`, which every trial applies
    /// (`run_in_container` reads it), so the reproducer is bounded as the
    /// trial was.
    /// A reproducer's directory is quoted for the shell: one with spaces and
    /// an apostrophe splits back to itself, and `sh` lands in it.
    #[test]
    fn a_reproducers_directory_is_quoted_for_the_shell() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("it's a dir");
        std::fs::create_dir(&dir).unwrap();
        let dir = dir.to_str().unwrap();
        let line = reproducer_line(Some(dir), "pwd -P".to_owned());
        let words = shell_words::split(&line).unwrap();
        assert_eq!(words[..3], ["cd", dir, "&&"], "{line}");
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&line)
            .current_dir(parent.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            Path::new(String::from_utf8(output.stdout).unwrap().trim()),
            Path::new(dir).canonicalize().unwrap()
        );
        assert_eq!(reproducer_line(None, "hermit run".to_owned()), "hermit run");
    }

    /// A reproducer changes to the directory its trial ran in before it runs
    /// the command, so the guest starts in the trial's working directory
    /// wherever the line is run (#4812). It does so with `--workdir` too,
    /// which leaves Hermit's own relative paths depending on its directory.
    #[test]
    fn analyzer_reproducer_starts_in_the_trials_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "cwd-repro".to_owned()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        let line = run.to_repro();
        let words = shell_words::split(&line).unwrap();
        assert_eq!(words[..3], ["cd", cwd.to_str().unwrap(), "&&"], "{line}");
        assert_eq!(
            words[3..],
            shell_words::split(&run.repro_command()).unwrap()[..],
            "{line}"
        );

        // Run from another directory, the line's own `cd` lands where the
        // trial ran.
        let elsewhere = tempfile::tempdir().unwrap();
        let (cd, _) = line.split_once(" && ").unwrap();
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{cd} && pwd -P"))
            .current_dir(elsewhere.path())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            Path::new(String::from_utf8(output.stdout).unwrap().trim()),
            cwd.canonicalize().unwrap()
        );

        let mut pinned =
            AnalyzeOpts::try_parse_from(["analyze", "--", "--workdir=/", "/bin/true"]).unwrap();
        pinned.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&pinned, "workdir-repro".to_owned()).unwrap();
        let line = run.to_repro();
        let words = shell_words::split(&line).unwrap();
        assert_eq!(words[..3], ["cd", cwd.to_str().unwrap(), "&&"], "{line}");
        assert!(words.contains(&"--workdir=/".to_owned()), "{line}");
    }

    #[test]
    fn analyzer_reproducer_retains_the_trial_timeout() {
        use clap::CommandFactory;
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--", "--timeout=7", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "timeout-repro".to_owned()).unwrap();
        let repro = run.repro_command();
        assert_eq!(repro.matches(" --timeout=7").count(), 1, "{repro}");
        let matches = crate::Args::command()
            .try_get_matches_from(shell_words::split(&repro).unwrap())
            .unwrap();
        let args = crate::args_from_matches_with_clock(&matches, || {
            panic!("analyzer reproduction must retain its actual fixed input")
        })
        .unwrap();
        let crate::Subcommand::Run(mut parsed) = args.command else {
            panic!("expected run")
        };
        parsed.validate_args().unwrap();
        assert_eq!(parsed.to_string(), run.runopts.to_string());
    }

    /// Through `main`: the refusal must be wired into analyze's preflight,
    /// ahead of the workspace and every trial. `--run1-schedule` is not
    /// implemented and `main` refuses it right after that preflight, so an
    /// analyze that passed the counter refusal stops at that refusal instead,
    /// before starting anything; the message assertion below tells them apart.
    /// A trial with `--namespace-only` or `--lite` is refused earlier in that
    /// preflight, for a dropped option
    /// (`trials_refuse_run_options_only_main_applies_before_their_workspace`);
    /// `strict_trials_refuse_an_inexact_branch_counter` keeps it covered at
    /// the counter refusal itself.
    #[test]
    fn strict_analyze_refuses_an_inexact_branch_counter_before_its_workspace() {
        let argv = [
            "hermit",
            "analyze",
            "--run1-schedule=/nonexistent/schedule.json",
            "--",
            "--strict",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.inexact_branch_counter = || Some("SpecLockMap is enabled".to_string());
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::container::PolicyRefusal>()
                .is_some(),
            "{error:#}"
        );
        assert!(
            error.to_string().contains("SpecLockMap is enabled"),
            "{error:#}"
        );
    }

    /// `run` has no `--backend`, so `hermit --backend <BACKEND> analyze` is the
    /// only way to choose the trials' backend. The selection must pass the
    /// scope check and reach the options every trial is launched with: the
    /// `GlobalOpts` that `RunOpts::main` takes its backend from, the validated
    /// trial `RunOpts`, and the printed reproducer.
    /// `hermit --max-log-bytes=N analyze` caps its trials with the
    /// invocation's shared budget. A trial without --verbose/--selfcheck traces
    /// to hermit's stderr, so an exempt trial would put uncapped output on the
    /// public stream (review of https://github.com/rrnewton/hermit/pull/3686,
    /// round 1).
    #[test]
    fn analyze_trials_charge_the_invocations_log_budget() {
        let argv = ["hermit", "--max-log-bytes=4K", "analyze", "--", "/bin/true"];
        let mut args = crate::Args::try_parse_from(argv).unwrap();
        args.global.prepare_log_budget().unwrap();
        let crate::Subcommand::Analyze(mut options) = args.command else {
            panic!("{argv:?} is not analyze")
        };
        options.apply_global(&args.global);
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let run = RunData::new_baseline(&options, "budget".to_owned()).unwrap();
        let trial = run.trial_global_opts();
        assert!(trial.log_file.is_none(), "this trial traces to stderr");
        assert_eq!(trial.max_log_bytes, Some(4096));
        let invocation = args.global.log_budget().expect("the invocation is capped");
        let charged = trial.log_budget().expect("the trial must be capped");
        assert!(
            charged.shares_counter_with(&invocation),
            "the trial must charge the invocation's counter, not a fresh one"
        );
    }

    #[test]
    fn global_backend_reaches_every_trial_run() {
        for flag in ["ptrace", "kvm"] {
            let argv = [
                "hermit".to_owned(),
                format!("--backend={flag}"),
                "analyze".to_owned(),
                "--".to_owned(),
                "/bin/true".to_owned(),
            ];
            let args = crate::Args::try_parse_from(&argv).unwrap();
            args.command
                .validate_backend_scope(args.global.backend)
                .unwrap_or_else(|error| panic!("{argv:?} must be in scope: {error:#}"));
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let run = RunData::new_baseline(&options, "backend".to_owned())
                .unwrap_or_else(|error| panic!("{flag}: {error:#}"));
            assert_eq!(
                run.trial_global_opts().backend,
                args.global.backend,
                "{flag}"
            );
            assert_eq!(
                run.runopts.global_backend_arg(),
                format!(" --backend={flag}")
            );
            let repro = run.repro_command();
            assert!(
                repro.contains(&format!(" --backend={flag} run ")),
                "{flag}: reproducer does not select the backend: {repro}"
            );
        }
    }

    #[test]
    fn trial_preemption_keeps_the_explicit_maximum_request() {
        for maximum in [
            None,
            Some("--max-timeslice=disabled"),
            Some("--max-timeslice=200000000"),
        ] {
            let mut argv = vec!["hermit", "--backend=kvm", "analyze", "--"];
            argv.extend(maximum);
            argv.push("/bin/true");
            let args = crate::Args::try_parse_from(argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("expected analyze")
            };
            options.apply_global(&args.global);
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let result = RunData::new_baseline(&options, "preemption".to_owned());
            if maximum == Some("--max-timeslice=200000000") {
                let error = result.err().expect("explicit maximum must refuse");
                assert!(
                    error
                        .downcast_ref::<crate::container::PolicyRefusal>()
                        .is_some()
                );
                assert!(
                    format!("{error:#}")
                        .contains("unsupported_preemption: --backend=kvm refuses --max-timeslice")
                );
            } else {
                assert_eq!(
                    result
                        .unwrap()
                        .runopts
                        .det_opts
                        .det_config
                        .max_timeslice
                        .map(u64::from),
                    if maximum.is_none() {
                        Some(200_000_000)
                    } else {
                        None
                    }
                );
            }
        }
    }

    #[test]
    fn trials_bind_only_a_workspace_the_private_tmp_would_hide() {
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--run1-seed=6",
            "--",
            "--epoch=2026-01-01T00:00:00Z",
            "/bin/true",
        ])
        .unwrap();
        let binds = |options: &AnalyzeOpts| -> Vec<String> {
            RunData::new_baseline(options, "trial".to_owned())
                .unwrap()
                .runopts
                .bind
                .iter()
                .map(|bind| bind.target.to_string_lossy().into_owned())
                .collect()
        };

        options.tmp_dir = Some(PathBuf::from("/tmp/hermit_analyzeXYZ"));
        assert_eq!(binds(&options), ["/tmp/hermit_analyzeXYZ"]);

        options.tmp_dir = Some(PathBuf::from("someone/analyze-ws/hermit_analyzeXYZ"));
        assert!(binds(&options).is_empty(), "visible as it is");

        // Where the workspace really is decides. A relative path to a
        // directory under /tmp is bound, as its canonical path.
        let under_tmp = tempfile::Builder::new()
            .prefix("hermit_analyze_bind")
            .tempdir_in("/tmp")
            .unwrap();
        let canonical = std::fs::canonicalize(under_tmp.path()).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert!(canonical.starts_with("/tmp"), "{}", canonical.display());
        let relative = PathBuf::from("../".repeat(cwd.components().count() - 1))
            .join(canonical.strip_prefix("/").unwrap());
        assert!(relative.is_relative());
        options.tmp_dir = Some(relative);
        assert_eq!(binds(&options), [canonical.to_str().unwrap()]);
        // A symlink under /tmp that leads out of it is not: the workspace is
        // visible where it really is.
        let link = canonical.join("out-of-tmp");
        std::os::unix::fs::symlink("/", &link).unwrap();
        options.tmp_dir = Some(link);
        assert!(binds(&options).is_empty(), "{:?}", binds(&options));
    }

    #[test]
    fn every_trial_hands_the_guest_the_target_seed_inputs() {
        let mut options = AnalyzeOpts::try_parse_from([
            "analyze",
            "--run1-seed=6",
            "--run2-seed=1",
            "--",
            "--chaos",
            "--epoch=2026-01-01T00:00:00Z",
            "/bin/true",
        ])
        .unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());

        let target = RunData::new_run1_target(&options, "target".to_owned()).unwrap();
        let target = &target.runopts.det_opts.det_config;
        assert_eq!(
            (target.sched_seed(), target.rng_seed()),
            (6, 6),
            "the failing run as found"
        );

        // A baseline or a schedule replay changes the schedule seed only.
        let mut baseline = RunData::new_baseline(&options, "baseline".to_owned()).unwrap();
        baseline.runopts.det_opts.det_config.seed = 1;
        let baseline = &baseline.runopts.det_opts.det_config;
        assert_eq!(baseline.sched_seed(), 1);
        assert_eq!((baseline.rng_seed(), baseline.fuzz_seed()), (6, 6));

        let mut explicit = options.clone();
        explicit.run_args = [
            "--chaos",
            "--rng-seed=9",
            "--epoch=2026-01-01T00:00:00Z",
            "/bin/true",
        ]
        .map(String::from)
        .to_vec();
        let replay = RunData::new_baseline(&explicit, "replay".to_owned()).unwrap();
        assert_eq!(
            replay.runopts.det_opts.det_config.rng_seed(),
            9,
            "an explicit --rng-seed in the run arguments is kept"
        );
    }

    #[test]
    fn analyzer_reproducer_retains_actual_default_and_fractional_epoch() {
        use clap::CommandFactory;
        for epoch in ["2026-01-01T00:00:00Z", "2000-12-31T23:59:59.123456789Z"] {
            let mut options = AnalyzeOpts::try_parse_from([
                "analyze",
                "--",
                "--epoch",
                epoch,
                "--seed=71",
                "/bin/true",
            ])
            .unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let run = RunData::new_baseline(&options, "epoch-repro".to_owned()).unwrap();
            let repro = run.repro_command();
            assert_eq!(repro.matches("--epoch=").count(), 1, "{repro}");
            let matches = crate::Args::command()
                .try_get_matches_from(shell_words::split(&repro).unwrap())
                .unwrap();
            let args = crate::args_from_matches_with_clock(&matches, || {
                panic!("analyzer reproduction must retain its actual fixed input")
            })
            .unwrap();
            let crate::Subcommand::Run(parsed) = args.command else {
                panic!("expected run")
            };
            assert_eq!(
                parsed.det_opts.det_config.epoch,
                run.runopts.det_opts.det_config.epoch
            );
            assert_eq!(
                parsed.det_opts.det_config.seed,
                run.runopts.det_opts.det_config.seed
            );
        }
    }

    #[test]
    fn preemption_replay_preserves_selected_seeds_and_original_timer_policy() {
        for explicit_imprecise_timers in [false, true] {
            let mut args = vec![
                "analyze",
                "--selfcheck",
                "--search",
                "--imprecise-search",
                "--run1-seed=211",
                "--",
            ];
            if explicit_imprecise_timers {
                args.push("--imprecise-timers");
            }
            args.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(args).unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            let mut selected = RunData::new_baseline(&options, "selected".to_owned()).unwrap();
            let selected_config = &mut selected.runopts.det_opts.det_config;
            selected_config.seed = 101;
            selected_config.rng_seed = Some(102);
            selected_config.fuzz_seed = Some(103);
            selected_config.sched_seed = Some(104);
            // This is the override launch_search applies, not a replay option.
            selected_config.imprecise_timers = true;
            let preempts = workspace.path().join("selected.preempts");
            let replay = RunData::new_run1_replay(&options, "replay".to_owned(), &selected.runopts)
                .unwrap()
                .with_preempts_path_in(preempts.clone())
                .with_preemption_recording();
            let config = &replay.runopts.det_opts.det_config;
            assert_eq!(config.seed, 101);
            assert_eq!(config.rng_seed, Some(102));
            assert_eq!(config.fuzz_seed, Some(103));
            assert_eq!(config.sched_seed, Some(104));
            assert_eq!(config.imprecise_timers, explicit_imprecise_timers);
            assert_eq!(config.replay_preemptions_from.as_ref(), Some(&preempts));
            assert_ne!(replay.runopts.save_config, selected.runopts.save_config);
            assert_ne!(replay.runopts.summary_json, selected.runopts.summary_json);
            assert_eq!(
                config.record_preemptions_to.as_ref(),
                Some(&workspace.path().join("replay_out.preempts")),
            );
        }
    }

    #[test]
    fn a_refused_run_keeps_its_type_reason_and_log_location() {
        use crate::container::LogCapExceeded;
        use crate::container::PolicyRefusal;
        use crate::container::RunTimeoutMarker;

        for (verbose, selfcheck) in [(false, false), (true, false), (false, true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
            options.tmp_dir = Some(dir.path().to_path_buf());
            options.verbose = verbose;
            options.selfcheck = selfcheck;
            let run = RunData::new_baseline(&options, "refused_trial".to_owned()).unwrap();
            let mut global = run.trial_global_opts();
            global.open_log_file().unwrap();
            for error in [
                anyhow::Error::new(PolicyRefusal),
                anyhow::Error::new(PolicyRefusal).context("unsupported operation: restart_syscall"),
                anyhow::Error::msg("unsupported operation: restart_syscall").context(PolicyRefusal),
                anyhow::Error::new(PolicyRefusal).context("unsupported configuration at admission"),
            ] {
                let reason = format!("{error:#}");
                let error = run.run_error(error, global.log_file.as_deref());
                assert!(error.downcast_ref::<PolicyRefusal>().is_some());
                let message = format!("{error:#}");
                assert!(message.contains(&reason), "{message}");
                assert!(message.contains("Hermit refused analysis run refused_trial"));
                assert!(message.contains("no program outcome was established"));
                assert!(
                    message.contains("use a supported operation or configuration before retrying")
                );
                if let Some(path) = &global.log_file {
                    assert!(path.exists());
                    assert!(message.contains(&format!("the trial log {}", path.display())));
                } else {
                    assert!(message.contains("Hermit's stderr above"));
                }
                assert!(!run.has_launched());
            }
            for error in [
                anyhow::Error::new(RunTimeoutMarker),
                anyhow::Error::new(LogCapExceeded),
            ] {
                let before = format!("{error:#}");
                let error = run.run_error(error, global.log_file.as_deref());
                assert_eq!(format!("{error:#}"), before);
                assert!(
                    error.downcast_ref::<RunTimeoutMarker>().is_some()
                        || error.downcast_ref::<LogCapExceeded>().is_some()
                );
            }
        }
    }

    /// A trial writes the schedule or the preemption record it replays into
    /// the workspace, never into a file the run arguments name
    /// (`--replay-schedule-from`, `--replay-preemptions-from`): that file is
    /// the user's own and stays as it was.
    #[test]
    fn a_trial_writes_what_it_replays_only_into_the_workspace() {
        for option in ["--replay-schedule-from", "--replay-preemptions-from"] {
            let outside = tempfile::tempdir().unwrap();
            let users = outside.path().join("users.preempts");
            // Content no trial writes, so that a write into it shows.
            PreemptionRecord::from_sched_events(Vec::new())
                .with_pipe_wake_latency(Some(1))
                .write_to_disk(&users)
                .unwrap();
            let before = std::fs::read(&users).unwrap();
            let replay = format!("{option}={}", users.display());
            let mut options =
                AnalyzeOpts::try_parse_from(["analyze", "--", replay.as_str(), "/bin/true"])
                    .unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            // As the bisection builds and writes a trial (`event_level_search`).
            let mut trial = RunData::new_baseline(&options, "bisect_round_001".to_owned())
                .unwrap()
                .with_schedule_replay()
                .with_schedule_recording();
            let schedule = trial.sched_path_in().to_path_buf();
            assert_eq!(schedule, workspace.path().join("bisect_round_001.preempts"));
            assert_eq!(
                trial.runopts.det_opts.det_config.replay_schedule_from,
                Some(schedule.clone())
            );
            options.write_trial_schedule(Vec::new(), &schedule).unwrap();
            // As the check of the critical preemption builds its run.
            let check = RunData::new_baseline(&options, "verify".to_owned())
                .unwrap()
                .with_preempts_in(PreemptionRecord::from_sched_events(Vec::new()));
            assert_eq!(
                check.runopts.det_opts.det_config.replay_preemptions_from,
                Some(workspace.path().join("verify.preempts"))
            );
            assert_eq!(
                std::fs::read(&users).unwrap(),
                before,
                "a trial wrote into {option}'s file"
            );
        }
    }
    /// `--trial-timeout=auto` bounds a run at max(60 s, 10 times the
    /// slowest endpoint run measured so far), rounded up to whole seconds,
    /// and gives no bound before an endpoint was measured; a number of
    /// seconds bounds every run but the target run, and `off` none.
    #[test]
    fn the_trial_bound_derives_from_the_slowest_endpoint_run() {
        use std::time::Duration;
        let options = |extra: &[&str]| {
            let mut argv = vec!["analyze"];
            argv.extend(extra);
            argv.extend(["--", "/bin/true"]);
            AnalyzeOpts::try_parse_from(argv).unwrap()
        };
        let mut auto = options(&[]);
        let trial = RunData::get_raw_runopts(&auto);
        assert_eq!(auto.trial_bound(&trial), None, "nothing measured yet");
        assert!(auto.endpoints.record(Duration::from_secs(2), None));
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(60), "the floor");
        assert!(auto.endpoints.record(Duration::from_millis(30_150), None));
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(302));
        assert!(
            !auto.endpoints.record(Duration::from_secs(1), None),
            "a faster endpoint changes nothing"
        );
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(302));
        auto.endpoints.record(Duration::MAX, None);
        assert_eq!(auto.trial_bound(&trial), NonZeroU64::new(u64::MAX));

        let explicit = options(&["--trial-timeout=5"]);
        assert_eq!(
            explicit.trial_bound(&trial),
            NonZeroU64::new(5),
            "an explicit bound needs no measurement"
        );
        let mut off = options(&["--trial-timeout=off"]);
        off.endpoints.record(Duration::from_secs(30), None);
        assert_eq!(off.trial_bound(&trial), None);
        for refused in ["--trial-timeout=0", "--trial-timeout=soon"] {
            let error = AnalyzeOpts::try_parse_from(["analyze", refused, "--", "/bin/true"])
                .expect_err(refused);
            assert!(
                error.to_string().contains("positive number of seconds"),
                "{refused}: {error}"
            );
        }
    }

    /// A run created after an endpoint run was measured runs under the
    /// bound, or under the run arguments' own `--timeout` where that is
    /// shorter, and so does its printed reproducer; the target run is not
    /// bounded.
    #[test]
    fn runs_take_the_bound_and_the_target_run_does_not() {
        let created = |run_args: &[&str], explicit: Option<&str>| {
            let mut argv = vec!["analyze"];
            argv.extend(explicit);
            argv.push("--");
            argv.extend(run_args);
            argv.push("/bin/true");
            let mut options = AnalyzeOpts::try_parse_from(argv).unwrap();
            let workspace = tempfile::tempdir().unwrap();
            options.tmp_dir = Some(workspace.path().to_path_buf());
            options
                .endpoints
                .record(std::time::Duration::from_secs(10), None);
            let trial = RunData::new_baseline(&options, "bounded".to_owned()).unwrap();
            let target = RunData::new_run1_target(&options, "target".to_owned()).unwrap();
            let seconds = |run: &RunData| run.runopts.run_timeout().map(|bound| bound.as_secs());
            assert_eq!(
                trial.to_repro().contains(" --timeout="),
                seconds(&trial).is_some(),
                "{}",
                trial.to_repro()
            );
            (seconds(&trial), seconds(&target), trial.trial_bound)
        };
        assert_eq!(created(&[], None), (Some(100), None, NonZeroU64::new(100)));
        assert_eq!(
            created(&[], Some("--trial-timeout=5")),
            (Some(5), None, NonZeroU64::new(5)),
            "an explicit trial bound also leaves the initial target attempt alone"
        );
        assert_eq!(created(&["--timeout=30"], None), (Some(30), Some(30), None));
        assert_eq!(
            created(&["--timeout=300"], None),
            (Some(100), Some(300), NonZeroU64::new(100))
        );
        assert_eq!(
            created(&[], Some("--trial-timeout=off")),
            (None, None, None)
        );
    }

    /// A run that the bound ended says which bound it was and how to change
    /// it; one that its own `--timeout` ended, or that failed otherwise, is
    /// left as it was.
    #[test]
    fn a_run_the_trial_bound_ends_names_the_bound() {
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        options
            .endpoints
            .record(std::time::Duration::from_secs(1), None);
        let run = RunData::new_baseline(&options, "run2_baseline".to_owned()).unwrap();
        let timed_out = run.name_the_trial_bound(anyhow::Error::new(RunTimeoutMarker));
        assert!(timed_out.downcast_ref::<RunTimeoutMarker>().is_some());
        let message = format!("{timed_out:#}");
        assert!(
            message.starts_with(
                "run2_baseline reached hermit analyze's bound of 60 s per run \
                 (--trial-timeout=SECONDS raises it, --trial-timeout=off lifts it)"
            ),
            "{message}"
        );
        let other = run.name_the_trial_bound(anyhow::anyhow!("no such file"));
        assert_eq!(format!("{other:#}"), "no such file");
    }

    /// While trials are bounded, a trial's log file is capped at
    /// max(64 MiB, 10 times the largest endpoint log); before an endpoint
    /// wrote one, or with `--trial-timeout=off`, by HERMIT_LOG_MAX_BYTES alone.
    #[test]
    fn a_trials_log_is_capped_by_the_largest_endpoint_log() {
        use std::time::Duration;
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        assert_eq!(options.trial_log_limit(), None);
        options
            .endpoints
            .record(Duration::from_secs(1), Some(1 << 20));
        assert_eq!(options.trial_log_limit(), Some(64 << 20));
        options
            .endpoints
            .record(Duration::from_secs(1), Some(100 << 20));
        assert_eq!(options.trial_log_limit(), Some(1000 << 20));
        options
            .endpoints
            .record(Duration::from_secs(1), Some(2 << 20));
        assert_eq!(options.trial_log_limit(), Some(1000 << 20));
        let trial = RunData::new_baseline(&options, "bounded_log".to_owned()).unwrap();
        assert_eq!(trial.trial_global_opts().log_file_limit, Some(1000 << 20));
        options.trial_timeout = TrialTimeout::Off;
        assert_eq!(options.trial_log_limit(), None);
        let trial = RunData::new_baseline(&options, "unbounded_log".to_owned()).unwrap();
        assert_eq!(trial.trial_global_opts().log_file_limit, None);
    }

    /// An explicit `--trial-timeout` that the runs cannot keep (KVM, which
    /// does not enforce `--timeout`, and the in-guest runtime, which refuses
    /// one) is refused before any workspace or run; `auto` does without it.
    #[test]
    fn an_unenforceable_trial_timeout_is_refused_and_auto_does_without() {
        let options = |argv: &[&str]| {
            let args = crate::Args::try_parse_from(argv).unwrap();
            let crate::Subcommand::Analyze(mut options) = args.command else {
                panic!("{argv:?} is not analyze")
            };
            options.apply_global(&args.global);
            options
        };
        let mut backends = vec!["kvm"];
        if cfg!(feature = "liteinst") {
            backends.push("liteinst");
        }
        for backend in backends {
            let backend = format!("--backend={backend}");
            let explicit = options(&[
                "hermit",
                &backend,
                "analyze",
                "--trial-timeout=5",
                "--",
                "/bin/true",
            ]);
            let error = explicit
                .refuse_unenforceable_trial_timeout()
                .expect_err("the runs cannot keep a --timeout");
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{backend}: {error:#}"
            );
            assert!(error.to_string().contains("--trial-timeout=5"), "{error:#}");

            let mut auto = options(&["hermit", &backend, "analyze", "--", "/bin/true"]);
            auto.refuse_unenforceable_trial_timeout().unwrap();
            auto.endpoints
                .record(std::time::Duration::from_secs(10), None);
            assert_eq!(
                auto.trial_bound(&RunData::get_raw_runopts(&auto)),
                None,
                "{backend}"
            );
        }
        options(&["hermit", "analyze", "--trial-timeout=5", "--", "/bin/true"])
            .refuse_unenforceable_trial_timeout()
            .unwrap();
    }
    /// The stack-trace replay the report takes must give the observed
    /// completed predicate, including a guest's nonzero exit, even when neither
    /// diagnostic stack is available: the report compares that outcome with
    /// the search's, and ends the analysis when they differ.
    #[test]
    fn final_replays_require_the_observed_completed_outcome() {
        use reverie::process::ExitStatus;

        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        for actual in [false, true] {
            let replay = options
                .replay_for_stacks(
                    "completed_final",
                    &dir.path().join("candidate.json"),
                    &[],
                    1,
                    None,
                    |run| {
                        assert!(!run.has_launched());
                        let output = Output {
                            status: ExitStatus::Exited(if actual { 122 } else { 0 }),
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                        };
                        // Model the completed observation made by launch. This
                        // module can access the private state without adding a
                        // product API that could manufacture completion.
                        run.is_a_match = Some(options.output_matches(&output));
                        assert_eq!(run.is_a_match(), actual);
                        // A completed replay records the schedule it ran.
                        PreemptionRecord::from_sched_events(Vec::new())
                            .write_to_disk(run.sched_path_out())
                            .map_err(anyhow::Error::msg)?;
                        Ok(())
                    },
                )
                .expect("a completed replay gives its outcome");
            assert_eq!(replay.met_target, actual);
        }
    }

    #[test]
    fn only_completed_runs_contribute_endpoint_measurements() {
        let dir = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.path().to_path_buf());
        let mut run = RunData::new_run1_target(&options, "initial".to_owned()).unwrap();
        assert_eq!(run.measurement(), None);
        // Even a partial timing measurement must not make an unfinished run
        // eligible. Both completed predicates can contribute measurements.
        run.wall = Some(Duration::from_secs(10));
        options.record_endpoint(&run);
        assert_eq!(options.endpoints.slowest, None);
        run.is_a_match = Some(false);
        options.record_endpoint(&run);
        assert_eq!(options.endpoints.slowest, Some(Duration::from_secs(10)));
        let earlier = RunData::new_baseline(&options, "earlier".to_owned()).unwrap();
        run.is_a_match = Some(true);
        run.wall = Some(Duration::from_secs(20));
        options.record_endpoint(&run);
        assert_eq!(options.endpoints.slowest, Some(Duration::from_secs(20)));
        let later = RunData::new_baseline(&options, "later".to_owned()).unwrap();
        assert_eq!(
            earlier.runopts.run_timeout(),
            Some(Duration::from_secs(100))
        );
        assert_eq!(later.runopts.run_timeout(), Some(Duration::from_secs(200)));
    }

    /// A `--continuation-timeslice` shorter than the run's `--max-timeslice`,
    /// or one for a run without PMU timeslices, is refused before any
    /// workspace or trial, naming what to pass instead; one at least as long
    /// is accepted.
    #[test]
    fn a_continuation_timeslice_that_would_change_the_prefix_is_refused() {
        let refusal = |argv: &[&str]| {
            let mut full = vec!["analyze", "--prefix-search"];
            full.extend(argv);
            full.push("/bin/true");
            AnalyzeOpts::try_parse_from(full)
                .unwrap()
                .refuse_unsound_continuation_timeslice()
        };
        for (argv, reason) in [
            (
                &[
                    "--continuation-timeslice=1000",
                    "--",
                    "--max-timeslice=100000",
                ][..],
                "shorter than the run's --max-timeslice=100000",
            ),
            (
                &["--continuation-timeslice=20000000", "--"][..],
                "shorter than the run's --max-timeslice=200000000",
            ),
            (
                &[
                    "--continuation-timeslice=1000",
                    "--",
                    "--max-timeslice=disabled",
                ][..],
                "disable PMU timeslices",
            ),
        ] {
            let error = refusal(argv).expect_err(reason);
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{error:#}"
            );
            let message = format!("{error:#}");
            assert!(
                message.contains(reason) && message.contains("Omit it (auto)"),
                "{argv:?}: {message}"
            );
        }
        refusal(&[
            "--continuation-timeslice=200000000",
            "--",
            "--max-timeslice=1000",
        ])
        .unwrap();
        refusal(&["--"]).unwrap();
    }

    /// The normalized copy of a run's preemption record is written beside it
    /// for reading, and the run keeps its own record: the target endpoint
    /// and --minimize's baseline replay the record that met the target.
    /// Before, the run switched to the copy, whose renumbered priorities can
    /// run another schedule, and a minimized record that met the target was
    /// then "still not matching" it.
    #[test]
    fn a_normalized_record_is_written_for_reading_and_not_replayed() {
        use detcore::preemptions::PreemptionRecord;
        let workspace = tempfile::tempdir().unwrap();
        let mut options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        options.tmp_dir = Some(workspace.path().to_path_buf());
        let recorded = workspace.path().join("round_014.preempts");
        let history = vec![
            (detcore::types::LogicalTime::from_nanos(1_000), 352),
            (detcore::types::LogicalTime::from_nanos(2_000), 4923),
        ];
        let record = PreemptionRecord::from_vecs(&std::collections::BTreeMap::from([(
            detcore::DetTid::from_raw(7),
            history,
        )]));
        record.write_to_disk(&recorded).unwrap();
        let mut run = RunData::from_minimize_output(
            &options,
            "minimized".to_owned(),
            RunData::get_raw_runopts(&options),
            record.clone(),
            recorded.clone(),
            workspace.path().join("round_014.log"),
        );
        let (normalized_path, normalized) = run.write_normalized_preempts();
        assert_eq!(
            normalized_path,
            workspace.path().join("round_014.normalized")
        );
        assert!(normalized_path.exists());
        assert_ne!(normalized, record, "normalizing renumbers the priorities");
        assert_eq!(run.preempts_path_out(), recorded.as_path());
        assert_eq!(run.preempts_out(), &record);
    }

    /// Analyze options with their workspace in `dir` and the journal on.
    fn journaled_options(dir: &Path) -> AnalyzeOpts {
        let mut options =
            AnalyzeOpts::try_parse_from(["analyze", "--", "--chaos", "/bin/true"]).unwrap();
        options.tmp_dir = Some(dir.to_path_buf());
        options.journal = Journal::open(dir).unwrap();
        options
    }

    /// A run the journal holds, with the files it wrote unchanged, takes its
    /// recorded outcome and wall time instead of running: `launch` here would
    /// otherwise start a guest. A changed file makes it run again.
    #[test]
    fn a_journaled_run_takes_its_recorded_outcome_instead_of_running() {
        let dir = tempfile::tempdir().unwrap();
        let options = journaled_options(dir.path());
        let mut first = RunData::new_baseline(&options, "prefix_round_007".into()).unwrap();
        let stdout = first.root_path().with_extension("stdout");
        std::fs::write(&stdout, "FAILED\n").unwrap();
        first.is_a_match = Some(true);
        first.wall = Some(Duration::from_millis(2500));
        let key = first.journal_key();
        first
            .journal_outcome(&options.journal, key.clone(), &Ok(()))
            .unwrap();
        let entry = options.journal.lookup(&key).unwrap();
        assert_eq!(entry.end, RecordedEnd::Finished { matched: true });
        assert_eq!(entry.wall_ms, Some(2500));
        assert_eq!(entry.outputs.len(), 1, "{:?}", entry.outputs);
        assert_eq!(entry.outputs[0].path, Path::new("prefix_round_007.stdout"));

        // A later invocation's run of the same trial: answered.
        let reopened = journaled_options(dir.path());
        let mut again = RunData::new_baseline(&reopened, "prefix_round_007".into()).unwrap();
        again.launch().unwrap();
        assert!(again.is_a_match());
        assert_eq!(
            again.measurement(),
            Some((Duration::from_millis(2500), None))
        );
        assert_eq!(reopened.journal.answered(), 1);

        // Another trial (another name, so another key) is not.
        let other = RunData::new_baseline(&reopened, "prefix_round_008".into()).unwrap();
        assert_ne!(other.journal_key(), key);

        // A file it wrote changed: it runs again.
        std::fs::write(&stdout, "PASSED\n").unwrap();
        let mut changed = RunData::new_baseline(&reopened, "prefix_round_007".into()).unwrap();
        assert!(changed.answer_from_journal(&entry).unwrap().is_none());
    }

    /// A run its own bound stopped is journaled as such, and answered as the
    /// same unfinished trial; one a refusal stopped likewise.
    #[test]
    fn a_journaled_timeout_or_refusal_is_answered_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let options = journaled_options(dir.path());
        for (name, error, expected) in [
            (
                "bisect_round_001",
                anyhow::Error::new(RunTimeoutMarker),
                TrialEnd::Unfinished(Stop::Bound),
            ),
            (
                "bisect_round_002",
                anyhow::Error::new(crate::container::PolicyRefusal),
                TrialEnd::Unfinished(Stop::Refusal),
            ),
        ] {
            let mut first = RunData::new_baseline(&options, name.into()).unwrap();
            let key = first.journal_key();
            first
                .journal_outcome(&options.journal, key, &Err(error))
                .unwrap();
            let mut again = RunData::new_baseline(&options, name.into()).unwrap();
            assert_eq!(again.launch_trial().unwrap(), expected, "{name}");
        }
        // Any other error is not journaled: it ends the analysis, and a
        // resume runs the trial again.
        let mut failed = RunData::new_baseline(&options, "bisect_round_003".into()).unwrap();
        let key = failed.journal_key();
        failed
            .journal_outcome(
                &options.journal,
                key.clone(),
                &Err(anyhow::anyhow!("log cap")),
            )
            .unwrap();
        assert_eq!(options.journal.lookup(&key), None);
    }

    /// A recording that analyze removed after reading it (to save disk) is
    /// restored from the schedule the run replayed, when the two held the
    /// same events, instead of running the trial again.
    #[test]
    fn a_removed_recording_is_restored_from_the_schedule_it_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let options = journaled_options(dir.path());
        let events = vec![
            SchedEvent::branches(detcore::types::DetTid::from_raw(3), 5),
            SchedEvent::branches(detcore::types::DetTid::from_raw(4), 2),
        ];
        let make = || {
            let mut run = RunData::new_baseline(&options, "bisect_round_004".into())
                .unwrap()
                .with_schedule_replay()
                .with_schedule_recording();
            let replayed = run.sched_path_in().to_path_buf();
            options
                .write_trial_schedule(events.clone(), &replayed)
                .unwrap();
            run
        };
        let mut first = make();
        let recorded = first.sched_path_out().to_path_buf();
        PreemptionRecord::from_sched_events(events.clone())
            .write_to_disk(&recorded)
            .unwrap();
        first.is_a_match = Some(false);
        let key = first.journal_key();
        first
            .journal_outcome(&options.journal, key.clone(), &Ok(()))
            .unwrap();
        assert!(
            options
                .journal
                .lookup(&key)
                .unwrap()
                .recording_replays_input
                .is_some()
        );
        std::fs::remove_file(&recorded).unwrap();
        std::fs::remove_file(first.sched_path_in()).unwrap();

        let mut again = make();
        again.launch().unwrap();
        assert!(!again.is_a_match());
        assert_eq!(again.sched_out().unwrap(), &events);
    }

    /// The budget shortens a run's `--timeout` to the time it has left,
    /// unless the run's own bound is shorter, and no run starts once it is
    /// spent.
    #[test]
    fn the_budget_bounds_each_run_and_starts_none_once_spent() {
        use crate::analyze::budget::Budget;
        use crate::analyze::budget::BudgetKind;
        use crate::analyze::budget::TimeBudget;
        use crate::analyze::budget::TrialBudget;
        let dir = tempfile::tempdir().unwrap();
        let mut options = journaled_options(dir.path());
        options.budget = Budget::with_clock(
            TimeBudget::Limit(Duration::from_secs(100)),
            TrialBudget::Limit(NonZeroU64::new(2).unwrap()),
            std::sync::Arc::new(|| Duration::from_secs(40)),
        );
        let mut run = RunData::new_baseline(&options, "prefix_round_001".into()).unwrap();
        assert_eq!(run.runopts.run_timeout_seconds(), None);
        assert!(run.bound_by_budget().unwrap());
        assert_eq!(run.runopts.run_timeout_seconds(), NonZeroU64::new(60));

        let mut own = RunData::new_baseline(&options, "prefix_round_002".into()).unwrap();
        own.runopts.bound_run_timeout(NonZeroU64::new(30).unwrap());
        assert!(!own.bound_by_budget().unwrap(), "its own 30 s is shorter");
        assert_eq!(own.runopts.run_timeout_seconds(), NonZeroU64::new(30));

        // The trial budget of two is spent: launch starts nothing.
        let mut third = RunData::new_baseline(&options, "prefix_round_003".into()).unwrap();
        let error = third.launch().unwrap_err();
        let expired = error.downcast_ref::<BudgetExpired>().unwrap();
        assert_eq!(expired.kind, BudgetKind::TrialBudget);
        assert!(!third.has_launched());
    }

    /// The key names the contents of the schedule a run replays, not only
    /// its name and command: the same trial replaying another schedule is
    /// another run.
    #[test]
    fn the_key_names_what_the_run_replays() {
        let dir = tempfile::tempdir().unwrap();
        let options = journaled_options(dir.path());
        let events = |count| {
            vec![SchedEvent::branches(
                detcore::types::DetTid::from_raw(3),
                count,
            )]
        };
        let mut run = RunData::new_baseline(&options, "bisect_round_005".into())
            .unwrap()
            .with_schedule_replay();
        let path = run.sched_path_in().to_path_buf();
        options.write_trial_schedule(events(5), &path).unwrap();
        let first = run.journal_key();
        options.write_trial_schedule(events(6), &path).unwrap();
        assert_ne!(run.journal_key(), first);
        options.write_trial_schedule(events(5), &path).unwrap();
        assert_eq!(run.journal_key(), first);
    }

    /// A run that the budget's shortened `--timeout` ends is cut: the
    /// analysis stops with the budget's error, journals nothing (a resume
    /// runs it again) and gives the run its own bound back. The same end
    /// under the run's own, shorter bound is the run's outcome, journaled as
    /// a timeout.
    #[test]
    fn a_run_the_budgets_bound_ends_is_cut_and_not_journaled() {
        use crate::analyze::budget::Budget;
        use crate::analyze::budget::BudgetKind;
        use crate::analyze::budget::TimeBudget;
        use crate::analyze::budget::TrialBudget;
        let dir = tempfile::tempdir().unwrap();
        let mut options = journaled_options(dir.path());
        options.budget = Budget::with_clock(
            TimeBudget::Limit(Duration::from_secs(100)),
            TrialBudget::Off,
            std::sync::Arc::new(|| Duration::from_secs(40)),
        );
        let bounds = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = bounds.clone();
        options.trial_runner = TrialRunner(Some(std::sync::Arc::new(move |run: &mut RunData| {
            seen.lock().unwrap().push(run.runopts.run_timeout_seconds());
            Err(anyhow::Error::new(RunTimeoutMarker))
        })));

        let mut own = RunData::new_baseline(&options, "prefix_round_001".into()).unwrap();
        own.runopts.bound_run_timeout(NonZeroU64::new(30).unwrap());
        assert_eq!(
            own.launch_trial().unwrap(),
            TrialEnd::Unfinished(Stop::Bound)
        );
        assert_eq!(options.journal.len(), 1, "its own bound ended it");

        let mut cut = RunData::new_baseline(&options, "prefix_round_002".into()).unwrap();
        let error = cut.launch().unwrap_err();
        let expired = error
            .downcast_ref::<BudgetExpired>()
            .unwrap_or_else(|| panic!("{error:#}"));
        assert_eq!(expired.kind, BudgetKind::TimeBudget);
        assert_eq!(expired.cut.as_deref(), Some("prefix_round_002"));
        assert_eq!(options.journal.len(), 1, "the cut run is not journaled");
        assert_eq!(cut.runopts.run_timeout_seconds(), None);
        assert_eq!(
            *bounds.lock().unwrap(),
            [NonZeroU64::new(30), NonZeroU64::new(60)]
        );
    }

    /// A run's reproducer carries its own `--timeout`, never the shorter one
    /// the budget gave it.
    #[test]
    fn the_reproducer_carries_the_runs_own_bound() {
        use crate::analyze::budget::Budget;
        use crate::analyze::budget::TimeBudget;
        use crate::analyze::budget::TrialBudget;
        let dir = tempfile::tempdir().unwrap();
        let mut options = journaled_options(dir.path());
        options.budget = Budget::with_clock(
            TimeBudget::Limit(Duration::from_secs(100)),
            TrialBudget::Off,
            std::sync::Arc::new(|| Duration::from_secs(40)),
        );
        for (name, own) in [
            ("prefix_round_001", None),
            ("prefix_round_002", NonZeroU64::new(200)),
        ] {
            let mut run = RunData::new_baseline(&options, name.into()).unwrap();
            run.runopts.set_run_timeout(own);
            assert!(run.bound_by_budget().unwrap());
            assert_eq!(run.runopts.run_timeout_seconds(), NonZeroU64::new(60));
            run.write_repro(own).unwrap();
            let repro = std::fs::read_to_string(run.root_path().with_extension("repro")).unwrap();
            assert!(!repro.contains("--timeout=60"), "{repro}");
            assert_eq!(repro.contains("--timeout=200"), own.is_some(), "{repro}");
            assert_eq!(run.runopts.run_timeout_seconds(), NonZeroU64::new(60));
        }
    }
}

#[cfg(test)]
mod replay_departure_tests {
    use clap::Parser;

    use super::*;
    use crate::container::PolicyRefusal;

    #[test]
    fn analysis_refuses_marked_inputs_and_outputs_before_discarding_provenance() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("raw.preempts");
        let output = directory.path().join("stripped.preempts");
        let options = AnalyzeOpts::try_parse_from(["analyze", "--", "/bin/true"]).unwrap();
        let mut event = SchedEvent::branches(detcore::types::DetTid::from_raw(3), 2);
        event.end_time = Some(detcore::types::LogicalTime::from_nanos(12));
        for marker in [None, Some(0)] {
            let record = PreemptionRecord::from_sched_events(vec![event.clone()])
                .retain_replay_departure(marker);
            record.write_to_disk(&input).unwrap();
            let raw = std::fs::read(&input).unwrap();
            std::fs::write(&output, b"existing destination").unwrap();
            let admitted = options.refuse_recordings_under_another_pipe_wake_latency(&[(
                "--run1-preemptions",
                &input,
            )]);
            let processed = RunData::post_process_events_file(&input, &output);
            if marker.is_some() {
                for result in [admitted, processed] {
                    let error = result.expect_err("known omissions cannot supply trial evidence");
                    assert!(error.downcast_ref::<PolicyRefusal>().is_some(), "{error:#}");
                    assert!(format!("{error:#}").contains("HERMIT_INCOMPLETE_REPLAY_RECORD"));
                }
                assert_eq!(std::fs::read(&output).unwrap(), b"existing destination");
            } else {
                admitted.unwrap();
                processed.unwrap();
                let actual = PreemptionReader::try_new(&output).unwrap().into_inner();
                let mut expected_event = event.clone();
                expected_event.end_time = None;
                let expected = PreemptionRecord::from_sched_events(vec![expected_event]);
                assert_eq!(actual, expected);
            }
            assert_eq!(
                std::fs::read(&input).unwrap(),
                raw,
                "retain raw diagnostics"
            );
        }
    }
}
