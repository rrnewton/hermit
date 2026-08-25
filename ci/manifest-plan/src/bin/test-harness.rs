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

use hermit_manifest_plan::runner::ManifestSet;
use hermit_manifest_plan::runner::Population;
use hermit_manifest_plan::runner::RunContext;
use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
use hermit_manifest_plan::runner::Selection;
use hermit_manifest_plan::runner::append_result;
use hermit_manifest_plan::runner::host_inapplicable_result;
use hermit_manifest_plan::runner::infrastructure_error_result;
use hermit_manifest_plan::runner::requires_capability;
use hermit_manifest_plan::runner::run_cell;
use hermit_manifest_plan::runner::write_junit;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::Value as YamlValue;

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

fn parse(mut values: impl Iterator<Item = String>) -> Args {
    let mut args = Args {
        format: "text".into(),
        ..Args::default()
    };
    while let Some(value) = values.next() {
        match value.as_str() {
            "--lane" => args.selection.lane = Some(required_value(&mut values, "--lane")),
            "--category" => {
                args.selection.category = Some(required_value(&mut values, "--category"))
            }
            "--test" => args.selection.test = Some(required_value(&mut values, "--test")),
            "--mode" => args.selection.mode = Some(required_value(&mut values, "--mode")),
            "--backend" => args.selection.backend = Some(required_value(&mut values, "--backend")),
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct HostCapabilityVerdict {
    present: bool,
    evidence: String,
}

fn parse_host_capability_verdict(stdout: &[u8]) -> Result<HostCapabilityVerdict, String> {
    let text = std::str::from_utf8(stdout)
        .map_err(|error| format!("host capability probe emitted non-UTF-8 output: {error}"))?
        .trim_end();
    let (state, evidence) = text
        .split_once('\t')
        .ok_or_else(|| format!("host capability probe emitted malformed output: {text:?}"))?;
    if evidence.trim().is_empty() {
        return Err("host capability probe emitted empty evidence".into());
    }
    let present = match state {
        "PRESENT" => true,
        "ABSENT" => false,
        other => {
            return Err(format!(
                "host capability probe emitted unknown state {other:?}"
            ));
        }
    };
    Ok(HostCapabilityVerdict {
        present,
        evidence: evidence.to_string(),
    })
}

fn probe_host_capability(root: &Path, capability: &str) -> HostCapabilityVerdict {
    let output = Command::new(root.join("scripts/validate.rs"))
        .args(["--probe-host-capability", capability])
        .current_dir(root)
        .output();
    match output {
        Ok(output) if output.status.success() => {
            match parse_host_capability_verdict(&output.stdout) {
                Ok(verdict) => verdict,
                Err(error) => HostCapabilityVerdict {
                    present: true,
                    evidence: format!("probe output was unusable ({error}); doubt runs the cell"),
                },
            }
        }
        Ok(output) => HostCapabilityVerdict {
            present: true,
            evidence: format!(
                "probe exited {}; doubt runs the cell ({})",
                output
                    .status
                    .code()
                    .map_or_else(|| "by signal".into(), |code| code.to_string()),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        },
        Err(error) => HostCapabilityVerdict {
            present: true,
            evidence: format!("probe could not start ({error}); doubt runs the cell"),
        },
    }
}

fn resolve_host_capabilities(
    root: &Path,
    cells: &[hermit_manifest_plan::runner::SelectedCell],
) -> BTreeMap<String, HostCapabilityVerdict> {
    let capabilities = cells
        .iter()
        .flat_map(|cell| cell.test.requires.iter())
        .filter_map(|token| requires_capability(token).ok().flatten())
        .collect::<BTreeSet<_>>();
    capabilities
        .into_iter()
        .map(|capability| {
            let verdict = probe_host_capability(root, capability);
            eprintln!(
                "Host capability {capability}: {} — {}",
                if verdict.present { "PRESENT" } else { "ABSENT" },
                verdict.evidence
            );
            (capability.to_string(), verdict)
        })
        .collect()
}

fn host_inapplicable_reason(
    requires: &[String],
    verdicts: &BTreeMap<String, HostCapabilityVerdict>,
) -> Option<(Vec<String>, String)> {
    let mut absent = requires
        .iter()
        .filter_map(|token| requires_capability(token).ok().flatten())
        .filter_map(|capability| {
            verdicts
                .get(capability)
                .filter(|verdict| !verdict.present)
                .map(|verdict| (capability.to_string(), verdict.evidence.clone()))
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
    let args = parse(values);
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
    run_audit(root, &root.join("tests/manifest-cli.rs"), &["self-test"]);
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

fn audit_expected_plan(root: &Path, manifests: &ManifestSet) -> usize {
    let cells = manifests
        .select(&Selection {
            population: Some(Population::Required),
            ..Selection::default()
        })
        .unwrap_or_else(|e| fail(e));
    let actual = cells
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
    let expected: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("ci/expected-e2e-plan.json")).unwrap()).unwrap();
    let expected = expected["cells"].as_array().cloned().unwrap_or_default();
    let normalize = |values: Vec<serde_json::Value>| {
        values
            .into_iter()
            .map(|v| serde_json::to_string(&v).unwrap())
            .collect::<BTreeSet<_>>()
    };
    if normalize(actual) != normalize(expected) {
        fail("required E2E plan changed; update ci/expected-e2e-plan.json in the same review");
    }
    cells.len()
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

#[derive(Deserialize)]
struct Dag {
    #[serde(default)]
    resource_caps: BTreeMap<String, u64>,
    default_step_timeout: Option<u64>,
    steps: Vec<DagStep>,
}

#[derive(Deserialize)]
struct DagStep {
    group: String,
    job: String,
    cmd: String,
    jobs_flag: Option<String>,
    #[serde(default)]
    deps: Vec<String>,
    timeout: Option<u64>,
    #[serde(default)]
    hint: Option<DagHint>,
    #[serde(default)]
    manifest: Option<DagManifest>,
}

#[derive(Deserialize)]
struct DagHint {
    preferred_inner_jobs: Option<u64>,
    #[serde(default)]
    resources: BTreeMap<String, u64>,
}

#[derive(Deserialize)]
struct DagManifest {
    lane: String,
    category: String,
}

fn command_jobs(command: &str) -> Result<Option<u64>, String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let mut jobs = None;
    let mut index = 0;
    while index < words.len() {
        if words[index] == "--jobs" {
            let value = words
                .get(index + 1)
                .ok_or_else(|| "manifest command has --jobs without a value".to_string())?
                .parse::<u64>()
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
    for lane in ["portable", "privileged"] {
        let path = root.join(format!("ci/dag/{lane}.json"));
        let dag: Dag = serde_json::from_slice(
            &fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?,
        )
        .map_err(|e| format!("{}: invalid DAG JSON: {e}", path.display()))?;
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
            .filter(|step| step.cmd == "target/debug/test-harness validate")
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
        if dag.steps.iter().filter(|step| step.cmd == build).count() != 1 {
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
        for step in dag
            .steps
            .iter()
            .filter(|step| step.group == "e2e" && step.job.starts_with("manifest_"))
        {
            let manifest = step.manifest.as_ref().ok_or_else(|| {
                format!("{}.{} lacks typed manifest identity", step.group, step.job)
            })?;
            if manifest.lane != lane {
                return Err(format!(
                    "{}.{} records lane {} in the {lane} DAG",
                    step.group, step.job, manifest.lane
                ));
            }
            let selector = format!(
                "target/debug/test-harness run --lane {lane} --category {} --ci-only --allow-empty --prebuilt",
                manifest.category
            );
            if !step.cmd.contains(&selector) {
                return Err(format!(
                    "{}.{} does not execute its typed selector literally",
                    step.group, step.job
                ));
            }
            if let Some(jobs) = command_jobs(&step.cmd)? {
                let hint = step.hint.as_ref().ok_or_else(|| {
                    format!(
                        "{}.{} has --jobs {jobs} without a resource hint",
                        step.group, step.job
                    )
                })?;
                let demand = hint.resources.get("manifest_guest").copied().unwrap_or(0);
                let cap = dag
                    .resource_caps
                    .get("manifest_guest")
                    .copied()
                    .unwrap_or(0);
                if demand != jobs
                    || cap < jobs
                    || hint.preferred_inner_jobs != Some(jobs)
                    || step.jobs_flag.as_deref() != Some("")
                {
                    return Err(format!(
                        "{}.{} runs --jobs {jobs} but declares manifest_guest={demand}, cap={cap}, preferred_inner_jobs={:?}, jobs_flag={:?}",
                        step.group, step.job, hint.preferred_inner_jobs, step.jobs_flag
                    ));
                }
            }
            if !actual.insert(manifest.category.clone()) {
                return Err(format!(
                    "{} has duplicate manifest bucket {}",
                    path.display(),
                    manifest.category
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
    let portable: Dag = serde_json::from_slice(
        &fs::read(root.join("ci/dag/portable.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("invalid portable DAG: {e}"))?;
    let privileged: Dag = serde_json::from_slice(
        &fs::read(root.join("ci/dag/privileged.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("invalid privileged DAG: {e}"))?;
    for (lane, dag) in [("portable", &portable), ("privileged", &privileged)] {
        for step in &dag.steps {
            if step.timeout.or(dag.default_step_timeout).is_none() {
                return Err(format!(
                    "{lane} node {}.{} has no derivable wall budget",
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
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or_else(|| {
                        format!(
                            "{lane} node {}.{} has an invalid CARGO_BUILD_JOBS prefix",
                            step.group, step.job
                        )
                    })?;
                if step
                    .hint
                    .as_ref()
                    .and_then(|hint| hint.preferred_inner_jobs)
                    != Some(jobs)
                {
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
            let timeout = step.timeout.or(portable.default_step_timeout).unwrap();
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
        let timeout = step.timeout.or(privileged.default_step_timeout).unwrap();
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

fn dag_critical_path(dag: &Dag) -> Result<u64, String> {
    let steps = dag
        .steps
        .iter()
        .map(|step| (format!("{}.{}", step.group, step.job), step))
        .collect::<std::collections::BTreeMap<_, _>>();
    fn visit(
        id: &str,
        dag: &Dag,
        steps: &std::collections::BTreeMap<String, &DagStep>,
        active: &mut BTreeSet<String>,
        memo: &mut std::collections::BTreeMap<String, u64>,
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
            .map(|dependency| visit(dependency, dag, steps, active, memo))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        active.remove(id);
        let value = predecessor + step.timeout.or(dag.default_step_timeout).unwrap();
        memo.insert(id.to_string(), value);
        Ok(value)
    }
    let mut memo = std::collections::BTreeMap::new();
    let mut maximum = 0;
    for id in steps.keys() {
        maximum = maximum.max(visit(id, dag, &steps, &mut BTreeSet::new(), &mut memo)?);
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
    for (category, test) in manifests.all_tests() {
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
        let cell = hermit_manifest_plan::runner::SelectedCell {
            category: category.into(),
            test: test.clone(),
            id: hermit_manifest_plan::runner::CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some(backend),
            },
            enabled: false,
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
/// each completed value to `consume` immediately.
///
/// The consumer stays on the calling thread so durable publication is
/// serialized even while the expensive cell executions overlap. This is
/// deliberately not a collect-then-publish helper: an outer bucket timeout
/// must not discard rows that completed before the timeout.
fn for_each_parallel<T: Send>(
    count: usize,
    capacity: ScheduledWorkerCapacity,
    execute: impl Fn(usize) -> T + Sync,
    mut consume: impl FnMut(usize, T),
) {
    if count == 0 {
        return;
    }
    let workers = capacity.workers_for(count);
    let next = AtomicUsize::new(0);
    let (sender, receiver) = mpsc::channel();
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
                    if sender.send((index, execute(index))).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for (index, value) in receiver {
            consume(index, value);
        }
    });
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
    let host_capabilities = resolve_host_capabilities(root, &cells);
    if cells.is_empty() && !args.allow_empty {
        fail("filters selected no cells");
    }
    let capacity = scheduled_worker_capacity(args);
    let context = RunContext::from_env(root.to_path_buf(), args.prebuilt)
        .unwrap_or_else(|e| fail(e))
        .with_scheduled_worker_capacity(capacity);
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
    if let Some(parent) = results_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&results_path, b"").unwrap();
    let mut indexed_results = Vec::new();
    let mut failed = false;
    let expected = cells.len();
    for_each_parallel(
        expected,
        capacity,
        |index| {
            let cell = &cells[index];
            if let Some((_, reason)) =
                host_inapplicable_reason(&cell.test.requires, &host_capabilities)
            {
                host_inapplicable_result(&context, cell, reason)
            } else {
                match run_cell(&context, cell) {
                    Ok(result) => result,
                    Err(error) => infrastructure_error_result(&context, cell, error),
                }
            }
        },
        |index, mut result| {
            // Publish before announcing the outcome. After a visible PASS line,
            // the complete typed row is already present even if the containing
            // bucket is killed before its JUnit/summary epilogue.
            if let Err(error) = append_result(&results_path, &result) {
                eprintln!(
                    "ERROR {} ({}/{}): completed cell result could not be published: {error}",
                    result.test,
                    result.mode,
                    result.backend.as_deref().unwrap_or("native")
                );
                result.outcome = "ERROR".into();
                result.error_kind = Some("result-publication".into());
                result.reason = Some(format!(
                    "completed cell result could not be published: {error}"
                ));
                failed = true;
            } else {
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
                //
                // This line was `FAIL <cell> (verify/ptrace)` and nothing else,
                // while `CellResult` already carried the divergence coordinates
                // and the reason. So a bucket failure could not be sorted into a
                // divergence class -- let alone located -- without re-running the
                // cell, and this class is INTERMITTENT: the c-programs bucket was
                // measured at 3 of 6 runs on the modern harness, so the
                // re-roll is not reliably available. Every observation was
                // costing a reproduction that might not come.
                //
                // PASS lines are deliberately untouched: they are the overwhelming
                // majority and carry nothing worth saying.
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
                    .filter_map(|(k, v)| v.map(|v| format!("{k}={v}")))
                    .collect::<Vec<_>>();
                    // Absent coordinates are OMITTED rather than printed as
                    // null: a cell that failed without ever reaching comparison
                    // has no location, and an empty bracket would suggest the
                    // lookup failed instead of that there is nothing to locate.
                    let mut suffix = String::new();
                    if !coords.is_empty() {
                        suffix.push_str(&format!(" [{}]", coords.join(" ")));
                    }
                    if let Some(reason) = result.reason.as_deref() {
                        suffix.push_str(&format!(" {reason}"));
                    }
                    // POINT AT THE EVIDENCE THAT ALREADY EXISTS.
                    //
                    // The per-cell artifacts are retained -- verify report,
                    // captures, the whole directory -- and the bucket log was
                    // the only thing standing between an investigator and them.
                    // 28 failures across 305 runs were investigable all along
                    // and were not investigated, because a FAIL line named the
                    // cell and no path. The coordinates above say WHERE the
                    // divergence is; this says WHAT ELSE IS AVAILABLE.
                    suffix.push_str(&format!("\n    evidence: {}", result.artifact_dir));
                    suffix
                };
                println!(
                    "{} {} ({}/{}){}",
                    result.outcome,
                    result.test,
                    result.mode,
                    result.backend.as_deref().unwrap_or("native"),
                    located
                );
                failed |= matches!(result.outcome.as_str(), "FAIL" | "ERROR");
            }
            indexed_results.push((index, result));
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
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use hermit_manifest_plan::runner::ScheduledWorkerCapacity;

    use super::HostCapabilityVerdict;
    use super::audit_privileged_unboxed_guard;
    use super::command_jobs;
    use super::for_each_parallel;
    use super::host_inapplicable_reason;
    use super::parse;
    use super::parse_host_capability_verdict;
    use super::scheduled_worker_capacity;

    #[test]
    fn host_capability_output_is_closed_and_evidence_bearing() {
        assert_eq!(
            parse_host_capability_verdict(b"PRESENT\tkernel accepted the probe\n").unwrap(),
            HostCapabilityVerdict {
                present: true,
                evidence: "kernel accepted the probe".into(),
            }
        );
        assert_eq!(
            parse_host_capability_verdict(b"ABSENT\tENODEV and no cpuinfo flag\n").unwrap(),
            HostCapabilityVerdict {
                present: false,
                evidence: "ENODEV and no cpuinfo flag".into(),
            }
        );
        for malformed in [
            b"PRESENT\n".as_slice(),
            b"PRESENT\t\n".as_slice(),
            b"UNKNOWN\tevidence\n".as_slice(),
        ] {
            assert!(parse_host_capability_verdict(malformed).is_err());
        }
    }

    #[test]
    fn only_a_declared_absent_capability_withholds_a_cell() {
        let absent = BTreeMap::from([(
            "cpuid-faulting".to_string(),
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
            "cpuid-faulting".to_string(),
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
            |index| {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
                active.fetch_sub(1, Ordering::SeqCst);
                index
            },
            |index, value| consumed.lock().unwrap().push((index, value)),
        );
        let mut rows = consumed.into_inner().unwrap();
        rows.sort_unstable();
        assert_eq!(rows, (0..8).map(|index| (index, index)).collect::<Vec<_>>());
        assert!(maximum.load(Ordering::SeqCst) > 1);
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
