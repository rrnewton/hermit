use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;

use dagrun::TestResult;
use dagrun::TestResults;
use hermit_manifest_plan::runner::CellResult;
use hermit_manifest_plan::runner::FailureClass;
use hermit_manifest_plan::runner::MAX_ATTEMPTS_PER_CELL;
use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::append_result;
use hermit_manifest_plan::runner::cell_result_after_retries;
use hermit_manifest_plan::runner::cell_result_and_attempts_after_retries;
use hermit_manifest_plan::runner::checked_add_cpu_usage;
use hermit_manifest_plan::runner::host_inapplicable_result;
use hermit_manifest_plan::runner::infrastructure_error_result;
use hermit_manifest_plan::runner::prepare_result_path;
use hermit_manifest_plan::runner::requires_capability;
use hermit_manifest_plan::runner::run_cell;
use hermit_manifest_plan::runner::write_junit;
use hermit_manifest_plan::stress_series::HostCapabilities;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapability;
#[cfg(test)]
use hermit_manifest_plan::stress_series::HostCapabilityVerdict;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

const EXPECTED_PLAN_SCHEMA: u64 = 1;

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("test-harness: {message}");
    std::process::exit(2);
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

#[derive(Default)]
struct Args {
    selection: Selection,
    prebuilt: bool,
    allow_empty: bool,
    ci_only: bool,
    probe_disabled: bool,
    results: Option<PathBuf>,
    junit: Option<PathBuf>,
    format: String,
    jobs: Option<usize>,
}

/// Take a single-valued selection flag, refusing a second occurrence.
///
/// ⚠️ LAST-VALUE-WINS SILENTLY UNDID THIS FILE'S OWN GUARD, WHICH IS WHY THIS EXISTS.
/// `plan` refuses a `--test` naming no known id. With plain assignment a SECOND
/// `--test` overwrote the first, so the unknown one was never looked up at all:
///
/// ```text
/// plan --lane portable --test no-such-test-xyz                             rc=2
/// plan --lane portable --test no-such-test-xyz --test applications/...     rc=0   []
/// plan --lane portable --test applications/... --test no-such-test-xyz     rc=2
/// ```
///
/// Measured 2026-08-26 at `979a50b17a75` by `agent(codex-rev-2686)` and confirmed
/// independently by `agent(hermit-012)` and by me. The asymmetry is the tell: the
/// same two ids in the other order refuse, because only the LAST occurrence is ever
/// examined. A bisection driver reading rc=0 there sees "nothing failed" for a list
/// containing an id that does not exist -- the exact silent green this guard was
/// added to remove, reappearing one layer up in the argument parser.
///
/// `--jobs` already refused a repeat; the selection flags did not. Refusing is right
/// rather than taking the first or the last, because a repeated selector has no
/// defensible meaning: the caller asked for two different things and we cannot serve
/// both from one field.
fn set_once(slot: &mut Option<String>, values: &mut impl Iterator<Item = String>, flag: &str) {
    let value = required_value(values, flag);
    if slot.replace(value).is_some() {
        fail(format!(
            "{flag} may be specified only once; a repeat silently overwrote the first \
             value, so an earlier id was never validated"
        ));
    }
}

fn parse(mut values: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        format: "text".into(),
        ..Args::default()
    };
    while let Some(value) = values.next() {
        match value.as_str() {
            "--lane" => set_once(&mut args.selection.lane, &mut values, "--lane"),
            "--category" => set_once(&mut args.selection.category, &mut values, "--category"),
            "--test" => set_once(&mut args.selection.test, &mut values, "--test"),
            "--mode" => set_once(&mut args.selection.mode, &mut values, "--mode"),
            "--backend" => set_once(&mut args.selection.backend, &mut values, "--backend"),
            "--ci-only" => {
                args.ci_only = true;
                args.selection.population = Some(Population::Required);
            }
            "--include-occasional" => args.selection.include_occasional = true,
            "--include-manual" => args.selection.include_manual = true,
            "--probe-disabled" => {
                args.probe_disabled = true;
                args.selection.population = Some(Population::Disabled);
            }
            "--prebuilt" => args.prebuilt = true,
            "--allow-empty" => args.allow_empty = true,
            "--results" => {
                args.results = Some(PathBuf::from(required_value(&mut values, "--results")))
            }
            "--junit" => args.junit = Some(PathBuf::from(required_value(&mut values, "--junit"))),
            "--format" => args.format = required_value(&mut values, "--format"),
            "--jobs" => {
                let value = required_value(&mut values, "--jobs");
                let jobs = value
                    .parse::<usize>()
                    .ok()
                    .filter(|jobs| *jobs > 0)
                    .unwrap_or_else(|| fail("--jobs requires a positive integer"));
                if args.jobs.replace(jobs).is_some() {
                    fail("--jobs may be specified only once");
                }
            }
            other => fail(format!("unknown option {other}")),
        }
    }
    args
}

fn structured_test_results(histories: &[Vec<CellResult>]) -> Result<TestResults, String> {
    let rows = histories
        .iter()
        .map(|history| {
            let (result, attempts) = cell_result_and_attempts_after_retries(history)?;
            Ok((result.outcome != "HOST-INAPPLICABLE").then(|| {
                (
                    format!(
                        "{} [{}/{}]",
                        result.test,
                        result.backend.as_deref().unwrap_or("native"),
                        result.mode
                    ),
                    result.outcome == "PASS",
                    attempts,
                )
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    structured_test_results_from_rows(rows.into_iter().flatten())
}

fn structured_test_results_from_rows(
    rows: impl IntoIterator<Item = (String, bool, u64)>,
) -> Result<TestResults, String> {
    let rows = rows
        .into_iter()
        .map(|(id, passed, attempts)| TestResult::new(id, passed, attempts))
        .collect::<Result<Vec<_>, _>>()?;
    TestResults::current(
        u64::try_from(rows.len()).map_err(|_| "cell result count does not fit u64")?,
        0,
        rows,
    )
}

fn accumulate_cell_cpu_usage(
    total: &mut Option<u64>,
    measurements: &mut usize,
    outcome: &str,
    usage: Option<u64>,
) {
    if outcome != "HOST-INAPPLICABLE" {
        *measurements += 1;
        *total = checked_add_cpu_usage(*total, usage);
    }
}

fn host_inapplicable_reason(
    requires: &[String],
    verdicts: &HostCapabilities,
) -> Option<(Vec<String>, String)> {
    let mut absent = requires
        .iter()
        .filter_map(|token| requires_capability(token).ok().flatten())
        .filter_map(|capability| {
            verdicts
                .get(&capability)
                .filter(|verdict| !verdict.present)
                .map(|verdict| (capability.value().to_string(), verdict.evidence.clone()))
        })
        .collect::<Vec<_>>();
    absent.sort();
    absent.dedup();
    if absent.is_empty() {
        return None;
    }
    let capabilities = absent
        .iter()
        .map(|(capability, _)| capability.clone())
        .collect::<Vec<_>>();
    let reason = format!(
        "NOT RUN, NOT a pass, no coverage: this machine lacks {}",
        absent
            .iter()
            .map(|(capability, evidence)| format!("{capability} ({evidence})"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Some((capabilities, reason))
}

fn scheduled_worker_capacity(args: &Args) -> ScheduledWorkerCapacity {
    ScheduledWorkerCapacity::new(args.jobs.unwrap_or(1))
}

fn required_value(values: &mut impl Iterator<Item = String>, option: &str) -> String {
    let value = values
        .next()
        .unwrap_or_else(|| fail(format!("{option} requires a value")));
    if value.trim().is_empty() {
        fail(format!("{option} requires a non-empty value"));
    }
    value
}

fn validate_args(command: &str, args: &Args) {
    if !matches!(args.format.as_str(), "text" | "json") {
        fail(format!("invalid format {}", args.format));
    }
    if args
        .selection
        .lane
        .as_deref()
        .is_some_and(|lane| !matches!(lane, "portable" | "privileged"))
    {
        fail("--lane must be portable or privileged");
    }
    if args
        .selection
        .mode
        .as_deref()
        .is_some_and(|mode| !matches!(mode, "verify" | "chaos" | "replay" | "naked" | "custom"))
    {
        fail("--mode must be verify, chaos, replay, naked, or custom");
    }
    if args
        .selection
        .backend
        .as_deref()
        .is_some_and(|backend| !matches!(backend, "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst"))
    {
        fail("--backend must name a Hermit backend");
    }
    if command == "build" && args.prebuilt {
        fail("build does not accept --prebuilt");
    }
    if command != "run" && args.jobs.is_some() {
        fail("--jobs is accepted by run only");
    }
    if args.selection.include_manual
        && (args.selection.test.is_none() || args.selection.mode.is_none())
    {
        fail("--include-manual requires exact --test and --mode filters");
    }
    if args.probe_disabled {
        if command != "run" {
            fail("--probe-disabled is accepted by run only");
        }
        if args.selection.test.is_none()
            || args.selection.mode.is_none()
            || args.selection.backend.is_none()
        {
            fail("--probe-disabled requires exact --test, --mode, and --backend filters");
        }
        if args.selection.include_manual || args.ci_only {
            fail("--probe-disabled is mutually exclusive with --include-manual and --ci-only");
        }
    }
    if args.allow_empty {
        if !args.ci_only {
            fail("--allow-empty requires --ci-only");
        }
        match command {
            "build" if args.selection.lane.is_some() || args.selection.category.is_some() => {}
            "run" if args.selection.category.is_some() => {}
            "build" => fail("build --allow-empty requires an explicit --lane or --category"),
            "run" => fail("run --allow-empty requires an explicit --category"),
            _ => fail("--allow-empty is accepted by build and run only"),
        }
    }
}

fn main() -> ExitCode {
    let mut values = std::env::args().skip(1);
    let command = values.next().unwrap_or_else(|| fail("missing command"));
    let values = values.collect::<Vec<_>>();
    if command == "expected-plan" && !values.is_empty() {
        fail("expected-plan accepts no options");
    }
    let args = parse(values.into_iter());
    validate_args(&command, &args);
    let root = root();
    let manifests = ManifestSet::load(&root).unwrap_or_else(|error| fail(error));
    // One front-door schema/inventory authority governs every command, not
    // only the metadata gate. This prevents a direct/manual run from accepting
    // a recipe that the canonical manifest planner would refuse.
    run_manifest_plan(&root);
    match command.as_str() {
        "validate" => validate(&root, &manifests),
        "plan" => print_plan(&manifests, &args, Population::Required),
        "expected-plan" => print_expected_plan(&root, &manifests),
        "audit-gaps" => print_plan(&manifests, &args, Population::Disabled),
        "audit-inventory" | "audit-test-binary-registration" => ExitCode::SUCCESS,
        "audit-test-footprints" => {
            run_audit(
                &root,
                &root.join("target/debug/generate-test-footprints"),
                &["--check"],
            );
            ExitCode::SUCCESS
        }
        "audit-ci" => {
            audit_dag_correspondence(&root, &manifests).unwrap_or_else(|error| fail(error));
            audit_budget_ordering(&root).unwrap_or_else(|error| fail(error));
            audit_expected_plan(&root, &manifests);
            ExitCode::SUCCESS
        }
        "build" => build(&root, &manifests, &args),
        "audit-compile" => audit_compile(&root, &manifests, &args),
        "run" => run(&root, &manifests, &args),
        other => fail(format!("unknown command {other}")),
    }
}

fn validate(root: &Path, manifests: &ManifestSet) -> ExitCode {
    // Keep the existing Rust manifest-plan front door as the authority for
    // inventory, schema, lane, workflow, and DAG consistency.  The cell
    // runner owns execution; it must not silently narrow `validate` to only
    // the expected-plan comparison during the shell removal.
    audit_dag_correspondence(root, manifests).unwrap_or_else(|error| fail(error));
    audit_budget_ordering(root).unwrap_or_else(|error| fail(error));
    run_audit(
        root,
        &root.join("target/debug/generate-test-footprints"),
        &["--check"],
    );
    run_audit(
        root,
        &root.join("tests/backend-parity/split_asymmetric_pr.py"),
        &["--self-test"],
    );
    audit_determinism_stress_evidence(root);
    run_audit(root, &root.join("tests/manifest-cli.rs"), &["self-test"]);
    // The DBT budget wrapper gates roughly twenty portable nodes and fails
    // CLOSED on a pin it is not calibrated for. Nothing else notices: a
    // truncated node reads like a fast one. This asserts end to end that the
    // wrapper still REACHES its wrapped command at the recorded pin.
    run_audit(
        root,
        &root.join("ci/run-with-reverie-dbt-budget-test.sh"),
        &[],
    );
    run_audit(
        root,
        &root.join("ci/compat-envelope/scorecard.rs"),
        &["self-test-and-check"],
    );
    run_audit(
        root,
        &root.join("ci/compat-envelope/pressure-test.rs"),
        &["self-test"],
    );
    // The removed shell front door accumulated plan/scheduler/receipt guards
    // that now belong to the Rust validate driver.  Exercise those brackets
    // here without executing the validation DAG; otherwise deleting the shell
    // would also silently delete its protection against incomplete plans.
    run_audit(root, &root.join("scripts/validate.rs"), &["--self-test"]);
    audit_cli_brackets(root);
    let cells = audit_expected_plan(root, manifests);
    println!(
        "PASS: {} YAML manifests, {} required cells",
        manifests.documents.len(),
        cells
    );
    ExitCode::SUCCESS
}

fn audit_cli_brackets(root: &Path) {
    let executable = std::env::current_exe().unwrap_or_else(|error| fail(error));
    for option in [
        "--lane",
        "--category",
        "--test",
        "--mode",
        "--backend",
        "--results",
        "--junit",
        "--format",
        "--jobs",
    ] {
        let status = Command::new(&executable)
            .args(["plan", option])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| fail(error));
        if status.success() {
            fail(format!("missing value for {option} was accepted"));
        }
        let status = Command::new(&executable)
            .args(["plan", option, ""])
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| fail(error));
        if status.success() {
            fail(format!("empty value for {option} was accepted"));
        }
    }
    let output = Command::new(&executable)
        .args([
            "plan",
            "--lane",
            "portable",
            "--ci-only",
            "--format",
            "json",
        ])
        .current_dir(root)
        .output()
        .unwrap_or_else(|error| fail(error));
    let cells = serde_json::from_slice::<Vec<JsonValue>>(&output.stdout).unwrap_or_default();
    if !output.status.success() || cells.is_empty() {
        fail("complete CLI control was refused or selected no cells");
    }
    for argv in [
        vec!["run", "--jobs", "0"],
        vec!["run", "--jobs", "not-a-number"],
        vec!["run", "--jobs", "2", "--jobs", "3"],
        vec!["plan", "--jobs", "2"],
    ] {
        let status = Command::new(&executable)
            .args(argv)
            .current_dir(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap_or_else(|error| fail(error));
        if status.success() {
            fail("invalid --jobs control was accepted");
        }
    }
}

fn run_manifest_plan(root: &Path) {
    let manifest_plan = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.parent()
                .map(|parent| parent.join("hermit-manifest-plan"))
        })
        .unwrap_or_else(|| root.join("target/debug/hermit-manifest-plan"));
    let status = Command::new(&manifest_plan)
        .args(["--format", "json"])
        .current_dir(root)
        .stdout(Stdio::null())
        .status()
        .unwrap_or_else(|error| {
            fail(format!(
                "cannot execute {}: {error}",
                manifest_plan.display()
            ))
        });
    if !status.success() {
        fail(format!(
            "{} rejected the manifest or validation surface",
            manifest_plan.display()
        ));
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct PlanCellIdentity {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

impl PlanCellIdentity {
    fn from_json(row: &JsonValue) -> Result<Self, String> {
        let field = |name: &str| {
            row.get(name)
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("plan row has no string `{name}`: {row}"))
        };
        Ok(Self {
            lane: field("lane")?,
            category: field("category")?,
            test: field("test")?,
            mode: field("mode")?,
            backend: field("backend")?,
        })
    }

    fn display(&self) -> String {
        format!(
            "{}/{}/{}/{}@{}",
            self.lane, self.category, self.test, self.mode, self.backend
        )
    }
}

fn unique_plan_rows(label: &str, rows: Vec<JsonValue>) -> Result<BTreeSet<String>, String> {
    let physical = rows.len();
    let mut identities = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    let mut normalized = BTreeSet::new();
    for row in rows {
        let identity = PlanCellIdentity::from_json(&row)?;
        if !identities.insert(identity.clone()) {
            duplicates.insert(identity);
        }
        normalized.insert(serde_json::to_string(&row).map_err(|error| error.to_string())?);
    }
    if !duplicates.is_empty() {
        let names = duplicates
            .iter()
            .map(PlanCellIdentity::display)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "{label} contains {physical} physical rows but only {} unique identities; duplicate identities: {names}",
            identities.len()
        ));
    }
    Ok(normalized)
}

fn required_plan_rows(manifests: &ManifestSet) -> (usize, Vec<JsonValue>) {
    let cells = manifests
        .select(&Selection {
            population: Some(Population::Required),
            ..Selection::default()
        })
        .unwrap_or_else(|e| fail(e));
    let mut actual = cells
        .iter()
        .map(|cell| {
            let capabilities = cell
                .test
                .requires
                .iter()
                .filter_map(|token| requires_capability(token).ok().flatten())
                .collect::<BTreeSet<_>>();
            let mut row = serde_json::json!({
                "test": cell.id.test,
                "category": cell.category,
                "lane": cell.test.lane,
                "mode": cell.id.mode,
                "backend": cell.id.backend,
            });
            if !capabilities.is_empty() {
                row["requires_host_capabilities"] = serde_json::json!(capabilities);
            }
            row
        })
        .collect::<Vec<_>>();
    actual.sort_by_key(|row| {
        PlanCellIdentity::from_json(row).expect("required plan rows have complete identities")
    });
    (cells.len(), actual)
}

fn expected_plan_document(root: &Path, manifests: &ManifestSet) -> JsonValue {
    let (_, cells) = required_plan_rows(manifests);
    let mut remaining = cells
        .into_iter()
        .map(|row| {
            let identity = PlanCellIdentity::from_json(&row)
                .expect("required plan rows have complete identities");
            (identity, row)
        })
        .collect::<BTreeMap<_, _>>();
    let mut cells = Vec::with_capacity(remaining.len());
    let path = root.join("ci/expected-e2e-plan.json");
    if let Ok(source) = fs::read(&path) {
        let current: JsonValue = serde_json::from_slice(&source)
            .unwrap_or_else(|error| fail(format!("cannot parse {}: {error}", path.display())));
        let current = current["cells"]
            .as_array()
            .unwrap_or_else(|| fail(format!("{} has no cells array", path.display())));
        let mut seen = BTreeSet::new();
        for row in current {
            let identity = PlanCellIdentity::from_json(row)
                .unwrap_or_else(|error| fail(format!("{}: {error}", path.display())));
            if !seen.insert(identity.clone()) {
                fail(format!(
                    "{} contains duplicate identity {}",
                    path.display(),
                    identity.display()
                ));
            }
            if let Some(row) = remaining.remove(&identity) {
                cells.push(row);
            }
        }
    }
    cells.extend(remaining.into_values());
    serde_json::json!({
        "schema": EXPECTED_PLAN_SCHEMA,
        "cells": cells,
    })
}

fn print_expected_plan(root: &Path, manifests: &ManifestSet) -> ExitCode {
    println!(
        "{}",
        serde_json::to_string_pretty(&expected_plan_document(root, manifests)).unwrap()
    );
    ExitCode::SUCCESS
}

fn audit_expected_plan(root: &Path, manifests: &ManifestSet) -> usize {
    let (cell_count, actual) = required_plan_rows(manifests);
    let expected: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap()).unwrap();
    if expected.get("schema").and_then(JsonValue::as_u64) != Some(EXPECTED_PLAN_SCHEMA) {
        fail(format!(
            "ci/expected-e2e-plan.json schema must be {EXPECTED_PLAN_SCHEMA}; regenerate it with `target/debug/test-harness expected-plan`"
        ));
    }
    let expected = expected["cells"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| fail("ci/expected-e2e-plan.json has no cells array"));
    let actual =
        unique_plan_rows("manifest required selection", actual).unwrap_or_else(|error| fail(error));
    let expected =
        unique_plan_rows("ci/expected-e2e-plan.json", expected).unwrap_or_else(|error| fail(error));
    if actual != expected {
        fail("required E2E plan changed; update ci/expected-e2e-plan.json in the same review");
    }
    cell_count
}

fn run_audit(root: &Path, program: &Path, args: &[&str]) {
    let status = Command::new(program)
        .args(args)
        .current_dir(root)
        .status()
        .unwrap_or_else(|error| fail(format!("cannot execute {}: {error}", program.display())));
    if !status.success() {
        // 127 IS AN ENVIRONMENT FAULT, NOT A FAILED AUDIT, AND SAYING "failed"
        // FOR BOTH COSTS A CI RUN ITS WHOLE E2E COVERAGE. These programs carry
        // `#!/usr/bin/env -S rust-script --force`; when that interpreter is
        // absent the kernel never runs the script and the shell reports 127.
        // The old message -- "tests/manifest-cli.rs self-test failed" -- reads
        // as the self-test having run and found a defect. Measured on hermit
        // run 32512027583: it had not run at all, and because build-debug gates
        // every e2e shard, a missing tool was read as a product break.
        if status.code() == Some(127) {
            fail(format!(
                "cannot run {}: exited 127, which means its interpreter was not found, \
                 not that the audit failed. This program runs under \
                 `#!/usr/bin/env -S rust-script --force`; install it with \
                 `cargo install rust-script` or put it on PATH (on a dev box it is \
                 usually ~/.cargo/bin, which a non-login shell does not inherit).",
                program.display()
            ));
        }
        fail(format!("{} {} failed", program.display(), args.join(" ")));
    }
}

fn audit_determinism_stress_evidence(root: &Path) {
    let program = root.join("tests/e2e/lib/determinism-stress/common.sh");
    let status = Command::new(&program)
        .env("DETERMINISM_STRESS_EVIDENCE_SELF_TEST", "1")
        .current_dir(root)
        .status()
        .unwrap_or_else(|error| fail(format!("cannot execute {}: {error}", program.display())));
    if !status.success() {
        fail(format!(
            "{} failed its comparison-evidence self-test",
            program.display()
        ));
    }
}

fn read_dag(path: &Path) -> Result<dagrun::DagConfig, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    dagrun::dag_from_json(&text)
        .map_err(|error| format!("{}: invalid DAG JSON: {error}", path.display()))
}

fn command_jobs(command: &str) -> Result<Option<i64>, String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let mut jobs = None;
    let mut index = 0;
    while index < words.len() {
        if words[index] == "--jobs" {
            let value = words
                .get(index + 1)
                .ok_or_else(|| "manifest command has --jobs without a value".to_string())?
                .parse::<i64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| "manifest command has invalid --jobs value".to_string())?;
            if jobs.replace(value).is_some() {
                return Err("manifest command repeats --jobs".into());
            }
            index += 1;
        }
        index += 1;
    }
    Ok(jobs)
}

fn audit_dag_correspondence(root: &Path, manifests: &ManifestSet) -> Result<(), String> {
    let committed_path = root.join("ci/dag/validate.json");
    let committed = read_dag(&committed_path)?;
    for lane in ["portable", "privileged"] {
        let path = &committed_path;
        let dag = dagrun::select_steps_by_labels(&committed, &[lane.to_string()])
            .map_err(|error| format!("{}: cannot select label {lane}: {error}", path.display()))?;
        if dag
            .steps
            .iter()
            .any(|step| step.cmd.contains("test_harness.sh"))
        {
            return Err(format!(
                "{} still invokes the removed shell harness",
                path.display()
            ));
        }
        let mut ids = BTreeSet::new();
        for step in &dag.steps {
            let id = format!("{}.{}", step.group, step.job);
            if !ids.insert(id.clone()) {
                return Err(format!("{} contains duplicate node {id}", path.display()));
            }
        }
        for step in &dag.steps {
            for dependency in &step.deps {
                if !ids.contains(dependency) {
                    return Err(format!(
                        "{} node {}.{} names missing dependency {dependency}",
                        path.display(),
                        step.group,
                        step.job
                    ));
                }
            }
        }
        if dag
            .steps
            .iter()
            .filter(|step| step.cmd.ends_with("target/debug/test-harness validate"))
            .count()
            != 1
        {
            return Err(format!(
                "{} must contain exactly one Rust metadata validation node",
                path.display()
            ));
        }
        let build =
            format!("target/debug/test-harness build --lane {lane} --ci-only --allow-empty");
        if dag
            .steps
            .iter()
            .filter(|step| step.cmd.ends_with(&build))
            .count()
            != 1
        {
            return Err(format!(
                "{} must contain exactly one Rust manifest build node",
                path.display()
            ));
        }
        let expected = manifests
            .documents
            .iter()
            .filter(|document| document.test.iter().any(|test| test.lane == lane))
            .map(|document| document.bucket.clone())
            .collect::<BTreeSet<_>>();
        let mut actual = BTreeSet::new();
        for step in dag.steps.iter().filter(|step| {
            step.manifest
                .as_ref()
                .is_some_and(|manifest| manifest.lane == lane)
        }) {
            let manifest = step.manifest.as_ref().ok_or_else(|| {
                format!("{}.{} lacks typed manifest identity", step.group, step.job)
            })?;
            let dagrun::DagManifest {
                lane: manifest_lane,
                category,
            } = manifest;
            if manifest_lane != lane {
                return Err(format!(
                    "{}.{} records lane {} in the {lane} DAG",
                    step.group, step.job, manifest_lane
                ));
            }
            let selector = format!(
                "target/debug/test-harness run --lane {lane} --category {} --ci-only --allow-empty --prebuilt",
                category
            );
            if !step.cmd.contains(&selector) {
                return Err(format!(
                    "{}.{} does not execute its typed selector literally",
                    step.group, step.job
                ));
            }
            if let Some(jobs) = command_jobs(&step.cmd)? {
                let demand = step
                    .hint
                    .resources
                    .get("manifest_guest")
                    .copied()
                    .unwrap_or(0);
                let cap = dag
                    .resource_caps
                    .get("manifest_guest")
                    .copied()
                    .unwrap_or(0);
                if demand != jobs
                    || cap < jobs
                    || step.hint.preferred_inner_jobs != Some(jobs)
                    || step.jobs_flag.as_deref() != Some("")
                {
                    return Err(format!(
                        "{}.{} runs --jobs {jobs} but declares manifest_guest={demand}, cap={cap}, preferred_inner_jobs={:?}, jobs_flag={:?}",
                        step.group, step.job, step.hint.preferred_inner_jobs, step.jobs_flag
                    ));
                }
            }
            if !actual.insert(category.clone()) {
                return Err(format!(
                    "{} has duplicate manifest bucket {}",
                    path.display(),
                    category
                ));
            }
        }
        if actual != expected {
            return Err(format!(
                "{} manifest buckets differ: expected={expected:?} actual={actual:?}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn audit_budget_ordering(root: &Path) -> Result<(), String> {
    let committed = read_dag(&root.join("ci/dag/validate.json"))?;
    let portable = dagrun::select_steps_by_labels(&committed, &["portable".into()])
        .map_err(|error| format!("cannot select portable DAG steps: {error}"))?;
    let privileged = dagrun::select_steps_by_labels(&committed, &["privileged".into()])
        .map_err(|error| format!("cannot select privileged DAG steps: {error}"))?;
    for (lane, dag) in [("portable", &portable), ("privileged", &privileged)] {
        for step in &dag.steps {
            if step.cpu_timeout <= 0 {
                return Err(format!(
                    "{lane} node {}.{} has no CPU budget",
                    step.group, step.job
                ));
            }
            let wall = dagrun::resolved_wall_timeout(
                step,
                dag.default_step_timeout,
                dag.cpu_timeout_multiplier,
            );
            let cpu = (step.cpu_timeout as f64 * dag.cpu_timeout_multiplier).round() as i64;
            if wall <= cpu {
                return Err(format!(
                    "{lane} node {}.{} resolves wall={wall}s at or below effective CPU={cpu}s",
                    step.group, step.job
                ));
            }
            if lane == "portable" {
                let Some(value) = step.cmd.strip_prefix("CARGO_BUILD_JOBS=") else {
                    continue;
                };
                let jobs = value
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<i64>().ok())
                    .ok_or_else(|| {
                        format!(
                            "{lane} node {}.{} has an invalid CARGO_BUILD_JOBS prefix",
                            step.group, step.job
                        )
                    })?;
                if step.hint.preferred_inner_jobs != Some(jobs) {
                    return Err(format!(
                        "{lane} node {}.{} declares CARGO_BUILD_JOBS={jobs} without matching preferred_inner_jobs",
                        step.group, step.job
                    ));
                }
            }
        }
    }

    let portable_workflow = parse_yaml(&root.join(".github/workflows/ci-portable.yml"))?;
    let debug_bound = workflow_job_timeout(&portable_workflow, "test-debug")? * 60;
    let release_bound = workflow_job_timeout(&portable_workflow, "test-release")? * 60;
    let shards: JsonValue = serde_json::from_slice(
        &fs::read(root.join("ci/portable-shards.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("invalid portable shard map: {e}"))?;
    const PORTABLE_JOB_SETUP_HEADROOM_S: u64 = 15 * 60;
    let string_array = |value: &JsonValue, location: &str| -> Result<BTreeSet<String>, String> {
        value
            .as_array()
            .ok_or_else(|| format!("{location} must be an array"))?
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{location} contains a non-string node"))
            })
            .collect()
    };
    let compat_nodes = portable
        .steps
        .iter()
        .map(dagrun::Step::tag)
        .filter(|tag| tag.starts_with("compat."))
        .collect::<BTreeSet<_>>();
    let mut job_selections = Vec::<(&str, BTreeSet<String>)>::new();
    for (job, key) in [
        ("preflight", "preflight_nodes"),
        ("checks", "check_nodes"),
        ("build-debug", "build_debug_nodes"),
        ("build-release", "build_dbt_nodes"),
        ("build-complete", "build_aux_nodes"),
        ("e2e", "e2e_nodes"),
        ("regular", "final_nodes"),
    ] {
        job_selections.push((job, string_array(&shards[key], key)?));
    }
    job_selections.push(("strict-compat", compat_nodes));
    for (job, key) in [
        ("test-debug", "debug_shards"),
        ("test-release", "release_shards"),
    ] {
        for shard in shards[key]
            .as_array()
            .ok_or_else(|| format!("{key} must be an array"))?
        {
            job_selections.push((job, string_array(&shard["nodes"], key)?));
        }
    }
    for (job, selected) in &job_selections {
        let critical_path = dag_selected_critical_path(&portable, selected)?;
        let job_bound = workflow_job_timeout(&portable_workflow, job)? * 60;
        if job_bound <= critical_path + PORTABLE_JOB_SETUP_HEADROOM_S {
            return Err(format!(
                "portable job {job} {job_bound}s must exceed selected DAG critical path {critical_path}s plus {PORTABLE_JOB_SETUP_HEADROOM_S}s setup headroom"
            ));
        }
    }
    let portable_steps = portable
        .steps
        .iter()
        .map(|step| (format!("{}.{}", step.group, step.job), step))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut current = BTreeSet::new();
    for (key, job, bound) in [
        ("debug_shards", "test-debug", debug_bound),
        ("release_shards", "test-release", release_bound),
    ] {
        for node in shards[key]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|shard| shard["nodes"].as_array().into_iter().flatten())
        {
            let node = node
                .as_str()
                .ok_or_else(|| format!("{key} contains a non-string node"))?;
            let step = portable_steps
                .get(node)
                .ok_or_else(|| format!("portable shard names missing DAG node {node}"))?;
            let timeout = u64::try_from(dagrun::resolved_wall_timeout(
                step,
                portable.default_step_timeout,
                portable.cpu_timeout_multiplier,
            ))
            .map_err(|_| format!("portable node {node} has invalid resolved wall timeout"))?;
            if timeout >= bound {
                current.insert(format!(
                    "{node} {timeout}s >= {bound}s (job {job} timeout-minutes)"
                ));
            }
        }
    }

    let privileged_workflow = fs::read_to_string(root.join(".github/workflows/ci-privileged.yml"))
        .map_err(|e| e.to_string())?;
    audit_privileged_unboxed_guard(&privileged_workflow)?;
    if privileged_workflow
        .matches("continue-on-error: true")
        .count()
        != 1
    {
        return Err(
            "privileged workflow must contain exactly one diagnostic continue-on-error".into(),
        );
    }
    let launcher_bound = privileged_workflow
        .lines()
        .find(|line| line.contains("ci/run-dag.sh privileged"))
        .and_then(|line| {
            let words = line.split_whitespace().collect::<Vec<_>>();
            words
                .iter()
                .position(|word| *word == "env")
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| words[index].strip_suffix('s'))
                .and_then(|value| value.parse::<u64>().ok())
        })
        .ok_or_else(|| "cannot derive privileged launcher timeout".to_string())?;
    let privileged_yaml = parse_yaml(&root.join(".github/workflows/ci-privileged.yml"))?;
    let privileged_job_bound = workflow_job_timeout(&privileged_yaml, "privileged")? * 60;
    let declared_step_budgets = workflow_step_timeout_sum(&privileged_yaml, "privileged")?;
    if privileged_job_bound <= declared_step_budgets {
        return Err(format!(
            "privileged job {privileged_job_bound}s must exceed {declared_step_budgets}s of explicit inner step budgets"
        ));
    }
    let critical_path = dag_critical_path(&privileged)?;
    if launcher_bound <= critical_path + 30 {
        return Err(format!(
            "privileged launcher {launcher_bound}s must exceed {critical_path}s DAG critical path plus 30s runner overhead"
        ));
    }
    for step in &privileged.steps {
        let timeout = u64::try_from(dagrun::resolved_wall_timeout(
            step,
            privileged.default_step_timeout,
            privileged.cpu_timeout_multiplier,
        ))
        .map_err(|_| {
            format!(
                "privileged node {}.{} has invalid resolved wall timeout",
                step.group, step.job
            )
        })?;
        if timeout >= launcher_bound {
            current.insert(format!(
                "{}.{} {timeout}s >= {launcher_bound}s (privileged launcher wrapper)",
                step.group, step.job
            ));
        }
    }

    let expected = fs::read_to_string(root.join("ci/budget-inversions-baseline.txt"))
        .map_err(|e| e.to_string())?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if current != expected {
        let new = current.difference(&expected).cloned().collect::<Vec<_>>();
        let fixed = expected.difference(&current).cloned().collect::<Vec<_>>();
        return Err(format!(
            "budget-inversion baseline drifted: new={new:?} fixed-but-listed={fixed:?}"
        ));
    }
    println!(
        "budget ordering: {} baseline inversion(s), {} portable sharded + {} privileged nodes checked",
        current.len(),
        shards["debug_shards"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(shards["release_shards"].as_array().into_iter().flatten())
            .map(|shard| shard["nodes"].as_array().map_or(0, Vec::len))
            .sum::<usize>(),
        privileged.steps.len()
    );
    Ok(())
}

fn audit_privileged_unboxed_guard(workflow: &str) -> Result<(), String> {
    const ACTIONS_GUARD: &str = "        if [[ ${GITHUB_ACTIONS:-} != true ]]; then";
    const REFUSAL: &str = "          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2";
    const FAIL_CLOSED: &str = "          exit 2";

    for (line, description) in [
        (ACTIONS_GUARD, "exact GitHub Actions context guard"),
        (REFUSAL, "explicit outside-Actions refusal"),
        (FAIL_CLOSED, "nonzero outside-Actions exit"),
    ] {
        if workflow
            .lines()
            .filter(|candidate| *candidate == line)
            .count()
            != 1
        {
            return Err(format!(
                "privileged workflow must contain exactly one {description}"
            ));
        }
    }

    if workflow.matches("--unsafe-no-cgroups").count() != 1 {
        return Err(
            "privileged workflow must select explicit unboxed execution exactly once".into(),
        );
    }
    if workflow
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .any(|line| line.contains("--allow-cgroup-failure"))
    {
        return Err(
            "privileged workflow must not execute with broad --allow-cgroup-failure".into(),
        );
    }
    Ok(())
}

fn dag_critical_path(dag: &dagrun::DagConfig) -> Result<u64, String> {
    let selected = dag.steps.iter().map(dagrun::Step::tag).collect();
    dag_selected_critical_path(dag, &selected)
}

fn dag_selected_critical_path(
    dag: &dagrun::DagConfig,
    selected: &BTreeSet<String>,
) -> Result<u64, String> {
    let steps = dag
        .steps
        .iter()
        .map(|step| (format!("{}.{}", step.group, step.job), step))
        .collect::<std::collections::BTreeMap<_, _>>();
    fn visit(
        id: &str,
        steps: &std::collections::BTreeMap<String, &dagrun::Step>,
        active: &mut BTreeSet<String>,
        memo: &mut std::collections::BTreeMap<String, u64>,
        default_step_timeout: i64,
        cpu_timeout_multiplier: f64,
        selected: &BTreeSet<String>,
    ) -> Result<u64, String> {
        if let Some(value) = memo.get(id) {
            return Ok(*value);
        }
        if !active.insert(id.to_string()) {
            return Err(format!("DAG dependency cycle reaches {id}"));
        }
        let step = steps
            .get(id)
            .ok_or_else(|| format!("DAG critical path references missing node {id}"))?;
        let predecessor = step
            .deps
            .iter()
            .filter(|dependency| selected.contains(*dependency))
            .map(|dependency| {
                visit(
                    dependency,
                    steps,
                    active,
                    memo,
                    default_step_timeout,
                    cpu_timeout_multiplier,
                    selected,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        active.remove(id);
        let timeout = u64::try_from(dagrun::resolved_wall_timeout(
            step,
            default_step_timeout,
            cpu_timeout_multiplier,
        ))
        .map_err(|_| format!("DAG node {id} has invalid resolved wall timeout"))?;
        let value = predecessor
            .checked_add(timeout)
            .ok_or_else(|| format!("DAG critical path overflows at {id}"))?;
        memo.insert(id.to_string(), value);
        Ok(value)
    }
    let mut memo = std::collections::BTreeMap::new();
    let mut maximum = 0;
    for id in selected {
        if !steps.contains_key(id) {
            return Err(format!(
                "selected DAG critical path references missing node {id}"
            ));
        }
        maximum = maximum.max(visit(
            id,
            &steps,
            &mut BTreeSet::new(),
            &mut memo,
            dag.default_step_timeout,
            dag.cpu_timeout_multiplier,
            selected,
        )?);
    }
    Ok(maximum)
}

fn parse_yaml(path: &Path) -> Result<YamlValue, String> {
    serde_yaml::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: invalid YAML: {e}", path.display()))
}

fn workflow_job_timeout(workflow: &YamlValue, job: &str) -> Result<u64, String> {
    workflow["jobs"][job]["timeout-minutes"]
        .as_u64()
        .ok_or_else(|| format!("workflow job {job} has no numeric timeout-minutes"))
}

fn workflow_step_timeout_sum(workflow: &YamlValue, job: &str) -> Result<u64, String> {
    let steps = workflow["jobs"][job]["steps"]
        .as_sequence()
        .ok_or_else(|| format!("workflow job {job} has no steps"))?;
    let mut sum = 0;
    for run in steps
        .iter()
        .filter_map(|step| step.get("run"))
        .filter_map(YamlValue::as_str)
    {
        for line in run.lines() {
            let words = line.split_whitespace().collect::<Vec<_>>();
            for index in 0..words.len() {
                if words[index] == "timeout" {
                    let budget = words[index + 1..]
                        .iter()
                        .find(|word| !word.starts_with('-'))
                        .and_then(|word| word.trim_end_matches('\\').strip_suffix('s'))
                        .and_then(|value| value.parse::<u64>().ok())
                        .ok_or_else(|| format!("cannot derive timeout budget from `{line}`"))?;
                    sum += budget;
                }
            }
        }
    }
    Ok(sum)
}

fn print_plan(manifests: &ManifestSet, args: &Args, population: Population) -> ExitCode {
    let mut selection = args.selection.clone();
    selection.population = Some(population);

    // ⚠️ AN UNKNOWN TEST ID IS A REFUSAL HERE, AND THIS IS THE ONLY SUBCOMMAND THAT
    // NEEDED IT. `run` and `build` already fail closed on an empty selection
    // (`filters selected no cells`), but `plan` printed an empty list and exited 0 --
    // measured 2026-08-26: `plan --lane portable --test no-such-test-xyz` is rc=0, and
    // so is the same command with a REAL id, so its exit code carried no information in
    // either direction. Anything driving a bisection off `plan` therefore reads a typo
    // as "nothing failed here" and converges, confidently, on the wrong commit.
    //
    // ⚠️ AND THE CHECK IS "UNKNOWN ID", NOT "EMPTY RESULT", WHICH IS NOT THE SAME FIX.
    // `print_plan` also serves `audit-gaps` (Population::Disabled), where an empty
    // answer legitimately means NO GAPS. Mirroring run's `cells.is_empty()` guard here
    // would turn that good answer into a failure. Asking whether the named id exists at
    // all separates the two: a real id with no cells in this population still prints
    // nothing and exits 0.
    if let Some(id) = selection.test.as_deref() {
        if !manifests.knows_test(id) {
            fail(format!(
                "unknown test id {id:?}: it is not in any manifest. An empty plan for a \
                 real id means that population has no cells; an empty plan for an id \
                 that does not exist means the filter is wrong, and refusing is what \
                 stops a bisection reading a typo as a pass."
            ));
        }
    }

    let cells = manifests.select(&selection).unwrap_or_else(|e| fail(e));
    if args.format == "json" {
        println!("{}", serde_json::to_string(&cells.iter().map(|c| {
            let backend = if population == Population::Disabled && c.id.mode == "naked" {
                Some("native")
            } else {
                c.id.backend.as_deref()
            };
            serde_json::json!({"test":c.id.test,"category":c.category,"lane":c.test.lane,"mode":c.id.mode,"backend":backend})
        }).collect::<Vec<_>>()).unwrap());
    } else {
        for cell in cells {
            let backend = if population == Population::Disabled && cell.id.mode == "naked" {
                "native"
            } else {
                cell.id.backend.as_deref().unwrap_or("-")
            };
            println!(
                "{}\t{}\t{}\t{}\t{}",
                cell.test.lane, cell.category, cell.id.test, cell.id.mode, backend
            );
        }
    }
    ExitCode::SUCCESS
}

fn build(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let mut selection = args.selection.clone();
    if selection.population.is_none() {
        selection.population = Some(if selection.include_manual {
            Population::Enabled
        } else {
            Population::Required
        });
    }
    let cells = manifests.select(&selection).unwrap_or_else(|e| fail(e));
    if cells.is_empty() && !args.allow_empty {
        fail("filters selected no cells");
    }
    let context = RunContext::from_env(root.to_path_buf(), false).unwrap_or_else(|e| fail(e));
    let mut seen = BTreeSet::new();
    let mut failed = false;
    for cell in cells {
        if !seen.insert(cell.id.test.clone()) {
            continue;
        }
        let dir = context.build_root.join(cell.id.test.replace('/', "-"));
        match hermit_manifest_plan::runner::prepare_test(&context, &cell, &dir) {
            Ok(_) => println!("BUILT {}", cell.id.test),
            Err(e) => {
                eprintln!("ERROR {}: {e}", cell.id.test);
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn audit_compile(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let context = RunContext::from_env(root.to_path_buf(), false).unwrap_or_else(|e| fail(e));
    let mut checked = 0;
    let mut failed = false;
    for (category, inherited_timeout_seconds, test) in manifests.all_tests() {
        if args
            .selection
            .lane
            .as_deref()
            .is_some_and(|lane| lane != test.lane)
            || args
                .selection
                .category
                .as_deref()
                .is_some_and(|value| value != category)
            || args
                .selection
                .test
                .as_deref()
                .is_some_and(|value| value != test.id)
            || !test
                .program
                .as_deref()
                .is_some_and(|program| program.ends_with(".c"))
        {
            continue;
        }
        let verify = test
            .modes
            .get("verify")
            .expect("validated manifests carry verify");
        let backend = verify
            .backends_enabled
            .first()
            .cloned()
            .unwrap_or_else(|| "ptrace".into());
        let timeout_seconds = verify
            .timeout_seconds
            .get(&backend)
            .copied()
            .unwrap_or(inherited_timeout_seconds);
        let cell = hermit_manifest_plan::runner::SelectedCell {
            category: category.into(),
            test: test.clone(),
            id: hermit_manifest_plan::runner::CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some(backend),
            },
            enabled: false,
            timeout_seconds,
        };
        checked += 1;
        let dir = context
            .result_root
            .join("audit-compile")
            .join(test.id.replace('/', "-"));
        if let Err(e) = hermit_manifest_plan::runner::prepare_test(&context, &cell, &dir) {
            eprintln!("ERROR {}: {e}", test.id);
            failed = true;
        }
    }
    if checked == 0 {
        fail("compile audit compiled zero guests");
    }
    if failed {
        ExitCode::FAILURE
    } else {
        println!("compile audit: {checked} compiled");
        ExitCode::SUCCESS
    }
}

/// Execute `count` independent items with at most `jobs` workers, delivering
/// each emitted value to `consume` immediately and waiting for its
/// acknowledgement before the worker may continue.
///
/// The consumer stays on the calling thread so durable publication is
/// serialized even while the expensive cell executions overlap. This is
/// deliberately not a collect-then-publish helper: an outer bucket timeout
/// must not discard rows that completed before the timeout, and a retry must
/// not start before the prior attempt is flushed.
fn for_each_parallel<T: Send>(
    count: usize,
    capacity: ScheduledWorkerCapacity,
    execute: impl Fn(usize, &mut dyn FnMut(T, bool) -> bool) + Sync,
    mut consume: impl FnMut(usize, T, bool) -> bool,
) {
    if count == 0 {
        return;
    }
    let workers = capacity.workers_for(count);
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel::<(usize, T, bool, mpsc::SyncSender<bool>)>();
    thread::scope(|scope| {
        for _ in 0..workers {
            let sender = sender.clone();
            let execute = &execute;
            let next = &next;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= count {
                        break;
                    }
                    let mut emit = |value, will_retry| {
                        let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
                        if sender.send((index, value, will_retry, ack_sender)).is_err() {
                            return false;
                        }
                        ack_receiver.recv().unwrap_or(false)
                    };
                    execute(index, &mut emit);
                }
            });
        }
        drop(sender);
        for (index, value, will_retry, ack_sender) in receiver {
            let acknowledged = consume(index, value, will_retry);
            let _ = ack_sender.send(acknowledged);
        }
    });
}

fn run_with_retry<T>(
    first_attempt: u64,
    mut execute: impl FnMut(u64) -> T,
    mut retryable: impl FnMut(&T) -> bool,
    mut emit: impl FnMut(T, bool) -> bool,
) {
    assert!(
        (1..=MAX_ATTEMPTS_PER_CELL).contains(&first_attempt),
        "first cell attempt must be within the shared attempt cap"
    );
    for attempt in first_attempt..=MAX_ATTEMPTS_PER_CELL {
        let result = execute(attempt);
        let will_retry = retryable(&result) && attempt < MAX_ATTEMPTS_PER_CELL;
        if !emit(result, will_retry) || !will_retry {
            break;
        }
    }
}

fn run(root: &Path, manifests: &ManifestSet, args: &Args) -> ExitCode {
    let mut selection = args.selection.clone();
    if selection.population.is_none() {
        selection.population = Some(if selection.include_manual {
            Population::Enabled
        } else {
            Population::Required
        });
    }
    let cells = manifests.select(&selection).unwrap_or_else(|e| fail(e));
    if cells.is_empty() && !args.allow_empty {
        fail("filters selected no cells");
    }
    let capacity = scheduled_worker_capacity(args);
    let context = RunContext::from_env(root.to_path_buf(), args.prebuilt)
        .unwrap_or_else(|e| fail(e))
        .with_scheduled_worker_capacity(capacity);
    for (capability, verdict) in &context.host_capabilities {
        eprintln!(
            "Host capability {}: {} — {}",
            capability.value(),
            if verdict.present { "PRESENT" } else { "ABSENT" },
            verdict.evidence
        );
    }
    let results_path = args.results.clone().unwrap_or_else(|| {
        context
            .result_root
            .join(&context.run_id)
            .join("results.jsonl")
    });
    let junit = args
        .junit
        .clone()
        .unwrap_or_else(|| context.result_root.join(&context.run_id).join("junit.xml"));
    prepare_result_path(&results_path).unwrap_or_else(|error| {
        fail(format!(
            "cannot prepare result path {}: {error}",
            results_path.display()
        ))
    });
    let mut indexed_results = Vec::new();
    let mut attempt_results = vec![Vec::new(); cells.len()];
    let mut failed = false;
    // Sum the producer-owned CPU measurement from EVERY executed observation,
    // including a failed row that is retried. This is specifically cell CPU;
    // the harness process itself remains in the enclosing DAG cgroup.
    let mut cell_cpu_usage_usec = Some(0u64);
    let mut cpu_measurements = 0usize;
    let expected = cells.len();
    for_each_parallel(
        expected,
        capacity,
        |index, emit| {
            let cell = &cells[index];
            if let Some((_, reason)) =
                host_inapplicable_reason(&cell.test.requires, &context.host_capabilities)
            {
                let _ = emit(host_inapplicable_result(&context, cell, reason), false);
                return;
            }

            run_with_retry(
                context.attempt,
                |attempt| {
                    let attempt_context = context.with_attempt(attempt);
                    match run_cell(&attempt_context, cell) {
                        Ok(result) => result,
                        Err(error) => infrastructure_error_result(&attempt_context, cell, error),
                    }
                },
                |result| !matches!(result.outcome.as_str(), "PASS" | "HOST-INAPPLICABLE"),
                emit,
            );
        },
        |index, mut result: CellResult, will_retry| {
            accumulate_cell_cpu_usage(
                &mut cell_cpu_usage_usec,
                &mut cpu_measurements,
                &result.outcome,
                result.cpu_usage_usec,
            );
            // Publish before announcing the outcome. After a visible PASS line,
            // the complete typed row is already present even if the containing
            // bucket is killed before its JUnit/summary epilogue. The worker
            // waits for this acknowledgement before starting a retry.
            let published = if let Err(error) = append_result(&results_path, &result) {
                eprintln!(
                    "ERROR {} ({}/{}): completed cell result could not be published: {error}",
                    result.test,
                    result.mode,
                    result.backend.as_deref().unwrap_or("native")
                );
                result.outcome = "ERROR".into();
                result.result = None;
                result.failure_class = Some(FailureClass::UnderstoodInfrastructureFailure);
                result.error_kind = Some("result-publication".into());
                result.reason = Some(format!(
                    "completed cell result could not be published: {error}"
                ));
                false
            } else {
                true
            };

            if result.outcome == "ERROR" {
                eprintln!(
                    "ERROR {} ({}/{}): {}",
                    result.test,
                    result.mode,
                    result.backend.as_deref().unwrap_or("native"),
                    result.reason.as_deref().unwrap_or("infrastructure error")
                );
            }
            // A FAILURE MUST SAY ENOUGH TO BE CLASSIFIED, NOT JUST COUNTED.
            let located = if result.outcome == "PASS" {
                String::new()
            } else if result.outcome == "HOST-INAPPLICABLE" {
                format!(
                    " {}",
                    result.reason.as_deref().unwrap_or("host-inapplicable")
                )
            } else {
                let coords = [
                    ("turn", result.first_divergent_scheduler_turn),
                    ("vns", result.first_divergent_virtual_nanoseconds),
                    ("rec", result.first_divergent_record),
                    ("sys", result.first_divergent_syscall),
                ]
                .iter()
                .filter_map(|(key, value)| value.map(|value| format!("{key}={value}")))
                .collect::<Vec<_>>();
                let mut suffix = String::new();
                if !coords.is_empty() {
                    suffix.push_str(&format!(" [{}]", coords.join(" ")));
                }
                if let Some(reason) = result.reason.as_deref() {
                    suffix.push_str(&format!(" {reason}"));
                }
                suffix.push_str(&format!("\n    evidence: {}", result.artifact_dir));
                suffix
            };
            let effective_will_retry = published && will_retry;
            let retry_note = if effective_will_retry {
                format!(
                    " [attempt {} of at most {}; retrying this cell only]",
                    result.attempt, MAX_ATTEMPTS_PER_CELL
                )
            } else {
                String::new()
            };
            println!(
                "{} {} ({}/{}){}{}",
                result.outcome,
                result.test,
                result.mode,
                result.backend.as_deref().unwrap_or("native"),
                retry_note,
                located
            );

            attempt_results[index].push(result);
            if !effective_will_retry {
                let result = match cell_result_after_retries(&attempt_results[index]) {
                    Ok(result) => result.clone(),
                    Err(error) => {
                        let mut result = attempt_results[index]
                            .last()
                            .expect("the current attempt was retained before reporting")
                            .clone();
                        result.outcome = "ERROR".into();
                        result.error_kind = Some("result-history".into());
                        result.reason = Some(error);
                        result
                    }
                };
                failed |= matches!(result.outcome.as_str(), "FAIL" | "ERROR");
                indexed_results.push((index, result));
            }
            published
        },
    );
    if indexed_results.len() != expected {
        eprintln!(
            "test-harness: only {} of {expected} selected cells returned a result",
            indexed_results.len()
        );
        failed = true;
    }
    indexed_results.sort_by_key(|(index, _)| *index);
    let results = indexed_results
        .into_iter()
        .map(|(_, result)| result)
        .collect::<Vec<_>>();
    if expected > 0 {
        println!(
            "test-harness: completed {} cell(s) with up to {} concurrent worker(s)",
            results.len(),
            capacity.workers_for(expected)
        );
    }
    let host_inapplicable = results
        .iter()
        .filter(|result| result.outcome == "HOST-INAPPLICABLE")
        .count();
    let cell_cpu_usage_usec = (cpu_measurements > 0)
        .then_some(cell_cpu_usage_usec)
        .flatten();
    if let Some(path) = std::env::var_os("DAGRUN_TEST_COUNTS_PATH") {
        let path = PathBuf::from(path);
        if let Err(error) =
            structured_test_results(&attempt_results).and_then(|report| report.write_current(&path))
        {
            eprintln!("test-harness: {error}");
            failed = true;
        }
    }
    if expected > 0 && host_inapplicable == expected {
        eprintln!(
            "test-harness: every one of the {expected} selected cell(s) was host-inapplicable; \
             a run that executed no cell is not a pass"
        );
        failed = true;
    }
    write_junit(&junit, &results).unwrap();
    let summary = serde_json::json!({
        "schema": 1,
        "cells": results.len(),
        "passed": results.iter().filter(|result| result.outcome == "PASS").count(),
        "failed": results.iter().filter(|result| result.outcome == "FAIL").count(),
        "errors": results.iter().filter(|result| result.outcome == "ERROR").count(),
        "host_inapplicable": host_inapplicable,
        "cell_cpu_usage_usec": cell_cpu_usage_usec,
        "host_inapplicable_cells": results
            .iter()
            .filter(|result| result.outcome == "HOST-INAPPLICABLE")
            .map(|result| serde_json::json!({
                "test": result.test,
                "mode": result.mode,
                "backend": result.backend,
                "reason": result.reason,
            }))
            .collect::<Vec<_>>(),
    });
    fs::write(
        results_path.parent().unwrap().join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use hermit_manifest_plan::runner::ManifestSet;
    use hermit_manifest_plan::runner::ScheduledWorkerCapacity;

    use super::EXPECTED_PLAN_SCHEMA;
    use super::HostCapability;
    use super::HostCapabilityVerdict;
    use super::accumulate_cell_cpu_usage;
    use super::audit_privileged_unboxed_guard;
    use super::command_jobs;
    use super::expected_plan_document;
    use super::for_each_parallel;
    use super::host_inapplicable_reason;
    use super::parse;
    use super::run_with_retry;
    use super::scheduled_worker_capacity;
    use super::structured_test_results_from_rows;
    use super::unique_plan_rows;

    #[test]
    fn generated_expected_plan_is_versioned_and_matches_the_tracked_file() {
        let root = super::root();
        let manifests = ManifestSet::load(&root).unwrap();
        let generated = expected_plan_document(&root, &manifests);
        assert_eq!(generated["schema"], EXPECTED_PLAN_SCHEMA);
        let tracked: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap())
                .unwrap();
        assert_eq!(tracked, generated);
    }

    fn duplicate_plan_fixture(mode: &str) -> Vec<serde_json::Value> {
        let mut rows = (0..307)
            .map(|index| {
                serde_json::json!({
                    "lane": "portable",
                    "category": "fixture",
                    "test": format!("fixture/test-{index:03}"),
                    "mode": if index == 0 { mode } else { "verify" },
                    "backend": "ptrace",
                })
            })
            .collect::<Vec<_>>();
        rows.push(rows[0].clone());
        rows
    }

    #[test]
    fn expected_plan_refuses_duplicate_comparable_and_custom_rows_before_set_comparison() {
        for mode in ["verify", "custom"] {
            let rows = duplicate_plan_fixture(mode);
            let error = unique_plan_rows("fixture expected plan", rows)
                .expect_err("308 physical rows with 307 identities must be refused");
            assert!(
                error.contains("308 physical rows but only 307 unique identities"),
                "{error}"
            );
            assert!(error.contains(&format!("fixture/test-000/{mode}@ptrace")));
        }
    }

    #[test]
    fn cell_cpu_summary_includes_retries_and_refuses_incomplete_measurements() {
        let mut total = Some(0);
        let mut measurements = 0;
        accumulate_cell_cpu_usage(&mut total, &mut measurements, "FAIL", Some(3));
        accumulate_cell_cpu_usage(&mut total, &mut measurements, "PASS", Some(4));
        accumulate_cell_cpu_usage(
            &mut total,
            &mut measurements,
            "HOST-INAPPLICABLE",
            Some(100),
        );
        assert_eq!(measurements, 2);
        assert_eq!(total, Some(7));

        accumulate_cell_cpu_usage(&mut total, &mut measurements, "ERROR", None);
        assert_eq!(measurements, 3);
        assert_eq!(total, None);
    }

    #[test]
    fn structured_test_results_are_machine_readable_and_exact_on_failure() {
        let path = std::env::temp_dir().join(format!(
            "hermit-manifest-counts-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        structured_test_results_from_rows([
            ("suite$passes".into(), true, 1),
            ("suite$fails".into(), false, 2),
        ])
        .unwrap()
        .write_current(&path)
        .unwrap();
        let counts: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            counts,
            serde_json::json!({
                "schema": 2,
                "executed_tests": 2,
                "filtered_tests": 0,
                "results": [
                    {"id": "suite$passes", "result": "pass", "attempts": 1},
                    {"id": "suite$fails", "result": "fail", "attempts": 2},
                ],
            })
        );
    }

    #[test]
    fn only_a_declared_absent_capability_withholds_a_cell() {
        let absent = BTreeMap::from([(
            HostCapability::CpuidFaulting,
            HostCapabilityVerdict {
                present: false,
                evidence: "planted absence".into(),
            },
        )]);
        let requires = vec!["linux".to_string(), "cpuid".to_string()];
        let (capabilities, reason) = host_inapplicable_reason(&requires, &absent).unwrap();
        assert_eq!(capabilities, ["cpuid-faulting"]);
        assert!(reason.contains("NOT RUN, NOT a pass, no coverage"));
        assert!(reason.contains("planted absence"));

        let undeclared = vec!["linux".to_string(), "ptrace".to_string()];
        assert!(host_inapplicable_reason(&undeclared, &absent).is_none());

        let present = BTreeMap::from([(
            HostCapability::CpuidFaulting,
            HostCapabilityVerdict {
                present: true,
                evidence: "planted presence".into(),
            },
        )]);
        assert!(host_inapplicable_reason(&requires, &present).is_none());
    }

    const GUARDED_WORKFLOW: &str = r#"    # --allow-cgroup-failure is documented here but not executed.
        if [[ ${GITHUB_ACTIONS:-} != true ]]; then
          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2
          exit 2
        fi
        timeout 720s ci/run-dag.sh privileged --unsafe-no-cgroups
"#;

    #[test]
    fn privileged_unboxed_execution_requires_the_exact_actions_guard() {
        assert!(audit_privileged_unboxed_guard(GUARDED_WORKFLOW).is_ok());
    }

    #[test]
    fn privileged_unboxed_execution_refuses_incomplete_guards() {
        for required in [
            "        if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n",
            "          echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n",
            "          exit 2\n",
        ] {
            let incomplete = GUARDED_WORKFLOW.replacen(required, "", 1);
            assert!(audit_privileged_unboxed_guard(&incomplete).is_err());
        }
    }

    #[test]
    fn privileged_unboxed_execution_requires_one_explicit_opt_out() {
        let missing = GUARDED_WORKFLOW.replace(" --unsafe-no-cgroups", "");
        assert!(audit_privileged_unboxed_guard(&missing).is_err());

        let duplicate = format!("{GUARDED_WORKFLOW}# --unsafe-no-cgroups\n");
        assert!(audit_privileged_unboxed_guard(&duplicate).is_err());
    }

    #[test]
    fn privileged_unboxed_execution_rejects_broad_boxing_failure_acceptance() {
        let executable = format!("{GUARDED_WORKFLOW}        run: tool --allow-cgroup-failure\n");
        assert!(audit_privileged_unboxed_guard(&executable).is_err());
    }

    #[test]
    fn scheduled_jobs_uses_the_parsed_worker_capacity() {
        let default = scheduled_worker_capacity(&parse(std::iter::empty()));
        assert_eq!(default.configured(), 1);

        let explicit =
            scheduled_worker_capacity(&parse(["--jobs", "7"].into_iter().map(str::to_string)));
        assert_eq!(explicit.configured(), 7);
        assert_eq!(explicit.workers_for(12), 7);
        assert_eq!(explicit.workers_for(1), 1);
    }

    #[test]
    fn parallel_runner_delivers_every_completion_before_returning() {
        let active = AtomicUsize::new(0);
        let maximum = AtomicUsize::new(0);
        let consumed = Mutex::new(Vec::new());
        for_each_parallel(
            8,
            ScheduledWorkerCapacity::new(4),
            |index, emit| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                assert!(emit(index, false));
            },
            |index, value, _| {
                consumed.lock().unwrap().push((index, value));
                true
            },
        );
        let mut rows = consumed.into_inner().unwrap();
        rows.sort_unstable();
        assert_eq!(rows, (0..8).map(|index| (index, index)).collect::<Vec<_>>());
        assert!(maximum.load(Ordering::SeqCst) > 1);
    }

    #[test]
    fn retry_waits_for_publication_and_stops_after_pass() {
        let published = AtomicUsize::new(0);
        let executions = AtomicUsize::new(0);
        let rows = Mutex::new(Vec::new());
        for_each_parallel(
            1,
            ScheduledWorkerCapacity::new(1),
            |_, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        if attempt == 2 {
                            assert_eq!(published.load(Ordering::SeqCst), 1);
                        }
                        executions.fetch_add(1, Ordering::SeqCst);
                        attempt
                    },
                    |attempt| *attempt == 1,
                    emit,
                );
            },
            |_, attempt, will_retry| {
                rows.lock().unwrap().push((attempt, will_retry));
                published.fetch_add(1, Ordering::SeqCst);
                true
            },
        );
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(rows.into_inner().unwrap(), [(1, true), (2, false)]);
    }

    #[test]
    fn retry_stops_after_two_failures() {
        let executions = AtomicUsize::new(0);
        let mut rows = Vec::new();
        run_with_retry(
            1,
            |attempt| {
                executions.fetch_add(1, Ordering::SeqCst);
                attempt
            },
            |_| true,
            |attempt, will_retry| {
                rows.push((attempt, will_retry));
                true
            },
        );
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(rows, [(1, true), (2, false)]);
    }

    #[test]
    fn retry_starting_at_second_attempt_cannot_create_a_third() {
        let mut rows = Vec::new();
        run_with_retry(
            2,
            |attempt| attempt,
            |_| true,
            |attempt, will_retry| {
                rows.push((attempt, will_retry));
                true
            },
        );
        assert_eq!(rows, [(2, false)]);
    }

    #[test]
    fn one_failing_cell_does_not_rerun_its_passing_peer() {
        let executions = [AtomicUsize::new(0), AtomicUsize::new(0)];
        let terminal = Mutex::new(Vec::new());
        for_each_parallel(
            2,
            ScheduledWorkerCapacity::new(2),
            |index, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        executions[index].fetch_add(1, Ordering::SeqCst);
                        (index, attempt)
                    },
                    |(index, attempt)| *index == 0 && *attempt == 1,
                    emit,
                );
            },
            |index, _, will_retry| {
                if !will_retry {
                    terminal.lock().unwrap().push(index);
                }
                true
            },
        );
        assert_eq!(executions[0].load(Ordering::SeqCst), 2);
        assert_eq!(executions[1].load(Ordering::SeqCst), 1);
        let mut terminal = terminal.into_inner().unwrap();
        terminal.sort_unstable();
        assert_eq!(terminal, [0, 1]);
    }

    #[test]
    fn publication_refusal_prevents_the_retry() {
        let executions = AtomicUsize::new(0);
        for_each_parallel(
            1,
            ScheduledWorkerCapacity::new(1),
            |_, emit| {
                run_with_retry(
                    1,
                    |attempt| {
                        executions.fetch_add(1, Ordering::SeqCst);
                        attempt
                    },
                    |_| true,
                    emit,
                );
            },
            |_, _, _| false,
        );
        assert_eq!(executions.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manifest_jobs_parser_rejects_missing_invalid_and_duplicate_widths() {
        assert_eq!(
            command_jobs("test-harness run --jobs 20").unwrap(),
            Some(20)
        );
        assert_eq!(command_jobs("test-harness run").unwrap(), None);
        for command in [
            "test-harness run --jobs",
            "test-harness run --jobs 0",
            "test-harness run --jobs no",
            "test-harness run --jobs 2 --jobs 3",
        ] {
            assert!(command_jobs(command).is_err(), "accepted {command}");
        }
    }
}
