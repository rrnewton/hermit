/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A mode for analyzing a hermit run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::bail;
use colored::Colorize;
use detcore::DetTid;
use detcore::FIRST_PRIORITY;
use detcore::LAST_PRIORITY;
use detcore::Priority;
use detcore::preemptions::PreemptionReader;
use detcore::preemptions::PreemptionRecord;
use detcore::types::LogicalTime;
use detcore::util::truncated;
use rand::RngExt as _;
use rand::SeedableRng;
use rand_pcg::Pcg64Mcg;

use crate::analyze::consts::LOG_EXT;
use crate::analyze::types::AnalyzeOpts;
use crate::global_opts::GlobalOpts;

/// Sanity check that a series of preemptions don't include duplicates and are monotonically increasing.
fn sanity_preempts(vec: &[(LogicalTime, Priority, Option<u64>)]) -> anyhow::Result<()> {
    let mut set = BTreeSet::new();

    let mut time_last = None;
    for (ns, prio, _) in vec {
        if let Some(last) = time_last
            && *ns <= last
        {
            bail!(
                "Timestamps failed to monotonically increase ({}), in series:\n {:?}",
                ns,
                vec
            );
        }
        assert!(*prio >= FIRST_PRIORITY);
        assert!(*prio <= LAST_PRIORITY);
        time_last = Some(*ns);
        if !set.insert(ns) {
            bail!(
                "sanity_preempts: time series of preemptions contained duplicate entries sharing the timestamp {}",
                ns
            );
        }
    }
    Ok(())
}

impl AnalyzeOpts {
    /// Iteratively minimize the schedule needed to produce the error.
    ///
    /// # Returns
    /// - Minimized preemption record (in memory),
    /// - Path of a file containing that same minimized preemption record,
    /// - Path of the log file that corresponds to the last matching (minimal) run.
    pub(super) fn minimize(
        &self,
        preempts_path: &Path,
        _global: &GlobalOpts,
    ) -> anyhow::Result<(PreemptionRecord, PathBuf, PathBuf)> {
        self.minimize_with(preempts_path, |runname, new_preempts_path| {
            Ok(self
                .launch_from_preempts_to_sched(runname, new_preempts_path, None)?
                .0)
        })
    }

    /// The minimization loop behind [`Self::minimize`]. `trial_matches` runs
    /// one trial with the preemptions in the given file and says whether it
    /// met the criteria. A trial that cannot finish (for example, one that
    /// `--max-log-bytes` ended) ends the minimization with its error
    /// unchanged, so main still reports it by its class.
    fn minimize_with(
        &self,
        preempts_path: &Path,
        mut trial_matches: impl FnMut(&str, &Path) -> anyhow::Result<bool>,
    ) -> anyhow::Result<(PreemptionRecord, PathBuf, PathBuf)> {
        let pr = PreemptionReader::new(preempts_path);
        let mut tmp_dir = preempts_path.to_path_buf();
        assert!(tmp_dir.pop());

        let tids: Vec<DetTid> = pr.all_threads();
        let init_schedule: PreemptionRecord = pr.load_all();
        let mut round: u64 = 0;

        // All the preemption points we don't know about yet.  Are they critical?
        let mut remaining_unknown: BTreeMap<DetTid, _> = BTreeMap::new();
        let mut init_intervention_count = 0;
        for (dtid, history) in init_schedule.extract_all() {
            // Each preemption keeps its recorded RCB target, so a round's record
            // stays RCB-anchored (https://github.com/rrnewton/hermit/issues/4497).
            let vec = history.as_vec_with_rcbs();
            sanity_preempts(&vec)?;
            init_intervention_count += vec.len();
            remaining_unknown.insert(dtid, vec);
        }

        eprintln!(
            ":: {}",
            format!(
                "RootCause: starting with schedule of {} interventions for {} threads.",
                init_intervention_count,
                tids.len()
            )
            .yellow()
            .bold()
        );
        if self.verbose {
            eprintln!(
                "Initial schedule:\n{}",
                truncated(1000, format!("{}", init_schedule))
            );
        }

        // The ones we know for sure are critical.
        let mut critical_preempts: BTreeMap<
            DetTid,
            BTreeSet<(LogicalTime, Priority, Option<u64>)>,
        > = BTreeMap::new();

        // We take threads out of consideration if they have no interventions:
        let all_threads = pr.all_threads();
        let mut remaining_threads = Vec::new();

        let min_seed = self.analyze_seed.unwrap_or_else(rand::random::<u64>);
        eprintln!(
            ":: {}",
            format!("Minimization search using RNG seed {}", min_seed)
                .yellow()
                .bold()
        );
        let mut rng = Pcg64Mcg::seed_from_u64(min_seed);

        // The number of preemptions to remove each round:
        let mut batch_sizes = BTreeMap::new();
        for tid in all_threads {
            let len = remaining_unknown.get(&tid).unwrap().len();
            batch_sizes.insert(tid, std::cmp::max(1, len / 4));
            if len > 0 {
                remaining_threads.push(tid)
            }
        }

        let mut last_attempt: Option<PreemptionRecord> = None;
        let mut last_matching_attempt = None;
        let mut last_matching_pr_file = None;
        let mut last_matching_log = None;

        // Algorithm: the union of remaining_unknown plus critical_preempts represents our
        // current point in the space of schedules to explore.
        loop {
            round += 1;
            {
                let sum: usize = remaining_unknown.values().map(|v| v.len()).sum();
                let sum2: usize = critical_preempts.values().map(|s| s.len()).sum();
                eprintln!(
                    ":: {}",
                    format!(
                        "Minimizing schedule, round {}, {} interventions plus {} critical found",
                        round, sum, sum2
                    )
                    .yellow()
                    .bold(),
                );
                if self.verbose {
                    eprintln!(":: All critical preemptions: {:?}", critical_preempts);
                }
                if remaining_threads.is_empty() {
                    if sum2 == 0 {
                        eprintln!(
                            ":: {}",
                            "Minimized to ZERO interventions.  The base run matches the critera."
                                .yellow()
                                .bold()
                        );
                    } else {
                        eprintln!(
                            ":: {}",
                            format!("Minimized to {} interventions.", sum2)
                                .green()
                                .bold(),
                        );
                    }
                    break;
                }
            }

            let selected_ix = rng.random_range(0..remaining_threads.len());
            let selected_tid = *remaining_threads.get(selected_ix).unwrap();
            let mut cut = {
                let batch = batch_sizes.get_mut(&selected_tid).unwrap();
                assert!(*batch > 0);
                let preempts = remaining_unknown.get_mut(&selected_tid).unwrap();
                assert!(!preempts.is_empty());
                *batch = std::cmp::min(*batch, preempts.len());

                let len = preempts.len();
                let cut = preempts.split_off(len - *batch);
                eprintln!(
                    ":: Shaving {} off of {} preemptions for tid {}: {}",
                    batch,
                    len,
                    selected_tid,
                    if self.verbose {
                        truncated(79, format!("{:?}", cut))
                    } else {
                        "".to_string()
                    }
                );
                if preempts.is_empty() {
                    remaining_threads.swap_remove(selected_ix);
                }
                cut
            };

            {
                let new_preempts_path =
                    tmp_dir.join(format!("round_{:0wide$}.preempts", round, wide = 3));
                let pr_new = {
                    // Expensive... union back in the critical_preempts:
                    let mut btmap = remaining_unknown.clone();
                    btmap
                        .iter_mut()
                        .try_for_each(|(tid, vec)| -> anyhow::Result<()> {
                            if let Some(set) = critical_preempts.get(tid) {
                                for preempt in set {
                                    vec.push(*preempt);
                                }
                                // if cfg!(debug)
                                {
                                    sanity_preempts(vec)?;
                                }
                            }
                            Ok(())
                        })?;
                    // The source record's epoch and chaos slowdown transitions
                    // come along, so the round replays under the same clock.
                    self.stamp_trial_record(
                        PreemptionRecord::from_vecs_with_rcbs(&btmap)
                            .with_replay_context_of(&init_schedule),
                    )
                };
                if let Err(e) = pr_new.validate() {
                    bail!(
                        "Hermit analyzer produced corrupt preemption record, cannot proceed.\n\
                        Error: {}\n\n\
                        Corrupt record: {}\n\n\
                        Last good state: {}",
                        e,
                        pr_new,
                        if let Some(last) = last_attempt {
                            last.to_string()
                        } else {
                            "<none>".to_string()
                        }
                    );
                }
                pr_new
                    .write_to_disk(&new_preempts_path)
                    .map_err(anyhow::Error::msg)?;
                last_attempt = Some(pr_new);

                let batch = batch_sizes.get_mut(&selected_tid).unwrap();
                let runname = format!("round_{:0wide$}", round, wide = 3);
                let log_path = tmp_dir.join(&runname).with_extension("log");
                if trial_matches(&runname, &new_preempts_path)? {
                    eprintln!(
                        ":: {}",
                        "New run matches criteria, continuing.".green().bold()
                    );
                    last_matching_attempt = last_attempt.clone();
                    last_matching_pr_file = Some(new_preempts_path.clone());
                    last_matching_log = Some(log_path);
                    *batch += 1;
                    continue;
                } else {
                    eprintln!(
                        ":: {}",
                        "New run fails criteria, backtracking..".red().bold()
                    );
                    if *batch == 1 {
                        let critical = cut.pop().unwrap();
                        assert!(cut.is_empty());
                        eprintln!(
                            ":: Knocked out only one preemption ({:?}), so concluding that one is critical.",
                            critical
                        );
                        let entry = critical_preempts.entry(selected_tid).or_default();

                        assert!(entry.insert(critical));
                        continue;
                    } else {
                        *batch /= 2;
                        eprintln!(
                            ":: Batch size for minimizing tid {} reduced to {}",
                            selected_tid, batch
                        );
                        // Hacky way to backtrack one step for now:
                        {
                            let preempts = remaining_unknown.get_mut(&selected_tid).unwrap();
                            preempts.append(&mut cut);
                        }
                        // A cut that took the thread's last unknown
                        // preemptions took the thread off the worklist; the
                        // restored ones still need deciding.
                        if !remaining_threads.contains(&selected_tid) {
                            remaining_threads.push(selected_tid);
                        }
                        continue;
                    }
                }
            }
        }
        match (
            last_matching_attempt,
            last_matching_pr_file,
            last_matching_log,
        ) {
            (Some(record), Some(file), Some(log)) => Ok((record, file, log)),
            // No trial met the criteria, so every preemption was critical:
            // the minimized record is the source record, whose target run
            // wrote its log beside it.
            _ => {
                eprintln!(
                    ":: {}",
                    "No minimization trial met the criteria: every preemption is critical."
                        .yellow()
                        .bold()
                );
                Ok((
                    init_schedule,
                    preempts_path.to_path_buf(),
                    preempts_path.with_extension(LOG_EXT),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::*;
    use crate::container::LogCapExceeded;

    /// A minimization trial that `--max-log-bytes` ended stops the
    /// minimization and returns that error, still a `LogCapExceeded`, so main
    /// reports it as the cap (123). Before, any trial's error panicked here.
    #[test]
    fn a_failing_minimize_trial_ends_minimization_with_its_error() {
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        // One thread with eight interventions: increasing times, ordinary
        // priorities. When every trial matches, minimizing it takes three
        // trials (batches of 2, 3 and 3 interventions).
        let history: Vec<(LogicalTime, Priority)> = (1..=8)
            .map(|k| (LogicalTime::from_nanos(k * 1000), FIRST_PRIORITY + k))
            .collect();
        PreemptionRecord::from_vecs(&BTreeMap::from([(DetTid::from_raw(3), history)]))
            .write_to_disk(&preempts_path)
            .unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();

        let mut total_trials = 0;
        options
            .minimize_with(&preempts_path, |_runname, _trial_preempts| {
                total_trials += 1;
                Ok(true)
            })
            .unwrap();
        assert_eq!(total_trials, 3);

        for failing_trial in 1..=total_trials {
            let mut trials = 0;
            let result = options.minimize_with(&preempts_path, |_runname, trial_preempts| {
                trials += 1;
                assert!(
                    trial_preempts.exists(),
                    "each trial's preemption file is written before it runs"
                );
                if trials == failing_trial {
                    return Err(anyhow::Error::new(LogCapExceeded));
                }
                // An earlier trial finished and still met the criteria.
                Ok(true)
            });
            let Err(error) = result else {
                panic!("trial {failing_trial} failed, yet minimization returned a record")
            };
            assert!(
                error.downcast_ref::<LogCapExceeded>().is_some(),
                "trial {failing_trial}: the error lost its type: {error:#}"
            );
            assert_eq!(
                trials, failing_trial,
                "minimization ran more trials after trial {failing_trial} failed"
            );
        }
    }

    /// Every round's record keeps the source record's RCB target for each
    /// preemption it keeps, and the source's epoch, so a minimization replay
    /// stays RCB-anchored and on the source's clock. Before, rounds were
    /// rebuilt time-only and without the epoch
    /// (https://github.com/rrnewton/hermit/issues/4497).
    #[test]
    fn minimize_rounds_keep_rcb_targets_and_the_epoch() {
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        let tid = DetTid::from_raw(3);
        let epoch = "2026-01-01T00:00:00Z";
        let mut writer = detcore::preemptions::PreemptionWriter::new(None);
        writer.register_thread(tid, FIRST_PRIORITY);
        let rcbs_at = |k: u64| k * 10;
        for k in 1..=8u64 {
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(k * 1000),
                rcbs_at(k),
                FIRST_PRIORITY + k as Priority - 1,
                FIRST_PRIORITY + k as Priority,
            );
        }
        let mut record: serde_json::Value = serde_json::from_str(&writer.into_string()).unwrap();
        record["epoch"] = serde_json::Value::from(epoch);
        std::fs::write(&preempts_path, record.to_string()).unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        let mut rounds = 0;
        options
            .minimize_with(&preempts_path, |_runname, trial_preempts| {
                rounds += 1;
                let text = std::fs::read_to_string(trial_preempts).unwrap();
                let round: PreemptionRecord = serde_json::from_str(&text).unwrap();
                let json: serde_json::Value = serde_json::from_str(&text).unwrap();
                assert_eq!(json["epoch"], epoch, "round {rounds}: {text}");
                let entries = &round.as_vecs_with_rcbs()[&tid];
                for (time, _prio, rcbs) in entries.iter().skip(1) {
                    assert_eq!(
                        *rcbs,
                        Some(rcbs_at(time.as_nanos() / 1000)),
                        "round {rounds}: the preemption at {time} lost its RCB target: {text}"
                    );
                }
                Ok(true)
            })
            .unwrap();
        assert!(rounds > 0);
    }

    /// Rounds that drop a critical preemption fail, so the minimizer puts it
    /// back: every round record, including those with critical preemptions
    /// restored, validates and keeps each preemption's own RCB target, and
    /// the minimized record keeps exactly the critical preemptions, anchored
    /// (https://github.com/rrnewton/hermit/issues/4497).
    #[test]
    fn critical_preemptions_put_back_keep_their_rcb_targets() {
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        let tid = DetTid::from_raw(3);
        let mut writer = detcore::preemptions::PreemptionWriter::new(None);
        writer.register_thread(tid, FIRST_PRIORITY);
        let rcbs_at = |k: u64| k * 10;
        for k in 1..=8u64 {
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(k * 1000),
                rcbs_at(k),
                FIRST_PRIORITY + k as Priority - 1,
                FIRST_PRIORITY + k as Priority,
            );
        }
        std::fs::write(&preempts_path, writer.into_string()).unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        let critical = [3000u64, 6000];
        // Each change point's time and RCB target; a change point without a
        // target fails here.
        let anchored_changes = |record: &PreemptionRecord| -> Vec<(u64, u64)> {
            record.as_vecs_with_rcbs()[&tid]
                .iter()
                .skip(1)
                .map(|(time, _prio, rcbs)| (time.as_nanos(), rcbs.expect("an RCB target")))
                .collect()
        };
        let (mut failed, mut restored) = (0, 0);
        let mut dropped = std::collections::BTreeSet::new();
        let (minimized, _, _) = options
            .minimize_with(&preempts_path, |runname, trial_preempts| {
                let text = std::fs::read_to_string(trial_preempts).unwrap();
                let round: PreemptionRecord = serde_json::from_str(&text).unwrap();
                round.validate().unwrap();
                let changes = anchored_changes(&round);
                for (time, rcbs) in &changes {
                    assert_eq!(*rcbs, rcbs_at(time / 1000), "{runname}: {text}");
                }
                let times: Vec<u64> = changes.iter().map(|(time, _)| *time).collect();
                for time in critical {
                    if !times.contains(&time) {
                        dropped.insert(time);
                    } else if dropped.contains(&time) {
                        restored += 1;
                    }
                }
                let matches = critical.iter().all(|time| times.contains(time));
                if !matches {
                    failed += 1;
                }
                Ok(matches)
            })
            .unwrap();
        assert!(
            failed > 0 && restored > 0,
            "failed {failed}, restored {restored}"
        );
        minimized.validate().unwrap();
        // Every other preemption was decided and dropped, including ones a
        // failed cut of a thread's last entries put back.
        assert_eq!(anchored_changes(&minimized), vec![(3000, 30), (6000, 60)]);
    }

    /// When no trial meets the criteria, every preemption is critical and the
    /// minimized record is the source record, with the target run's log
    /// beside it; this used to panic on the missing matching trial.
    #[test]
    fn a_minimization_with_no_matching_trial_returns_the_source_record() {
        let workspace = tempfile::tempdir().unwrap();
        let preempts_path = workspace.path().join("target.preempts");
        let tid = DetTid::from_raw(3);
        let mut writer = detcore::preemptions::PreemptionWriter::new(None);
        writer.register_thread(tid, FIRST_PRIORITY);
        for k in 1..=5u64 {
            writer.insert_reprioritization(
                tid,
                LogicalTime::from_nanos(k * 1000),
                k * 10,
                FIRST_PRIORITY + k as Priority - 1,
                FIRST_PRIORITY + k as Priority,
            );
        }
        std::fs::write(&preempts_path, writer.into_string()).unwrap();
        let options =
            AnalyzeOpts::try_parse_from(["analyze", "--analyze-seed=7", "--", "/bin/true"])
                .unwrap();
        let mut trials = 0;
        let (minimized, file, log) = options
            .minimize_with(&preempts_path, |_runname, _trial| {
                trials += 1;
                Ok(false)
            })
            .unwrap();
        assert!(trials > 0);
        assert_eq!(file, preempts_path);
        assert_eq!(log, workspace.path().join("target.log"));
        assert_eq!(
            minimized.as_vecs_with_rcbs(),
            PreemptionReader::new(&preempts_path)
                .load_all()
                .as_vecs_with_rcbs()
        );
    }
}
