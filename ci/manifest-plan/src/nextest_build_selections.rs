// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Authored Cargo build selections for the committed validation graph.
//! Runtime consumers read the selection recorded in that graph. The generator
//! independently checks these declarations against the actual runner commands.

pub(super) fn for_step(tag: &str) -> Option<&'static [&'static str]> {
    match tag {
        "test.regular_crates" | "test.isolated_detcore_workdir" => Some(&[
            "--workspace",
            "--exclude",
            "hermit-detcore",
            "--exclude",
            "hermit",
            "--exclude",
            "hermetic_infra_hermit_flaky-tests",
        ]),
        "test.hermit_unit" | "privileged-test.pmu_ptrace_completion_cases" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-native-test-support",
            "--lib",
            "--bins",
        ]),
        "test.detcore_unit" => Some(&["-p", "hermit-detcore", "--lib", "--bins"]),
        "test.detcore_misc"
        | "privileged-build.privileged_tests"
        | "privileged-cpuid.faulting"
        | "privileged-only-cpuid.faulting"
        | "privileged-only-cpuid.faulting_on_host"
        | "super.post_fork_scheduling_diagnostics"
        | "super.network_syscall_determinism_diagnostic" => {
            Some(&["-p", "hermit-detcore", "--test", "tests_misc"])
        }
        "test.detcore_parallel"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_bottom_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_default_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_middle_detcore"
        | "super.weekly_pmu_parallel_memory_diagnostic_mem_race_top_detcore" => {
            Some(&["-p", "hermit-detcore", "--test", "tests_parallelism"])
        }
        "test.recorded_clocks" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "record_replay",
            "--test",
            "flock_exclusion",
        ]),
        "test.hermit_integration" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "aio_nr_determinism",
            "--test",
            "arch_status_determinism",
            "--test",
            "chaos_sched_yield_progress",
            "--test",
            "chaos_stress_pmu_detection",
            "--test",
            "child_time_rpc",
            "--test",
            "chown_virtual_root_identity",
            "--test",
            "cli_owned_lifecycle",
            "--test",
            "clock_determinism",
            "--test",
            "clock_discipline_determinism",
            "--test",
            "container_init_deadline",
            "--test",
            "cpufreq_avg_determinism",
            "--test",
            "epoll_determinism",
            "--test",
            "epoll_pwait_zero_timeout_progress",
            "--test",
            "file_nr_determinism",
            "--test",
            "fp_reduction_determinism",
            "--test",
            "futex2_refusal",
            "--test",
            "hashseed_determinism",
            "--test",
            "inode_nr_determinism",
            "--test",
            "kernel_keyring",
            "--test",
            "key_users_determinism",
            "--test",
            "mmap_determinism",
            "--test",
            "node_vmstat_determinism",
            "--test",
            "numa_maps_determinism",
            "--test",
            "perf_event_refusal",
            "--test",
            "pidfd_creation",
            "--test",
            "process_isolation_refusals",
            "--test",
            "proc_fdinfo_determinism",
            "--test",
            "proc_locks_determinism",
            "--test",
            "procfs_determinism",
            "--test",
            "procfs_positioned_determinism",
            "--test",
            "pty_nr_determinism",
            "--test",
            "python_stdlib",
            "--test",
            "robust_futex_owner_death",
            "--test",
            "run_evidence",
            "--test",
            "self_sched_determinism",
            "--test",
            "self_schedstat_determinism",
            "--test",
            "signal_determinism",
            "--test",
            "smaps_determinism",
            "--test",
            "smaps_rollup_determinism",
            "--test",
            "softnet_stat_determinism",
            "--test",
            "sockstat_determinism",
            "--test",
            "swaps_determinism",
            "--test",
            "thp_stats_determinism",
            "--test",
            "verification_report_cli",
            "--test",
            "verification_report_consumers",
            "--test",
            "writev_determinism",
            "--test",
            "zero_copy_pipe_fallback",
        ]),
        "test.arbitrary_binaries" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "arbitrary_binaries",
        ]),
        "test.cli"
        | "test.isolated_dbt_workdir"
        | "test.cli_on_host"
        | "privileged-test.pmu_cli_cases"
        | "super.liteinst_python3_verify_diagnostics"
        | "super.dbt_pipe_backpressure_diagnostic"
        | "super.dbt_failed_exec_recovery_diagnostic"
        | "super.dbt_unsupported_syscall_aggregation_diagnostic"
        | "super.dbt_strict_blocked_stdin_teardown_diagnostic"
        | "super.dbt_guest_stderr_isolation_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "cli",
        ]),
        "privileged-test.cli_kvm"
        | "privileged-only-test.cli_kvm"
        | "privileged-only-test.cli_kvm_on_host" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-execution-tests",
            "--lib",
            "--test",
            "cli",
        ]),
        "test.liteinst_strict" | "liteinst.strict" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "liteinst_advanced",
        ]),
        "test.sabre_examples" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "sabre_examples",
        ]),
        "test.hermit_modes"
        | "test.hermit_modes_on_host"
        | "privileged-test.pmu_buck_chaos_cases"
        | "super.chaos_hello_race_verification_diagnostic"
        | "super.weekly_relaxed_default_mode_cases"
        | "super.pmu_buck_chaos_cases"
        | "privileged-only-test.pmu_buck_chaos_cases"
        | "privileged-only-test.pmu_buck_chaos_cases_on_host" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "hermit_modes",
        ]),
        "test.app_strict_verify" | "super.managed_jvm_strict_verify_diagnostics" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "app_strict_verify",
        ]),
        "test.command_strict_verify" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "command_strict_verify",
        ]),
        "test.ignored_syscall_regressions" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "epoll_determinism",
            "--test",
            "rcx_canonicalization",
        ]),
        "test.rr_suite_contract" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "rr_suite",
        ]),
        "quick.detcore_unit" => Some(&["-p", "hermit-detcore", "--lib"]),
        "super.relaxed_hermit_flag_matrix" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "relaxed_flag_matrix",
        ]),
        "super.pselect_signal_interruption_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "pselect6_simulation",
        ]),
        "super.record_replay_matrix_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "record_replay",
        ]),
        "super.ipc_determinism_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "ipc_determinism",
        ]),
        "super.random_source_determinism_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "random_determinism",
        ]),
        "super.threaded_integration_matrix_diagnostic" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "integration_matrix",
        ]),
        "super.weekly_portable_chaos_cases" | "super.weekly_ignored_portable_chaos_cases" => {
            Some(&[
                "-p",
                "hermit",
                "--features",
                "third-party-backends",
                "--test",
                "stress_suite",
            ])
        }
        "super.pmu_analyze_hello_race_stress_calibrated_skid" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "analyze",
        ]),
        "super.full_leveldb_strict_determinism" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "leveldb",
        ]),
        "super.sqlite_veryquick_strict_determinism" => Some(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends",
            "--test",
            "sqlite_veryquick",
        ]),
        _ => None,
    }
}

/// Recover the exact authored payload from the one supported execution wrapper.
/// Both the generator and its preparation audit inspect the same command bytes
/// the container's final bash executes, including literal shell quoting.
pub(super) fn execution_command(step: &dagrun::model::Step) -> Result<String, String> {
    if !step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ") {
        return Ok(step.cmd.clone());
    }
    let args = shell_words::split(&step.cmd).map_err(|error| format!("{}: {error}", step.tag()))?;
    let boundary = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| format!("{} omits its pinned-root command boundary", step.tag()))?;
    match &args[boundary + 1..] {
        [shell, option, guard, argv0, payload]
            if shell == "bash"
                && option == "-c"
                && argv0 == "bash"
                && (guard == crate::validation_dag::PINNED_ROOT_COMMAND_GUARD
                    || guard == crate::validation_dag::LEGACY_PINNED_ROOT_COMMAND_GUARD) =>
        {
            Ok(payload.clone())
        }
        _ => Err(format!(
            "{} has an unrecognized pinned-root command",
            step.tag()
        )),
    }
}

fn command_arguments(command: &str, marker: &str) -> Result<Vec<String>, String> {
    let (_, tail) = command
        .split_once(marker)
        .ok_or_else(|| format!("missing {marker}"))?;
    let tail = tail.replace("${CI:+--profile ci}", "");
    let mut quote = None;
    let mut escape = false;
    let mut end = tail.len();
    for (offset, ch) in tail.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escape = true;
            continue;
        }
        if let Some(active) = quote {
            if ch == active {
                quote = None;
            }
        } else if ch == '\'' || ch == '"' {
            quote = Some(ch);
        } else if matches!(ch, ';' | '&' | '|' | '>' | '<') {
            end = offset;
            break;
        }
    }
    let args = shell_words::split(&tail[..end])
        .map_err(|e| format!("cannot parse authored Nextest arguments: {e}"))?;
    Ok(args)
}

pub(super) fn assert_command_selection(step: &dagrun::model::Step) -> Result<(), String> {
    use crate::nextest_binaries::REQUIRED_ENV;
    use crate::nextest_binaries::SELECTION_ENV;
    use crate::nextest_binaries::split_arguments;
    let tag = step.tag();
    let command = execution_command(step)?;
    let raw = step
        .env
        .get(SELECTION_ENV)
        .ok_or_else(|| format!("{tag} has no prepared build selection"))?;
    let expected: Vec<String> = serde_json::from_str(raw).map_err(|e| format!("{tag}: {e}"))?;
    if step.env.get(REQUIRED_ENV).map(String::as_str) != Some("1") {
        return Err(format!(
            "{tag} does not require prepared Nextest executables"
        ));
    }
    for marker in ["run-nextest-counted.sh", "nextest-binaries.rs list"] {
        if !command.contains(marker) {
            continue;
        }
        let mut arguments = command_arguments(&command, marker)?;
        if marker == "run-nextest-counted.sh" {
            match arguments.first().map(String::as_str) {
                Some("--calibration-host") => {
                    arguments.remove(0);
                }
                Some("--calibration-launch-proof") => {
                    if arguments.get(1).map(String::as_str)
                        != Some(crate::nextest_cohort::PINNED_PROOF_PATH)
                    {
                        return Err(format!("{tag} has an unsupported calibration launch proof"));
                    }
                    arguments.drain(..2);
                }
                _ => {}
            }
        }
        let parsed = split_arguments(&arguments).map_err(|error| format!("{tag}: {error}"))?;
        if parsed.build != expected {
            return Err(format!(
                "{tag} command has Cargo selection {:?}, declared {expected:?}",
                parsed.build
            ));
        }
    }
    let direct = "nextest-binaries.rs executable ";
    if let Some((_, rest)) = command.split_once(direct) {
        if command.matches(direct).count() != 1 {
            return Err(format!("{tag} has ambiguous prepared executable lookups"));
        }
        let arguments = rest
            .split_once(')')
            .ok_or_else(|| format!("{tag} has no executable lookup boundary"))?
            .0;
        let arguments = shell_words::split(arguments).map_err(|e| format!("{tag}: {e}"))?;
        if arguments.len() != 2 {
            return Err(format!(
                "{tag} must name one prepared package and test target"
            ));
        }
        let actual = vec![
            "-p".to_string(),
            arguments[0].clone(),
            "--test".to_string(),
            arguments[1].clone(),
        ];
        if actual != expected {
            return Err(format!(
                "{tag} executable lookup selects {actual:?}, declared {expected:?}"
            ));
        }
    }
    if command.contains("cargo nextest list") || command.contains("cargo nextest run") {
        return Err(format!("{tag} can bypass prepared metadata and compile"));
    }
    Ok(())
}

pub(super) fn assert_preparation_dependencies(
    cfg: &dagrun::model::DagConfig,
) -> Result<(), String> {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    use crate::nextest_binaries::SELECTION_ENV;
    use crate::nextest_binaries::config_selections;
    use crate::nextest_binaries::selection_key;
    let by_tag = cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step))
        .collect::<BTreeMap<_, _>>();
    let mut producers = BTreeMap::new();
    for step in &cfg.steps {
        let command = execution_command(step)?;
        if let Some((_, profile)) = command.split_once("./ci/nextest-binaries.rs prepare ") {
            if profile.is_empty() || profile.contains(char::is_whitespace) {
                return Err(format!("{} has an ambiguous prepared profile", step.tag()));
            }
            producers.insert(
                step.tag(),
                (
                    step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh "),
                    config_selections(cfg, profile)?,
                ),
            );
        }
    }
    for step in &cfg.steps {
        let Some(raw) = step.env.get(SELECTION_ENV) else {
            continue;
        };
        let args: Vec<String> = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        let key = selection_key(&args);
        let mut ancestors = BTreeSet::new();
        let mut pending = step.deps.clone();
        while let Some(dependency) = pending.pop() {
            if ancestors.insert(dependency.clone()) {
                let ancestor = by_tag
                    .get(&dependency)
                    .ok_or_else(|| format!("{} has missing dependency {dependency}", step.tag()))?;
                pending.extend(ancestor.deps.iter().cloned());
            }
        }
        if !ancestors.iter().any(|tag| {
            producers.get(tag).is_some_and(|(pinned, selections)| {
                *pinned == step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ")
                    && selections.get(&key) == Some(&args)
            })
        }) {
            return Err(format!(
                "{} can run before any producer in the same filesystem root of its exact Cargo selection {args:?}",
                step.tag()
            ));
        }
    }
    Ok(())
}

/// The feature every Cargo selection that builds the hermit package enables.
const CANONICAL_HERMIT_FEATURE: &str = "third-party-backends";

/// Features that leave the code of the uplifted hermit executable unchanged:
/// the members of the canonical feature, and `kvm-execution-tests`, which
/// gates only `#[cfg(all(test, feature = "kvm-execution-tests"))]` code in
/// hermit-cli/src/lib.rs that a normal binary build never compiles.
/// `kvm-execution-tests` still changes the executable's bytes, because Cargo
/// hashes the enabled feature list into the crate's build metadata. The
/// preparation hashes each uplifted file only after its last Cargo selection
/// (`nextest_binaries::prepare`), so every prepared record still names the
/// bytes its consumers run.
const EXECUTABLE_NEUTRAL_HERMIT_FEATURES: &[&str] = &[
    CANONICAL_HERMIT_FEATURE,
    "dbt",
    "sabre",
    "e9patch",
    "kvm-execution-tests",
];

/// Every prepared or official profile must compile the same hermit code into
/// its uplifted executable.
///
/// A preparation builds its selections one after another. Every selection
/// with an integration-test target also builds the hermit binary, and Cargo
/// uplifts each such build to the single `target/debug/hermit` path that
/// `CARGO_BIN_EXE_hermit` names. If two such selections enabled different
/// executable features, the code every harness runs would depend on which
/// selection happened to build last. Every hermit selection therefore enables
/// the canonical feature. Any further feature that changes the executable's
/// code is accepted only in a selection limited to `--lib`/`--bins` test
/// harnesses, which Cargo never uplifts. This fixes the code, not the bytes:
/// see `EXECUTABLE_NEUTRAL_HERMIT_FEATURES`.
pub(super) fn assert_hermit_selections_carry_canonical_features(
    cfg: &dagrun::model::DagConfig,
) -> Result<(), String> {
    use std::collections::BTreeSet;

    use crate::nextest_binaries::SELECTION_ENV;
    let mut profiles = BTreeSet::from([
        "full".to_string(),
        "portable".to_string(),
        crate::validation_dag::HOSTED_PORTABLE_LABEL.to_string(),
    ]);
    for step in &cfg.steps {
        let command = execution_command(step)?;
        if let Some((_, profile)) = command.split_once("./ci/nextest-binaries.rs prepare ") {
            profiles.insert(profile.to_string());
        }
    }
    for profile in &profiles {
        let selected = dagrun::select_steps_by_labels(cfg, std::slice::from_ref(profile))
            .map_err(|error| format!("profile {profile}: {error}"))?;
        for step in &selected.steps {
            let Some(raw) = step.env.get(SELECTION_ENV) else {
                continue;
            };
            let args: Vec<String> =
                serde_json::from_str(raw).map_err(|error| format!("{}: {error}", step.tag()))?;
            hermit_selection_features(&args).map_err(|reason| {
                format!(
                    "{} in profile {profile} {reason}; selection {args:?}",
                    step.tag()
                )
            })?;
        }
    }
    Ok(())
}

fn hermit_selection_features(args: &[String]) -> Result<(), String> {
    use std::collections::BTreeSet;

    let values = |options: &[&str]| {
        args.windows(2)
            .filter(|pair| options.contains(&pair[0].as_str()))
            .map(|pair| pair[1].as_str())
            .collect::<Vec<_>>()
    };
    let packages = values(&["-p", "--package"]);
    let builds_hermit = if packages.is_empty() {
        !values(&["--exclude"]).contains(&"hermit")
    } else {
        packages.contains(&"hermit")
    };
    if !builds_hermit {
        return Ok(());
    }
    // --all-features would also enable kvm-native-test-support, which
    // rebuilds reverie-kvm and therefore changes the uplifted executable.
    if args.iter().any(|arg| arg == "--all-features") {
        return Err("enables every hermit feature instead of the canonical feature".into());
    }
    let features = values(&["-F", "--features"])
        .into_iter()
        .flat_map(|value| value.split([',', ' ']))
        .filter(|feature| !feature.is_empty())
        .map(|feature| feature.strip_prefix("hermit/").unwrap_or(feature))
        .collect::<BTreeSet<_>>();
    if !features.contains(CANONICAL_HERMIT_FEATURE) {
        return Err(format!(
            "builds hermit without the canonical {CANONICAL_HERMIT_FEATURE} feature"
        ));
    }
    let executable_features = features
        .iter()
        .filter(|feature| !EXECUTABLE_NEUTRAL_HERMIT_FEATURES.contains(feature))
        .collect::<Vec<_>>();
    let harness_only = args
        .iter()
        .filter(|arg| {
            matches!(
                arg.as_str(),
                "--lib"
                    | "--bins"
                    | "--bin"
                    | "--test"
                    | "--tests"
                    | "--all-targets"
                    | "--example"
                    | "--examples"
                    | "--bench"
                    | "--benches"
            )
        })
        .fold(None, |only, arg| {
            Some(only.unwrap_or(true) && matches!(arg.as_str(), "--lib" | "--bins" | "--bin"))
        })
        .unwrap_or(false);
    if !executable_features.is_empty() && !harness_only {
        return Err(format!(
            "enables executable-changing features {executable_features:?} while building the uplifted hermit binary"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nextest_binaries::REQUIRED_ENV;
    use crate::nextest_binaries::SELECTION_ENV;

    #[test]
    fn child_time_rpc_is_prepared_and_executed_in_both_integration_variants() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_preparation_dependencies(&graph).unwrap();
        for tag in ["test.hermit_integration", "test.hermit_integration_on_host"] {
            let step = graph.steps.iter().find(|step| step.tag() == tag).unwrap();
            assert_command_selection(step).unwrap();
            let args: Vec<String> = serde_json::from_str(&step.env[SELECTION_ENV]).unwrap();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "child_time_rpc"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "child_time_rpc")
            );
            assert_eq!(step.env["NEXTEST_EXPECTED_EXECUTED"], "173");
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--test", "clock_determinism"])
            );
            assert!(
                step.integration_test_binaries
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|binary| binary == "clock_determinism")
            );

            let mut omitted_execution = step.clone();
            omitted_execution.cmd = omitted_execution.cmd.replace("--test child_time_rpc ", "");
            assert!(assert_command_selection(&omitted_execution).is_err());

            let mut omitted_preparation = step.clone();
            let mut args = args;
            let position = args.iter().position(|arg| arg == "child_time_rpc").unwrap();
            args.drain(position - 1..=position);
            omitted_preparation
                .env
                .insert(SELECTION_ENV.into(), serde_json::to_string(&args).unwrap());
            assert!(assert_command_selection(&omitted_preparation).is_err());
        }
    }

    #[test]
    fn prepared_metadata_requires_the_consumers_filesystem_root() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_preparation_dependencies(&graph).unwrap();
        let host = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace")
            .unwrap();
        let hosted = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace_on_host")
            .unwrap();
        let image = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace_in_pinned_root")
            .unwrap();
        assert_eq!(execution_command(image).unwrap(), host.cmd);
        for (consumer, producer, wrong_command) in [
            ("test.regular_crates", image.tag(), host.cmd.clone()),
            (
                "test.regular_crates_on_host",
                hosted.tag(),
                image.cmd.clone(),
            ),
        ] {
            let mut wrong_root = graph.clone();
            wrong_root
                .steps
                .iter_mut()
                .find(|step| step.tag() == producer)
                .unwrap()
                .cmd = wrong_command;
            // Check this consumer first while retaining the complete authored
            // preparation population and all other nodes/dependencies.
            wrong_root.steps.sort_by_key(|step| step.tag() != consumer);
            let error = assert_preparation_dependencies(&wrong_root).unwrap_err();
            assert!(
                error.starts_with(consumer) && error.contains("same filesystem root"),
                "{error}"
            );
        }
    }

    #[test]
    fn every_nextest_command_and_inventory_requires_the_declared_preparation() {
        let graph = dagrun::io::dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        assert_preparation_dependencies(&graph).unwrap();
        let original = graph
            .steps
            .iter()
            .find(|step| step.tag() == "privileged-test.cli_kvm")
            .unwrap();
        assert_command_selection(original).unwrap();
        let portable = crate::nextest_binaries::config_selections(&graph, "portable").unwrap();
        let full = crate::nextest_binaries::config_selections(&graph, "full").unwrap();
        let hardware: Vec<String> = serde_json::from_str(&original.env[SELECTION_ENV]).unwrap();
        let hardware_key = crate::nextest_binaries::selection_key(&hardware);
        assert_eq!(full.get(&hardware_key), Some(&hardware));
        assert!(!portable.contains_key(&hardware_key));
        assert_eq!(full.len(), portable.len() + 1);
        for (key, selection) in &portable {
            assert_eq!(full.get(key), Some(selection));
        }
        // The recorder-clock consumers read the broad preparation, exactly
        // like the other hermit integration consumers, and their selection
        // carries the canonical executable feature.
        let clock_args = for_step("test.recorded_clocks")
            .unwrap()
            .iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>();
        assert!(
            clock_args
                .windows(2)
                .any(|pair| pair == ["--features", "third-party-backends"])
        );
        assert_hermit_selections_carry_canonical_features(&graph).unwrap();
        for profile in ["full", "portable", "hosted-portable"] {
            let prepared = crate::nextest_binaries::config_selections(&graph, profile).unwrap();
            assert_eq!(
                prepared.get(&crate::nextest_binaries::selection_key(&clock_args)),
                Some(&clock_args),
                "{profile} preparation omits the recorder-clock selection"
            );
        }
        // RUN1900 appended the inherited -j flag to the strict prepare
        // PROFILE interface of a resizable producer. The broad producers that
        // now prepare these consumers are resizable too, so keep that
        // regression observable and refused for host, pinned-root and hosted
        // commands.
        for (suffix, profile) in [
            ("", "full"),
            ("_in_pinned_root", "full"),
            ("_on_host", "hosted-portable"),
        ] {
            let producer_tag = format!("build.workspace{suffix}");
            let producer = graph
                .steps
                .iter()
                .find(|step| step.tag() == producer_tag)
                .unwrap();
            for width in [1, 4] {
                let rendered = dagrun::model::command_with_inner_jobs(
                    producer,
                    &graph.default_jobs_flag,
                    Some(width),
                );
                assert_eq!(rendered, producer.cmd, "{producer_tag}, width {width}");
                assert_eq!(
                    dagrun::model::env_with_inner_jobs(
                        producer,
                        &graph.default_jobs_env,
                        Some(width),
                    ),
                    Some(("CARGO_BUILD_JOBS".into(), width.to_string())),
                );
                let mut rendered_producer = producer.clone();
                rendered_producer.cmd = rendered;
                assert_eq!(
                    command_arguments(
                        &execution_command(&rendered_producer).unwrap(),
                        "./ci/nextest-binaries.rs ",
                    )
                    .unwrap(),
                    ["prepare", profile],
                );
                let mut inherited = graph.clone();
                let changed = inherited
                    .steps
                    .iter_mut()
                    .find(|step| step.tag() == producer_tag)
                    .unwrap();
                changed.jobs_flag = None;
                changed.cmd = dagrun::model::command_with_inner_jobs(
                    changed,
                    &graph.default_jobs_flag,
                    Some(width),
                );
                assert_eq!(changed.cmd, format!("{} -j {width}", producer.cmd));
                let error = assert_preparation_dependencies(&inherited).unwrap_err();
                let reason = if suffix == "_in_pinned_root" {
                    "has an unrecognized pinned-root command"
                } else {
                    "has an ambiguous prepared profile"
                };
                assert_eq!(error, format!("{producer_tag} {reason}"));
            }
        }
        for (consumer, producer) in [
            ("test.recorded_clocks", "build.e2e_artifact_in_pinned_root"),
            ("test.recorded_clocks_on_host", "build.e2e_artifact_on_host"),
        ] {
            let step = graph
                .steps
                .iter()
                .find(|step| step.tag() == consumer)
                .unwrap();
            assert_command_selection(step).unwrap();
            assert_eq!(step.env["NEXTEST_EXPECTED_EXECUTED"], "6");
            assert!(step.deps.iter().any(|dependency| dependency == producer));
            assert!(
                !step
                    .deps
                    .iter()
                    .any(|dependency| dependency.starts_with("build.recorded_clocks"))
            );
            let args: Vec<String> = serde_json::from_str(&step.env[SELECTION_ENV]).unwrap();
            assert_eq!(args, clock_args);
            assert!(!dagrun::model::step_width_is_resizable(
                step,
                &graph.default_jobs_flag,
                &graph.default_jobs_env,
            ));
            for width in [1, 4] {
                let mut rendered = step.clone();
                rendered.cmd = dagrun::model::command_with_inner_jobs(
                    step,
                    &graph.default_jobs_flag,
                    Some(width),
                );
                assert_eq!(rendered.cmd, step.cmd, "{consumer}, width {width}");
                assert_eq!(
                    dagrun::model::env_with_inner_jobs(step, &graph.default_jobs_env, Some(width)),
                    None,
                );
                let arguments = command_arguments(
                    &execution_command(&rendered).unwrap(),
                    "./ci/run-nextest-counted.sh ",
                )
                .unwrap();
                let parsed = crate::nextest_binaries::split_arguments(&arguments).unwrap();
                assert_eq!(parsed.build, clock_args);
                let jobs = parsed
                    .runtime
                    .windows(2)
                    .filter(|pair| pair[0] == "-j")
                    .map(|pair| pair[1].as_str())
                    .collect::<Vec<_>>();
                assert_eq!(jobs, ["1"], "{consumer} must remain serial");

                // The command already supplies -j 1. Restoring inheritance
                // adds a second flag, which Nextest refuses even at width 1.
                rendered.jobs_flag = None;
                rendered.cmd = dagrun::model::command_with_inner_jobs(
                    &rendered,
                    &graph.default_jobs_flag,
                    Some(width),
                );
                assert_eq!(rendered.cmd, format!("{} -j {width}", step.cmd));
                if consumer == "test.recorded_clocks" {
                    assert_eq!(
                        execution_command(&rendered).unwrap_err(),
                        format!("{consumer} has an unrecognized pinned-root command"),
                    );
                } else {
                    let arguments = command_arguments(
                        &execution_command(&rendered).unwrap(),
                        "./ci/run-nextest-counted.sh ",
                    )
                    .unwrap();
                    let mutated = crate::nextest_binaries::split_arguments(&arguments).unwrap();
                    assert_eq!(mutated.build, parsed.build);
                    let mut duplicate = parsed.runtime;
                    duplicate.extend(["-j".into(), width.to_string()]);
                    assert_eq!(mutated.runtime, duplicate);
                }
            }
        }
        // Dropping the canonical feature from both the declared selection and
        // the command keeps the command audit and the preparation edge
        // consistent, so only the executable-feature invariant refuses it.
        for (replacement, reason) in [
            (
                "-p hermit --test record_replay",
                "builds hermit without the canonical third-party-backends feature",
            ),
            (
                "-p hermit --features third-party-backends,kvm-native-test-support --test record_replay",
                "enables executable-changing features [\"kvm-native-test-support\"] while building the uplifted hermit binary",
            ),
            (
                "-p hermit --all-features --test record_replay",
                "enables every hermit feature instead of the canonical feature",
            ),
        ] {
            let mut changed = graph.clone();
            let step = changed
                .steps
                .iter_mut()
                .find(|step| step.tag() == "test.recorded_clocks")
                .unwrap();
            let authored = "-p hermit --features third-party-backends --test record_replay";
            assert_eq!(step.cmd.matches(authored).count(), 1);
            step.cmd = step.cmd.replace(authored, replacement);
            let args = shell_words::split(replacement)
                .unwrap()
                .into_iter()
                .chain(["--test".into(), "flock_exclusion".into()])
                .collect::<Vec<String>>();
            step.env
                .insert(SELECTION_ENV.into(), serde_json::to_string(&args).unwrap());
            assert_command_selection(step).unwrap();
            assert_preparation_dependencies(&changed).unwrap();
            assert_eq!(
                assert_hermit_selections_carry_canonical_features(&changed).unwrap_err(),
                format!("test.recorded_clocks in profile full {reason}; selection {args:?}"),
            );
        }
        let selection = |args: &[&str]| {
            hermit_selection_features(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        };
        selection(&["-p", "hermit-detcore", "--lib", "--bins"]).unwrap();
        selection(&["--workspace", "--exclude", "hermit"]).unwrap();
        selection(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-execution-tests",
            "--lib",
            "--test",
            "cli",
        ])
        .unwrap();
        selection(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-native-test-support",
            "--lib",
            "--bins",
        ])
        .unwrap();
        selection(&["--workspace"]).unwrap_err();
        selection(&[
            "-p",
            "hermit",
            "--features",
            "third-party-backends,kvm-native-test-support",
        ])
        .unwrap_err();
        // Removing the only edge to the broad producer refuses even while that
        // producer exists elsewhere in the committed graph.
        let mut unprepared = graph.clone();
        unprepared
            .steps
            .sort_by_key(|step| step.tag() != "test.recorded_clocks_on_host");
        unprepared.steps[0].deps = vec!["build.e2e_artifact_on_host".into()];
        assert_preparation_dependencies(&unprepared).unwrap();
        unprepared.steps[0].deps.clear();
        let error = assert_preparation_dependencies(&unprepared).unwrap_err();
        assert!(
            error.starts_with("test.recorded_clocks_on_host")
                && error.contains("same filesystem root"),
            "{error}"
        );
        for tag in ["test.hermit_unit", "test.hermit_unit_on_host"] {
            let step = graph.steps.iter().find(|step| step.tag() == tag).unwrap();
            let selection: Vec<String> = serde_json::from_str(&step.env[SELECTION_ENV]).unwrap();
            assert!(selection.contains(&"third-party-backends,kvm-native-test-support".into()));
            assert!(
                !selection
                    .iter()
                    .any(|arg| arg.contains("kvm-execution-tests"))
            );
        }
        let hosted_producer = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.workspace_on_host")
            .unwrap();
        assert!(
            hosted_producer
                .cmd
                .ends_with("./ci/nextest-binaries.rs prepare hosted-portable")
        );
        let hosted_artifact = graph
            .steps
            .iter()
            .find(|step| step.tag() == "build.e2e_artifact_on_host")
            .unwrap();
        let hosted_unit = graph
            .steps
            .iter()
            .find(|step| step.tag() == "test.hermit_unit_on_host")
            .unwrap();
        assert!(
            hosted_unit
                .deps
                .iter()
                .any(|dependency| dependency == "build.e2e_artifact_on_host")
        );
        assert!(
            hosted_artifact
                .deps
                .iter()
                .any(|dependency| dependency == "build.workspace_on_host")
        );
        let mut missing_hardware = graph.clone();
        let producer = missing_hardware
            .steps
            .iter_mut()
            .find(|step| step.tag() == "build.workspace_in_pinned_root")
            .unwrap();
        assert!(
            execution_command(producer)
                .unwrap()
                .ends_with("./ci/nextest-binaries.rs prepare full")
        );
        producer.cmd = producer.cmd.replace(
            "./ci/nextest-binaries.rs prepare full",
            "./ci/nextest-binaries.rs prepare portable",
        );
        missing_hardware
            .steps
            .sort_by_key(|step| step.tag() != "privileged-test.cli_kvm");
        let error = assert_preparation_dependencies(&missing_hardware).unwrap_err();
        assert!(
            error.starts_with("privileged-test.cli_kvm") && error.contains("same filesystem root"),
            "{error}"
        );
        let direct = graph
            .steps
            .iter()
            .find(|step| step.tag() == "privileged-only-cpuid.faulting")
            .unwrap();
        assert_command_selection(direct).unwrap();
        let mut wrong_target = direct.clone();
        wrong_target.cmd = wrong_target.cmd.replace(
            "executable hermit-detcore tests_misc",
            "executable hermit-detcore tests_parallelism",
        );
        assert!(assert_command_selection(&wrong_target).is_err());
        let mut missing_direct = direct.clone();
        missing_direct.env.remove(SELECTION_ENV);
        assert!(assert_command_selection(&missing_direct).is_err());
        for mutation in ["required", "selection", "run", "list", "raw-cargo"] {
            let mut changed = original.clone();
            match mutation {
                "required" => {
                    changed.env.remove(REQUIRED_ENV);
                }
                "selection" => {
                    changed.env.insert(SELECTION_ENV.into(), "[]".into());
                }
                "run" => {
                    changed.cmd = changed.cmd.replace(
                        "./ci/run-nextest-counted.sh",
                        "./ci/run-nextest-counted.sh --all-features",
                    );
                }
                "list" => {
                    changed.cmd = changed.cmd.replace(
                        "nextest-binaries.rs list",
                        "nextest-binaries.rs list --all-features",
                    );
                }
                "raw-cargo" => {
                    changed.cmd = changed
                        .cmd
                        .replace("./ci/nextest-binaries.rs list", "cargo nextest list");
                }
                _ => unreachable!(),
            }
            assert!(
                assert_command_selection(&changed).is_err(),
                "accepted {mutation}"
            );
        }
        let mut missing = graph.clone();
        missing
            .steps
            .iter_mut()
            .find(|step| step.tag() == "quick.detcore_unit")
            .unwrap()
            .deps
            .retain(|dep| dep != "quick.build");
        assert!(
            assert_preparation_dependencies(&missing)
                .unwrap_err()
                .contains("quick.detcore_unit")
        );
    }

    #[test]
    fn build_selection_parser_keeps_quoted_filter_punctuation_literal() {
        let args = command_arguments("./ci/run-nextest-counted.sh ${CI:+--profile ci} -p hermit --test cli -E 'test(/a; b/)' -- --ignored; exit $?", "run-nextest-counted.sh").unwrap();
        assert_eq!(
            args,
            [
                "-p",
                "hermit",
                "--test",
                "cli",
                "-E",
                "test(/a; b/)",
                "--",
                "--ignored"
            ]
        );
    }
}
