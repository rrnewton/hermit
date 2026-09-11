use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;

use dagrun::TestResult;
use dagrun::TestResults;
use hermit_manifest_plan::cli_help::is_help_flag;
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
const VALIDATE_AUDIT_JOBS: usize = 2;
const PREBUILT_RUST_SCRIPTS_REQUIRED: &str = "HERMIT_PREBUILT_RUST_SCRIPTS_REQUIRED";
const DEFAULT_BUILD_JOBS: usize = 16;

const HELP: &str = "\
Usage: test-harness <COMMAND> [OPTIONS]

Validate, inspect, build, and run the centralized Hermit end-to-end manifests.

Commands:
  validate                         Validate manifests and their CI/DAG correspondence
  plan                             List selected required cells
  expected-plan                    Print the expected CI plan as JSON
  audit-gaps                       List selected disabled cells
  audit-inventory                  Validate the manifest-owned test inventory
  audit-test-binary-registration   Validate test binary registration
  audit-test-footprints            Check the generated test footprints
  audit-ci                         Audit DAG, budget, and expected-plan correspondence
  build                            Prepare selected test programs
  audit-compile                    Compile selected C test programs
  run                              Execute selected cells

Selection options:
  --lane <portable|privileged>
  --category <CATEGORY>
  --test <ID>
  --mode <verify|chaos|replay|naked|custom>
  --backend <ptrace|dbt|kvm|sabre|liteinst>
  --ci-only                        Select required CI cells
  --include-occasional             Include occasional cells
  --include-manual                 Include manual cells; requires exact test and mode
  --probe-disabled                 Run one exact disabled cell

Execution and output options:
  --prebuilt                       Reuse prepared test programs (run only)
  --allow-empty                    Permit an empty explicit CI selection
  --results <PATH>                 Write JSONL cell results to PATH
  --junit <PATH>                   Write JUnit output to PATH
  --format <text|json>             Plan output format (default: text)
  --jobs <N>                       Prepare/run at most N tests/cells concurrently
  -h, --help                       Print this help";

const PUBLIC_EXECUTION_ENVIRONMENT: &str =
    "  E2E_RESULT_ROOT=<PATH>                 Result root (default: ignored/e2e)
  E2E_BUILD_ROOT=<PATH>                  Prepared-program build root
  E2E_RUN_ID=<ID>                        Run identifier and result subdirectory
  E2E_RUN_INDEX=<N>                      Non-negative run index recorded in results
  E2E_MACHINE_SHORTNAME=<NAME>           Machine name recorded in results
  E2E_KERNEL_VERSION=<VERSION>           Kernel version recorded in results
  HERMIT_BIN=<PATH>                      Hermit executable (default: target/debug/hermit)
  HERMIT_E2E_EMPTY_WORKDIR=/test         Use the isolated /test working directory
  E2E_KEEP_VERIFY_LOGS=1                 Retain successful verification logs
  HERMIT_TEST_CPU_TIMEOUT_MULTIPLIER=<N> Positive finite CPU-time multiplier
  HERMIT_TEST_WALL_TIMEOUT_MULTIPLIER=<N> Positive finite wall-time multiplier";

const FILTER_OPTIONS: &str = "  --lane <portable|privileged>
  --category <CATEGORY>
  --test <ID>
  --mode <verify|chaos|replay|naked|custom>
  --backend <ptrace|dbt|kvm|sabre|liteinst>";

const AMBIENT_PREPARATION_ENVIRONMENT: &str =
    "  HOME=<PATH>                            Base for default Rust toolchain homes
  RUSTUP_HOME=<PATH>                     Rustup home preserved during preparation only
  CARGO_HOME=<PATH>                      Cargo home preserved during preparation only";

#[derive(Clone, Copy)]
enum CommandEnvironment {
    None,
    Execution,
    Run,
}

fn print_command_help(command: &str) -> bool {
    let (summary, filter_options, options, environment) = match command {
        "validate" => (
            "Validate manifests and their CI/DAG correspondence.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "plan" => (
            "List selected required cells.",
            true,
            "  --include-occasional             Include occasional cells\n  \
             --include-manual                 Include manual cells; requires exact test and mode\n  \
             --format <text|json>             Output format (default: text)",
            CommandEnvironment::None,
        ),
        "expected-plan" => (
            "Print the expected CI plan as JSON.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-gaps" => (
            "List selected disabled cells.",
            true,
            "  --include-occasional             Include occasional cells\n  \
             --format <text|json>             Output format (default: text)",
            CommandEnvironment::None,
        ),
        "audit-inventory" => (
            "Validate the manifest-owned test inventory.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-test-binary-registration" => (
            "Validate test binary registration.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-test-footprints" => (
            "Check the generated test footprints.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "audit-ci" => (
            "Audit DAG, budget, and expected-plan correspondence.",
            false,
            "",
            CommandEnvironment::None,
        ),
        "build" => (
            "Prepare selected test programs.",
            true,
            "  --ci-only                        Select required CI cells\n  \
             --include-occasional             Include occasional cells\n  \
             --include-manual                 Include manual cells; requires exact test and mode\n  \
             --allow-empty                    Permit an empty CI selection; requires --ci-only and lane/category",
            CommandEnvironment::Execution,
        ),
        "audit-compile" => (
            "Compile selected C test programs.",
            false,
            "  --lane <portable|privileged>\n  --category <CATEGORY>\n  --test <ID>",
            CommandEnvironment::Execution,
        ),
        "run" => (
            "Execute selected cells.",
            true,
            "  --ci-only                        Select required CI cells\n  \
             --include-occasional             Include occasional cells\n  \
             --include-manual                 Include manual cells; requires exact test and mode\n  \
             --probe-disabled                 Run one disabled cell; requires exact test/mode/backend\n  \
             --prebuilt                       Reuse prepared test programs\n  \
             --allow-empty                    Permit an empty CI selection; requires --ci-only and category\n  \
             --results <PATH>                 Write JSONL cell results to PATH\n  \
             --junit <PATH>                   Write JUnit output to PATH\n  \
             --jobs <N>                       Run at most N cells concurrently",
            CommandEnvironment::Run,
        ),
        _ => return false,
    };
    let options_marker = if filter_options || !options.is_empty() {
        " [OPTIONS]"
    } else {
        ""
    };
    println!("Usage: test-harness {command}{options_marker}\n\n{summary}\n\nOptions:");
    if filter_options {
        println!("{FILTER_OPTIONS}");
    }
    if !options.is_empty() {
        println!("{options}");
    }
    println!("  -h, --help                       Print this help");
    if matches!(
        environment,
        CommandEnvironment::Execution | CommandEnvironment::Run
    ) {
        println!("\nEnvironment:\n{PUBLIC_EXECUTION_ENVIRONMENT}");
        println!("\nAmbient fixture-preparation environment:\n{AMBIENT_PREPARATION_ENVIRONMENT}");
    }
    if matches!(environment, CommandEnvironment::Run) {
        println!(
            "\nInternal runner protocol:\n  \
             DAGRUN_TEST_COUNTS_PATH=<PATH>       Write schema-2 test counts for dagrun"
        );
    }
    true
}

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

fn build_worker_capacity(args: &Args) -> ScheduledWorkerCapacity {
    ScheduledWorkerCapacity::new(args.jobs.unwrap_or(DEFAULT_BUILD_JOBS))
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
    if !matches!(command, "build" | "run") && args.jobs.is_some() {
        fail("--jobs is accepted by build and run only");
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
    let values = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(values.as_slice(), [flag] if is_help_flag(flag)) {
        println!("{HELP}\n\nEnvironment:\n{PUBLIC_EXECUTION_ENVIRONMENT}");
        return ExitCode::SUCCESS;
    }
    if let [command, flag] = values.as_slice() {
        if is_help_flag(flag) && print_command_help(command) {
            return ExitCode::SUCCESS;
        }
    }
    let mut values = values.into_iter();
    let command = values
        .next()
        .unwrap_or_else(|| fail("missing command; try `test-harness --help`"));
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
    audit_determinism_stress_evidence(root);
    // These self-contained audits read the same checked-out tree but keep all
    // generated state in their own temporary directories. The validation DAG
    // supplies immutable prebuilt rust-script binaries; without that guarantee,
    // retain the old serial order instead of making rust-script compilers contend
    // for their shared cache. Run no more than two at once: two is the largest
    // clean concurrent validation width established on this host, and a wider
    // unmeasured default would turn this speed change into a new concurrency
    // assumption. Capture each child independently and replay it in the original
    // order so diagnostics remain attributable.
    let audit_jobs = if std::env::var(PREBUILT_RUST_SCRIPTS_REQUIRED).as_deref() == Ok("1") {
        VALIDATE_AUDIT_JOBS
    } else {
        1
    };
    run_audits_parallel(
        root,
        &[
            (
                root.join("target/debug/generate-test-footprints"),
                vec!["--check"],
            ),
            (
                root.join("tests/backend-parity/split_asymmetric_pr.py"),
                vec!["--self-test"],
            ),
            (root.join("tests/manifest-cli.rs"), vec!["self-test"]),
            // The DBT budget wrapper gates roughly twenty portable nodes and
            // fails CLOSED on a pin it is not calibrated for. Nothing else
            // notices: a truncated node reads like a fast one. This asserts end
            // to end that the wrapper still REACHES its wrapped command at the
            // recorded pin.
            (root.join("ci/run-with-reverie-dbt-budget-test.sh"), vec![]),
            (
                root.join("ci/compat-envelope/scorecard.rs"),
                vec!["self-test-and-check"],
            ),
            (
                root.join("ci/compat-envelope/pressure-test.rs"),
                vec!["self-test"],
            ),
            // The removed shell front door accumulated plan/scheduler/receipt
            // guards that now belong to the Rust validate driver. Exercise
            // those brackets without executing the validation DAG.
            (root.join("scripts/validate.rs"), vec!["--self-test"]),
        ],
        audit_jobs,
    );
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

fn run_audits_parallel(root: &Path, audits: &[(PathBuf, Vec<&str>)], jobs: usize) {
    let mut results = std::iter::repeat_with(|| None)
        .take(audits.len())
        .collect::<Vec<Option<Result<Output, String>>>>();
    for_each_parallel(
        audits.len(),
        ScheduledWorkerCapacity::new(jobs),
        |index, emit| {
            let (program, args) = &audits[index];
            let result = Command::new(program)
                .args(args)
                .current_dir(root)
                .output()
                .map_err(|error| format!("cannot execute {}: {error}", program.display()));
            let _ = emit(result, false);
        },
        |index, result, _| {
            results[index] = Some(result);
            true
        },
    );

    for ((program, args), result) in audits.iter().zip(results) {
        let output = result
            .expect("every validation audit worker returns one result")
            .unwrap_or_else(|error| fail(error));
        std::io::stdout()
            .write_all(&output.stdout)
            .unwrap_or_else(|error| {
                fail(format!(
                    "cannot replay {} stdout: {error}",
                    program.display()
                ))
            });
        std::io::stderr()
            .write_all(&output.stderr)
            .unwrap_or_else(|error| {
                fail(format!(
                    "cannot replay {} stderr: {error}",
                    program.display()
                ))
            });
        if !output.status.success() {
            if output.status.code() == Some(127) {
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
    println!(
        "test-harness: completed {} independent validation audits with up to {} concurrent workers",
        audits.len(),
        jobs.min(audits.len())
    );
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
    for lane in ["portable", "privileged"] {
        let path = root.join(format!("ci/dag/{lane}.json"));
        let dag = read_dag(&path)?;
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
            let dagrun::DagManifest {
                lane: manifest_lane,
                category,
                ..
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
    audit_workflow_run_dag_runners(root)?;
    let portable = read_dag(&root.join("ci/dag/portable.json"))?;
    let privileged = read_dag(&root.join("ci/dag/privileged.json"))?;
    for (lane, dag) in [("portable", &portable), ("privileged", &privileged)] {
        for step in &dag.steps {
            if step.timeout <= 0 {
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
            let timeout = u64::try_from(step.timeout).map_err(|_| {
                format!("portable node {node} has invalid timeout {}", step.timeout)
            })?;
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
    let launcher_line = privileged_workflow
        .lines()
        .find(|line| line.contains("ci/run-dag.sh privileged"))
        .ok_or_else(|| "cannot find privileged launcher command".to_string())?;
    let launcher_bound = command_timeout_seconds(launcher_line)?
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
        let timeout = u64::try_from(step.timeout).map_err(|_| {
            format!(
                "privileged node {}.{} has invalid timeout {}",
                step.group, step.job, step.timeout
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

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd)]
struct RunDagWorkflowBinding {
    id: String,
    run: String,
    input_max_mem: Option<String>,
}

fn audit_workflow_run_dag_runners(root: &Path) -> Result<(), String> {
    let expected = [
        RunDagWorkflowBinding {
            id: ".github/workflows/ci-dag.yml job dag-portable step 8".into(),
            run: "args=()\nif [[ -n $INPUT_MAX_MEM ]]; then\n  args=(--max-mem \"$INPUT_MAX_MEM\")\nfi\nci/run-dag.sh portable \"${args[@]}\" -v\n".into(),
            input_max_mem: Some("${{ inputs.max_mem }}".into()),
        },
        RunDagWorkflowBinding {
            id: ".github/workflows/ci-dag.yml job dag-privileged step 3".into(),
            run: "ci/run-dag.sh privileged -j 2 -v".into(),
            input_max_mem: None,
        },
        RunDagWorkflowBinding {
            id: ".github/workflows/ci-privileged.yml job privileged step 6".into(),
            run: "if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n  echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n  exit 2\nfi\ntimeout --foreground --kill-after=10s 1560s ci/run-dag.sh privileged -j 2 --unsafe-no-cgroups --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v\n".into(),
            input_max_mem: None,
        },
        RunDagWorkflowBinding {
            id: ".github/workflows/validation-levels.yml job full step 4".into(),
            run: "ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v".into(),
            input_max_mem: None,
        },
    ]
    .into_iter()
    .collect::<BTreeSet<_>>();

    let workflows = root.join(".github/workflows");
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(&workflows).map_err(|error| {
        format!(
            "cannot discover workflows in {}: {error}",
            workflows.display()
        )
    })? {
        let path = entry
            .map_err(|error| format!("cannot read workflow directory entry: {error}"))?
            .path();
        if !matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| format!("cannot relativize workflow {}: {error}", path.display()))?
            .to_path_buf();
        let label = relative.to_string_lossy();
        let workflow = parse_yaml(&path)?;
        actual.extend(collect_run_dag_workflow_bindings(&label, &workflow)?);
    }
    if actual != expected {
        let unexpected = actual.difference(&expected).collect::<Vec<_>>();
        let missing = expected.difference(&actual).collect::<Vec<_>>();
        return Err(format!(
            "run-dag workflow bindings drifted: unexpected={unexpected:#?} missing={missing:#?}"
        ));
    }
    Ok(())
}

fn collect_run_dag_workflow_bindings(
    label: &str,
    workflow: &YamlValue,
) -> Result<Vec<RunDagWorkflowBinding>, String> {
    let jobs = workflow["jobs"]
        .as_mapping()
        .ok_or_else(|| format!("workflow {label} has no jobs mapping"))?;
    reject_forbidden_run_dag_env(workflow, &format!("workflow {label}"))?;
    let mut bindings = Vec::new();
    for (job_name, job) in jobs {
        let job_name = job_name
            .as_str()
            .ok_or_else(|| format!("workflow {label} has a non-string job name"))?;
        let job_location = format!("workflow {label} job {job_name}");
        reject_forbidden_run_dag_env(job, &job_location)?;
        let Some(steps_value) = job.get("steps") else {
            continue;
        };
        let steps = steps_value
            .as_sequence()
            .ok_or_else(|| format!("workflow {label} job {job_name} steps must be a sequence"))?;
        for (step_index, step) in steps.iter().enumerate() {
            let Some(run_value) = step.get("run") else {
                continue;
            };
            let run = run_value.as_str().ok_or_else(|| {
                format!("workflow {label} job {job_name} step {step_index} run must be a string")
            })?;
            if run.lines().any(|line| {
                let line = line.trim_start();
                !line.starts_with('#') && line.contains("ci/run-dag.sh")
            }) {
                let location = format!("{job_location} step {step_index}");
                reject_forbidden_run_dag_env(step, &location)?;
                bindings.push(RunDagWorkflowBinding {
                    id: format!("{label} job {job_name} step {step_index}"),
                    run: run.to_string(),
                    input_max_mem: workflow_env_value(step, "INPUT_MAX_MEM", &location)?,
                });
            } else if run.contains("DAGRUN_BIN")
                || run.contains("DAGRUN_ENGINE")
                || run.contains("RUN_DAG_FILE_OVERRIDE")
            {
                return Err(format!(
                    "{job_location} step {step_index} sets a run-dag environment variable without invoking ci/run-dag.sh"
                ));
            }
        }
    }
    Ok(bindings)
}

fn workflow_env_value(
    scope: &YamlValue,
    key: &str,
    location: &str,
) -> Result<Option<String>, String> {
    let Some(environment) = scope.get("env") else {
        return Ok(None);
    };
    let environment = environment
        .as_mapping()
        .ok_or_else(|| format!("{location} env must be a mapping"))?;
    let key = YamlValue::String(key.to_string());
    let Some(value) = environment.get(&key) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|value| Some(value.to_string()))
        .ok_or_else(|| format!("{location} {} must be a string", key.as_str().unwrap()))
}

fn reject_forbidden_run_dag_env(scope: &YamlValue, location: &str) -> Result<(), String> {
    for key in ["DAGRUN_BIN", "DAGRUN_ENGINE", "RUN_DAG_FILE_OVERRIDE"] {
        if workflow_env_value(scope, key, location)?.is_some() {
            return Err(format!(
                "{location} sets {key}; ci/run-dag.sh chooses the constructed DAG and tracked Rust runner"
            ));
        }
    }
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
            .map(|dependency| visit(dependency, steps, active, memo))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .max()
            .unwrap_or(0);
        active.remove(id);
        let timeout = u64::try_from(step.timeout)
            .map_err(|_| format!("DAG node {id} has invalid timeout {}", step.timeout))?;
        let value = predecessor
            .checked_add(timeout)
            .ok_or_else(|| format!("DAG critical path overflows at {id}"))?;
        memo.insert(id.to_string(), value);
        Ok(value)
    }
    let mut memo = std::collections::BTreeMap::new();
    let mut maximum = 0;
    for id in steps.keys() {
        maximum = maximum.max(visit(id, &steps, &mut BTreeSet::new(), &mut memo)?);
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

fn command_timeout_seconds(command: &str) -> Result<Option<u64>, String> {
    let words = command.split_whitespace().collect::<Vec<_>>();
    let Some(index) = words.iter().position(|word| *word == "timeout") else {
        return Ok(None);
    };
    let budget = words[index + 1..]
        .iter()
        .find(|word| !word.starts_with('-'))
        .and_then(|word| word.trim_end_matches('\\').strip_suffix('s'))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| format!("cannot derive timeout budget from `{command}`"))?;
    Ok(Some(budget))
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
    let capacity = build_worker_capacity(args);
    let context = RunContext::from_env(root.to_path_buf(), false).unwrap_or_else(|e| fail(e));
    let mut seen = BTreeSet::new();
    let cells = cells
        .into_iter()
        .filter(|cell| seen.insert(cell.id.test.clone()))
        .collect::<Vec<_>>();
    let mut results = std::iter::repeat_with(|| None)
        .take(cells.len())
        .collect::<Vec<Option<Result<(), String>>>>();
    for_each_parallel(
        cells.len(),
        capacity,
        |index, emit| {
            let cell = &cells[index];
            let dir = context.build_root.join(cell.id.test.replace('/', "-"));
            let result = hermit_manifest_plan::runner::prepare_test(&context, cell, &dir).map(drop);
            let _ = emit(result, false);
        },
        |index, result, _| {
            results[index] = Some(result);
            true
        },
    );
    let mut failed = false;
    for (cell, result) in cells.iter().zip(results) {
        match result.expect("every fixture preparation worker returns one result") {
            Ok(_) => println!("BUILT {}", cell.id.test),
            Err(e) => {
                eprintln!("ERROR {}: {e}", cell.id.test);
                failed = true;
            }
        }
    }
    println!(
        "test-harness: completed {} preparation(s) with up to {} concurrent worker(s)",
        cells.len(),
        capacity.workers_for(cells.len())
    );
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
    for (category, inherited_timeout_seconds, inherited_cpu_timeout_seconds, test) in
        manifests.all_tests()
    {
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
        let cpu_timeout_seconds = verify
            .cpu_timeout_seconds
            .get(&backend)
            .copied()
            .unwrap_or(inherited_cpu_timeout_seconds);
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
            cpu_timeout_seconds,
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
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::path::PathBuf;
    use std::process::Command;
    use std::process::Output;
    use std::process::Stdio;
    use std::process::{self};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::thread;
    use std::time::Duration;

    use hermit_manifest_plan::runner::ManifestSet;
    use hermit_manifest_plan::runner::ScheduledWorkerCapacity;

    use super::DEFAULT_BUILD_JOBS;
    use super::EXPECTED_PLAN_SCHEMA;
    use super::HostCapability;
    use super::HostCapabilityVerdict;
    use super::accumulate_cell_cpu_usage;
    use super::audit_privileged_unboxed_guard;
    use super::audit_workflow_run_dag_runners;
    use super::build_worker_capacity;
    use super::collect_run_dag_workflow_bindings;
    use super::command_jobs;
    use super::command_timeout_seconds;
    use super::expected_plan_document;
    use super::for_each_parallel;
    use super::host_inapplicable_reason;
    use super::parse;
    use super::run_with_retry;
    use super::scheduled_worker_capacity;
    use super::structured_test_results_from_rows;
    use super::unique_plan_rows;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            for _ in 0..100 {
                let path = env::temp_dir().join(format!(
                    "hermit-test-harness-{label}-{}-{}",
                    process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("cannot create {}: {error}", path.display()),
                }
            }
            panic!("cannot allocate a unique temporary directory for {label}");
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn chmod(path: &Path, mode: u32) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn wait_for_file(path: &Path, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        path.exists()
    }

    fn read_pid(path: &Path) -> u32 {
        fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap()
    }

    fn process_is_live(pid: u32) -> bool {
        Path::new("/proc").join(pid.to_string()).exists()
    }

    fn proc_stat_field(pid: u32, index_after_comm: usize) -> Option<String> {
        let stat =
            fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("stat")).ok()?;
        let rest = stat.rsplit_once(") ")?.1;
        rest.split_whitespace()
            .nth(index_after_comm)
            .map(ToOwned::to_owned)
    }

    fn process_group(pid: u32) -> Option<u32> {
        proc_stat_field(pid, 2)?.parse::<u32>().ok()
    }

    fn processes_in_group(pgid: u32) -> Vec<u32> {
        let mut pids = fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().to_string_lossy().parse::<u32>().ok())
            .filter(|pid| process_group(*pid) == Some(pgid))
            .collect::<Vec<_>>();
        pids.sort_unstable();
        pids
    }

    const SLEEP_EXECUTABLE: &str = "/usr/bin/sleep";
    const SLEEP_30_CMDLINE: &[u8] = b"/usr/bin/sleep\0\x33\x30\0";

    #[derive(Clone)]
    struct RecordedProcess {
        pid: u32,
        start_time: String,
        cmdline: Vec<u8>,
    }

    impl RecordedProcess {
        fn capture(pid: u32) -> Self {
            Self {
                pid,
                start_time: proc_stat_field(pid, 19)
                    .unwrap_or_else(|| panic!("cannot read starttime for pid {pid}")),
                cmdline: fs::read(Path::new("/proc").join(pid.to_string()).join("cmdline"))
                    .unwrap_or_else(|error| panic!("cannot read cmdline for pid {pid}: {error}")),
            }
        }

        fn still_matches(&self) -> bool {
            proc_stat_field(self.pid, 19).as_deref() == Some(self.start_time.as_str())
                && fs::read(
                    Path::new("/proc")
                        .join(self.pid.to_string())
                        .join("cmdline"),
                )
                .is_ok_and(|cmdline| cmdline == self.cmdline)
        }

        fn wait_for_cmdline(
            mut self,
            expected_pgid: u32,
            expected_cmdline: &[u8],
            timeout: Duration,
        ) -> Result<Self, String> {
            let pid = self.pid;
            let recorded_start_time = self.start_time.clone();
            let cmdline_path = Path::new("/proc").join(pid.to_string()).join("cmdline");
            let deadline = std::time::Instant::now() + timeout;
            let context = || {
                format!(
                    "pid {pid} with recorded starttime {} and expected pgid {expected_pgid} waiting for cmdline {expected_cmdline:?}",
                    recorded_start_time
                )
            };
            loop {
                let observed_start_time = proc_stat_field(pid, 19).ok_or_else(|| {
                    format!("refusing to record {}: process disappeared", context())
                })?;
                if observed_start_time != recorded_start_time {
                    return Err(format!(
                        "refusing to record {}: starttime changed to {observed_start_time}",
                        context()
                    ));
                }
                let observed_pgid = process_group(pid).ok_or_else(|| {
                    format!(
                        "refusing to record {}: process group disappeared",
                        context()
                    )
                })?;
                if observed_pgid != expected_pgid {
                    return Err(format!(
                        "refusing to record {}: pgid changed to {observed_pgid}",
                        context()
                    ));
                }

                let cmdline = fs::read(&cmdline_path);
                if cmdline
                    .as_deref()
                    .is_ok_and(|bytes| bytes == expected_cmdline)
                {
                    let final_start_time = proc_stat_field(pid, 19).ok_or_else(|| {
                        format!(
                            "refusing to record {}: process disappeared after cmdline matched",
                            context()
                        )
                    })?;
                    if final_start_time != recorded_start_time {
                        return Err(format!(
                            "refusing to record {}: starttime changed to {final_start_time} after cmdline matched",
                            context()
                        ));
                    }
                    let final_pgid = process_group(pid).ok_or_else(|| {
                        format!("refusing to record {}: process group disappeared after cmdline matched", context())
                    })?;
                    if final_pgid != expected_pgid {
                        return Err(format!(
                            "refusing to record {}: pgid changed to {final_pgid} after cmdline matched",
                            context()
                        ));
                    }
                    self.cmdline = cmdline
                        .as_ref()
                        .expect("checked successful cmdline read")
                        .clone();
                    if self.still_matches() && process_group(pid) == Some(expected_pgid) {
                        return Ok(self);
                    }
                }

                if std::time::Instant::now() >= deadline {
                    return Err(format!(
                        "timed out after {timeout:?} while {}; last starttime {observed_start_time}, last pgid {observed_pgid}, last cmdline observation: {cmdline:?}",
                        context()
                    ));
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    struct UnboxedProcessGroupCleanup {
        pgid: u32,
        processes: Vec<RecordedProcess>,
        armed: bool,
        allow_group_signal: bool,
    }

    impl UnboxedProcessGroupCleanup {
        fn new(pgid: u32, processes: Vec<RecordedProcess>) -> Self {
            let harness_pgid =
                process_group(process::id()).expect("cannot read test harness process group");
            assert_ne!(
                pgid, harness_pgid,
                "unboxed cleanup must not target the test harness process group"
            );
            assert!(
                processes
                    .iter()
                    .all(|process| process_group(process.pid) == Some(pgid)),
                "recorded test processes must all be in pgid {pgid}"
            );
            let cleanup = Self {
                pgid,
                processes,
                armed: true,
                allow_group_signal: true,
            };
            let unrecorded = cleanup.unrecorded_processes_in_group();
            if !unrecorded.is_empty() {
                let mut cleanup = cleanup;
                cleanup.allow_group_signal = false;
                panic!(
                    "unboxed cleanup must not target a process group with unrecorded members: {:?}",
                    unrecorded
                );
            }
            cleanup
        }

        fn matching_processes(&self) -> Vec<u32> {
            self.processes
                .iter()
                .filter(|process| {
                    process.still_matches() && process_group(process.pid) == Some(self.pgid)
                })
                .map(|process| process.pid)
                .collect()
        }

        fn unrecorded_processes_in_group(&self) -> Vec<u32> {
            processes_in_group(self.pgid)
                .into_iter()
                .filter(|pid| {
                    !self.processes.iter().any(|process| {
                        process.pid == *pid
                            && process.still_matches()
                            && process_group(process.pid) == Some(self.pgid)
                    })
                })
                .collect()
        }

        fn cleanup(&mut self) {
            if self.matching_processes().is_empty() {
                self.armed = false;
                return;
            }
            let unrecorded = self.unrecorded_processes_in_group();
            if !unrecorded.is_empty() {
                self.allow_group_signal = false;
                panic!(
                    "refusing to signal pgid {} because it contains unrecorded processes: {:?}",
                    self.pgid, unrecorded
                );
            }
            let status = Command::new("kill")
                .arg("-TERM")
                .arg(format!("-{}", self.pgid))
                .status()
                .unwrap_or_else(|error| {
                    panic!("cannot signal test process group {}: {error}", self.pgid)
                });
            assert!(
                status.success(),
                "failed to signal test process group {}",
                self.pgid
            );
            for _ in 0..200 {
                if self.matching_processes().is_empty() {
                    self.armed = false;
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!(
                "recorded test processes still live in pgid {}: {:?}",
                self.pgid,
                self.matching_processes()
            );
        }

        fn cleanup_matching_pids(&mut self) {
            for pid in self.matching_processes() {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(pid.to_string())
                    .status();
            }
            for _ in 0..200 {
                if self.matching_processes().is_empty() {
                    self.armed = false;
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    impl Drop for UnboxedProcessGroupCleanup {
        fn drop(&mut self) {
            if self.armed && !self.matching_processes().is_empty() {
                if self.allow_group_signal && self.unrecorded_processes_in_group().is_empty() {
                    let _ = Command::new("kill")
                        .arg("-TERM")
                        .arg(format!("-{}", self.pgid))
                        .status();
                    for _ in 0..200 {
                        if self.matching_processes().is_empty() {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                } else {
                    self.cleanup_matching_pids();
                }
            }
        }
    }

    struct RecordedPidCleanup {
        process: RecordedProcess,
    }

    impl RecordedPidCleanup {
        fn new(process: RecordedProcess) -> Self {
            Self { process }
        }

        fn cleanup(&mut self) {
            if self.process.still_matches() {
                let _ = Command::new("kill")
                    .arg("-TERM")
                    .arg(self.process.pid.to_string())
                    .status();
                for _ in 0..200 {
                    if !self.process.still_matches() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    impl Drop for RecordedPidCleanup {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    struct SpawnedChildCleanup(Option<process::Child>);

    impl SpawnedChildCleanup {
        fn new(child: process::Child) -> Self {
            Self(Some(child))
        }

        fn child_mut(&mut self) -> &mut process::Child {
            self.0.as_mut().expect("spawned child already cleaned up")
        }

        fn cleanup(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl Drop for SpawnedChildCleanup {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    fn managed_scope_for_pid(pid: u32) -> (String, PathBuf) {
        let cgroup = fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("cgroup"))
            .unwrap_or_else(|error| panic!("cannot read cgroup for pid {pid}: {error}"));
        let relative = cgroup
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .unwrap_or_else(|| panic!("pid {pid} has no cgroup-v2 entry: {cgroup}"));
        let scope_end = relative
            .find(".scope")
            .map(|index| index + ".scope".len())
            .unwrap_or_else(|| panic!("pid {pid} is not in a dagrun scope: {cgroup}"));
        let scope_relative = &relative[..scope_end];
        let scope = scope_relative
            .split('/')
            .find(|segment| segment.starts_with("dagrun-") && segment.ends_with(".scope"))
            .unwrap_or_else(|| panic!("pid {pid} is not in a dagrun scope: {cgroup}"))
            .to_owned();
        (
            scope,
            Path::new("/sys/fs/cgroup").join(scope_relative.trim_start_matches('/')),
        )
    }

    fn managed_scope_processes(cgroup_root: &Path) -> Vec<u32> {
        if !cgroup_root.exists() {
            return Vec::new();
        }
        let mut pids = Vec::new();
        let mut stack = vec![cgroup_root.to_path_buf()];
        while let Some(path) = stack.pop() {
            if let Ok(text) = fs::read_to_string(path.join("cgroup.procs")) {
                pids.extend(
                    text.lines()
                        .filter_map(|line| line.trim().parse::<u32>().ok()),
                );
            }
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                        stack.push(entry.path());
                    }
                }
            }
        }
        pids.sort_unstable();
        pids.dedup();
        pids
    }

    struct ManagedScopeCleanup {
        scope: String,
        cgroup_path: PathBuf,
        pids: Vec<u32>,
        armed: bool,
    }

    impl ManagedScopeCleanup {
        fn new(scope: String, cgroup_path: PathBuf, pids: Vec<u32>) -> Self {
            Self {
                scope,
                cgroup_path,
                pids,
                armed: true,
            }
        }

        fn cleanup(&mut self) {
            let output = Command::new("systemctl")
                .args(["--user", "stop", &self.scope])
                .output()
                .unwrap_or_else(|error| panic!("cannot stop {}: {error}", self.scope));
            if !output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let inactive = stdout.contains("not loaded")
                    || stderr.contains("not loaded")
                    || stdout.contains("inactive")
                    || stderr.contains("inactive");
                assert!(
                    inactive,
                    "systemctl stop {} failed:\nstdout:\n{}\nstderr:\n{}",
                    self.scope, stdout, stderr
                );
            }
            for _ in 0..200 {
                if self.pids.iter().all(|pid| !process_is_live(*pid))
                    && managed_scope_processes(&self.cgroup_path).is_empty()
                {
                    self.armed = false;
                    return;
                }
                thread::sleep(Duration::from_millis(10));
            }
            panic!(
                "managed scope {} still has pids {:?}; recorded pids still live: {:?}",
                self.scope,
                managed_scope_processes(&self.cgroup_path),
                self.pids
                    .iter()
                    .copied()
                    .filter(|pid| process_is_live(*pid))
                    .collect::<Vec<_>>()
            );
        }
    }

    impl Drop for ManagedScopeCleanup {
        fn drop(&mut self) {
            if self.armed {
                let _ = Command::new("systemctl")
                    .args(["--user", "stop", &self.scope])
                    .status();
                for _ in 0..200 {
                    if self.pids.iter().all(|pid| !process_is_live(*pid))
                        && managed_scope_processes(&self.cgroup_path).is_empty()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    struct RunDagFixture {
        root: TestDirectory,
        construct_marker: PathBuf,
        runner_marker: PathBuf,
        args_marker: PathBuf,
        dag_bytes_marker: PathBuf,
        runner_pid_marker: PathBuf,
        signal_marker: PathBuf,
        config_marker: PathBuf,
    }

    impl RunDagFixture {
        fn new(label: &str, runner: RunnerFixture) -> Self {
            let root = TestDirectory::new(label);
            fs::create_dir_all(root.path().join("ci")).unwrap();
            fs::create_dir_all(root.path().join("scripts")).unwrap();
            fs::create_dir_all(root.path().join("agent-utils/rs/bin")).unwrap();
            fs::create_dir_all(root.path().join("target/validation")).unwrap();
            fs::copy(
                super::root().join("ci/run-dag.sh"),
                root.path().join("ci/run-dag.sh"),
            )
            .unwrap();
            chmod(&root.path().join("ci/run-dag.sh"), 0o755);
            fs::write(
                root.path().join("ci/configure-build-jobs.sh"),
                r#"if [[ ${CI_DAG_BUILD_JOBS:-} == not-a-number ]]; then
  printf evaluated > "$RUN_DAG_CONFIG_MARKER"
  echo "fake configure-build-jobs: invalid CI_DAG_BUILD_JOBS" >&2
  return 65
fi
CARGO_BUILD_JOBS=16
export CARGO_BUILD_JOBS
"#,
            )
            .unwrap();
            let validate = root.path().join("scripts/validate.rs");
            fs::write(
                &validate,
                r#"#!/usr/bin/env bash
set -euo pipefail
printf constructed > "$RUN_DAG_CONSTRUCT_MARKER"
output=
while (($#)); do
  if [[ $1 == --write-constructed-dag ]]; then
    shift
    output=$1
  fi
  shift || true
done
if [[ -z ${output:-} ]]; then
  echo "fake validate: missing --write-constructed-dag" >&2
  exit 44
fi
if [[ -n ${RUN_DAG_VALIDATE_EXIT:-} ]]; then
  echo "fake validate: planted construction failure" >&2
  exit "$RUN_DAG_VALIDATE_EXIT"
fi
if [[ -n ${RUN_DAG_VALIDATE_STDOUT:-} ]]; then
  printf '%s\n' "$RUN_DAG_VALIDATE_STDOUT"
fi
printf '{"schema":1,"steps":[{"id":"fixture","result_manifests":[]}]}' > "$output"
if [[ -n ${RUN_DAG_VALIDATE_UNREADABLE_OUTPUT:-} ]]; then
  chmod 000 "$output"
fi
"#,
            )
            .unwrap();
            chmod(&validate, 0o755);

            let runner_path = root.path().join("agent-utils/rs/bin/dagrun");
            match runner {
                RunnerFixture::Missing => {}
                RunnerFixture::NonExecutable => {
                    fs::write(&runner_path, "#!/usr/bin/env bash\nexit 99\n").unwrap();
                    chmod(&runner_path, 0o644);
                }
                RunnerFixture::Executable => {
                    fs::write(
                        &runner_path,
                        r#"#!/usr/bin/env bash
	set -euo pipefail
	verb=${1:-}
	printf runner > "$RUN_DAG_RUNNER_MARKER"
	printf '%s\n' "$@" > "$RUN_DAG_ARGS_MARKER"
	dag=
while (($#)); do
  if [[ "$1" == --dag ]]; then
    shift
    dag=$1
  fi
  shift || true
done
if [[ "$dag" != - ]]; then
  echo "fake runner: expected stdin DAG marker '-', got $dag" >&2
  exit 41
fi
if ! compgen -G "$PWD/target/validation/run-dag.*" >/dev/null; then
  echo "fake runner: generated DAG directory was not retained before runner execution" >&2
  exit 42
	fi
	cat > "$RUN_DAG_DAG_BYTES_MARKER"
	case "$verb" in
	  json) printf '{"runner":"json"}\n' ;;
	  dot) printf 'digraph dagrun {}\n' ;;
	esac
	"#,
                    )
                    .unwrap();
                    chmod(&runner_path, 0o755);
                }
                RunnerFixture::Exit(code) => {
                    fs::write(
                        &runner_path,
                        format!(
                            r#"#!/usr/bin/env bash
set -euo pipefail
printf runner > "$RUN_DAG_RUNNER_MARKER"
printf '%s\n' "$@" > "$RUN_DAG_ARGS_MARKER"
dag=
while (($#)); do
  if [[ "$1" == --dag ]]; then
    shift
    dag=$1
  fi
  shift || true
done
if [[ "$dag" != - ]]; then
  echo "fake runner: expected stdin DAG marker '-', got $dag" >&2
  exit 41
fi
if ! compgen -G "$PWD/target/validation/run-dag.*" >/dev/null; then
  echo "fake runner: generated DAG directory was not retained before runner execution" >&2
  exit 42
fi
cat > "$RUN_DAG_DAG_BYTES_MARKER"
exit {code}
"#
                        ),
                    )
                    .unwrap();
                    chmod(&runner_path, 0o755);
                }
                RunnerFixture::Terminate => {
                    fs::write(
                        &runner_path,
                        r#"#!/usr/bin/env bash
set -euo pipefail
printf runner > "$RUN_DAG_RUNNER_MARKER"
printf '%s\n' "$@" > "$RUN_DAG_ARGS_MARKER"
dag=
while (($#)); do
  if [[ "$1" == --dag ]]; then
    shift
    dag=$1
  fi
  shift || true
done
if [[ "$dag" != - ]]; then
  echo "fake runner: expected stdin DAG marker '-', got $dag" >&2
  exit 41
fi
if ! compgen -G "$PWD/target/validation/run-dag.*" >/dev/null; then
  echo "fake runner: generated DAG directory was not retained before runner execution" >&2
  exit 42
fi
cat > "$RUN_DAG_DAG_BYTES_MARKER"
kill -TERM $$
"#,
                    )
                    .unwrap();
                    chmod(&runner_path, 0o755);
                }
                RunnerFixture::Sleep => {
                    fs::write(
                        &runner_path,
                        r#"#!/usr/bin/env bash
set -euo pipefail
printf runner > "$RUN_DAG_RUNNER_MARKER"
printf '%s\n' "$@" > "$RUN_DAG_ARGS_MARKER"
dag=
while (($#)); do
  if [[ "$1" == --dag ]]; then
    shift
    dag=$1
  fi
  shift || true
done
if [[ "$dag" != - ]]; then
  echo "fake runner: expected stdin DAG marker '-', got $dag" >&2
  exit 41
fi
if ! compgen -G "$PWD/target/validation/run-dag.*" >/dev/null; then
  echo "fake runner: generated DAG directory was not retained before runner execution" >&2
  exit 42
fi
cat > "$RUN_DAG_DAG_BYTES_MARKER"
wait_child=
finish_signal() {
  signal=$1
  code=$2
  if [[ -n ${wait_child:-} ]]; then
    kill "$wait_child" 2>/dev/null || true
    wait "$wait_child" 2>/dev/null || true
  fi
  printf '%s' "$signal" > "$RUN_DAG_SIGNAL_MARKER"
  exit "$code"
}
trap 'finish_signal HUP 129' HUP
trap 'finish_signal INT 130' INT
trap 'finish_signal TERM 143' TERM
sleep 30 &
wait_child=$!
printf '%s\n' "$$" > "$RUN_DAG_RUNNER_PID_MARKER"
wait "$wait_child"
"#,
                    )
                    .unwrap();
                    chmod(&runner_path, 0o755);
                }
            }

            Self {
                construct_marker: root.path().join("constructed.marker"),
                runner_marker: root.path().join("runner.marker"),
                args_marker: root.path().join("runner.args"),
                dag_bytes_marker: root.path().join("runner.dag.json"),
                runner_pid_marker: root.path().join("runner.pid"),
                signal_marker: root.path().join("runner.signal"),
                config_marker: root.path().join("configure.marker"),
                root,
            }
        }

        fn command(&self, args: &[&str], envs: &[(&str, &str)]) -> Command {
            let mut command = Command::new("bash");
            command
                .arg(self.root.path().join("ci/run-dag.sh"))
                .args(args)
                .current_dir(self.root.path())
                .env_remove("DAGRUN_BIN")
                .env_remove("DAGRUN_ENGINE")
                .env_remove("RUN_DAG_FILE_OVERRIDE")
                .env("RUN_DAG_CONSTRUCT_MARKER", &self.construct_marker)
                .env("RUN_DAG_RUNNER_MARKER", &self.runner_marker)
                .env("RUN_DAG_ARGS_MARKER", &self.args_marker)
                .env("RUN_DAG_DAG_BYTES_MARKER", &self.dag_bytes_marker)
                .env("RUN_DAG_RUNNER_PID_MARKER", &self.runner_pid_marker)
                .env("RUN_DAG_SIGNAL_MARKER", &self.signal_marker)
                .env("RUN_DAG_CONFIG_MARKER", &self.config_marker);
            for (key, value) in envs {
                command.env(key, value);
            }
            command
        }

        fn run(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
            self.command(args, envs).output().unwrap()
        }

        fn run_with_stdin(&self, args: &[&str], envs: &[(&str, &str)], stdin: &[u8]) -> Output {
            let mut child = self
                .command(args, envs)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.as_mut().unwrap().write_all(stdin).unwrap();
            child.wait_with_output().unwrap()
        }

        fn mutate_run_dag(&self, from: &str, to: &str) {
            let path = self.root.path().join("ci/run-dag.sh");
            let script = fs::read_to_string(&path).unwrap();
            assert_eq!(
                script.matches(from).count(),
                1,
                "run-dag.sh mutation target must be exact: {from:?}"
            );
            fs::write(path, script.replace(from, to)).unwrap();
        }

        fn assert_no_construction_or_runner(&self) {
            assert!(
                !self.construct_marker.exists(),
                "validate construction marker must not be written"
            );
            assert!(
                !self.runner_marker.exists(),
                "runner marker must not be written"
            );
        }

        fn assert_runner_never_consumed_stdin(&self) {
            assert!(
                !self.runner_marker.exists(),
                "runner marker must not be written"
            );
            assert!(
                !self.dag_bytes_marker.exists(),
                "runner must not consume ambient stdin"
            );
        }

        fn assert_no_generated_dag_dirs(&self) {
            let leftovers = fs::read_dir(self.root.path().join("target/validation"))
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("run-dag."))
                .collect::<Vec<_>>();
            assert!(
                leftovers.is_empty(),
                "generated DAG directories were created before preflight refusal: {leftovers:?}"
            );
        }

        fn generated_dag_dirs(&self) -> Vec<PathBuf> {
            fs::read_dir(self.root.path().join("target/validation"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("run-dag."))
                })
                .collect()
        }

        fn assert_generated_dag_dir_retained(&self) -> PathBuf {
            let leftovers = self.generated_dag_dirs();
            assert_eq!(
                leftovers.len(),
                1,
                "expected exactly one retained generated DAG directory, got {leftovers:?}"
            );
            leftovers.into_iter().next().unwrap()
        }

        fn assert_runner_read_valid_dag(&self) {
            let dag: serde_json::Value =
                serde_json::from_slice(&fs::read(&self.dag_bytes_marker).unwrap()).unwrap();
            let steps = dag["steps"].as_array().expect("fixture DAG has steps");
            assert_eq!(steps[0]["id"].as_str(), Some("fixture"));
        }
    }

    enum RunnerFixture {
        Missing,
        NonExecutable,
        Executable,
        Exit(u8),
        Terminate,
        Sleep,
    }

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
    fn run_dag_workflow_bindings_reject_runner_overrides() {
        for (scope, yaml) in [
            (
                "workflow",
                "env:\n  DAGRUN_ENGINE: py\njobs:\n  validation:\n    steps:\n      - run: ci/run-dag.sh portable -v\n",
            ),
            (
                "job",
                "jobs:\n  validation:\n    env:\n      DAGRUN_BIN: agent-utils/rs/bin/dagrun\n    steps:\n      - run: ci/run-dag.sh portable -v\n",
            ),
            (
                "step",
                "jobs:\n  validation:\n    steps:\n      - env:\n          RUN_DAG_FILE_OVERRIDE: ci/dag/raw.json\n        run: ci/run-dag.sh privileged -v\n",
            ),
        ] {
            let workflow: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
            let error = collect_run_dag_workflow_bindings("fixture.yml", &workflow)
                .expect_err("workflow bindings must not select a runner or raw DAG");
            assert!(error.contains(scope), "{error}");
        }
    }

    #[test]
    fn run_dag_workflow_discovery_requires_the_current_four_bindings() {
        let root = TestDirectory::new("workflow-discovery");
        let workflows = root.path().join(".github/workflows");
        fs::create_dir_all(&workflows).unwrap();
        fs::write(
            workflows.join("ci-dag.yml"),
            "jobs:\n  dag-portable:\n    steps:\n      - run: echo one\n      - run: echo two\n      - run: echo three\n      - run: echo four\n      - run: echo five\n      - run: echo six\n      - run: echo seven\n      - run: echo eight\n      - env:\n          INPUT_MAX_MEM: ${{ inputs.max_mem }}\n        run: |\n          args=()\n          if [[ -n $INPUT_MAX_MEM ]]; then\n            args=(--max-mem \"$INPUT_MAX_MEM\")\n          fi\n          ci/run-dag.sh portable \"${args[@]}\" -v\n  dag-privileged:\n    steps:\n      - run: echo one\n      - run: echo two\n      - run: echo three\n      - run: ci/run-dag.sh privileged -j 2 -v\n",
        )
        .unwrap();
        fs::write(
            workflows.join("ci-privileged.yml"),
            "jobs:\n  privileged:\n    steps:\n      - run: echo one\n      - run: echo two\n      - run: echo three\n      - run: echo four\n      - run: echo five\n      - run: echo six\n      - run: |\n          if [[ ${GITHUB_ACTIONS:-} != true ]]; then\n            echo 'privileged DAG: refusing explicit unboxed execution outside GitHub Actions' >&2\n            exit 2\n          fi\n          timeout --foreground --kill-after=10s 1560s ci/run-dag.sh privileged -j 2 --unsafe-no-cgroups --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v\n",
        )
        .unwrap();
        fs::write(
            workflows.join("validation-levels.yml"),
            "jobs:\n  full:\n    steps:\n      - run: echo one\n      - run: echo two\n      - run: echo three\n      - run: echo four\n      - run: ci/run-dag.sh privileged -j 2 --allow-cgroup-failure --perf-dir \"$RUNNER_TEMP/hermit-privileged-dag-perf\" -v\n",
        )
        .unwrap();
        audit_workflow_run_dag_runners(root.path()).unwrap();

        fs::write(
            workflows.join("new-caller.yaml"),
            "jobs:\n  validation:\n    steps:\n      - run: ci/run-dag.sh portable -v\n",
        )
        .unwrap();
        let error = audit_workflow_run_dag_runners(root.path())
            .expect_err("a newly added workflow caller must be discovered as unexpected");
        assert!(error.contains("new-caller.yaml"), "{error}");
    }

    #[test]
    fn run_dag_rejects_runtime_overrides_before_construction() {
        for (key, value) in [
            ("DAGRUN_BIN", "agent-utils/py/bin/dagrun"),
            ("DAGRUN_BIN", "agent-utils/rs/bin/dagrun"),
            ("DAGRUN_ENGINE", "py"),
            ("DAGRUN_ENGINE", "rust"),
            ("RUN_DAG_FILE_OVERRIDE", "ci/dag/raw.json"),
        ] {
            let fixture = RunDagFixture::new(key, RunnerFixture::Executable);
            let output = fixture.run(&["portable", "list"], &[(key, value)]);
            assert_eq!(output.status.code(), Some(2));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(key), "{stderr}");
            fixture.assert_no_construction_or_runner();
            fixture.assert_no_generated_dag_dirs();
        }
    }

    #[test]
    fn run_dag_help_accepts_short_and_long_forms() {
        let fixture = RunDagFixture::new("help", RunnerFixture::Executable);
        let baseline = fixture.run(&["-h"], &[]);
        assert!(baseline.status.success());
        let help_surfaces = [
            vec!["-h"],
            vec!["--help"],
            vec!["portable", "-h"],
            vec!["portable", "--help"],
            vec!["privileged", "-h"],
            vec!["privileged", "--help"],
            vec!["portable", "list", "-h"],
            vec!["portable", "list", "--help"],
            vec!["portable", "ascii", "-h"],
            vec!["portable", "ascii", "--help"],
            vec!["portable", "dot", "-h"],
            vec!["portable", "dot", "--help"],
            vec!["portable", "json", "-h"],
            vec!["portable", "json", "--help"],
            vec!["privileged", "list", "-h"],
            vec!["privileged", "list", "--help"],
            vec!["privileged", "ascii", "-h"],
            vec!["privileged", "ascii", "--help"],
            vec!["privileged", "dot", "-h"],
            vec!["privileged", "dot", "--help"],
            vec!["privileged", "json", "-h"],
            vec!["privileged", "json", "--help"],
        ];
        for args in help_surfaces {
            let output = fixture.run(args.as_slice(), &[]);
            assert!(output.status.success());
            assert_eq!(
                output.stdout, baseline.stdout,
                "help bytes changed for args {args:?}"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let normalized_stdout = stdout.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(stdout.contains("Usage:"), "{stdout}");
            assert!(
                normalized_stdout.contains("agent-utils/rs/bin/dagrun with --dag -"),
                "{stdout}"
            );
            assert!(
                normalized_stdout.contains("feeds the constructed DAG on stdin"),
                "{stdout}"
            );
            assert!(
                normalized_stdout.contains("run default; executes the DAG")
                    && normalized_stdout.contains("list | ascii | dot | json"),
                "{stdout}"
            );
            assert!(
                stdout.contains("DAGRUN_BIN") && stdout.contains("forwarded --dag"),
                "{stdout}"
            );
            assert!(stdout.contains("CI_DAG_BUILD_JOBS"), "{stdout}");
            assert!(
                normalized_stdout.contains("before configuration validation"),
                "{stdout}"
            );
            assert!(
                normalized_stdout.contains("After a generated DAG directory has been allocated"),
                "{stdout}"
            );
            assert!(
                normalized_stdout.contains("Pre-allocation refusals have no retained path"),
                "{stdout}"
            );
            assert!(
                normalized_stdout
                    .contains("When a retained path is printed, inspect that exact path"),
                "{stdout}"
            );
            assert!(
                normalized_stdout.contains(
                    "remove it manually after confirming no active process still needs it"
                ),
                "{stdout}"
            );
            assert!(
                stdout.contains("--unsafe-no-cgroups")
                    && normalized_stdout.contains("not containment guarantees"),
                "{stdout}"
            );
            assert!(
                !stdout.contains("independently boxed"),
                "help must not make an unconditional containment claim: {stdout}"
            );
            assert!(
                normalized_stdout.contains("raw graph"),
                "help must include the complete override sentence: {stdout}"
            );
            fixture.assert_no_construction_or_runner();
            fixture.assert_no_generated_dag_dirs();

            for (key, value) in [
                ("DAGRUN_BIN", "agent-utils/py/bin/dagrun"),
                ("DAGRUN_ENGINE", "py"),
                ("RUN_DAG_FILE_OVERRIDE", "ci/dag/raw.json"),
                ("CI_DAG_BUILD_JOBS", "not-a-number"),
            ] {
                let configured = fixture.run(args.as_slice(), &[(key, value)]);
                assert!(configured.status.success());
                assert_eq!(
                    configured.stdout, baseline.stdout,
                    "help output for {args:?} must not depend on {key}"
                );
                assert!(
                    !fixture.config_marker.exists(),
                    "help evaluated configuration for {args:?} with {key}"
                );
                fixture.assert_no_construction_or_runner();
                fixture.assert_no_generated_dag_dirs();
            }
        }
    }

    #[test]
    fn run_dag_does_not_treat_arbitrary_forwarded_help_as_wrapper_help() {
        let fixture = RunDagFixture::new("forwarded-help", RunnerFixture::Executable);
        let output = fixture.run(&["portable", "run", "-h"], &[]);
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(!stdout.contains("Usage:"), "{stdout}");
        let args = fs::read_to_string(&fixture.args_marker).unwrap();
        assert!(args.starts_with("run\n--dag\n-\nrun\n-h\n"), "{args}");
        fixture.assert_runner_read_valid_dag();
        fixture.assert_generated_dag_dir_retained();
    }

    #[test]
    fn run_dag_rejects_forwarded_dag_args_before_construction() {
        for arg in ["--dag", "--dag=ci/dag/raw.json"] {
            let fixture = RunDagFixture::new("forbidden-dag-arg", RunnerFixture::Executable);
            let output = fixture.run(&["privileged", "list", arg], &[]);
            assert_eq!(output.status.code(), Some(2));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("--dag"), "{stderr}");
            fixture.assert_no_construction_or_runner();
            fixture.assert_no_generated_dag_dirs();
        }
    }

    #[test]
    fn run_dag_refuses_missing_or_unexecutable_runner_before_construction() {
        let missing = RunDagFixture::new("missing-runner", RunnerFixture::Missing);
        let output = missing.run(&["portable", "list"], &[]);
        assert_eq!(output.status.code(), Some(127));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("agent-utils/rs/bin/dagrun"), "{stderr}");
        assert!(
            stderr.contains("Build or repair the tracked agent-utils checkout"),
            "{stderr}"
        );
        missing.assert_no_construction_or_runner();
        missing.assert_no_generated_dag_dirs();

        let non_executable =
            RunDagFixture::new("non-executable-runner", RunnerFixture::NonExecutable);
        let output = non_executable.run(&["portable", "list"], &[]);
        assert_eq!(output.status.code(), Some(126));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("not an executable file"), "{stderr}");
        assert!(
            stderr.contains("Build or repair the tracked agent-utils checkout"),
            "{stderr}"
        );
        non_executable.assert_no_construction_or_runner();
        non_executable.assert_no_generated_dag_dirs();
    }

    #[test]
    fn run_dag_accepts_the_tracked_symlinked_runner_shape() {
        let fixture = RunDagFixture::new("symlink-runner", RunnerFixture::Missing);
        let runner_target = fixture.root.path().join("agent-utils/rs/bin/cargo-runner");
        fs::write(
            &runner_target,
            r#"#!/usr/bin/env bash
set -euo pipefail
printf runner > "$RUN_DAG_RUNNER_MARKER"
dag=
while (($#)); do
  if [[ "$1" == --dag ]]; then
    shift
    dag=$1
  fi
  shift || true
done
cat "$dag" > "$RUN_DAG_DAG_BYTES_MARKER"
"#,
        )
        .unwrap();
        chmod(&runner_target, 0o755);
        std::os::unix::fs::symlink(
            "cargo-runner",
            fixture.root.path().join("agent-utils/rs/bin/dagrun"),
        )
        .unwrap();

        let output = fixture.run(&["portable", "list"], &[]);
        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(&fixture.runner_marker).unwrap(),
            "runner"
        );
        fixture.assert_runner_read_valid_dag();
        let retained = fixture.assert_generated_dag_dir_retained();
        assert!(retained.join("portable.json").exists());
    }

    #[test]
    fn run_dag_constructs_retains_and_pipes_the_lane_to_the_tracked_rust_runner() {
        let fixture = RunDagFixture::new("positive", RunnerFixture::Executable);
        let mut child = fixture
            .command(&["privileged", "list", "-v"], &[])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"ambient stdin sentinel\n")
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(&fixture.construct_marker).unwrap(),
            "constructed"
        );
        assert_eq!(
            fs::read_to_string(&fixture.runner_marker).unwrap(),
            "runner"
        );
        let args = fs::read_to_string(&fixture.args_marker).unwrap();
        assert!(args.starts_with("list\n--dag\n-\n"), "{args}");
        assert!(args.contains("\n-v\n"), "{args}");
        fixture.assert_runner_read_valid_dag();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("retained generated DAG directory:"),
            "{stderr}"
        );
        let retained = fixture.assert_generated_dag_dir_retained();
        assert!(retained.join("privileged.json").exists());
    }

    #[test]
    fn run_dag_retains_generated_dags_for_both_lanes_after_success() {
        for lane in ["portable", "privileged"] {
            let fixture = RunDagFixture::new(lane, RunnerFixture::Executable);
            let output = fixture.run(&[lane, "list"], &[]);
            assert!(output.status.success());
            fixture.assert_runner_read_valid_dag();
            let retained = fixture.assert_generated_dag_dir_retained();
            assert!(retained.join(format!("{lane}.json")).exists());
        }
    }

    #[test]
    fn run_dag_preserves_runner_status_and_retains_generated_dag() {
        for lane in ["portable", "privileged"] {
            let failed = RunDagFixture::new("cleanup-exit", RunnerFixture::Exit(23));
            let output = failed.run(&[lane, "list"], &[]);
            assert_eq!(output.status.code(), Some(23));
            failed.assert_runner_read_valid_dag();
            let retained = failed.assert_generated_dag_dir_retained();
            assert!(retained.join(format!("{lane}.json")).exists());
        }

        let numeric_143 = RunDagFixture::new("cleanup-exit-143", RunnerFixture::Exit(143));
        let output = numeric_143.run(&["portable", "list"], &[]);
        assert_eq!(output.status.code(), Some(143));
        assert_eq!(output.status.signal(), None);
        numeric_143.assert_runner_read_valid_dag();
        let retained = numeric_143.assert_generated_dag_dir_retained();
        assert!(retained.join("portable.json").exists());

        let signaled = RunDagFixture::new("cleanup-signal", RunnerFixture::Terminate);
        let output = signaled.run(&["privileged", "list"], &[]);
        assert_eq!(output.status.code(), None);
        assert_eq!(output.status.signal(), Some(15));
        signaled.assert_runner_read_valid_dag();
        let retained = signaled.assert_generated_dag_dir_retained();
        assert!(retained.join("privileged.json").exists());
    }

    #[test]
    fn run_dag_refuses_unreadable_constructed_dag_before_runner() {
        let fixture = RunDagFixture::new("unreadable-dag", RunnerFixture::Executable);
        let output = fixture.run_with_stdin(
            &["portable", "list"],
            &[("RUN_DAG_VALIDATE_UNREADABLE_OUTPUT", "1")],
            b"ambient stdin sentinel\n",
        );
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("constructed DAG is not readable"),
            "{stderr}"
        );
        assert!(
            stderr.contains("remove it only after confirming no validation run is active"),
            "{stderr}"
        );
        fixture.assert_runner_never_consumed_stdin();
        let retained = fixture.assert_generated_dag_dir_retained();
        assert!(stderr.contains(&retained.display().to_string()), "{stderr}");
    }

    #[test]
    fn run_dag_refuses_stdin_handoff_failures_before_runner() {
        for (label, from, to, expected) in [
            (
                "dup-failure",
                r#"if ! exec <&"$dag_fd"; then"#,
                "if ! exec <&999999; then",
                "could not attach constructed DAG to runner stdin",
            ),
            (
                "close-failure",
                r#"if ! exec {dag_fd}<&-; then"#,
                "if ! exec {dag_fd}<&999999; then",
                "could not close constructed DAG descriptor after stdin handoff",
            ),
        ] {
            let fixture = RunDagFixture::new(label, RunnerFixture::Executable);
            fixture.mutate_run_dag(from, to);
            let output =
                fixture.run_with_stdin(&["portable", "list"], &[], b"ambient stdin sentinel\n");
            assert_eq!(output.status.code(), Some(2));
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains(expected), "{stderr}");
            assert!(
                stderr.contains("remove it only after confirming no validation run is active"),
                "{stderr}"
            );
            fixture.assert_runner_never_consumed_stdin();
            let retained = fixture.assert_generated_dag_dir_retained();
            assert!(stderr.contains(&retained.display().to_string()), "{stderr}");
        }
    }

    #[test]
    fn run_dag_does_not_prefix_runner_json_or_dot_stdout_with_construction_output() {
        for (verb, expected_stdout) in [
            ("json", "{\"runner\":\"json\"}\n"),
            ("dot", "digraph dagrun {}\n"),
        ] {
            let fixture = RunDagFixture::new(verb, RunnerFixture::Executable);
            let output = fixture.run(
                &["portable", verb],
                &[("RUN_DAG_VALIDATE_STDOUT", "fake construction stdout")],
            );
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("fake construction stdout"), "{stderr}");
            let retained = fixture.assert_generated_dag_dir_retained();
            assert!(stderr.contains(&retained.display().to_string()), "{stderr}");
            fixture.assert_runner_read_valid_dag();
        }
    }

    #[test]
    fn real_run_dag_json_and_dot_stdout_are_machine_readable_from_byte_zero() {
        for verb in ["json", "dot"] {
            let output = Command::new(super::root().join("ci/run-dag.sh"))
                .args(["portable", verb])
                .current_dir(super::root())
                .output()
                .unwrap();
            assert!(output.status.success());
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("retained generated DAG directory:"),
                "{stderr}"
            );
            assert!(
                !stdout.contains("PLAN ONLY")
                    && !stdout.contains("retained generated DAG directory")
                    && !stdout.contains("complete constructed outer DAG"),
                "runner stdout must not contain construction diagnostics: {stdout}"
            );
            match verb {
                "json" => {
                    assert!(
                        output
                            .stdout
                            .first()
                            .is_some_and(|byte| *byte == b'{' || *byte == b'['),
                        "json stdout must start with JSON from byte zero: {stdout}"
                    );
                    let _: serde_json::Value = serde_json::from_slice(&output.stdout)
                        .unwrap_or_else(|error| {
                            panic!("json stdout must parse exactly: {error}\n{stdout}")
                        });
                }
                "dot" => {
                    assert!(
                        stdout.starts_with("digraph"),
                        "dot stdout must start with DOT from byte zero: {stdout}"
                    );
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn external_signals_reach_execed_runner_and_retain_generated_dag_dir() {
        for (name, signal, expected_code) in [
            ("hup", "HUP", 129),
            ("int", "INT", 130),
            ("term", "TERM", 143),
        ] {
            let fixture = RunDagFixture::new(name, RunnerFixture::Sleep);
            let mut child = fixture
                .command(&["portable", "list"], &[])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..1000 {
                if fixture.runner_pid_marker.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(
                fixture.runner_pid_marker.exists(),
                "fake dagrun did not reach the supervised sleep point"
            );
            let runner_pid = read_pid(&fixture.runner_pid_marker);
            assert_eq!(
                runner_pid,
                child.id(),
                "run-dag.sh must exec the runner instead of supervising it"
            );
            fixture.assert_runner_read_valid_dag();
            let live_dirs = fixture.generated_dag_dirs();
            assert_eq!(
                live_dirs.len(),
                1,
                "generated DAG should exist while dagrun runs: {live_dirs:?}"
            );
            let status = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(child.id().to_string())
                .status()
                .unwrap();
            assert!(status.success());
            let wait_started = std::time::Instant::now();
            let status = child.wait().unwrap();
            let elapsed = wait_started.elapsed();
            assert_eq!(status.code(), Some(expected_code));
            assert!(
                elapsed < Duration::from_secs(5),
                "trapped {signal} should not wait for the 30s sleeper; elapsed {elapsed:?}"
            );
            assert_eq!(fs::read_to_string(&fixture.signal_marker).unwrap(), signal);
            let retained = fixture.assert_generated_dag_dir_retained();
            assert!(retained.join("portable.json").exists());
        }
    }

    #[test]
    fn real_unboxed_dagrun_reports_raw_external_signals() {
        let runner = super::root().join("agent-utils/rs/bin/dagrun");
        for (name, signal, expected_signal) in
            [("hup", "HUP", 1), ("int", "INT", 2), ("term", "TERM", 15)]
        {
            let fixture = TestDirectory::new(&format!("real-unboxed-signal-{name}"));
            let marker = fixture.path().join("step.started");
            let shell_pid_path = fixture.path().join("step-shell.pid");
            let sleep_pid_path = fixture.path().join("step-sleep.pid");
            let dag = fixture.path().join("sleep.json");
            let command = format!(
                "printf started > {}; printf '%s\\n' $$ > {}; {SLEEP_EXECUTABLE} 30 & printf '%s\\n' $! > {}; wait",
                marker.display(),
                shell_pid_path.display(),
                sleep_pid_path.display()
            );
            let doc = serde_json::json!({
                "steps": [{
                    "group": "g",
                    "job": "sleep",
                    "cmd": command,
                    "timeout": 60,
                }]
            });
            fs::write(&dag, serde_json::to_vec(&doc).unwrap()).unwrap();
            let mut child = Command::new(&runner)
                .args([
                    "run",
                    "--dag",
                    dag.to_str().unwrap(),
                    "-q",
                    "--unsafe-no-cgroups",
                    "--no-profile",
                ])
                .current_dir(fixture.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..500 {
                if marker.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(marker.exists(), "real dagrun step did not start");
            assert!(
                wait_for_file(&shell_pid_path, Duration::from_secs(10)),
                "real dagrun step shell pid was not recorded"
            );
            assert!(
                wait_for_file(&sleep_pid_path, Duration::from_secs(10)),
                "real dagrun step sleep pid was not recorded"
            );
            let shell_pid = read_pid(&shell_pid_path);
            let sleep_pid = read_pid(&sleep_pid_path);
            let pgid = process_group(shell_pid)
                .unwrap_or_else(|| panic!("cannot read step shell pgid for pid {shell_pid}"));
            assert_eq!(
                process_group(sleep_pid),
                Some(pgid),
                "step sleeper must be in the recorded shell process group"
            );
            let sleep_process = RecordedProcess::capture(sleep_pid)
                .wait_for_cmdline(pgid, SLEEP_30_CMDLINE, Duration::from_secs(10))
                .unwrap_or_else(|error| panic!("{error}"));
            let mut cleanup = UnboxedProcessGroupCleanup::new(
                pgid,
                vec![RecordedProcess::capture(shell_pid), sleep_process],
            );
            let status = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(child.id().to_string())
                .status()
                .unwrap();
            assert!(status.success());
            let status = child.wait().unwrap();
            assert_eq!(status.signal(), Some(expected_signal));
            cleanup.cleanup();
        }
    }

    #[test]
    fn recorded_process_capture_after_exec_replaces_pre_exec_identity() {
        let fixture = TestDirectory::new("recorded-process-exec-identity");
        let ready = fixture.path().join("ready");
        let script = format!("printf started > \"$1\"; read -r _; exec {SLEEP_EXECUTABLE} 30");
        let child = Command::new("setsid")
            .args(["bash", "-c", &script, "bash"])
            .arg(&ready)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = SpawnedChildCleanup::new(child);
        let pid = child.child_mut().id();
        assert!(
            wait_for_file(&ready, Duration::from_secs(10)),
            "pre-exec fixture did not become ready"
        );
        let pgid = process_group(pid)
            .unwrap_or_else(|| panic!("cannot read pre-exec fixture pgid for pid {pid}"));
        let pre_exec = RecordedProcess::capture(pid);
        assert!(
            pre_exec.still_matches(),
            "pre-exec identity must match before the fixture is released"
        );
        let timeout_error = match pre_exec.clone().wait_for_cmdline(
            pgid,
            SLEEP_30_CMDLINE,
            Duration::from_millis(50),
        ) {
            Ok(_) => panic!("blocked pre-exec process must not match the expected sleeper"),
            Err(error) => error,
        };
        assert!(
            timeout_error.contains(&format!("pid {pid}"))
                && timeout_error.contains("recorded starttime")
                && timeout_error.contains("expected pgid")
                && timeout_error.contains("last cmdline observation"),
            "pre-exec timeout must report the recorded and observed identity: {timeout_error}"
        );

        child
            .child_mut()
            .stdin
            .as_mut()
            .expect("pre-exec fixture stdin must be piped")
            .write_all(b"continue\n")
            .unwrap();
        drop(child.child_mut().stdin.take());
        let post_exec = pre_exec
            .clone()
            .wait_for_cmdline(pgid, SLEEP_30_CMDLINE, Duration::from_secs(10))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            post_exec.start_time, pre_exec.start_time,
            "exec must preserve the recorded starttime"
        );
        assert_eq!(
            process_group(pid),
            Some(pgid),
            "exec must preserve the recorded process group"
        );
        assert!(
            !pre_exec.still_matches(),
            "pre-exec identity must stop matching after exec"
        );
        assert!(
            post_exec.still_matches(),
            "post-exec identity must match the running sleeper"
        );

        child.cleanup();
        assert!(
            !post_exec.still_matches(),
            "post-exec fixture must be gone after exact child cleanup"
        );
    }

    #[test]
    fn unboxed_cleanup_identity_selection_rejects_mismatched_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let pgid = process_group(pid).unwrap_or_else(|| panic!("cannot read pgid for pid {pid}"));
        let mut record = RecordedProcess::capture(pid);
        record.start_time.push_str("-not-this-process");
        let cleanup = UnboxedProcessGroupCleanup {
            pgid,
            processes: vec![record],
            armed: false,
            allow_group_signal: false,
        };
        assert!(
            cleanup.matching_processes().is_empty(),
            "mismatched identity must not be selected for cleanup"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "negative-control process should remain live until its own cleanup"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn unboxed_cleanup_refusal_does_not_signal_unrecorded_group_member() {
        let fixture = TestDirectory::new("unboxed-cleanup-unrecorded-member");
        let shell_pid_path = fixture.path().join("shell.pid");
        let recorded_sleep_pid_path = fixture.path().join("recorded-sleep.pid");
        let unrecorded_sleep_pid_path = fixture.path().join("unrecorded-sleep.pid");
        let script = format!(
            "printf '%s\\n' $$ > {}; sleep 30 & printf '%s\\n' $! > {}; sleep 30 & printf '%s\\n' $! > {}; wait",
            shell_pid_path.display(),
            recorded_sleep_pid_path.display(),
            unrecorded_sleep_pid_path.display()
        );
        let mut shell = Command::new("setsid")
            .args(["bash", "-c", &script])
            .spawn()
            .unwrap();
        assert!(
            wait_for_file(&shell_pid_path, Duration::from_secs(10)),
            "test shell pid was not recorded"
        );
        assert!(
            wait_for_file(&recorded_sleep_pid_path, Duration::from_secs(10)),
            "recorded sleep pid was not recorded"
        );
        assert!(
            wait_for_file(&unrecorded_sleep_pid_path, Duration::from_secs(10)),
            "unrecorded sleep pid was not recorded"
        );
        let shell_pid = read_pid(&shell_pid_path);
        let recorded_sleep_pid = read_pid(&recorded_sleep_pid_path);
        let unrecorded_sleep_pid = read_pid(&unrecorded_sleep_pid_path);
        let pgid = process_group(shell_pid)
            .unwrap_or_else(|| panic!("cannot read test shell pgid for pid {shell_pid}"));
        assert_eq!(process_group(recorded_sleep_pid), Some(pgid));
        assert_eq!(process_group(unrecorded_sleep_pid), Some(pgid));
        let shell_record = RecordedProcess::capture(shell_pid);
        let recorded_sleep = RecordedProcess::capture(recorded_sleep_pid);
        let unrecorded_sleep = RecordedProcess::capture(unrecorded_sleep_pid);
        let mut unrecorded_guard = RecordedPidCleanup::new(unrecorded_sleep.clone());

        let refusal = std::panic::catch_unwind(|| {
            let _cleanup = UnboxedProcessGroupCleanup::new(
                pgid,
                vec![shell_record.clone(), recorded_sleep.clone()],
            );
        });
        assert!(refusal.is_err(), "unrecorded process must refuse cleanup");
        for _ in 0..200 {
            if !shell_record.still_matches() && !recorded_sleep.still_matches() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !shell_record.still_matches(),
            "recorded shell should be cleaned individually after refusal"
        );
        assert!(
            !recorded_sleep.still_matches(),
            "recorded sleep should be cleaned individually after refusal"
        );
        assert!(
            unrecorded_guard.process.still_matches(),
            "unrecorded process must survive the refused whole-PGID cleanup"
        );
        unrecorded_guard.cleanup();
        assert!(
            !unrecorded_guard.process.still_matches(),
            "unrecorded process should be cleaned only by its separate guard"
        );
        let _ = shell.wait();
    }

    #[test]
    fn real_managed_dagrun_reports_signal_and_retains_generated_dag() {
        for (name, signal, expected_raw_signal, expected_message) in [
            ("hup", "HUP", Some(1), None),
            ("int", "INT", None, Some("signal 2")),
            ("term", "TERM", None, Some("signal 15")),
        ] {
            let fixture = TestDirectory::new(&format!("real-managed-signal-{name}"));
            fs::create_dir_all(fixture.path().join("ci")).unwrap();
            fs::create_dir_all(fixture.path().join("scripts")).unwrap();
            fs::create_dir_all(fixture.path().join("agent-utils/rs/bin")).unwrap();
            fs::create_dir_all(fixture.path().join("target/validation")).unwrap();
            fs::copy(
                super::root().join("ci/run-dag.sh"),
                fixture.path().join("ci/run-dag.sh"),
            )
            .unwrap();
            chmod(&fixture.path().join("ci/run-dag.sh"), 0o755);
            fs::write(
                fixture.path().join("ci/configure-build-jobs.sh"),
                "CARGO_BUILD_JOBS=16\nexport CARGO_BUILD_JOBS\n",
            )
            .unwrap();
            std::os::unix::fs::symlink(
                super::root().join("agent-utils/rs/bin/dagrun"),
                fixture.path().join("agent-utils/rs/bin/dagrun"),
            )
            .unwrap();

            let shell_pid = fixture.path().join("step-shell.pid");
            let sleep_pid = fixture.path().join("step-sleep.pid");
            let validate = fixture.path().join("scripts/validate.rs");
            fs::write(
                &validate,
                r#"#!/usr/bin/env bash
set -euo pipefail
output=
while (($#)); do
  if [[ $1 == --write-constructed-dag ]]; then
    shift
    output=$1
  fi
  shift || true
done
python3 - "$output" "$RUN_DAG_STEP_SHELL_PID" "$RUN_DAG_STEP_SLEEP_PID" <<'PY'
import json
import sys
output, shell_pid, sleep_pid = sys.argv[1:4]
cmd = (
    "printf '%s\n' $$ > " + json.dumps(shell_pid) +
    "; sleep 30 & printf '%s\n' $! > " + json.dumps(sleep_pid) +
    "; wait"
)
with open(output, "w", encoding="utf-8") as f:
    json.dump({"steps": [{"group": "g", "job": "sleep", "cmd": cmd, "timeout": 60}]}, f)
PY
"#,
            )
            .unwrap();
            chmod(&validate, 0o755);

            let child = Command::new("bash")
                .arg(fixture.path().join("ci/run-dag.sh"))
                .args(["portable", "-q", "--no-profile"])
                .current_dir(fixture.path())
                .env("RUN_DAG_STEP_SHELL_PID", &shell_pid)
                .env("RUN_DAG_STEP_SLEEP_PID", &sleep_pid)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            assert!(
                wait_for_file(&shell_pid, Duration::from_secs(10)),
                "managed dagrun did not reach the step shell"
            );
            assert!(
                wait_for_file(&sleep_pid, Duration::from_secs(10)),
                "managed dagrun did not start the sleeping descendant"
            );
            let shell_pid = read_pid(&shell_pid);
            let sleep_pid = read_pid(&sleep_pid);
            let (scope, cgroup_path) = managed_scope_for_pid(shell_pid);
            let mut cleanup =
                ManagedScopeCleanup::new(scope, cgroup_path, vec![shell_pid, sleep_pid]);
            let status = Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(child.id().to_string())
                .status()
                .unwrap();
            assert!(status.success());
            let output = child.wait_with_output().unwrap();
            assert!(
                !output.status.success(),
                "managed dagrun signal must not report success"
            );
            if let Some(expected_raw_signal) = expected_raw_signal {
                assert_eq!(output.status.signal(), Some(expected_raw_signal));
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if let Some(expected_message) = expected_message {
                assert!(
                    stderr.contains(expected_message),
                    "managed dagrun did not name its termination signal:\n{stderr}"
                );
            }
            let retained = fs::read_dir(fixture.path().join("target/validation"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("run-dag."))
                })
                .collect::<Vec<_>>();
            assert_eq!(
                retained.len(),
                1,
                "expected exactly one retained generated DAG directory, got {retained:?}"
            );
            let retained_path = retained[0].display().to_string();
            assert!(
                stderr.contains(&retained_path),
                "run-dag.sh did not print the exact retained path {retained_path}:\n{stderr}"
            );
            assert!(
                stderr.contains("Validation checkout lifecycle cleanup may remove it"),
                "run-dag.sh did not print the cleanup owner message:\n{stderr}"
            );
            assert!(retained[0].join("portable.json").exists());
            cleanup.cleanup();
        }
    }

    #[test]
    fn run_dag_retains_generated_dag_when_construction_fails() {
        let fixture = RunDagFixture::new("construction-fails", RunnerFixture::Executable);
        let output = fixture.run(&["portable", "list"], &[("RUN_DAG_VALIDATE_EXIT", "44")]);
        assert_eq!(output.status.code(), Some(44));
        assert_eq!(
            fs::read_to_string(&fixture.construct_marker).unwrap(),
            "constructed"
        );
        assert!(
            !fixture.runner_marker.exists(),
            "runner must not start after construction failure"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("DAG construction failed"), "{stderr}");
        assert!(
            stderr.contains("retained generated DAG directory:"),
            "{stderr}"
        );
        assert!(
            stderr.contains("remove it only after confirming no validation run is active"),
            "{stderr}"
        );
        let retained = fixture.assert_generated_dag_dir_retained();
        assert!(!retained.join("portable.json").exists());
    }

    #[test]
    fn run_dag_preserves_boxed_and_explicit_unboxed_policy_args() {
        let boxed = RunDagFixture::new("boxed-control", RunnerFixture::Executable);
        let output = boxed.run(&["privileged", "-j", "2", "-v"], &[]);
        assert!(output.status.success());
        let args = fs::read_to_string(&boxed.args_marker).unwrap();
        assert!(args.starts_with("run\n--dag\n-\n"), "{args}");
        assert!(!args.contains("--unsafe-no-cgroups"), "{args}");
        assert!(args.contains("\n-j\n2\n"), "{args}");
        boxed.assert_generated_dag_dir_retained();

        let unboxed = RunDagFixture::new("unboxed-control", RunnerFixture::Executable);
        let output = unboxed.run(
            &[
                "privileged",
                "-j",
                "2",
                "--unsafe-no-cgroups",
                "--perf-dir",
                "/tmp/perf",
                "-v",
            ],
            &[("GITHUB_ACTIONS", "true")],
        );
        assert!(output.status.success());
        let args = fs::read_to_string(&unboxed.args_marker).unwrap();
        assert!(args.contains("\n--unsafe-no-cgroups\n"), "{args}");
        assert!(args.contains("\n--perf-dir\n/tmp/perf\n"), "{args}");
        unboxed.assert_generated_dag_dir_retained();
    }

    #[test]
    fn constructed_ci_dags_have_explicit_structured_result_manifests() {
        for (label, level) in [
            ("portable", "portable-only"),
            ("privileged", "--privileged-only"),
        ] {
            let output_dir = TestDirectory::new(label);
            let dag = output_dir.path().join(format!("{label}.json"));
            let status = Command::new(super::root().join("scripts/validate.rs"))
                .arg(level)
                .arg("--write-constructed-dag")
                .arg(&dag)
                .status()
                .unwrap();
            assert!(status.success(), "validate construction failed for {label}");
            let graph: serde_json::Value =
                serde_json::from_slice(&fs::read(&dag).unwrap()).unwrap();
            let steps = graph["steps"]
                .as_array()
                .expect("DAG steps must be an array");
            assert!(!steps.is_empty(), "{label} DAG must contain steps");
            let mut structured_producers = 0;
            for step in steps {
                let manifests = step
                    .get("result_manifests")
                    .unwrap_or_else(|| panic!("{label} step lacks explicit result_manifests"));
                let manifests = manifests
                    .as_array()
                    .unwrap_or_else(|| panic!("{label} step result_manifests must be an array"));
                if manifests.iter().any(|manifest| {
                    manifest.get("kind").and_then(serde_json::Value::as_str)
                        == Some("structured-test-results")
                }) {
                    structured_producers += 1;
                }
            }
            assert!(
                structured_producers > 0,
                "{label} DAG must declare at least one structured-test-results producer"
            );
        }
    }

    #[test]
    fn privileged_launcher_timeout_does_not_depend_on_an_env_prefix() {
        for command in [
            "timeout --foreground --kill-after=10s 1560s ci/run-dag.sh privileged -v",
            "timeout --foreground --kill-after=10s 1560s env INPUT_MAX_MEM=32G ci/run-dag.sh privileged -v",
        ] {
            assert_eq!(command_timeout_seconds(command).unwrap(), Some(1560));
        }
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
    fn build_jobs_default_to_the_measured_useful_width_and_accept_an_override() {
        let default = build_worker_capacity(&parse(std::iter::empty()));
        assert_eq!(default.configured(), DEFAULT_BUILD_JOBS);

        let explicit =
            build_worker_capacity(&parse(["--jobs", "3"].into_iter().map(str::to_string)));
        assert_eq!(explicit.configured(), 3);
        assert_eq!(explicit.workers_for(2), 2);
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
