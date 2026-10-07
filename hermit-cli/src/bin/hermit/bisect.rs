/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Locate the scheduling event that changes a program from passing to failing.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::bail;
use clap::Parser;
use detcore::preemptions::PreemptionRecord;
use detcore::types::SchedEvent;
use hermit::Error;
use regex::Regex;
use reverie::process::ExitStatus;
use tracing::metadata::LevelFilter;

use super::analyze::AnalyzeOpts;
use super::analyze::ExitStatusConstraint;
use super::global_opts::GlobalOpts;

/// Bisect two recorded schedules to identify the event ordering that causes a failure.
#[derive(Debug, Parser)]
pub struct BisectOpts {
    /// A recorded schedule whose replay succeeds.
    #[clap(long, value_name = "SCHEDULE")]
    good: PathBuf,

    /// A recorded schedule whose replay exhibits the target failure.
    #[clap(long, value_name = "SCHEDULE")]
    bad: PathBuf,

    /// Treat stdout matching this regular expression as part of the target failure.
    #[clap(long, value_name = "REGEX")]
    target_stdout: Option<Regex>,

    /// Treat stderr matching this regular expression as part of the target failure.
    #[clap(long, value_name = "REGEX")]
    target_stderr: Option<Regex>,

    /// Exit status identifying the target failure.
    #[clap(long, default_value = "nonzero", value_name = "NUM|nonzero|any")]
    target_exit_code: ExitStatusConstraint,

    /// Logging level for replayed guest runs.
    #[clap(short, long, value_name = "LEVEL", env = "HERMIT_LOG")]
    guest_log: Option<LevelFilter>,

    /// Write the machine-readable race report to this path.
    #[clap(long, value_name = "PATH")]
    report_file: Option<PathBuf>,

    /// Use Needleman-Wunsch alignment while selecting midpoint schedules.
    #[clap(long)]
    needleman: bool,

    /// Maximum accepted edit-distance jitter in a realized replay schedule.
    #[clap(long, value_name = "EVENTS")]
    jitter_dist: Option<usize>,

    /// Number of schedule events to show around the localized race.
    #[clap(long, value_name = "EVENTS", default_value = "5")]
    execution_context: usize,

    /// Print replay commands and guest output for each bisection step.
    #[clap(long, short)]
    verbose: bool,

    /// Arguments for the underlying `hermit run`, followed by the program and its arguments.
    #[clap(value_name = "RUN_ARGS", required = true)]
    run_args: Vec<String>,

    /// Why this host's retired-branch counter is inexact, if it is (see
    /// `AnalyzeOpts::inexact_branch_counter`).
    #[clap(skip = crate::host_capabilities::host_inexact_branch_counter as fn() -> Option<String>)]
    inexact_branch_counter: fn() -> Option<String>,
}

impl BisectOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let mut analyzer = AnalyzeOpts {
            target_stdout: self.target_stdout.clone(),
            target_stderr: self.target_stderr.clone(),
            target_exit_code: self.target_exit_code.clone(),
            guest_log: self.guest_log,
            selfcheck: false,
            search: false,
            run_needleman: self.needleman,
            minimize: false,
            imprecise_search: false,
            run1_seed: None,
            run1_preemptions: None,
            run1_schedule: None,
            run2_seed: None,
            run2_preemptions: None,
            run2_schedule: None,
            report_file: self.report_file.clone(),
            analyze_seed: None,
            verbose: self.verbose,
            jitter_dist: self.jitter_dist,
            execution_context: self.execution_context,
            tmp_dir: None,
            success_exit_code: None,
            run_arg: Vec::new(),
            run_args: self.run_args.clone(),
            // Replays run on the globally selected backend
            // (`hermit --backend <BACKEND> bisect ...`) and charge the
            // invocation's shared `--max-log-bytes` budget.
            backend: global.backend,
            max_log_bytes: global.max_log_bytes,
            log_budget: global.log_budget(),
            inexact_branch_counter: self.inexact_branch_counter,
        };
        // Before the schedules are read: a refused cap or counter reads and
        // starts nothing.
        analyzer.refuse_unsupervised_log_cap()?;
        analyzer.refuse_options_trials_do_not_apply()?;
        analyzer.refuse_outputs_trials_overwrite()?;
        analyzer.refuse_unqualified_trial_timeout()?;
        analyzer.install_trial_pmu_config()?;
        analyzer.refuse_strict_with_inexact_branch_counter()?;

        let good = read_schedule(&self.good, "good")?;
        let bad = read_schedule(&self.bad, "bad")?;
        if good == bad {
            bail!("the --good and --bad schedules contain identical event traces");
        }

        analyzer.bisect_schedule_pair(good, bad)
    }
}

fn read_schedule(path: &Path, label: &str) -> anyhow::Result<Vec<SchedEvent>> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read --{label} schedule {}", path.display()))?;
    let record: PreemptionRecord = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse --{label} schedule {}", path.display()))?;
    record
        .validate()
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("--{label} schedule {} failed validation", path.display()))?;
    if !record.contains_schedevents() {
        bail!(
            "--{label} schedule {} contains no global schedule events",
            path.display()
        );
    }
    Ok(record.into_global())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through `main`: bisect refuses `--strict` in its run arguments on an
    /// inexact retired-branch counter before it reads either schedule
    /// (https://github.com/rrnewton/hermit/issues/3810). The schedules do not
    /// exist, so a bisect that passed the refusal fails on reading them. With
    /// `--namespace-only` or `--lite`, which its replays ignore, bisect refuses
    /// earlier, for a dropped option
    /// (`bisect_refuses_run_options_its_replays_do_not_apply`).
    #[test]
    fn strict_bisect_refuses_an_inexact_branch_counter_before_reading_schedules() {
        let argv = [
            "hermit",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--strict",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Bisect(mut options) = args.command else {
            panic!("{argv:?} is not bisect")
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

        options.inexact_branch_counter = || None;
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error.to_string().contains("failed to read --good schedule"),
            "{error:#}"
        );
    }

    /// Through `main`: bisect refuses run arguments that its replays would
    /// accept and run without (`RunOpts::options_only_main_applies`), before
    /// it reads either schedule. Without one, it reaches the missing
    /// schedule instead.
    #[test]
    fn bisect_refuses_run_options_its_replays_do_not_apply() {
        let bisect = |run_args: &[&str]| {
            let mut argv = vec![
                "hermit",
                "bisect",
                "--good=/nonexistent/good.json",
                "--bad=/nonexistent/bad.json",
                "--",
            ];
            argv.extend(run_args);
            argv.push("/bin/true");
            let args = crate::Args::try_parse_from(&argv).unwrap();
            let crate::Subcommand::Bisect(options) = args.command else {
                panic!("{argv:?} is not bisect")
            };
            options.main(&args.global).unwrap_err()
        };
        for (run_args, named) in [
            (&["--namespace-only"][..], "--namespace-only"),
            (&["--lite"][..], "--namespace-only"),
            (&["--verify"][..], "--verify"),
            (&["--save-config=requested.config"][..], "--save-config"),
            (&["--summary-json=requested.summary"][..], "--summary-json"),
        ] {
            let error = bisect(run_args);
            assert!(
                error
                    .downcast_ref::<crate::container::PolicyRefusal>()
                    .is_some(),
                "{run_args:?}: {error:#}"
            );
            assert!(error.to_string().contains(named), "{run_args:?}: {error:#}");
        }
        for admitted in [&["--no-namespace"][..], &["--timeout=3"][..]] {
            let error = bisect(admitted);
            assert!(
                error.to_string().contains("failed to read --good schedule"),
                "{admitted:?}: {error:#}"
            );
        }

        // A KVM replay with --timeout is refused as `hermit --backend=kvm run
        // --timeout` is (rel-041's review of
        // https://github.com/rrnewton/hermit/pull/3836).
        let argv = [
            "hermit",
            "--backend=kvm",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--timeout=3",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Bisect(options) = args.command else {
            panic!("{argv:?} is not bisect")
        };
        let error = options.main(&args.global).unwrap_err();
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
    }

    /// Bisect's replays apply `--skid-margin`: it is installed as Reverie's
    /// per-process PMU configuration before either schedule is read, so a
    /// later installation is refused. This relies on nextest running each
    /// test in a process of its own, as the validation DAG does.
    #[test]
    fn bisect_installs_the_replays_skid_margin() {
        let argv = [
            "hermit",
            "bisect",
            "--good=/nonexistent/good.json",
            "--bad=/nonexistent/bad.json",
            "--",
            "--skid-margin=4321",
            "/bin/true",
        ];
        let args = crate::Args::try_parse_from(argv).unwrap();
        let crate::Subcommand::Bisect(options) = args.command else {
            panic!("{argv:?} is not bisect")
        };
        let error = options.main(&args.global).unwrap_err();
        assert!(
            error.to_string().contains("failed to read --good schedule"),
            "{error:#}"
        );
        assert!(
            reverie_ptrace::set_pmu_config(reverie_ptrace::PmuConfig::new()).is_err(),
            "bisect did not install the replays' --skid-margin"
        );
    }

    #[test]
    fn schedule_fixture_contains_events() {
        let schedule = read_schedule(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("test-resources/flaky_cas_sequence_schedules-passing.json"),
            "good",
        )
        .unwrap();
        assert!(!schedule.is_empty());
    }
}
