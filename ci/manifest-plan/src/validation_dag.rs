// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Build the one committed validation DAG from validate's declarative sources.

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

pub const OUTPUT: &str = "ci/dag/validate.json";
const EXPECTED_PLAN: &str = "ci/expected-e2e-plan.json";
const SUPER_REPETITIONS: &str = "20";
const DESCRIPTION: &str =
    "Hermit validation superset; select quick, portable, full, super, or privileged by step label";

#[derive(Clone, Copy)]
struct Profile {
    label: &'static str,
    argv: &'static [&'static str],
    direct_steps: usize,
    selected_steps: usize,
}

const PROFILES: [Profile; 5] = [
    Profile {
        label: "full",
        argv: &["full"],
        direct_steps: 259,
        selected_steps: 259,
    },
    Profile {
        label: "portable",
        argv: &["portable-only"],
        direct_steps: 251,
        selected_steps: 251,
    },
    Profile {
        label: "quick",
        argv: &["quick"],
        direct_steps: 13,
        selected_steps: 13,
    },
    Profile {
        label: "super",
        argv: &["super"],
        direct_steps: 143,
        selected_steps: 143,
    },
    Profile {
        label: "privileged",
        argv: &["--privileged-only"],
        direct_steps: 9,
        selected_steps: 14,
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

fn source_plan(root: &Path, scratch: &Path, profile: Profile) -> Result<DagConfig, String> {
    let path = scratch.join(format!("{}.json", profile.label));
    let mut command = Command::new(root.join("scripts/validate.rs"));
    command
        .current_dir(root)
        .args(profile.argv)
        .arg("--write-source-plan")
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
    let output = command.output().map_err(|error| {
        format!(
            "cannot run scripts/validate.rs for {}: {error}",
            profile.label
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "source-plan export for {} failed with {}: {}",
            profile.label,
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

fn replace_generated_token(value: &mut String, prefix: &str, replacement: &str) {
    while let Some(start) = value.find(prefix) {
        let tail = &value[start..];
        let end = tail
            .char_indices()
            .find_map(|(index, ch)| {
                (index >= prefix.len() && (ch.is_ascii_whitespace() || ch == '\'' || ch == '"'))
                    .then_some(index)
            })
            .unwrap_or(tail.len());
        value.replace_range(start..start + end, replacement);
    }
}

fn normalize_step(step: &mut Step, root: &Path, run_state: &Path) -> Result<(), String> {
    let root = root
        .to_str()
        .ok_or_else(|| "repository root is not valid UTF-8".to_string())?;
    let run_state = run_state
        .to_str()
        .ok_or_else(|| "generator scratch path is not valid UTF-8".to_string())?;
    step.labels.clear();
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
    replace_generated_token(
        &mut step.cmd,
        "$PWD/target/real-compat-fixtures-",
        "$VALIDATE_RUN_STATE/super-compat-fixtures",
    );
    if step.tag() == "pre.reverie_pin" {
        let command_start = step
            .cmd
            .find("with-proxy $PWD/ci/run-reverie-pin-check.sh")
            .or_else(|| step.cmd.find("$PWD/ci/run-reverie-pin-check.sh"))
            .ok_or_else(|| format!("{} has an unrecognized command: {}", step.tag(), step.cmd))?;
        let prefix = &step.cmd[..command_start];
        step.cmd = format!(
            "{prefix}if command -v with-proxy >/dev/null 2>&1; then exec with-proxy ./ci/run-reverie-pin-check.sh --repo \"$PWD\"; else exec ./ci/run-reverie-pin-check.sh --repo \"$PWD\"; fi"
        );
    }
    if step.tag() == "build.rust_scripts" {
        step.cpu_timeout = 900;
        step.hint.est_duration_s = 190.0;
        step.hint.rss_baseline_bytes = Some(1024 * 1024 * 1024);
        step.hint.hard_mem_max_bytes = Some(2 * 1024 * 1024 * 1024);
    }
    for (tag, resource) in [
        ("test.cli", "integration_test_binaries.cli"),
        (
            "test.hermit_modes",
            "integration_test_binaries.hermit_modes",
        ),
    ] {
        if step.tag() == tag {
            step.hint.resources.insert(resource.into(), 1);
        }
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

fn rename_map(profile: &str) -> BTreeMap<String, String> {
    let pairs: &[(&str, &str)] = match profile {
        "full" => &[("scorecard.compatibility", "full-scorecard.compatibility")],
        "super" => &[("compatprep.fixtures", "super-compatprep.fixtures")],
        "privileged" => &[
            ("build.manifest_guests", "privileged-build.manifest_guests"),
            (
                "build.privileged_tests",
                "privileged-only-build.privileged_tests",
            ),
            ("cpuid.faulting", "privileged-only-cpuid.faulting"),
            ("pmu.preemption", "privileged-only-pmu.preemption"),
            (
                "test.pmu_buck_chaos_cases",
                "privileged-only-test.pmu_buck_chaos_cases",
            ),
            (
                "e2e.manifest_applications",
                "privileged-only-e2e.manifest_applications",
            ),
            (
                "e2e.manifest_backend_parity_c",
                "privileged-only-e2e.manifest_backend_parity_c",
            ),
            ("test.cli_kvm", "privileged-only-test.cli_kvm"),
            (
                "scorecard.compatibility",
                "privileged-scorecard.compatibility",
            ),
        ],
        _ => &[],
    };
    pairs
        .iter()
        .map(|(old, new)| ((*old).into(), (*new).into()))
        .collect()
}

fn set_tag(step: &mut Step, tag: &str) -> Result<(), String> {
    let (group, job) = tag
        .split_once('.')
        .ok_or_else(|| format!("generated step tag has no group separator: {tag}"))?;
    step.group = group.to_string();
    step.job = job.to_string();
    Ok(())
}

fn apply_renames(cfg: &mut DagConfig, profile: &str) -> Result<(), String> {
    let renames = rename_map(profile);
    for step in &mut cfg.steps {
        let old = step.tag();
        if let Some(new) = renames.get(&old) {
            set_tag(step, new)?;
        }
        for dep in &mut step.deps {
            if let Some(new) = renames.get(dep) {
                *dep = new.clone();
            }
        }
        for explained in &mut step.explains {
            if let Some(new) = renames.get(explained) {
                *explained = new.clone();
            }
        }
        if let Some(family) = &mut step.fail_fast_family {
            if let Some(new) = renames.get(family) {
                *family = new.clone();
            }
        }
    }
    Ok(())
}

fn privileged_direct_tags() -> BTreeSet<&'static str> {
    [
        "privileged-build.manifest_guests",
        "privileged-only-build.privileged_tests",
        "privileged-only-cpuid.faulting",
        "privileged-only-pmu.preemption",
        "privileged-only-test.pmu_buck_chaos_cases",
        "privileged-only-e2e.manifest_applications",
        "privileged-only-e2e.manifest_backend_parity_c",
        "privileged-only-test.cli_kvm",
        "privileged-scorecard.compatibility",
    ]
    .into_iter()
    .collect()
}

fn prepare_profile(
    mut cfg: DagConfig,
    profile: Profile,
    root: &Path,
    run_state: &Path,
) -> Result<DagConfig, String> {
    for step in &mut cfg.steps {
        normalize_step(step, root, run_state)?;
    }
    apply_renames(&mut cfg, profile.label)?;
    let privileged_direct = privileged_direct_tags();
    for step in &mut cfg.steps {
        let labelled =
            profile.label != "privileged" || privileged_direct.contains(step.tag().as_str());
        if labelled {
            step.labels.push(profile.label.to_string());
        }
    }
    cfg.default_step_timeout = 0;
    Ok(cfg)
}

fn comparable_step(step: &Step) -> String {
    let mut step = step.clone();
    step.labels.clear();
    // The full profile owns shared-node rationale. Quick/super keep shorter
    // prose for terminal output, but that prose is not an execution contract.
    step.description.clear();
    let mut cfg = DagConfig::default();
    cfg.steps.push(step);
    dag_to_json(&cfg)
}

fn merge_profile(target: &mut DagConfig, source: DagConfig, profile: &str) -> Result<(), String> {
    for (resource, capacity) in source.resource_caps {
        target
            .resource_caps
            .entry(resource)
            .and_modify(|current| *current = (*current).max(capacity))
            .or_insert(capacity);
    }
    for mut step in source.steps {
        let tag = step.tag();
        if let Some(existing) = target
            .steps
            .iter_mut()
            .find(|candidate| candidate.tag() == tag)
        {
            if comparable_step(existing) != comparable_step(&step) {
                return Err(format!(
                    "profile {profile} collides with a different definition of {tag}"
                ));
            }
            existing.labels.append(&mut step.labels);
            existing.labels.sort();
            existing.labels.dedup();
        } else {
            target.steps.push(step);
        }
    }
    Ok(())
}

fn attach_result_ownership(cfg: &mut DagConfig, cells: &[DagManifest]) -> Result<(), String> {
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
    Ok(())
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
    if cfg.steps.len() != 412 {
        return Err(format!(
            "superset has {} steps, expected 412",
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
    let scratch = Scratch::create()?;
    let mut sources = BTreeMap::new();
    for profile in PROFILES {
        let source = source_plan(root, &scratch.0, profile)?;
        let prepared = prepare_profile(source, profile, root, &scratch.0.join("run-state"))?;
        sources.insert(profile.label, prepared);
    }
    let mut merged = sources
        .remove("full")
        .ok_or_else(|| "full source plan was not generated".to_string())?;
    merged.description = DESCRIPTION.into();
    for profile in ["portable", "quick", "super", "privileged"] {
        let source = sources
            .remove(profile)
            .ok_or_else(|| format!("{profile} source plan was not generated"))?;
        merge_profile(&mut merged, source, profile)?;
    }
    let cells = expected_cells(root)?;
    attach_result_ownership(&mut merged, &cells)?;
    assert_invariants(&merged, &cells)?;
    Ok(merged)
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
    fn generated_token_replacement_consumes_only_one_path_token() {
        let mut value = "$PWD/target/real-compat-fixtures-123/README.md next".to_string();
        replace_generated_token(
            &mut value,
            "$PWD/target/real-compat-fixtures-",
            "$VALIDATE_RUN_STATE/super-compat-fixtures",
        );
        assert_eq!(value, "$VALIDATE_RUN_STATE/super-compat-fixtures next");
    }
}
