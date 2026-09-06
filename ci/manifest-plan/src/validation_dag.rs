// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Refresh and audit the generated partition of the committed validation DAG.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::io::dag_from_json;
use dagrun::io::dag_to_json;
use dagrun::model::DagConfig;
use dagrun::model::DagManifest;
use dagrun::model::Step;
use dagrun::model::result_manifest_owner;
use dagrun::select_steps_by_labels;
use serde::Deserialize;

use crate::runner::E2E_KERNEL_VERSION_ENV;
use crate::runner::E2E_MACHINE_SHORTNAME_ENV;

pub const OUTPUT: &str = "ci/dag/validate.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SUPER_REPETITIONS: &str = "20";
const PINNED_ROOT_FETCH_TAG: &str = "setup.pinned_root_fetch";
const PINNED_ROOT_FETCH_COMMAND: &str = "seed=(); if [ -n \"${CARGO_HOME:-}\" ]; then seed=(--seed-cargo \"$CARGO_HOME\"); fi; ./ci/hermetic/run-split-validate.sh --fetch-only \"${seed[@]}\"";
const PINNED_ROOT_TWIN_SUFFIX: &str = "_in_pinned_root";
pub const HOSTED_PORTABLE_LABEL: &str = "hosted-portable";
const HOSTED_VARIANT_SUFFIX: &str = "_on_host";
const PINNED_ROOT_PRODUCER_STEPS: &[&str] = &[
    "build.rust_scripts",
    "setup.manifest_plan",
    "build.workspace",
    "build.runtime_release",
    "build.e2e_artifact",
    "build.manifest_guests",
    "quick.build",
];
const PINNED_ROOT_FORWARDED_ENV: &[&str] = &[
    "CARGO_BUILD_JOBS",
    "DAGRUN_STEP_STARTED_MONOTONIC_NS",
    "E2E_BUILD_ROOT",
    E2E_KERNEL_VERSION_ENV,
    E2E_MACHINE_SHORTNAME_ENV,
    "E2E_RESULT_ROOT",
    "E2E_RUN_ID",
    "HERMIT_E2E_EMPTY_WORKDIR",
    "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT",
    "L4_REPS",
    "PR_NUMBER",
    "SUPER_REPETITIONS",
    "THIRD_PARTY_BUILD_JOBS",
    "VALIDATE_VERBOSITY",
];
#[derive(Clone, Copy)]
struct Profile {
    label: &'static str,
    direct_steps: usize,
    selected_steps: usize,
}

const PROFILES: [Profile; 6] = [
    Profile {
        label: "full",
        direct_steps: 266,
        selected_steps: 267,
    },
    Profile {
        label: "portable",
        direct_steps: 257,
        selected_steps: 258,
    },
    Profile {
        label: "quick",
        direct_steps: 16,
        selected_steps: 17,
    },
    Profile {
        label: "super",
        direct_steps: 145,
        selected_steps: 146,
    },
    Profile {
        label: "privileged",
        direct_steps: 11,
        selected_steps: 22,
    },
    Profile {
        label: HOSTED_PORTABLE_LABEL,
        direct_steps: 251,
        selected_steps: 251,
    },
];

#[derive(Deserialize)]
struct ExpectedPlan {
    cells: Vec<ExpectedCell>,
}

#[derive(Deserialize)]
struct ExpectedCell {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

impl From<ExpectedCell> for DagManifest {
    fn from(cell: ExpectedCell) -> Self {
        Self {
            lane: cell.lane,
            category: cell.category,
            test: Some(cell.test),
            mode: Some(cell.mode),
            backend: Some(cell.backend),
        }
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Result<Self, String> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("clock is before Unix epoch: {error}"))?
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hermit-generate-validation-dag-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).map_err(|error| {
            format!(
                "cannot create scratch directory {}: {error}",
                path.display()
            )
        })?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("cannot run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn generated_plan(root: &Path, scratch: &Path) -> Result<DagConfig, String> {
    let path = scratch.join("generated.json");
    let mut command = Command::new(root.join("scripts/validate.rs"));
    command
        .current_dir(root)
        .arg("--write-generated-plan")
        .arg(&path);
    command
        .env(
            "HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT",
            "cpuid-faulting,kvm",
        )
        .env("SUPER_REPETITIONS", SUPER_REPETITIONS)
        .env("VALIDATE_VERBOSITY", "1");
    for name in [
        "VALIDATE_LEVEL",
        "VALIDATE_FORCE_FULL",
        "VALIDATE_GATE_TIMEOUT_SECONDS",
        "VALIDATE_GATE_CPU_TIMEOUT_SECONDS",
        "HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS",
        "DAGRUN_CPU_TIMEOUT_MULTIPLIER",
        "DAGRUN_CPU_TIMEOUT_PLATFORM",
    ] {
        command.env_remove(name);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run scripts/validate.rs for generated nodes: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "generated-partition export failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read generated {}: {error}", path.display()))?;
    dag_from_json(&text).map_err(|error| format!("invalid generated {}: {error}", path.display()))
}

fn expected_cells(root: &Path) -> Result<Vec<DagManifest>, String> {
    let path = root.join(EXPECTED_PLAN);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let plan: ExpectedPlan = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    Ok(plan.cells.into_iter().map(Into::into).collect())
}

fn normalize_step(step: &mut Step, root: &Path, run_state: &Path) -> Result<(), String> {
    let root = root
        .to_str()
        .ok_or_else(|| "repository root is not valid UTF-8".to_string())?;
    let run_state = run_state
        .to_str()
        .ok_or_else(|| "generator scratch path is not valid UTF-8".to_string())?;
    step.result_manifests = Some(Vec::new());
    step.cmd = step
        .cmd
        .replace(run_state, "$VALIDATE_RUN_STATE")
        .replace(root, "$PWD");
    step.desc = step
        .desc
        .replace(run_state, "$VALIDATE_RUN_STATE")
        .replace(root, "$PWD");
    step.description = step
        .description
        .replace(run_state, "$VALIDATE_RUN_STATE")
        .replace(root, "$PWD");
    for value in step.env.values_mut() {
        *value = value
            .replace(run_state, "$VALIDATE_RUN_STATE")
            .replace(root, "$PWD");
    }
    if step.timeout <= 0 || step.cpu_timeout <= 0 {
        return Err(format!(
            "{} does not carry explicit wall/CPU budgets: wall={} cpu={}",
            step.tag(),
            step.timeout,
            step.cpu_timeout
        ));
    }
    if step.hint.rss_baseline_bytes.is_none() && step.hint.hard_mem_max_bytes.is_none() {
        return Err(format!(
            "{} does not carry an explicit memory budget",
            step.tag()
        ));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&byte))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn pinned_root_twin_tag(tag: &str) -> String {
    format!("{tag}{PINNED_ROOT_TWIN_SUFFIX}")
}

fn is_manifest_run(step: &Step) -> bool {
    step.manifest.is_some() || (step.group == "quick" && step.job == "e2e_verify")
}

fn is_hosted_variant(step: &Step) -> bool {
    step.job.ends_with(HOSTED_VARIANT_SUFFIX)
}

fn is_pinned_root_producer(step: &Step) -> bool {
    PINNED_ROOT_PRODUCER_STEPS.contains(&step.tag().as_str())
        || step.job == "manifest_guests"
        || (step.job == "privileged_tests"
            && step.cmd.contains("cargo ")
            && step.cmd.contains("publish-hermit-e2e-artifact.sh"))
}

fn pinned_root_command(step: &Step) -> String {
    let mut env_names = PINNED_ROOT_FORWARDED_ENV
        .iter()
        .copied()
        .collect::<BTreeSet<_>>();
    if is_manifest_run(step) {
        env_names.insert("DAGRUN_TEST_COUNTS_PATH");
    }
    env_names.extend(step.env.keys().map(String::as_str));
    let mut argv = vec![
        "./ci/hermetic/run-in-pinned-root.sh".to_string(),
        "--src".into(),
        ".".into(),
        "--out".into(),
        "ignored/hermetic/split".into(),
        "--src-rw".into(),
        "--cargo-home".into(),
        "ignored/hermetic/split/cargo".into(),
    ];
    for name in env_names {
        argv.extend(["--env".into(), name.into()]);
    }
    argv.extend([
        "--".into(),
        "bash".into(),
        "-c".into(),
        "/src/ci/hermetic/assert-no-network.sh && \
         /src/ci/hermetic/assert-build-dependencies.sh && exec bash -c \"$1\""
            .into(),
        "bash".into(),
        step.cmd.clone(),
    ]);
    argv.iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

fn pinned_root_fetch() -> Result<Step, String> {
    let text = format!(
        r#"{{"description":"Pinned-root fetch node","steps":[{{"group":"setup","job":"pinned_root_fetch","desc":"Fetch locked Cargo inputs","description":"Fetch locked Cargo inputs before network-disabled pinned-root commands.","cmd":{},"deps":[],"env":{{"VALIDATE_VERBOSITY":"1"}},"labels":[],"result_manifests":[],"timeout":600,"cpu_timeout":600,"hint":{{"hard_mem_max_bytes":1073741824}},"fail_fast_family":"setup.pinned_root_fetch"}}]}}"#,
        serde_json::to_string(PINNED_ROOT_FETCH_COMMAND).expect("constant is serializable")
    );
    dag_from_json(&text)
        .map_err(|error| format!("internal pinned-root fetch node is invalid: {error}"))?
        .steps
        .into_iter()
        .next()
        .ok_or_else(|| "internal pinned-root fetch node disappeared".to_string())
}

/// Restore the explicit hosted selection after refreshing generated rows.
///
/// The selection itself is committed in `validate.json`. Generated compat
/// rows may be replaced during maintenance, so their hosted label is restored
/// by typed step identity from the previous committed selection. No runtime
/// path calls this function.
fn restore_hosted_portable_selection(
    cfg: &mut DagConfig,
    hosted: &DagConfig,
) -> Result<(), String> {
    let expected = hosted.steps.iter().map(Step::tag).collect::<BTreeSet<_>>();
    if expected.len() != 251 {
        return Err(format!(
            "committed {HOSTED_PORTABLE_LABEL} selection has {} unique steps, expected 251",
            expected.len()
        ));
    }
    for step in &mut cfg.steps {
        step.labels.retain(|label| label != HOSTED_PORTABLE_LABEL);
        if expected.contains(&step.tag()) {
            step.labels.push(HOSTED_PORTABLE_LABEL.into());
            step.labels.sort();
            step.labels.dedup();
        }
    }
    let actual = cfg
        .steps
        .iter()
        .filter(|step| {
            step.labels
                .iter()
                .any(|label| label == HOSTED_PORTABLE_LABEL)
        })
        .map(Step::tag)
        .collect::<BTreeSet<_>>();
    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "refreshed DAG lost {HOSTED_PORTABLE_LABEL} step(s): {}",
            missing.join(", ")
        ));
    }
    let variants = actual
        .iter()
        .filter(|tag| tag.ends_with(HOSTED_VARIANT_SUFFIX))
        .count();
    if variants != 14 {
        return Err(format!(
            "{HOSTED_PORTABLE_LABEL} selection has {variants} host-only variants, expected 14"
        ));
    }
    Ok(())
}

/// Materialize the pinned-root execution split as ordinary committed nodes.
///
/// This transform belongs to maintenance-time generation. Runtime validation
/// sends the selected committed graph to dagrun without cloning producers or
/// rewriting commands/dependencies. Existing twins are replaced from their
/// host-side producers, while already-wrapped manifest commands remain the
/// authored source text in the sole committed DAG.
fn materialize_pinned_root(cfg: &mut DagConfig) -> Result<(), String> {
    cfg.steps.retain(|step| {
        step.tag() != PINNED_ROOT_FETCH_TAG && !step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX)
    });

    let producers = cfg
        .steps
        .iter()
        .filter(|step| is_pinned_root_producer(step))
        .cloned()
        .collect::<Vec<_>>();
    let producer_tags = producers.iter().map(Step::tag).collect::<BTreeSet<_>>();
    let has_rust_scripts = producer_tags.contains("build.rust_scripts");

    for step in &mut cfg.steps {
        if is_hosted_variant(step) {
            continue;
        }
        if !is_manifest_run(step) {
            continue;
        }
        step.env
            .insert("HERMIT_E2E_EMPTY_WORKDIR".into(), "/test".into());
        step.deps = step
            .deps
            .iter()
            .map(|dependency| {
                if producer_tags.contains(dependency) {
                    pinned_root_twin_tag(dependency)
                } else {
                    dependency.clone()
                }
            })
            .collect();
        if step.group.starts_with("privileged")
            && producer_tags.contains("build.e2e_artifact")
            && !step
                .deps
                .iter()
                .any(|dependency| dependency == "build.e2e_artifact_in_pinned_root")
        {
            step.deps.push("build.e2e_artifact_in_pinned_root".into());
        }
        if !step
            .deps
            .iter()
            .any(|dependency| dependency == PINNED_ROOT_FETCH_TAG)
        {
            step.deps.push(PINNED_ROOT_FETCH_TAG.into());
        }
        step.deps.sort();
        step.deps.dedup();
        if !step.cmd.starts_with("./ci/hermetic/run-in-pinned-root.sh ") {
            step.cmd = pinned_root_command(step);
        }
    }

    let mut twins = Vec::with_capacity(producers.len());
    for producer in &producers {
        let mut twin = producer.clone();
        twin.job.push_str(PINNED_ROOT_TWIN_SUFFIX);
        twin.labels.retain(|label| label != HOSTED_PORTABLE_LABEL);
        twin.deps = producer
            .deps
            .iter()
            .filter(|dependency| producer_tags.contains(*dependency))
            .map(|dependency| pinned_root_twin_tag(dependency))
            .collect();
        if producer.tag() != "build.rust_scripts" && has_rust_scripts {
            twin.deps.push("build.rust_scripts_in_pinned_root".into());
        }
        if producer.job == "manifest_guests" && producer_tags.contains("setup.manifest_plan") {
            twin.deps.push("setup.manifest_plan_in_pinned_root".into());
        }
        twin.deps.push(PINNED_ROOT_FETCH_TAG.into());
        twin.deps.sort();
        twin.deps.dedup();
        twin.env
            .insert("HERMIT_E2E_EMPTY_WORKDIR".into(), "/test".into());
        twin.cmd = pinned_root_command(&twin);
        twins.push(twin);
    }

    cfg.steps.push(pinned_root_fetch()?);
    cfg.steps.extend(twins);
    Ok(())
}

fn materialize_runtime_policy(cfg: &mut DagConfig) {
    for step in &mut cfg.steps {
        if step.fail_fast_family.is_none() {
            step.fail_fast_family = Some(step.tag());
        }
        // The CLI verbosity is scheduler invocation policy. Leaving a fixed
        // value on every node both duplicates that policy and prevents a
        // caller-selected level from reaching child helpers.
        step.env.remove("VALIDATE_VERBOSITY");
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum GeneratedPartition {
    PortableCompat,
    PortableFocusedCompat,
    StrictCompat,
    SabreCompat,
    E9patchCompat,
    RrCompat,
    SuperCompat,
    SuperStress,
}

fn generated_partition(step: &Step) -> Option<GeneratedPartition> {
    match step.group.as_str() {
        "portablecompat" | "portablecompatprep" => {
            return Some(GeneratedPartition::PortableFocusedCompat);
        }
        "strictcompat" | "strictcompatprep" => return Some(GeneratedPartition::StrictCompat),
        "sabrecompat" | "sabrecompatprep" => return Some(GeneratedPartition::SabreCompat),
        "e9patchcompat" | "e9patchcompatprep" => {
            return Some(GeneratedPartition::E9patchCompat);
        }
        "rrcompat" | "rrcompatprep" => return Some(GeneratedPartition::RrCompat),
        _ => {}
    }
    if step.group == "superstress" {
        return Some(GeneratedPartition::SuperStress);
    }
    if step.tag() == "super-compatprep.fixtures"
        || (step.group == "compat" && step.labels.iter().any(|label| label == "super"))
    {
        return Some(GeneratedPartition::SuperCompat);
    }
    if step.tag() == "compatprep.fixtures"
        || (step.group == "compat"
            && step
                .labels
                .iter()
                .any(|label| matches!(label.as_str(), "portable" | "full")))
    {
        return Some(GeneratedPartition::PortableCompat);
    }
    None
}

fn refresh_generated_partitions(
    mut committed: DagConfig,
    generated: DagConfig,
) -> Result<DagConfig, String> {
    let mut replacements = BTreeMap::<GeneratedPartition, Vec<Step>>::new();
    for step in generated.steps {
        let Some(partition) = generated_partition(&step) else {
            if step.labels == ["generator-dependency-anchor"] || step.tag() == "build.rust_scripts"
            {
                continue;
            }
            return Err(format!(
                "generated-plan exporter emitted static-looking node {}",
                step.tag()
            ));
        };
        replacements.entry(partition).or_default().push(step);
    }
    for (partition, expected) in [
        (GeneratedPartition::PortableCompat, 190usize),
        (GeneratedPartition::PortableFocusedCompat, 190usize),
        (GeneratedPartition::StrictCompat, 194usize),
        (GeneratedPartition::SabreCompat, 213usize),
        (GeneratedPartition::E9patchCompat, 174usize),
        (GeneratedPartition::RrCompat, 140usize),
        (GeneratedPartition::SuperCompat, 5usize),
        (GeneratedPartition::SuperStress, 102usize),
    ] {
        let actual = replacements.get(&partition).map_or(0, Vec::len);
        if actual != expected {
            return Err(format!(
                "generated {partition:?} partition has {actual} nodes, expected {expected}"
            ));
        }
    }

    let mut inserted = BTreeSet::new();
    let mut steps = Vec::with_capacity(committed.steps.len());
    for step in committed.steps {
        let Some(partition) = generated_partition(&step) else {
            steps.push(step);
            continue;
        };
        if inserted.insert(partition) {
            steps.extend(
                replacements
                    .remove(&partition)
                    .expect("validated generated partition exists"),
            );
        }
    }
    // A newly introduced derived partition has no committed anchor yet. Append
    // it once in enum order; subsequent generations replace it in place.
    for partition in [
        GeneratedPartition::PortableFocusedCompat,
        GeneratedPartition::StrictCompat,
        GeneratedPartition::SabreCompat,
        GeneratedPartition::E9patchCompat,
        GeneratedPartition::RrCompat,
    ] {
        if inserted.insert(partition) {
            steps.extend(
                replacements
                    .remove(&partition)
                    .expect("validated generated partition exists"),
            );
        }
    }
    if inserted.len() != 8 {
        return Err(format!(
            "committed DAG has anchors for {} of 8 generated partitions",
            inserted.len()
        ));
    }
    committed.steps = steps;
    Ok(committed)
}

fn attach_result_ownership(cfg: &mut DagConfig, cells: &[DagManifest]) {
    for step in &mut cfg.steps {
        let mut owned = if let Some(selector) = &step.manifest {
            cells
                .iter()
                .filter(|cell| cell.lane == selector.lane && cell.category == selector.category)
                .cloned()
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if step.tag() == "quick.e2e_verify" {
            owned.extend(
                cells
                    .iter()
                    .filter(|cell| {
                        cell.lane == "portable"
                            && cell.mode.as_deref() == Some("verify")
                            && cell.backend.as_deref() == Some("ptrace")
                    })
                    .cloned(),
            );
        }
        owned.sort_by_key(result_identity);
        step.result_manifests = Some(owned);
    }
}

fn result_identity(result: &DagManifest) -> String {
    format!(
        "{}/{}/{}/{}/{}",
        result.lane,
        result.category,
        result.test.as_deref().unwrap_or(""),
        result.mode.as_deref().unwrap_or(""),
        result.backend.as_deref().unwrap_or("")
    )
}

fn expected_for_label<'a>(label: &str, cells: &'a [DagManifest]) -> Vec<&'a DagManifest> {
    cells
        .iter()
        .filter(|cell| match label {
            "full" => true,
            "portable" => cell.lane == "portable",
            HOSTED_PORTABLE_LABEL => cell.lane == "portable",
            "privileged" => cell.lane == "privileged",
            "quick" => {
                cell.lane == "portable"
                    && cell.mode.as_deref() == Some("verify")
                    && cell.backend.as_deref() == Some("ptrace")
            }
            "super" => false,
            _ => false,
        })
        .collect()
}

fn assert_invariants(cfg: &DagConfig, cells: &[DagManifest]) -> Result<(), String> {
    if cfg.steps.len() != 1368 {
        return Err(format!(
            "superset has {} steps, expected 1368",
            cfg.steps.len()
        ));
    }
    let canonical = dag_to_json(cfg);
    let reparsed = dag_from_json(&canonical)
        .map_err(|error| format!("generated DAG fails strict reload: {error}"))?;
    if dag_to_json(&reparsed) != canonical {
        return Err("generated DAG is not byte-stable across a strict reload".into());
    }
    for profile in PROFILES {
        let direct = cfg
            .steps
            .iter()
            .filter(|step| step.labels.iter().any(|label| label == profile.label))
            .count();
        if direct != profile.direct_steps {
            return Err(format!(
                "{} label has {direct} direct steps, expected {}",
                profile.label, profile.direct_steps
            ));
        }
        let selected = select_steps_by_labels(cfg, &[profile.label.to_string()])
            .map_err(|error| format!("{} label selection failed: {error}", profile.label))?;
        if selected.steps.len() != profile.selected_steps {
            return Err(format!(
                "{} label closes over {} steps, expected {}",
                profile.label,
                selected.steps.len(),
                profile.selected_steps
            ));
        }
        for result in expected_for_label(profile.label, cells) {
            result_manifest_owner(&selected.steps, result)
                .map_err(|error| format!("{} result ownership failed: {error}", profile.label))?;
        }
        if profile.label == "super"
            && selected
                .steps
                .iter()
                .any(|step| !step.effective_result_manifests().is_empty())
        {
            return Err("super selection unexpectedly owns manifest result rows".into());
        }
        if profile.label == HOSTED_PORTABLE_LABEL {
            let pinned = selected
                .steps
                .iter()
                .filter(|step| {
                    step.tag() == PINNED_ROOT_FETCH_TAG
                        || step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX)
                        || step.cmd.contains("run-in-pinned-root.sh")
                })
                .map(Step::tag)
                .collect::<Vec<_>>();
            if !pinned.is_empty() {
                return Err(format!(
                    "{HOSTED_PORTABLE_LABEL} selection contains local pinned-root step(s): {}",
                    pinned.join(", ")
                ));
            }
        }
    }
    let known_results = cells.iter().map(result_identity).collect::<BTreeSet<_>>();
    for step in &cfg.steps {
        if step.result_manifests.is_none() {
            return Err(format!("{} omits explicit result ownership", step.tag()));
        }
        if step.timeout <= 0 || step.cpu_timeout <= 0 {
            return Err(format!("{} omits an explicit wall/CPU budget", step.tag()));
        }
        for result in step.effective_result_manifests() {
            let identity = result_identity(result);
            if !known_results.contains(&identity) {
                return Err(format!("{} owns unknown result {identity}", step.tag()));
            }
        }
        for forbidden in ["dagrun run", "scripts/validate.rs", "pressure-test.rs"] {
            if step.cmd.contains(forbidden) {
                return Err(format!(
                    "{} contains forbidden nested scheduler boundary {forbidden:?}",
                    step.tag()
                ));
            }
        }
    }
    Ok(())
}

pub fn generate(root: &Path) -> Result<DagConfig, String> {
    let committed_path = root.join(OUTPUT);
    let committed_text = fs::read_to_string(&committed_path)
        .map_err(|error| format!("cannot read {}: {error}", committed_path.display()))?;
    let committed = dag_from_json(&committed_text)
        .map_err(|error| format!("invalid {}: {error}", committed_path.display()))?;
    let hosted =
        select_steps_by_labels(&committed, &[HOSTED_PORTABLE_LABEL.into()]).map_err(|error| {
            format!("committed DAG has no valid {HOSTED_PORTABLE_LABEL} selection: {error}")
        })?;
    let scratch = Scratch::create()?;
    let mut generated = generated_plan(root, &scratch.0)?;
    for step in &mut generated.steps {
        normalize_step(step, root, &scratch.0.join("run-state"))?;
    }
    let cells = expected_cells(root)?;
    let mut refreshed = refresh_generated_partitions(committed, generated)?;
    restore_hosted_portable_selection(&mut refreshed, &hosted)?;
    materialize_pinned_root(&mut refreshed)?;
    materialize_runtime_policy(&mut refreshed);
    attach_result_ownership(&mut refreshed, &cells);
    assert_invariants(&refreshed, &cells)?;
    Ok(refreshed)
}

pub fn canonical_text(cfg: &DagConfig) -> String {
    format!("{}\n", dag_to_json(cfg))
}

pub fn require_fresh(committed: &str, generated: &str) -> Result<(), String> {
    if committed == generated {
        return Ok(());
    }
    let first = committed
        .lines()
        .zip(generated.lines())
        .position(|(left, right)| left != right)
        .map(|line| line + 1)
        .unwrap_or_else(|| committed.lines().count().min(generated.lines().count()) + 1);
    Err(format!(
        "{OUTPUT} is stale (first differing line {first}); regenerate with: \
         cargo run -p hermit-manifest-plan --bin generate-validation-dag -- --write"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(test: &str) -> DagManifest {
        DagManifest {
            lane: "portable".into(),
            category: "applications".into(),
            test: Some(test.into()),
            mode: Some("verify".into()),
            backend: Some("ptrace".into()),
        }
    }

    fn owner(result_manifests: Vec<DagManifest>) -> Step {
        let text = r#"{"description":"","steps":[{"group":"e2e","job":"owner","cmd":"true","timeout":1,"cpu_timeout":1,"hint":{"rss_baseline_bytes":1,"hard_mem_max_bytes":1}}]}"#;
        let mut step = dag_from_json(text).unwrap().steps.remove(0);
        step.result_manifests = Some(result_manifests);
        step
    }

    #[test]
    fn freshness_is_exact_and_detects_every_contract_axis() {
        let baseline = r#"{
  "cmd": "run",
  "deps": ["build.x"],
  "labels": ["portable"],
  "cpu_timeout": 30,
  "result_manifests": [{"lane":"portable"}],
  "resource_caps": {"guest": 1}
}
"#;
        assert!(require_fresh(baseline, baseline).is_ok());
        for (from, to) in [
            ("\"run\"", "\"run changed\""),
            ("build.x", "build.y"),
            ("portable", "quick"),
            ("30", "31"),
            ("manifest", "manifest_changed"),
            ("\"guest\": 1", "\"guest\": 2"),
        ] {
            let changed = baseline.replacen(from, to, 1);
            assert!(
                require_fresh(baseline, &changed).is_err(),
                "mutation {from:?} passed"
            );
        }
    }

    #[test]
    fn result_ownership_accepts_one_owner_and_refuses_zero_or_two() {
        let result = exact("applications/echo");
        let first = owner(vec![result.clone()]);
        assert_eq!(
            result_manifest_owner(std::slice::from_ref(&first), &result)
                .unwrap()
                .tag(),
            "e2e.owner"
        );
        assert!(
            result_manifest_owner(&[owner(Vec::new())], &result)
                .unwrap_err()
                .contains("no owning step")
        );
        let mut second = first.clone();
        second.job = "duplicate".into();
        assert!(
            result_manifest_owner(&[first, second], &result)
                .unwrap_err()
                .contains("multiple owning steps")
        );
    }

    #[test]
    fn refresh_replaces_generated_mutations_but_preserves_static_edits() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        let generated = committed.with_steps(
            committed
                .steps
                .iter()
                .filter(|step| generated_partition(step).is_some())
                .cloned()
                .collect(),
        );

        let mut generated_mutation = committed.clone();
        generated_mutation
            .steps
            .iter_mut()
            .find(|step| step.tag() == "compat.echo")
            .unwrap()
            .cmd
            .push_str(" --planted-generated-mutation");
        let refreshed =
            refresh_generated_partitions(generated_mutation, generated.clone()).unwrap();
        assert!(
            !refreshed
                .steps
                .iter()
                .find(|step| step.tag() == "compat.echo")
                .unwrap()
                .cmd
                .contains("planted-generated-mutation")
        );

        let mut static_edit = committed;
        static_edit
            .steps
            .iter_mut()
            .find(|step| step.tag() == "quick.run_smoke")
            .unwrap()
            .description = "intentional static edit".into();
        let refreshed = refresh_generated_partitions(static_edit, generated).unwrap();
        assert_eq!(
            refreshed
                .steps
                .iter()
                .find(|step| step.tag() == "quick.run_smoke")
                .unwrap()
                .description,
            "intentional static edit"
        );
    }

    #[test]
    fn hosted_selection_is_complete_and_excludes_local_pinned_root_steps() {
        let committed = dag_from_json(include_str!("../../dag/validate.json")).unwrap();
        let selected =
            select_steps_by_labels(&committed, &[HOSTED_PORTABLE_LABEL.to_string()]).unwrap();
        assert_eq!(selected.steps.len(), 251);
        assert_eq!(
            selected
                .steps
                .iter()
                .filter(|step| is_hosted_variant(step))
                .count(),
            14
        );
        assert!(selected.steps.iter().all(|step| {
            step.tag() != PINNED_ROOT_FETCH_TAG
                && !step.job.ends_with(PINNED_ROOT_TWIN_SUFFIX)
                && !step.cmd.contains("run-in-pinned-root.sh")
        }));
        let cells = expected_cells(&repo_root().unwrap()).unwrap();
        for result in expected_for_label(HOSTED_PORTABLE_LABEL, &cells) {
            result_manifest_owner(&selected.steps, result).unwrap();
        }

        let mut planted_pinned_command = committed.clone();
        planted_pinned_command
            .steps
            .iter_mut()
            .find(|step| step.tag() == "e2e.manifest_applications_on_host")
            .unwrap()
            .cmd
            .push_str(" && ./ci/hermetic/run-in-pinned-root.sh");
        let error = assert_invariants(&planted_pinned_command, &cells).unwrap_err();
        assert!(error.contains("local pinned-root step"), "{error}");

        let mut planted_coverage_loss = committed;
        planted_coverage_loss
            .steps
            .iter_mut()
            .find(|step| step.tag() == "check.dagrun_naming")
            .unwrap()
            .labels
            .retain(|label| label != HOSTED_PORTABLE_LABEL);
        let error = assert_invariants(&planted_coverage_loss, &cells).unwrap_err();
        assert!(
            error.contains("hosted-portable label has 250 direct steps"),
            "{error}"
        );
    }
}
