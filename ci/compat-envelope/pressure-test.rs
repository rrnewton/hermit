#!/usr/bin/env -S rust-script --force
//! Safely retry red compatibility cells and repeat one committed green cell.
//!
//! ```cargo
//! [dependencies]
//! csv = "1"
//! safe-ci-dag-runner = { path = "../../agent-utils/rs/safe-ci-dag-runner" }
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use safe_ci_dag_runner::io::dag_from_json;
use safe_ci_dag_runner::io::dag_to_json;
use safe_ci_dag_runner::model::DagConfig;
use safe_ci_dag_runner::model::ResourceHint;
use safe_ci_dag_runner::model::Step;
use safe_ci_dag_runner::model::StepClass;
use safe_ci_dag_runner::model::effective_cpu_count;
use safe_ci_dag_runner::model::effective_cpu_timeout;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use serde_json::json;

const TRACKED_CELLS: &str = "ci/compat-envelope/cells.json";
const PORTABLE_DAG: &str = "ci/dag/portable.json";
const TRACKED_CELLS_SCHEMA: u64 = 3;
const RUN_SCHEMA: u64 = 3;
const REQUIRED_BUILD_TAGS: [&str; 6] = [
    "setup.manifest_plan",
    "e2e.metadata",
    "build.workspace",
    "build.runtime_release",
    "build.e2e_artifact",
    "build.liteinst_runtime_release",
];
/// Written before a cell starts. If the cell's cgroup is killed before the
/// harness can report, this remains a conservative non-pass attempt marker.
const INCOMPLETE_ATTEMPT_STATUS: i32 = 125;
/// Written when a cell is not invoked because its serialized fixture
/// preparation did not complete successfully.
const PREPARATION_FAILED_STATUS: i32 = 126;
/// The shipped portable DAG gives a whole manifest bucket 600 seconds. A red
/// cell gets that complete existing allowance to itself; cells whose repeated
/// mode could theoretically consume longer remain red when this pressure
/// boundary cuts them. This bounds a known-bad cell without redefining green.
const PRESSURE_CELL_TIMEOUT_SECONDS: i64 = 600;
/// The prior 432-cell measurement completed in nine minutes on this host. Two
/// hours is an operational stop for the periodic experiment, not a pass
/// threshold: breach makes the run incomplete and publishes no promotion.
const PRESSURE_RUN_TIMEOUT_SECONDS: i64 = 2 * 60 * 60;

const USAGE: &str = r#"Hermit compatibility pressure test

Ordinary `validate` reruns the committed green compatibility cells and fails on
regressions. This tool probes cells that are currently red: either one exact
test/mode/backend cell during investigation, or a bounded batch during a
periodic search for newly working behavior. Every attempt runs under safe-ci
resource and time limits, retains its raw evidence under ignored/, and remains
red unless a later reviewed scorecard change deliberately promotes it.

Usage: ci/compat-envelope/pressure-test.rs COMMAND [OPTIONS]

Commands:
  run [--results DIR] [--mode MODE] [--sample COUNT] [--seed SEED]
      Run bounded probes for the selected red cells. An exact-cell run uses the
      current working tree for fast fix/test iteration; a dirty result is
      labelled exploratory and cannot promote the scorecard. Batch runs require
      a clean commit and use an isolated checkout. With no filters, this
      selects all red cells, but refuses a plan whose declared worst-case cell
      occupancy cannot fit the whole-run wall bound. Use --sample for a bounded
      random batch, --mode to narrow its population, or give --test, --mode,
      and --backend together for exactly one cell. The default result directory
      is ignored/compat-envelope/pressure-<SHA>-<time>. A red chaos cell whose
      manifest declares no seeds remains red but is unavailable: exact requests
      refuse it, while batches report and omit it rather than inventing a run.
      Add --repetitions N to an exact currently green cell to run N independent
      boxed checks against the same committed source. This reports flakiness;
      it never edits or demotes the scorecard.
  plan --results DIR [--mode MODE] [--sample COUNT] [--seed SEED]
      Generate the same safe-ci execution plan without running it. The default
      output is DIR/dag.json.
  summarize --results DIR
      Re-read a completed run, print its per-backend outcome table, and rewrite
      DIR/summary.json. This never edits or promotes the checked-in scorecard.
  self-test
      Test pressure-runner selection, timeout, execution-plan, and retained-
      evidence checks without running a guest.

Exact-cell options (run and plan):
  --test TEST-ID           Exact manifest test ID, such as
                           applications/example-timed-progress-bar
  --mode MODE              verify, replay, chaos, or naked
  --backend BACKEND        ptrace, dbt, kvm, sabre, liteinst, or native
  --cell-timeout SECONDS   Tighter cap for each selected cell; requires either
                           an exact cell or --sample
  --repetitions COUNT      Repeat one exact currently green cell concurrently.
                           COUNT must be at least 2. Requires a clean commit and
                           cannot be combined with --sample.

Bounded-batch options (run and plan):
  --sample COUNT           Seeded random sample of red cells. Without --mode,
                           samples verify, replay, and chaos; custom and naked
                           are omitted. Sampling draws only from cells whose
                           manifests provide executable commands.
  --seed SEED              Reproduce one sample. If omitted, a generated seed
                           and every selected identity are retained in run.json.
  --run-timeout SECONDS    Whole-run WALL-CLOCK bound (default 7200). This is
                           not a CPU budget and never weakens per-cell limits.

Examples:
  # Probe one currently red ptrace/verify cell with a 60-second boxed wall cap.
  ./ci/compat-envelope/pressure-test.rs run \
    --test applications/example-timed-progress-bar \
    --mode verify --backend ptrace --cell-timeout 60

  # Reproducibly sample ten red verify/replay/chaos cells, sixty seconds each.
  ./ci/compat-envelope/pressure-test.rs run \
    --sample 10 --seed 42 --cell-timeout 60

  # Check one committed green cell 100 times under the same boxed limits.
  ./ci/compat-envelope/pressure-test.rs run \
    --test backend-parity-c/fork-exec-pipeline \
    --mode verify --backend ptrace --repetitions 100 --cell-timeout 120

  # Inspect the bounded plan without executing it.
  ./ci/compat-envelope/pressure-test.rs plan \
    --results ignored/compat-envelope/pressure-review \
    --mode verify --sample 10 --seed 42 --cell-timeout 60

Other options:
  --results DIR            Retained ignored/ result directory
  --help                    Show this text

How it runs:
  The generated graph reuses the canonical Hermit/resource build commands from
  ci/dag/portable.json. Fixture preparation is serialized. Every selected red
  cell then runs in its own safe-ci cgroup. A failure, timeout, OOM, or missing
  result stays red but does not intentionally stop later selected cells.
  The combined crash/error bucket contains remaining nonzero harness exits,
  including signal-caused crashes when the shell reports a nonzero status; the
  pressure runner does not currently distinguish the originating signal.
"#;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CellId {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
}

#[derive(Debug, Deserialize)]
struct TrackedCells {
    schema: u64,
    cells: Vec<TrackedCell>,
}

#[derive(Clone, Debug, Deserialize)]
struct TrackedCell {
    #[serde(flatten)]
    id: CellId,
    enabled: bool,
    status: String,
}

struct PressureCells {
    selected: Vec<TrackedCell>,
    unavailable: Vec<TrackedCell>,
    preparation_by_test: BTreeMap<String, CellId>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct CellSelection {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    test: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    cell_timeout_seconds: Option<i64>,
    #[serde(default)]
    sample: Option<usize>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    run_timeout_seconds: Option<i64>,
    #[serde(default)]
    repetitions: Option<usize>,
}

impl CellSelection {
    fn is_exact(&self) -> bool {
        self.test.is_some() && self.mode.is_some() && self.backend.is_some()
    }

    fn repeats_green_cell(&self) -> bool {
        self.repetitions.is_some()
    }

    fn uses_shared_preparation(&self) -> bool {
        !self.is_exact() || self.repeats_green_cell()
    }

    fn run_count(&self) -> usize {
        self.repetitions.unwrap_or(1)
    }

    fn allows_dirty_source(&self) -> bool {
        self.is_exact() && !self.repeats_green_cell()
    }
}

struct FreshCheckout {
    source: PathBuf,
    path: PathBuf,
    sha: String,
}

struct SelfTestDirectory {
    path: PathBuf,
    armed: bool,
}

impl SelfTestDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn remove(mut self) -> Result<(), String> {
        fs::remove_dir_all(&self.path).map_err(|e| {
            format!(
                "cannot remove self-test directory {}: {e}",
                self.path.display()
            )
        })?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for SelfTestDirectory {
    fn drop(&mut self) {
        if self.armed
            && self.path.parent() == Some(env::temp_dir().as_path())
            && self
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("hermit-pressure-self-test-"))
        {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

impl FreshCheckout {
    fn prepare(source: &Path, sha: &str) -> Result<Self, String> {
        let parent = source
            .parent()
            .filter(|path| path.join("ci-hub").is_dir())
            .map(|path| path.join("ignored"))
            .unwrap_or_else(env::temp_dir);
        fs::create_dir_all(&parent).map_err(|e| {
            format!(
                "cannot create fresh-checkout parent {}: {e}",
                parent.display()
            )
        })?;
        let template = parent.join("pressure-fresh-XXXXXXXX");
        let output = Command::new("mktemp")
            .args(["-d", &template.to_string_lossy()])
            .output()
            .map_err(|e| format!("cannot create fresh checkout: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "mktemp refused fresh checkout creation: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        let checkout = Self {
            source: source.to_path_buf(),
            path,
            sha: sha.to_string(),
        };
        let initialize = (|| {
            command_ok(
                Command::new("git")
                    .args(["clone", "--local", "--no-checkout"])
                    .arg(source)
                    .arg(&checkout.path),
                "materialize fresh pressure-test checkout",
            )?;
            let marker = checkout.path.join(".pressure-test-generated-checkout");
            fs::write(
                &marker,
                format!("source={}\nsha={}\n", source.display(), sha),
            )
            .map_err(|e| format!("cannot write {}: {e}", marker.display()))?;
            command_ok(
                Command::new("git")
                    .args([
                        "-C",
                        &checkout.path.to_string_lossy(),
                        "checkout",
                        "--detach",
                    ])
                    .arg(sha),
                "check out exact pressure-test commit",
            )?;
            let observed = git_output(&checkout.path, &["rev-parse", "HEAD"])?;
            if observed != sha {
                return Err(format!(
                    "fresh pressure-test checkout resolved to {observed}, expected {sha}"
                ));
            }
            command_ok(
                Command::new("git").args([
                    "-C",
                    &checkout.path.to_string_lossy(),
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                ]),
                "initialize pressure-test submodules",
            )?;
            for required in [
                "ci/compat-envelope/pressure-test.rs",
                "agent-utils/rs/safe-ci-dag-runner/Cargo.toml",
            ] {
                if !checkout.path.join(required).is_file() {
                    return Err(format!(
                        "fresh pressure-test checkout is missing required file {required}"
                    ));
                }
            }
            Ok(())
        })();
        if let Err(error) = initialize {
            let cleanup = checkout.cleanup();
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; fresh-checkout cleanup also failed: {cleanup}"),
            });
        }
        Ok(checkout)
    }

    fn cleanup(&self) -> Result<(), String> {
        let expected_parent = self
            .source
            .parent()
            .filter(|path| path.join("ci-hub").is_dir())
            .map(|path| path.join("ignored"))
            .unwrap_or_else(env::temp_dir);
        let name_ok = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("pressure-fresh-"));
        if self.path.parent() != Some(expected_parent.as_path()) || !name_ok {
            return Err(format!(
                "refusing to remove generated checkout at unexpected path {}",
                self.path.display()
            ));
        }
        let marker = self.path.join(".pressure-test-generated-checkout");
        let expected_marker = format!("source={}\nsha={}\n", self.source.display(), self.sha);
        match fs::read_to_string(&marker) {
            Ok(observed_marker) if observed_marker == expected_marker => {}
            Ok(_) => {
                return Err(format!(
                    "refusing cleanup because {} does not match this run",
                    marker.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Initialization may fail before the marker is written. This
                // object still owns the freshly minted, parent/name-checked
                // directory, so refusing here would leak every failed clone.
            }
            Err(error) => {
                return Err(format!(
                    "refusing cleanup without readable {}: {error}",
                    marker.display()
                ));
            }
        }
        fs::remove_dir_all(&self.path)
            .map_err(|e| format!("cannot remove generated clone {}: {e}", self.path.display()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CellBudget {
    timeout_seconds: i64,
    attempts: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct ManifestBudgetRow {
    test: String,
    mode: String,
    timeout_seconds: i64,
    attempts: JsonValue,
}

#[derive(Debug, Deserialize)]
struct ResultRow {
    schema: u64,
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    test: String,
    category: String,
    lane: String,
    mode: String,
    backend: Option<String>,
    classification: String,
    outcome: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    error_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(transparent)]
struct RequiredNullableU64(Option<u64>);

#[derive(Debug, Deserialize)]
struct VerificationEvidence {
    verified: bool,
    bitwise_parity: bool,
    verdict: String,
    comparison: JsonValue,
    compared_log_messages: JsonValue,
    first_divergent_scheduler_turn: RequiredNullableU64,
    first_divergent_virtual_nanoseconds: RequiredNullableU64,
}

#[derive(Debug, Deserialize, Serialize)]
struct RunMetadata {
    schema: u64,
    hermit_sha: String,
    detcore_tree: String,
    source_tree_dirty: bool,
    run_timeout_seconds: i64,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    test: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    cell_timeout_seconds: Option<i64>,
    #[serde(default)]
    sample: Option<usize>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    unavailable_cells: usize,
    #[serde(default)]
    repetitions: Option<usize>,
    cells: Vec<CellId>,
}

#[derive(Clone, Copy, Debug, Default)]
struct RunnerEvidence {
    seen: bool,
    ok: bool,
    timed_out: bool,
    oom: bool,
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("compatibility pressure test: {message}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1).peekable();
    let Some(command) = args.next() else {
        return Err(format!("missing command\n\n{USAGE}"));
    };
    if matches!(command.as_str(), "-h" | "--help" | "help") {
        print!("{USAGE}");
        return Ok(());
    }
    if args
        .peek()
        .is_some_and(|argument| matches!(argument.as_str(), "-h" | "--help" | "help"))
    {
        args.next();
        if args.next().is_some() {
            return Err("help accepts no additional options".into());
        }
        print!("{USAGE}");
        return Ok(());
    }
    let root = repo_root()?;
    match command.as_str() {
        "plan" => {
            let (results, output, selection) = result_options(&root, &mut args, false, true)?;
            if output.is_some() {
                return Err(
                    "plan does not accept --output; its retained and executed plan is always RESULTS/dag.json"
                        .into(),
                );
            }
            require_empty_result_dir(&results)?;
            let output = results.join("dag.json");
            let metadata = write_plan(&root, &results, &output, &selection)?;
            println!("DAG: {}", output.display());
            println!("Results: {}", results.display());
            println!(
                "Cell runs: {}",
                metadata
                    .cells
                    .len()
                    .saturating_mul(metadata.repetitions.unwrap_or(1))
            );
            print_unavailable(&metadata);
            println!("Whole-run bound: {}s", metadata.run_timeout_seconds);
            print_sample(&metadata);
            if selection.is_exact() {
                print_exact_manifest_command(&root, &metadata.cells[0], &selection)?;
            }
            println!(
                "Run: RUN_DAG_FILE_OVERRIDE={} ./ci/run-dag.sh portable -k --perf-dir {} --profile --run-timeout {}",
                shell_quote(&output.to_string_lossy()),
                shell_quote(&results.join("runner-profile").to_string_lossy()),
                metadata.run_timeout_seconds
            );
        }
        "run" => {
            let (results, output, selection) = result_options(&root, &mut args, true, true)?;
            if output.is_some() {
                return Err(
                    "run does not accept --output; its plan is always RESULTS/dag.json".into(),
                );
            }
            let exact_cell = selection.is_exact();
            if !selection.allows_dirty_source() && worktree_dirty(&root)? {
                return Err("run refuses a dirty checkout; commit first so every row binds to reproducible source".into());
            }
            require_empty_result_dir(&results)?;
            let started = Instant::now();
            let sha = git_output(&root, &["rev-parse", "HEAD"])?;
            let fresh = if selection.allows_dirty_source() {
                eprintln!(
                    "compatibility pressure test: exact-cell iteration uses the current working tree; dirty results are exploratory and cannot promote the scorecard"
                );
                None
            } else {
                let fresh = FreshCheckout::prepare(&root, &sha)?;
                println!("Fresh checkout: {}", fresh.path.display());
                Some(fresh)
            };
            let execution_root = fresh
                .as_ref()
                .map(|checkout| checkout.path.as_path())
                .unwrap_or(root.as_path());
            let output = results.join("dag.json");
            let run_result = (|| {
                let metadata = write_plan(execution_root, &results, &output, &selection)?;
                print_unavailable(&metadata);
                print_sample(&metadata);
                if exact_cell {
                    print_exact_manifest_command(execution_root, &metadata.cells[0], &selection)?;
                }
                let mut prior_progress = progress_marker_count(&results, &metadata);
                loop {
                    let remaining = metadata
                        .run_timeout_seconds
                        .saturating_sub(started.elapsed().as_secs() as i64);
                    if remaining <= 0 {
                        return Err(format!(
                            "pressure run reached its {}s whole-run bound; retained artifacts are in {}",
                            metadata.run_timeout_seconds,
                            results.display()
                        ));
                    }
                    let status = Command::new(execution_root.join("ci/run-dag.sh"))
                        .args([
                            "portable",
                            "-k",
                            "--perf-dir",
                            &results.join("runner-profile").to_string_lossy(),
                            "--profile",
                            "--run-timeout",
                            &remaining.to_string(),
                        ])
                        .env("RUN_DAG_FILE_OVERRIDE", &output)
                        .current_dir(execution_root)
                        .status()
                        .map_err(|e| format!("cannot start safe-ci-dag-runner: {e}"))?;
                    let progress = progress_marker_count(&results, &metadata);
                    if status.success() || all_cells_attempted(&results, &metadata) {
                        break;
                    }
                    if !required_builds_complete(&results, &metadata) {
                        return Err(format!(
                            "canonical pressure-test setup failed with {}; retained artifacts are in {}",
                            status,
                            results.display()
                        ));
                    }
                    if progress <= prior_progress {
                        return Err(format!(
                            "safe-ci-dag-runner failed with {} without recording a new bounded attempt; retained artifacts are in {}",
                            status,
                            results.display()
                        ));
                    }
                    eprintln!(
                        "compatibility pressure test: a bounded red-cell attempt stopped this DAG pass; resuming the remaining cells ({progress} progress markers)"
                    );
                    prior_progress = progress;
                }
                summarize(execution_root, &results, exact_cell)
            })();
            let cleanup_result = match fresh {
                Some(fresh) => fresh.cleanup(),
                None => Ok(()),
            };
            match (run_result, cleanup_result) {
                (Ok(()), Ok(())) => {}
                (Err(run), Ok(())) => return Err(run),
                (Ok(()), Err(cleanup)) => return Err(cleanup),
                (Err(run), Err(cleanup)) => {
                    return Err(format!(
                        "{run}; fresh-checkout cleanup also failed: {cleanup}"
                    ));
                }
            }
        }
        "summarize" => {
            let (results, output, _) = result_options(&root, &mut args, false, false)?;
            if output.is_some() {
                return Err("summarize does not accept --output".into());
            }
            // Dirty retained results are admissible only when their own
            // metadata proves they came from one exact cell; summarize()
            // enforces that boundary before reading any evidence.
            summarize(&root, &results, true)?;
        }
        "self-test" => {
            if args.next().is_some() {
                return Err("self-test accepts no options".into());
            }
            self_test(&root)?;
        }
        _ => return Err(format!("unknown command `{command}`\n\n{USAGE}")),
    }
    Ok(())
}

fn print_sample(metadata: &RunMetadata) {
    let Some(count) = metadata.sample else {
        return;
    };
    println!(
        "Sample: {count} cell(s), seed {}",
        metadata.seed.unwrap_or(0)
    );
    for cell in &metadata.cells {
        println!("  {}", display_id(cell));
    }
}

fn print_unavailable(metadata: &RunMetadata) {
    if metadata.unavailable_cells > 0 {
        println!(
            "Unavailable red chaos cells omitted: {} (their manifests declare no seeds, so no guest command exists)",
            metadata.unavailable_cells
        );
    }
}

fn result_options(
    root: &Path,
    args: &mut impl Iterator<Item = String>,
    default_results: bool,
    allow_selection: bool,
) -> Result<(PathBuf, Option<PathBuf>, CellSelection), String> {
    let mut results = None;
    let mut output = None;
    let mut selection = CellSelection::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--results" => {
                results = Some(PathBuf::from(
                    args.next().ok_or("--results requires a directory")?,
                ));
            }
            "--output" => {
                output = Some(PathBuf::from(
                    args.next().ok_or("--output requires a file")?,
                ));
            }
            "--mode" if allow_selection => {
                let value = args.next().ok_or("--mode requires a value")?;
                if !matches!(value.as_str(), "verify" | "replay" | "chaos" | "naked") {
                    return Err(format!(
                        "unknown mode `{value}`; expected verify, replay, chaos, or naked"
                    ));
                }
                if selection.mode.replace(value).is_some() {
                    return Err("--mode may be specified only once".into());
                }
            }
            "--test" if allow_selection => {
                let value = args.next().ok_or("--test requires a manifest test ID")?;
                if value.is_empty() {
                    return Err("--test requires a nonempty manifest test ID".into());
                }
                if selection.test.replace(value).is_some() {
                    return Err("--test may be specified only once".into());
                }
            }
            "--backend" if allow_selection => {
                let value = args.next().ok_or("--backend requires a backend")?;
                if !matches!(
                    value.as_str(),
                    "ptrace" | "dbt" | "kvm" | "sabre" | "liteinst" | "native"
                ) {
                    return Err(format!(
                        "unknown backend `{value}`; expected ptrace, dbt, kvm, sabre, liteinst, or native"
                    ));
                }
                if selection.backend.replace(value).is_some() {
                    return Err("--backend may be specified only once".into());
                }
            }
            "--cell-timeout" if allow_selection => {
                let raw = args.next().ok_or("--cell-timeout requires seconds")?;
                let value = raw.parse::<i64>().map_err(|_| {
                    format!("invalid --cell-timeout `{raw}`; expected positive seconds")
                })?;
                if value <= 0 {
                    return Err("--cell-timeout must be positive".into());
                }
                if selection.cell_timeout_seconds.replace(value).is_some() {
                    return Err("--cell-timeout may be specified only once".into());
                }
            }
            "--sample" if allow_selection => {
                let raw = args.next().ok_or("--sample requires a count")?;
                let value = raw.parse::<usize>().map_err(|_| {
                    format!("invalid --sample `{raw}`; expected a positive integer")
                })?;
                if value == 0 {
                    return Err("--sample must be positive".into());
                }
                if selection.sample.replace(value).is_some() {
                    return Err("--sample may be specified only once".into());
                }
            }
            "--seed" if allow_selection => {
                let raw = args.next().ok_or("--seed requires an unsigned integer")?;
                let value = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --seed `{raw}`; expected an unsigned integer"))?;
                if selection.seed.replace(value).is_some() {
                    return Err("--seed may be specified only once".into());
                }
            }
            "--run-timeout" if allow_selection => {
                let raw = args.next().ok_or("--run-timeout requires seconds")?;
                let value = raw.parse::<i64>().map_err(|_| {
                    format!("invalid --run-timeout `{raw}`; expected positive seconds")
                })?;
                if value <= 0 {
                    return Err("--run-timeout must be positive".into());
                }
                if selection.run_timeout_seconds.replace(value).is_some() {
                    return Err("--run-timeout may be specified only once".into());
                }
            }
            "--repetitions" if allow_selection => {
                let raw = args.next().ok_or("--repetitions requires a count")?;
                let value = raw.parse::<usize>().map_err(|_| {
                    format!("invalid --repetitions `{raw}`; expected an integer of at least 2")
                })?;
                if value < 2 {
                    return Err("--repetitions must be at least 2".into());
                }
                if selection.repetitions.replace(value).is_some() {
                    return Err("--repetitions may be specified only once".into());
                }
            }
            _ => return Err(format!("unknown option `{arg}`\n\n{USAGE}")),
        }
    }
    let exact_fields = [
        selection.test.is_some(),
        selection.mode.is_some() && (selection.test.is_some() || selection.backend.is_some()),
        selection.backend.is_some(),
    ];
    if exact_fields.iter().any(|present| *present) && !exact_fields.iter().all(|present| *present) {
        return Err(
            "an exact-cell selection requires --test, --mode, and --backend together".into(),
        );
    }
    if selection.cell_timeout_seconds.is_some()
        && !(selection.is_exact() || selection.sample.is_some())
    {
        return Err("--cell-timeout requires an exact cell or --sample".into());
    }
    if selection.sample.is_some() && selection.is_exact() {
        return Err(
            "--sample and an exact --test/--mode/--backend cell are mutually exclusive".into(),
        );
    }
    if selection.repetitions.is_some() && !selection.is_exact() {
        return Err("--repetitions requires an exact --test/--mode/--backend cell".into());
    }
    if selection.repetitions.is_some() && selection.sample.is_some() {
        return Err("--repetitions and --sample are mutually exclusive".into());
    }
    if selection.seed.is_some() && selection.sample.is_none() {
        return Err("--seed requires --sample".into());
    }
    if selection.sample.is_some() && selection.seed.is_none() {
        selection.seed = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
                .as_nanos() as u64,
        );
    }
    let results = match (results, default_results) {
        (Some(path), _) => absolute_from(root, path),
        (None, true) => default_result_root(root)?,
        (None, false) => return Err("command requires --results DIR".into()),
    };
    let output = output.map(|path| absolute_from(root, path));
    Ok((results, output, selection))
}

fn absolute_from(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn print_exact_manifest_command(
    root: &Path,
    cell: &CellId,
    selection: &CellSelection,
) -> Result<(), String> {
    println!("Cell: {}/{}/{}", cell.test, cell.mode, cell.backend);
    let budgets = load_budgets(root)?;
    let budget = budgets
        .get(&(cell.test.clone(), cell.mode.clone()))
        .ok_or_else(|| format!("no manifest budget for {}/{}", cell.test, cell.mode))?;
    println!(
        "Boxed cell wall cap: {}s (the manifest's per-attempt timeout remains nested and cannot extend this cap)",
        pressure_timeout(budget, selection.cell_timeout_seconds)?
    );
    println!("Manifest command inside that boxed cell:");
    let output = Command::new(root.join("tests/manifest-cli.rs"))
        .args([
            "get",
            &cell.test,
            "--mode",
            &cell.mode,
            "--backend",
            &cell.backend,
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot ask manifest-cli for the exact cell command: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "manifest-cli could not render the exact cell command: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    print!("{}", String::from_utf8_lossy(&output.stdout));
    Ok(())
}

fn require_empty_result_dir(results: &Path) -> Result<(), String> {
    if !results.exists() {
        return Ok(());
    }
    if !results.is_dir() {
        return Err(format!(
            "pressure result path is not a directory: {}",
            results.display()
        ));
    }
    let mut entries =
        fs::read_dir(results).map_err(|e| format!("cannot inspect {}: {e}", results.display()))?;
    if entries.next().is_some() {
        return Err(format!(
            "run refuses nonempty result directory {}; choose a fresh directory so old rows cannot satisfy this run",
            results.display()
        ));
    }
    Ok(())
}

fn default_result_root(root: &Path) -> Result<PathBuf, String> {
    let sha = git_output(root, &["rev-parse", "--short=12", "HEAD"])?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
        .as_secs();
    Ok(root
        .join("ignored/compat-envelope")
        .join(format!("pressure-{sha}-{now}")))
}

fn repo_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| format!("cannot run git rev-parse: {e}"))?;
    if !output.status.success() {
        return Err("not inside a Git checkout".into());
    }
    let root = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    if !root.join(TRACKED_CELLS).is_file() {
        return Err(format!("{} is not the Hermit checkout", root.display()));
    }
    Ok(root)
}

fn worktree_dirty(root: &Path) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot inspect worktree: {e}"))?;
    if !output.status.success() {
        return Err("git status failed".into());
    }
    Ok(!output.stdout.is_empty())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run git {}: {e}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command_ok(command: &mut Command, purpose: &str) -> Result<(), String> {
    let status = command
        .status()
        .map_err(|e| format!("cannot {purpose}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cannot {purpose}: {status}"))
    }
}

fn check_scorecard(root: &Path) -> Result<(), String> {
    let status = Command::new(root.join("ci/compat-envelope/scorecard.rs"))
        .arg("check")
        .current_dir(root)
        .status()
        .map_err(|e| format!("cannot run scorecard check: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("tracked scorecard is stale; update it before generating a pressure run".into())
    }
}

fn pressure_cells(root: &Path, selection: &CellSelection) -> Result<PressureCells, String> {
    let budgets = load_budgets(root)?;
    let path = root.join(TRACKED_CELLS);
    let text =
        fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let tracked: TrackedCells = serde_json::from_str(&text)
        .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    if tracked.schema != TRACKED_CELLS_SCHEMA {
        return Err(format!(
            "unsupported tracked cell schema {}",
            tracked.schema
        ));
    }
    let mut seen = BTreeSet::new();
    let mut selected_cells = Vec::new();
    let mut unavailable = Vec::new();
    let mut enabled_by_test = BTreeMap::new();
    for cell in tracked.cells {
        if !seen.insert(cell.id.clone()) {
            return Err("tracked cells contain a duplicate identity".into());
        }
        if cell.enabled {
            enabled_by_test
                .entry(cell.id.test.clone())
                .or_insert_with(|| cell.id.clone());
        }
        let selected = selection
            .mode
            .as_deref()
            .map_or(true, |value| cell.id.mode == value)
            && selection
                .test
                .as_deref()
                .map_or(true, |value| cell.id.test == value)
            && selection
                .backend
                .as_deref()
                .map_or(true, |value| cell.id.backend == value)
            && !(selection.sample.is_some()
                && selection.mode.is_none()
                && !matches!(cell.id.mode.as_str(), "verify" | "replay" | "chaos"));
        match cell.status.as_str() {
            "red" if selected && !selection.repeats_green_cell() => {
                let budget = budgets
                    .get(&(cell.id.test.clone(), cell.id.mode.clone()))
                    .ok_or_else(|| {
                        format!(
                            "no manifest execution budget for {}/{}",
                            cell.id.test, cell.id.mode
                        )
                    })?;
                if budget.attempts.is_some() {
                    selected_cells.push(cell);
                } else if selection.is_exact() {
                    return Err(format!(
                        "{}/{}/{} is red but unavailable: its manifest declares no chaos seeds, so there is no guest command to run",
                        cell.id.test, cell.id.mode, cell.id.backend
                    ));
                } else {
                    unavailable.push(cell);
                }
            }
            "red" => {}
            "green" if selected && selection.repeats_green_cell() && cell.enabled => {
                let budget = budgets
                    .get(&(cell.id.test.clone(), cell.id.mode.clone()))
                    .ok_or_else(|| {
                        format!(
                            "no manifest execution budget for {}/{}",
                            cell.id.test, cell.id.mode
                        )
                    })?;
                if budget.attempts.is_none() {
                    return Err(format!(
                        "{}/{}/{} is green but its manifest has no executable attempt recipe",
                        cell.id.test, cell.id.mode, cell.id.backend
                    ));
                }
                selected_cells.push(cell);
            }
            "green" => {}
            other => return Err(format!("unknown cell status `{other}`")),
        }
    }
    selected_cells.sort_by(|left, right| left.id.cmp(&right.id));
    if selected_cells.is_empty() {
        if !unavailable.is_empty() {
            return Err(format!(
                "the selected red population has no executable commands; {} chaos cell(s) are unavailable because their manifests declare no seeds",
                unavailable.len()
            ));
        }
        return Err(
            if let (Some(test), Some(mode), Some(backend)) = (
                selection.test.as_deref(),
                selection.mode.as_deref(),
                selection.backend.as_deref(),
            ) {
                if selection.repeats_green_cell() {
                    format!(
                        "{test}/{mode}/{backend} is not an enabled green tracked cell; use the scorecard or manifest CLI to inspect it"
                    )
                } else {
                    format!(
                        "{test}/{mode}/{backend} is not a currently red tracked cell; use the scorecard or manifest CLI to inspect it"
                    )
                }
            } else if let Some(mode) = selection.mode.as_deref() {
                format!("tracked scorecard has no red cells for mode `{mode}`")
            } else {
                "tracked scorecard has no red cells".into()
            },
        );
    }
    if let Some(count) = selection.sample {
        if count > selected_cells.len() {
            return Err(format!(
                "--sample {count} exceeds the {} red cells with executable commands in the selected population; {} selected red chaos cell(s) are unavailable because their manifests declare no seeds",
                selected_cells.len(),
                unavailable.len()
            ));
        }
        let seed = selection.seed.expect("sample always has a seed");
        selected_cells.sort_by(|left, right| {
            sample_score(&left.id, seed)
                .cmp(&sample_score(&right.id, seed))
                .then_with(|| left.id.cmp(&right.id))
        });
        selected_cells.truncate(count);
        selected_cells.sort_by(|left, right| left.id.cmp(&right.id));
    }
    let mut preparation_by_test = BTreeMap::new();
    for cell in &selected_cells {
        let prepared_with = enabled_by_test.get(&cell.id.test).ok_or_else(|| {
            format!(
                "{} has no manifest-enabled mode available to build its fixture",
                cell.id.test
            )
        })?;
        preparation_by_test
            .entry(cell.id.test.clone())
            .or_insert_with(|| prepared_with.clone());
    }
    Ok(PressureCells {
        selected: selected_cells,
        unavailable,
        preparation_by_test,
    })
}

/// Stable seeded ordering for a retained random sample. The selected identities
/// are also written to run.json, so replay does not depend on this arithmetic
/// remaining unchanged across future tool versions.
fn sample_score(cell: &CellId, seed: u64) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64 ^ seed;
    for byte in display_id(cell).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    // SplitMix64 finalizer: deterministic, inexpensive, and sufficiently
    // well-distributed for choosing a diagnostic sample without a new runtime
    // dependency.
    let mut value = hash.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

fn load_budgets(root: &Path) -> Result<BTreeMap<(String, String), CellBudget>, String> {
    let output = Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "-p",
            "hermit-manifest-plan",
            "--",
            "--format",
            "matrix-json",
        ])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cannot run hermit-manifest-plan: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "hermit-manifest-plan failed while loading execution budgets:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    decode_budgets(&output.stdout)
}

fn decode_budgets(matrix_json: &[u8]) -> Result<BTreeMap<(String, String), CellBudget>, String> {
    let rows: Vec<ManifestBudgetRow> = serde_json::from_slice(matrix_json)
        .map_err(|e| format!("manifest-plan emitted invalid matrix JSON: {e}"))?;
    if rows.is_empty() {
        return Err("manifest-plan emitted an empty matrix".into());
    }
    let mut out: BTreeMap<(String, String), CellBudget> = BTreeMap::new();
    for row in rows {
        if !(1..=1800).contains(&row.timeout_seconds) {
            return Err(format!(
                "manifest-plan emitted timeout {} outside 1..=1800 for {}/{}",
                row.timeout_seconds, row.test, row.mode
            ));
        }
        let attempts = if row.attempts.is_null() {
            None
        } else {
            Some(row.attempts.as_i64().ok_or_else(|| {
                format!(
                    "manifest-plan emitted a non-integer attempt count for {}/{}",
                    row.test, row.mode
                )
            })?)
        };
        if attempts.is_none() && row.mode != "chaos" {
            return Err(format!(
                "manifest-plan emitted no attempt count for non-chaos mode {}/{}",
                row.test, row.mode
            ));
        }
        if attempts.is_some_and(|attempts| attempts <= 0) {
            return Err(format!(
                "manifest-plan emitted a nonpositive attempt count for {}/{}",
                row.test, row.mode
            ));
        }
        let key = (row.test, row.mode);
        let budget = CellBudget {
            timeout_seconds: row.timeout_seconds,
            attempts,
        };
        if let Some(existing) = out.get(&key) {
            if existing != &budget {
                return Err(format!(
                    "manifest-plan emitted conflicting execution budgets for {}/{}",
                    key.0, key.1
                ));
            }
        } else {
            out.insert(key, budget);
        }
    }
    Ok(out)
}

/// The harness may spend one manifest timeout preparing the fixture, then one
/// timeout per attempt. Every timeout has a documented 10-second TERM/KILL
/// grace. The final 30 seconds is the existing nextest/reporting grace used by
/// this repository, not a backend multiplier or a guessed speed ratio.
fn outer_timeout(budget: &CellBudget) -> Result<i64, String> {
    let attempts = budget.attempts.ok_or(
        "cannot derive a wall cap for a cell whose manifest has no executable attempt recipe",
    )?;
    let phases = attempts + 1;
    Ok(phases * (budget.timeout_seconds + 10) + 30)
}

fn pressure_timeout(budget: &CellBudget, selected_cap: Option<i64>) -> Result<i64, String> {
    Ok(outer_timeout(budget)?.min(
        selected_cap
            .unwrap_or(PRESSURE_CELL_TIMEOUT_SECONDS)
            .min(PRESSURE_CELL_TIMEOUT_SECONDS),
    ))
}

fn preparation_node_timeout(budget: &CellBudget, selected_cap: Option<i64>) -> Result<i64, String> {
    Ok(pressure_timeout(budget, selected_cap)? + 20)
}

fn require_cell_occupancy_fits(
    cells: &[TrackedCell],
    budgets: &BTreeMap<(String, String), CellBudget>,
    selected_cap: Option<i64>,
    run_timeout_seconds: i64,
    repetitions: usize,
) -> Result<(), String> {
    let mut all_seconds = 0_i64;
    let mut kvm_seconds = 0_i64;
    for tracked in cells {
        let budget = budgets
            .get(&(tracked.id.test.clone(), tracked.id.mode.clone()))
            .ok_or_else(|| {
                format!(
                    "no manifest budget for {}/{}",
                    tracked.id.test, tracked.id.mode
                )
            })?;
        let seconds = pressure_timeout(budget, selected_cap)?;
        let seconds = seconds.saturating_mul(i64::try_from(repetitions).unwrap_or(i64::MAX));
        all_seconds = all_seconds.saturating_add(seconds);
        if tracked.id.backend == "kvm" {
            kvm_seconds = kvm_seconds.saturating_add(seconds);
        }
    }
    // The generated graph permits at most four manifest guests, and at most one
    // KVM guest, at a time. If every selected cell consumes its declared cap,
    // these resource limits impose this minimum wall time even before build and
    // preparation work. Refuse an impossible public bound instead of printing a
    // command which cannot satisfy its own contract.
    let guest_floor = (all_seconds + 3) / 4;
    let occupancy_floor = guest_floor.max(kvm_seconds);
    if occupancy_floor >= run_timeout_seconds {
        return Err(format!(
            "selected {} cell run(s) have at least {occupancy_floor}s of declared worst-case cell occupancy at manifest_guest=4/kvm=1, which cannot fit the {run_timeout_seconds}s whole-run WALL bound; use --sample (and optionally --cell-timeout), reduce --repetitions, or deliberately raise --run-timeout",
            cells.len().saturating_mul(repetitions)
        ));
    }
    Ok(())
}

fn build_marker(results: &Path, tag: &str) -> PathBuf {
    results.join("state").join(format!("{}.ok", sanitize(tag)))
}

fn required_build_tags(
    exact_cell: Option<(&str, &str)>,
    includes_liteinst: bool,
) -> BTreeSet<&'static str> {
    // Batch cells consume the canonical prebuilt artifact, so retain its full
    // dependency closure. LiteInst's separate runtime build is part of that
    // closure only when at least one selected cell uses LiteInst. Exact
    // ptrace/KVM cells use a lean Hermit build, DBT/SaBRe retain the canonical
    // third-party runtime build, LiteInst retains its full chain, and a naked
    // native command needs no Hermit build.
    if let Some((mode, backend)) = exact_cell {
        if mode == "naked" && backend == "native" {
            return BTreeSet::new();
        }
        if backend != "liteinst" {
            return BTreeSet::from(["build.runtime_release"]);
        }
    }
    REQUIRED_BUILD_TAGS
        .into_iter()
        .filter(|tag| includes_liteinst || *tag != "build.liteinst_runtime_release")
        .collect()
}

fn required_builds_complete(results: &Path, metadata: &RunMetadata) -> bool {
    let exact_cell = (metadata.test.is_some()
        && metadata.mode.is_some()
        && metadata.backend.is_some()
        && metadata.cells.len() == 1)
        .then(|| {
            (
                metadata.mode.as_deref().expect("checked exact mode"),
                metadata.backend.as_deref().expect("checked exact backend"),
            )
        });
    let includes_liteinst = metadata.cells.iter().any(|cell| cell.backend == "liteinst");
    required_build_tags(exact_cell, includes_liteinst)
        .iter()
        .all(|tag| build_marker(results, tag).is_file())
}

fn selected_cell_dependencies(
    exact_cell: bool,
    mode: &str,
    backend: &str,
    preparation_tag: Option<&str>,
) -> Vec<String> {
    if exact_cell {
        if mode == "naked" && backend == "native" {
            return Vec::new();
        }
        return vec![if backend == "liteinst" {
            "build.liteinst_runtime_release".into()
        } else {
            "build.runtime_release".into()
        }];
    }
    let mut deps = vec![
        preparation_tag
            .expect("batch cell has a preparation tag")
            .into(),
        "build.e2e_artifact".into(),
    ];
    if backend == "liteinst" {
        deps.push("build.liteinst_runtime_release".into());
    }
    deps
}

fn base_cell_slug(cell: &CellId) -> String {
    sanitize(&format!(
        "{}-{}-{}-{}-{}",
        cell.lane, cell.category, cell.test, cell.mode, cell.backend
    ))
}

fn repetition_numbers(repetitions: Option<usize>) -> Vec<Option<usize>> {
    repetitions
        .map(|count| (1..=count).map(Some).collect())
        .unwrap_or_else(|| vec![None])
}

fn cell_run_slug(cell: &CellId, repetition: Option<usize>) -> String {
    let base = base_cell_slug(cell);
    repetition.map_or(base.clone(), |number| {
        format!("{base}-repetition-{number:04}")
    })
}

fn cell_status_path(results: &Path, cell: &CellId, repetition: Option<usize>) -> PathBuf {
    results
        .join("cells")
        .join(cell_run_slug(cell, repetition))
        .join("harness-status")
}

fn all_cells_attempted(results: &Path, metadata: &RunMetadata) -> bool {
    metadata.cells.iter().all(|cell| {
        repetition_numbers(metadata.repetitions)
            .into_iter()
            .all(|repetition| cell_status_path(results, cell, repetition).is_file())
    })
}

fn progress_marker_count(results: &Path, metadata: &RunMetadata) -> usize {
    let cell_markers = metadata
        .cells
        .iter()
        .flat_map(|cell| {
            repetition_numbers(metadata.repetitions)
                .into_iter()
                .map(move |repetition| (cell, repetition))
        })
        .filter(|(cell, repetition)| cell_status_path(results, cell, *repetition).is_file())
        .count();
    let preparation_markers = fs::read_dir(results.join("prepare"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("status").is_file())
        .count();
    cell_markers + preparation_markers
}

fn write_plan(
    root: &Path,
    results: &Path,
    output: &Path,
    selection: &CellSelection,
) -> Result<RunMetadata, String> {
    check_scorecard(root)?;
    let PressureCells {
        selected: cells,
        unavailable,
        preparation_by_test: all_preparations,
    } = pressure_cells(root, selection)?;
    let preparation_by_test = if selection.uses_shared_preparation() {
        all_preparations
    } else {
        BTreeMap::new()
    };
    let budgets = load_budgets(root)?;
    let run_timeout_seconds = selection
        .run_timeout_seconds
        .unwrap_or(PRESSURE_RUN_TIMEOUT_SECONDS);
    require_cell_occupancy_fits(
        &cells,
        &budgets,
        selection.cell_timeout_seconds,
        run_timeout_seconds,
        selection.run_count(),
    )?;
    fs::create_dir_all(results).map_err(|e| format!("cannot create {}: {e}", results.display()))?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    let canonical_text = fs::read_to_string(root.join(PORTABLE_DAG))
        .map_err(|e| format!("cannot read {PORTABLE_DAG}: {e}"))?;
    let canonical =
        dag_from_json(&canonical_text).map_err(|e| format!("invalid {PORTABLE_DAG}: {e}"))?;
    let includes_liteinst = cells.iter().any(|tracked| tracked.id.backend == "liteinst");
    let lean_exact_cell = (selection.is_exact() && !selection.repeats_green_cell()).then(|| {
        (
            selection.mode.as_deref().expect("exact selection has mode"),
            selection
                .backend
                .as_deref()
                .expect("exact selection has backend"),
        )
    });
    let required_builds = required_build_tags(lean_exact_cell, includes_liteinst);
    let mut steps = Vec::new();
    for mut step in canonical.steps.iter().cloned() {
        let tag = step.tag();
        if required_builds.contains(tag.as_str()) {
            let marker = build_marker(results, &tag);
            let direct_backend_build = lean_exact_cell.is_some()
                && matches!(selection.backend.as_deref(), Some("ptrace" | "kvm"));
            let command = if direct_backend_build {
                "CARGO_BUILD_JOBS=8 cargo build --release --locked -p hermit --bin hermit".into()
            } else {
                step.cmd.clone()
            };
            step.cmd = format!(
                "mkdir -p {state}; if test -f {marker}; then exit 0; fi; ( {command} ) && printf 'ok\\n' > {marker}",
                state = shell_quote(&marker.parent().unwrap().to_string_lossy()),
                marker = shell_quote(&marker.to_string_lossy()),
            );
            if lean_exact_cell.is_some() && selection.backend.as_deref() != Some("liteinst") {
                step.deps.clear();
            }
            if direct_backend_build {
                step.timeout = 600;
                step.cpu_timeout = 1200;
                step.hint = ResourceHint {
                    resources: BTreeMap::from([("cargo_writer".into(), 1)]),
                    rss_baseline_bytes: Some(4_294_967_296),
                    hard_mem_max_bytes: Some(17_179_869_184),
                    classification: StepClass::CpuBound,
                    preferred_inner_jobs: Some(8),
                    ..ResourceHint::default()
                };
            }
            if step.cpu_timeout <= 0 {
                step.cpu_timeout = step.timeout * 2;
            }
            steps.push(step);
        }
    }
    if steps.len() != required_builds.len() {
        return Err(format!(
            "canonical build extraction found {} of {} required nodes",
            steps.len(),
            required_builds.len()
        ));
    }

    let sha = git_output(root, &["rev-parse", "HEAD"])?;
    let detcore_tree = git_output(root, &["rev-parse", "HEAD:detcore"])?;
    let build_root = results.join("build").join(&sha);
    let mut preparation_tags = BTreeMap::new();
    for (test, cell) in preparation_by_test {
        let budget = budgets
            .get(&(test.clone(), cell.mode.clone()))
            .ok_or_else(|| format!("no manifest budget for {test}/{}", cell.mode))?;
        let job = sanitize(&test);
        let tag = format!("prepare.{job}");
        let status_path = results.join("prepare").join(&job).join("status");
        let backend = if cell.backend == "native" {
            String::new()
        } else {
            format!(" --backend {}", shell_quote(&cell.backend))
        };
        let pressure_seconds = pressure_timeout(budget, selection.cell_timeout_seconds)?;
        let cmd = format!(
            "mkdir -p {status_dir}; if test -f {status}; then exit 0; fi; \
             printf '{incomplete}\\n' > {status}; status=0; \
             timeout --kill-after=10s {pressure_seconds}s env \
             E2E_RESULT_ROOT={results} E2E_BUILD_ROOT={build_root} \
             ./ci/test_harness.sh build --include-manual --include-occasional \
             --test {test} --mode {mode}{backend} || status=$?; \
             printf '%s\\n' \"$status\" > {status}; exit 0",
            status_dir = shell_quote(&status_path.parent().unwrap().to_string_lossy()),
            results = shell_quote(&results.to_string_lossy()),
            build_root = shell_quote(&build_root.to_string_lossy()),
            test = shell_quote(&test),
            mode = shell_quote(&cell.mode),
            backend = backend,
            status = shell_quote(&status_path.to_string_lossy()),
            incomplete = INCOMPLETE_ATTEMPT_STATUS,
        );
        let wall = preparation_node_timeout(budget, selection.cell_timeout_seconds)?;
        steps.push(Step {
            group: "prepare".into(),
            job,
            desc: format!("Prepare red-cell fixture {test}"),
            description: String::new(),
            cmd,
            deps: vec!["build.e2e_artifact".into()],
            env: BTreeMap::new(),
            hint: ResourceHint {
                resources: BTreeMap::from([("cargo_writer".into(), 1)]),
                rss_baseline_bytes: Some(1_073_741_824),
                hard_mem_max_bytes: Some(3_221_225_472),
                classification: StepClass::CpuBound,
                ..ResourceHint::default()
            },
            networkonly: false,
            engine_only: false,
            timeout: wall,
            cpu_timeout: wall * 2,
            jobs_flag: None,
            skip_reason: None,
        });
        preparation_tags.insert(test, tag);
    }

    let mut cell_tags = Vec::new();
    let mut cell_timeouts = BTreeMap::new();
    for tracked in &cells {
        let cell = &tracked.id;
        let budget = budgets
            .get(&(cell.test.clone(), cell.mode.clone()))
            .ok_or_else(|| format!("no manifest budget for {}/{}", cell.test, cell.mode))?;
        for repetition in repetition_numbers(selection.repetitions) {
            let slug = cell_run_slug(cell, repetition);
            let tag = format!("cell.{slug}");
            let cell_dir = results.join("cells").join(&slug);
            let result_file = cell_dir.join("results.jsonl");
            let result_in_progress = cell_dir.join("results.in-progress.jsonl");
            let junit = cell_dir.join("junit.xml");
            let junit_in_progress = cell_dir.join("junit.in-progress.xml");
            let status_file = cell_dir.join("harness-status");
            let (selector, backend) = if tracked.enabled {
                let backend = if cell.backend == "native" {
                    String::new()
                } else {
                    format!(" --backend {}", shell_quote(&cell.backend))
                };
                ("--include-manual", backend)
            } else {
                (
                    "--probe-disabled",
                    format!(" --backend {}", shell_quote(&cell.backend)),
                )
            };
            let preparation_guard = if selection.uses_shared_preparation() {
                let preparation_status = results
                    .join("prepare")
                    .join(sanitize(&cell.test))
                    .join("status");
                format!(
                    "if ! test \"$(cat {preparation_status} 2>/dev/null)\" = 0; then printf '{failed}\\n' > {status_file}; exit 0; fi; ",
                    preparation_status = shell_quote(&preparation_status.to_string_lossy()),
                    failed = PREPARATION_FAILED_STATUS,
                    status_file = shell_quote(&status_file.to_string_lossy()),
                )
            } else {
                String::new()
            };
            let harness = if selection.allows_dirty_source() {
                format!(
                    "HERMIT_BIN=\"$PWD/target/release/hermit\" ./ci/test_harness.sh run {selector} --include-occasional --test {test} --mode {mode}{backend} --results {result_file} --junit {junit}",
                    selector = selector,
                    test = shell_quote(&cell.test),
                    mode = shell_quote(&cell.mode),
                    backend = backend,
                    result_file = shell_quote(&result_in_progress.to_string_lossy()),
                    junit = shell_quote(&junit_in_progress.to_string_lossy()),
                )
            } else {
                format!(
                    "./ci/run-with-hermit-e2e-artifact.sh --require-install ./ci/test_harness.sh run {selector} --include-occasional --prebuilt --test {test} --mode {mode}{backend} --results {result_file} --junit {junit}",
                    selector = selector,
                    test = shell_quote(&cell.test),
                    mode = shell_quote(&cell.mode),
                    backend = backend,
                    result_file = shell_quote(&result_in_progress.to_string_lossy()),
                    junit = shell_quote(&junit_in_progress.to_string_lossy()),
                )
            };
            let cmd = format!(
                "mkdir -p {cell_dir}; if test -f {status_file}; then exit 0; fi; \
             printf '{incomplete}\\n' > {status_file}; {preparation_guard}status=0; \
             env E2E_RESULT_ROOT={results} E2E_BUILD_ROOT={build_root} E2E_RUN_ID={run_id} \
             E2E_KEEP_VERIFY_LOGS=1 \
             {harness} \
             || status=$?; \
             if test -e {result_in_progress}; then mv -- {result_in_progress} {result_file} || status=$?; fi; \
             if test -e {junit_in_progress}; then mv -- {junit_in_progress} {junit} || status=$?; fi; \
             printf '%s\\n' \"$status\" > {status_file}; exit \"$status\"",
                cell_dir = shell_quote(&cell_dir.to_string_lossy()),
                results = shell_quote(&results.to_string_lossy()),
                build_root = shell_quote(&build_root.to_string_lossy()),
                run_id = shell_quote(&slug),
                harness = harness,
                result_in_progress = shell_quote(&result_in_progress.to_string_lossy()),
                result_file = shell_quote(&result_file.to_string_lossy()),
                junit_in_progress = shell_quote(&junit_in_progress.to_string_lossy()),
                junit = shell_quote(&junit.to_string_lossy()),
                status_file = shell_quote(&status_file.to_string_lossy()),
                incomplete = INCOMPLETE_ATTEMPT_STATUS,
                preparation_guard = preparation_guard,
            );
            let wall = pressure_timeout(budget, selection.cell_timeout_seconds)?;
            cell_timeouts.insert(tag.clone(), wall);
            // KVM's canonical privileged nodes are boxed at 16 GiB even when the
            // manifest cell itself is in the portable lane. Preserve that safety
            // boundary here; a 3 GiB generic portable cap kills the VM before its
            // compatibility result exists.
            let memory = if cell.lane == "privileged" || cell.backend == "kvm" {
                16_i64 * 1024 * 1024 * 1024
            } else {
                3_i64 * 1024 * 1024 * 1024
            };
            let mut resources = BTreeMap::from([("manifest_guest".into(), 1)]);
            if cell.backend == "kvm" {
                resources.insert("kvm".into(), 1);
            }
            let deps = selected_cell_dependencies(
                !selection.uses_shared_preparation(),
                &cell.mode,
                &cell.backend,
                preparation_tags.get(&cell.test).map(String::as_str),
            );
            steps.push(Step {
                group: "cell".into(),
                job: slug,
                desc: if let Some(number) = repetition {
                    format!(
                        "Repeat green cell {}/{}/{}@{} ({number}/{})",
                        cell.test,
                        cell.mode,
                        cell.backend,
                        cell.lane,
                        selection.run_count()
                    )
                } else {
                    format!(
                        "Attempt red cell {}/{}/{}@{}",
                        cell.test, cell.mode, cell.backend, cell.lane
                    )
                },
                description: String::new(),
                cmd,
                deps,
                env: BTreeMap::new(),
                hint: ResourceHint {
                    resources,
                    rss_baseline_bytes: Some(memory / 3),
                    hard_mem_max_bytes: Some(memory),
                    classification: StepClass::LatencyBound,
                    ..ResourceHint::default()
                },
                networkonly: false,
                engine_only: false,
                timeout: wall,
                cpu_timeout: wall * 2,
                jobs_flag: None,
                skip_reason: None,
            });
            cell_tags.push(tag);
        }
    }

    steps.push(Step {
        group: "pressure".into(),
        job: "summarize".into(),
        desc: if selection.repeats_green_cell() {
            "Wait for every repeated green-cell check before reading retained runner evidence"
                .into()
        } else {
            "Wait for every red-cell attempt before reading retained runner evidence".into()
        },
        description: String::new(),
        cmd: "true".into(),
        deps: cell_tags,
        env: BTreeMap::new(),
        hint: ResourceHint {
            rss_baseline_bytes: Some(268_435_456),
            hard_mem_max_bytes: Some(1_073_741_824),
            classification: StepClass::Light,
            ..ResourceHint::default()
        },
        networkonly: false,
        engine_only: false,
        timeout: 120,
        cpu_timeout: 120,
        jobs_flag: None,
        skip_reason: None,
    });

    let max_timeout = steps.iter().map(|step| step.timeout).max().unwrap_or(120);
    let mut dag = canonical;
    dag.resource_caps = BTreeMap::from([
        ("cargo_writer".into(), 1),
        ("manifest_guest".into(), 4),
        ("kvm".into(), 1),
    ]);
    dag.default_step_timeout = max_timeout;
    dag.default_step_cpu_timeout = max_timeout * 2;
    dag.steps = steps;
    let expected_runs = cells.len().saturating_mul(selection.run_count());
    audit_dag(&dag, expected_runs, run_timeout_seconds, &cell_timeouts)?;
    let mut dag_text = dag_to_json(&dag);
    dag_text.push('\n');
    let reparsed = dag_from_json(&dag_text)
        .map_err(|e| format!("generated pressure DAG does not parse: {e}"))?;
    assert_plan_round_trip(&dag, &reparsed)?;
    audit_dag(
        &reparsed,
        expected_runs,
        run_timeout_seconds,
        &cell_timeouts,
    )?;
    let retained_output = results.join("dag.json");
    fs::write(&retained_output, &dag_text)
        .map_err(|e| format!("cannot write {}: {e}", retained_output.display()))?;
    if output != retained_output {
        return Err(format!(
            "pressure plan must be retained and executed at {}; refusing alternate output {}",
            retained_output.display(),
            output.display()
        ));
    }

    let metadata = RunMetadata {
        schema: RUN_SCHEMA,
        hermit_sha: sha,
        detcore_tree,
        source_tree_dirty: worktree_dirty(root)?,
        run_timeout_seconds,
        mode: selection.mode.clone(),
        test: selection.test.clone(),
        backend: selection.backend.clone(),
        cell_timeout_seconds: selection.cell_timeout_seconds,
        sample: selection.sample,
        seed: selection.seed,
        unavailable_cells: unavailable.len(),
        repetitions: selection.repetitions,
        cells: cells.into_iter().map(|cell| cell.id).collect(),
    };
    let mut metadata_text = serde_json::to_string_pretty(&metadata)
        .map_err(|e| format!("cannot serialize run metadata: {e}"))?;
    metadata_text.push('\n');
    fs::write(results.join("run.json"), metadata_text)
        .map_err(|e| format!("cannot write run metadata: {e}"))?;
    Ok(metadata)
}

fn audit_dag(
    dag: &DagConfig,
    expected_cells: usize,
    run_timeout: i64,
    expected_cell_timeouts: &BTreeMap<String, i64>,
) -> Result<(), String> {
    let mut tags = BTreeSet::new();
    let mut deps = Vec::new();
    let mut cells = 0usize;
    let mut summaries = 0usize;
    for step in &dag.steps {
        let tag = step.tag();
        if !tags.insert(tag.clone()) {
            return Err(format!("generated DAG has duplicate tag {tag}"));
        }
        let timeout = step.timeout;
        let cpu_timeout = step.cpu_timeout;
        if timeout <= 0 || cpu_timeout <= 0 || timeout >= run_timeout {
            return Err(format!(
                "{tag} has invalid timeout ladder wall={timeout} cpu={cpu_timeout} run={run_timeout}"
            ));
        }
        if step.hint.hard_mem_max_bytes.unwrap_or(0) <= 0 {
            return Err(format!("{tag} has no hard memory cap"));
        }
        for (resource, demand) in &step.hint.resources {
            let capacity = dag.resource_caps.get(resource).copied().unwrap_or(0);
            if *demand <= 0 || capacity < *demand {
                return Err(format!(
                    "{tag} requests {demand} unit(s) of {resource}, but the DAG grants {capacity}"
                ));
            }
        }
        for dep in &step.deps {
            deps.push((tag.clone(), dep.clone()));
        }
        if step.group == "cell" {
            cells += 1;
            let expected_timeout = expected_cell_timeouts
                .get(&tag)
                .ok_or_else(|| format!("{tag} has no derived cell wall cap"))?;
            if timeout != *expected_timeout {
                return Err(format!(
                    "{tag} wall timeout {timeout}s does not equal its derived {expected_timeout}s cap"
                ));
            }
            let cmd = &step.cmd;
            let enabled_selector = cmd.contains("--include-manual");
            let disabled_selector = cmd.contains("--probe-disabled");
            let prepared_input = cmd.contains("--prebuilt")
                || cmd.contains("HERMIT_BIN=\"$PWD/target/release/hermit\"");
            if cmd.contains("timeout --kill-after=10s")
                || !cmd.contains("printf '125")
                || !cmd.contains("exit \"$status\"")
                || !cmd.contains("results.in-progress.jsonl")
                || !cmd.contains("mv --")
                || enabled_selector == disabled_selector
                || !prepared_input
                || !cmd.contains("--test")
                || !cmd.contains("--mode")
                || !cmd.contains("--results")
                || !cmd.contains("--junit")
            {
                return Err(format!(
                    "{tag} lost its runner-bounded exact-cell harness command"
                ));
            }
            if cmd.contains("--prebuilt")
                && (!cmd.contains("/prepare/")
                    || !cmd.contains("/status")
                    || !cmd.contains("printf '126"))
            {
                return Err(format!(
                    "{tag} can consume a prebuilt fixture without refusing failed preparation"
                ));
            }
        }
        if tag == "pressure.summarize" {
            summaries += 1;
        }
    }
    for (tag, dep) in deps {
        if !tags.contains(&dep) {
            return Err(format!("{tag} depends on absent step {dep}"));
        }
    }
    if cells != expected_cells || expected_cell_timeouts.len() != expected_cells || summaries != 1 {
        return Err(format!(
            "generated DAG shape mismatch: cells={cells}/{expected_cells}, timeout_caps={}/{expected_cells}, summaries={summaries}/1",
            expected_cell_timeouts.len()
        ));
    }
    Ok(())
}

/// Prove that the JSON consumed by run-dag.sh preserves the typed plan's
/// commands, dependencies, and effective containment. The pinned serializer
/// intentionally omits DagConfig's default step CPU/memory/core fields. That
/// is harmless only because every generated node declares wall, CPU, and hard
/// memory caps; compare their effective values here rather than assuming a
/// structural round trip.
fn assert_plan_round_trip(expected: &DagConfig, actual: &DagConfig) -> Result<(), String> {
    if dag_to_json(expected) != dag_to_json(actual) {
        return Err("generated pressure DAG changed during typed JSON round trip".into());
    }
    if expected.resource_caps != actual.resource_caps {
        return Err("generated pressure DAG changed named resource capacities".into());
    }
    if expected.steps.len() != actual.steps.len() {
        return Err("generated pressure DAG changed its step count".into());
    }
    for (before, after) in expected.steps.iter().zip(&actual.steps) {
        let tag = before.tag();
        if tag != after.tag()
            || before.timeout != after.timeout
            || before.hint.hard_mem_max_bytes != after.hint.hard_mem_max_bytes
            || before.hint.resources != after.hint.resources
            || effective_cpu_timeout(before, expected.default_step_cpu_timeout)
                != effective_cpu_timeout(after, actual.default_step_cpu_timeout)
            || effective_cpu_count(before, expected.default_step_cpu_count)
                != effective_cpu_count(after, actual.default_step_cpu_count)
        {
            return Err(format!(
                "generated pressure DAG changed effective caps or resource demand for {tag}"
            ));
        }
    }
    Ok(())
}

fn validate_run_contract(
    root: &Path,
    results: &Path,
    metadata: &RunMetadata,
    allow_dirty_exact_cell: bool,
) -> Result<BTreeMap<CellId, bool>, String> {
    if metadata.source_tree_dirty && !allow_dirty_exact_cell {
        return Err("pressure run metadata claims a dirty source tree".into());
    }
    if metadata.source_tree_dirty && metadata.repetitions.is_some() {
        return Err("repeated green-cell results require a clean committed source tree".into());
    }
    let detcore_tree = git_output(root, &["rev-parse", "HEAD:detcore"])?;
    if detcore_tree != metadata.detcore_tree {
        return Err(format!(
            "pressure run detcore tree is {}, checkout has {}",
            metadata.detcore_tree, detcore_tree
        ));
    }

    let selection = CellSelection {
        mode: metadata.mode.clone(),
        test: metadata.test.clone(),
        backend: metadata.backend.clone(),
        cell_timeout_seconds: metadata.cell_timeout_seconds,
        sample: metadata.sample,
        seed: metadata.seed,
        run_timeout_seconds: Some(metadata.run_timeout_seconds),
        repetitions: metadata.repetitions,
    };
    let pressure_cells = pressure_cells(root, &selection)?;
    if metadata.unavailable_cells != pressure_cells.unavailable.len() {
        return Err(format!(
            "run metadata records {} unavailable red cell(s), current manifest selection has {}",
            metadata.unavailable_cells,
            pressure_cells.unavailable.len()
        ));
    }
    let expected_cells = pressure_cells.selected;
    let mut expected = BTreeMap::new();
    for tracked in expected_cells {
        if expected.insert(tracked.id, tracked.enabled).is_some() {
            return Err("tracked scorecard contains a duplicate red-cell identity".into());
        }
    }
    let actual: BTreeSet<_> = metadata.cells.iter().cloned().collect();
    if actual.len() != metadata.cells.len() {
        return Err("run metadata contains a duplicate cell identity".into());
    }
    let expected_ids: BTreeSet<_> = expected.keys().cloned().collect();
    if actual != expected_ids {
        let missing = expected_ids
            .difference(&actual)
            .next()
            .map(display_id)
            .unwrap_or_else(|| "none".into());
        let extra = actual
            .difference(&expected_ids)
            .next()
            .map(display_id)
            .unwrap_or_else(|| "none".into());
        return Err(format!(
            "run metadata does not match the current selected population: expected={} actual={} first_missing={} first_extra={}",
            expected_ids.len(),
            actual.len(),
            missing,
            extra
        ));
    }

    let dag_path = results.join("dag.json");
    let dag_text = fs::read_to_string(&dag_path)
        .map_err(|e| format!("cannot read {}: {e}", dag_path.display()))?;
    let dag =
        dag_from_json(&dag_text).map_err(|e| format!("invalid {}: {e}", dag_path.display()))?;
    let budgets = load_budgets(root)?;
    let mut expected_cell_timeouts = BTreeMap::new();
    for cell in expected.keys() {
        let budget = budgets
            .get(&(cell.test.clone(), cell.mode.clone()))
            .ok_or_else(|| format!("no manifest budget for {}/{}", cell.test, cell.mode))?;
        for repetition in repetition_numbers(metadata.repetitions) {
            expected_cell_timeouts.insert(
                format!("cell.{}", cell_run_slug(cell, repetition)),
                pressure_timeout(budget, metadata.cell_timeout_seconds)?,
            );
        }
    }
    audit_dag(
        &dag,
        expected
            .len()
            .saturating_mul(metadata.repetitions.unwrap_or(1)),
        metadata.run_timeout_seconds,
        &expected_cell_timeouts,
    )?;
    let dag_cells: BTreeSet<_> = dag
        .steps
        .iter()
        .filter(|step| step.group == "cell")
        .map(|step| step.job.clone())
        .collect();
    let expected_jobs: BTreeSet<_> = expected
        .keys()
        .flat_map(|cell| {
            repetition_numbers(metadata.repetitions)
                .into_iter()
                .map(move |repetition| cell_run_slug(cell, repetition))
        })
        .collect();
    if dag_cells != expected_jobs {
        return Err("generated DAG cell identities do not match run metadata".into());
    }
    Ok(expected)
}

fn load_runner_evidence(
    results: &Path,
    hermit_sha: &str,
) -> Result<BTreeMap<String, RunnerEvidence>, String> {
    let profile_dir = results.join("runner-profile");
    let mut files = Vec::new();
    for entry in fs::read_dir(&profile_dir).map_err(|e| {
        format!(
            "cannot read retained runner profiles {}: {e}",
            profile_dir.display()
        )
    })? {
        let entry = entry.map_err(|e| format!("cannot read runner-profile entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("step_profiles_") && name.ends_with(".csv") {
            files.push(entry.path());
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!(
            "no retained per-step runner profile under {}",
            profile_dir.display()
        ));
    }

    let mut evidence = BTreeMap::<String, RunnerEvidence>::new();
    for path in files {
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(true)
            .from_path(&path)
            .map_err(|e| format!("cannot read {} as CSV: {e}", path.display()))?;
        let headers = reader
            .headers()
            .map_err(|e| format!("cannot read {} CSV header: {e}", path.display()))?
            .clone();
        let column = |name: &str| {
            headers
                .iter()
                .position(|candidate| candidate == name)
                .ok_or_else(|| format!("{} has no `{name}` column", path.display()))
        };
        let sha_column = column("git_sha")?;
        let step_column = column("step")?;
        let ok_column = column("ok")?;
        let timeout_column = column("timed_out")?;
        let cpu_timeout_column = column("cpu_timed_out")?;
        let oom_column = column("oom_kills")?;
        for (row_index, record) in reader.records().enumerate() {
            let fields = record
                .map_err(|e| format!("{}:{} is invalid CSV: {e}", path.display(), row_index + 2))?;
            if fields.get(sha_column) != Some(hermit_sha) {
                continue;
            }
            if fields.len() != headers.len() {
                return Err(format!(
                    "{}:{} has {} CSV fields, expected {}",
                    path.display(),
                    row_index + 2,
                    fields.len(),
                    headers.len()
                ));
            }
            let Some(step) = fields.get(step_column) else {
                continue;
            };
            let parse_bool = |column: usize, name: &str| -> Result<bool, String> {
                match fields.get(column).map(str::to_ascii_lowercase).as_deref() {
                    Some("true") => Ok(true),
                    Some("false") => Ok(false),
                    other => Err(format!(
                        "{}:{} has invalid {name} value {:?}",
                        path.display(),
                        row_index + 2,
                        other
                    )),
                }
            };
            let ok = parse_bool(ok_column, "ok")?;
            let timed_out = parse_bool(timeout_column, "timed_out")?
                || parse_bool(cpu_timeout_column, "cpu_timed_out")?;
            let oom_kills = fields
                .get(oom_column)
                .ok_or_else(|| {
                    format!(
                        "{}:{} has no oom_kills value",
                        path.display(),
                        row_index + 2
                    )
                })?
                .parse::<u64>()
                .map_err(|e| {
                    format!(
                        "{}:{} has invalid oom_kills: {e}",
                        path.display(),
                        row_index + 2
                    )
                })?;
            let row = evidence.entry(step.to_string()).or_default();
            row.seen = true;
            row.ok |= ok;
            row.timed_out |= timed_out;
            row.oom |= oom_kills > 0;
        }
    }
    if evidence.is_empty() {
        return Err(format!(
            "retained runner profiles contain no rows for Hermit {}",
            hermit_sha
        ));
    }
    Ok(evidence)
}

fn reason_reports_timeout(reason: Option<&str>) -> bool {
    reason.is_some_and(|reason| {
        reason.ends_with("(innermost E2E timeout: deadline reached (exit 124))")
            || reason.ends_with("(innermost E2E timeout: SIGKILL after 10 s grace (exit 137))")
    })
}

fn is_proven_timeout_attempt(runner: RunnerEvidence, harness_status: Option<i32>) -> bool {
    // safe-ci owns the cell wall clock. Its exact-SHA/exact-step timeout row
    // and the marker written before the harness starts are both required.
    // A child may exit 124 on its own, so a terminal status is never timeout
    // proof by itself.
    runner.seen && runner.timed_out && harness_status == Some(INCOMPLETE_ATTEMPT_STATUS)
}

fn is_proven_oom_attempt(runner: RunnerEvidence, harness_status: Option<i32>) -> bool {
    // The per-step row is already selected by exact source SHA and exact DAG
    // step name. The numeric marker proves that this cell began before its
    // cgroup reported an OOM kill; without both records, absence of terminal
    // artifacts is not evidence of a guest OOM.
    runner.seen && runner.oom && harness_status.is_some()
}

fn runner_observed_terminal_attempt(runner: RunnerEvidence, harness_status: Option<i32>) -> bool {
    runner.seen
        && !runner.oom
        && !runner.timed_out
        && harness_status.is_some_and(|status| {
            !matches!(
                status,
                INCOMPLETE_ATTEMPT_STATUS | PREPARATION_FAILED_STATUS
            )
        })
}

fn result_row_matches_cell(
    row: &ResultRow,
    slug: &str,
    metadata: &RunMetadata,
    cell: &CellId,
    expected_required: bool,
    harness_status: Option<i32>,
) -> bool {
    let observed_backend = row.backend.as_deref().or_else(|| {
        if row.mode == "naked" {
            Some("native")
        } else {
            None
        }
    });
    let identity_matches = row.schema == 3
        && row.run_id == slug
        && row.hermit_sha == metadata.hermit_sha
        && row.source_tree_dirty == metadata.source_tree_dirty
        && row.test == cell.test
        && row.category == cell.category
        && row.lane == cell.lane
        && row.mode == cell.mode
        && observed_backend == Some(cell.backend.as_str())
        && row.classification
            == if expected_required {
                "required"
            } else {
                "disabled"
            };
    let exit_matches = match row.outcome.as_str() {
        "PASS" => harness_status == Some(0),
        "FAIL" | "ERROR" => harness_status.is_some_and(|status| status != 0),
        _ => false,
    };
    identity_matches && exit_matches
}

fn classify_result(
    runner: RunnerEvidence,
    harness_status: Option<i32>,
    outcome: &str,
    row_valid: bool,
    reason: Option<&str>,
    mode: &str,
    verification_verdict: Option<&str>,
    verification_logs_retained: bool,
    verification_evidence_valid: bool,
) -> &'static str {
    if !runner.seen {
        "infrastructure-error"
    } else if runner.oom {
        if is_proven_oom_attempt(runner, harness_status) && verification_evidence_valid {
            "oom"
        } else {
            "infrastructure-error"
        }
    } else if is_proven_timeout_attempt(runner, harness_status) {
        if verification_evidence_valid {
            "timeout"
        } else {
            "infrastructure-error"
        }
    } else if runner.timed_out || !verification_evidence_valid {
        "infrastructure-error"
    } else if reason_reports_timeout(reason) {
        "timeout"
    } else if mode == "verify"
        && matches!(verification_verdict, Some("matched" | "diverged"))
        && !verification_logs_retained
    {
        "infrastructure-error"
    } else if row_valid && mode == "verify" && verification_verdict == Some("diverged") {
        "determinism-failure"
    } else if row_valid && mode == "replay" && verification_verdict == Some("diverged") {
        "replay-failure"
    } else if outcome == "PASS"
        && row_valid
        && (!matches!(mode, "verify" | "replay") || verification_verdict == Some("matched"))
    {
        "pass"
    } else if row_valid && harness_status.is_some_and(|status| status != 0) {
        "crash-error"
    } else {
        "infrastructure-error"
    }
}

fn verification_report_path(results: &Path, cell: &CellId, run_id: &str) -> PathBuf {
    let harness_cell = format!(
        "{}-{}-{}",
        cell.test.replace('/', "-"),
        cell.mode,
        cell.backend
    );
    results
        .join("runs")
        .join(run_id)
        .join(harness_cell)
        .join("verify-1.json")
}

fn retained_verification_logs(
    results: &Path,
    cell: &CellId,
    run_id: &str,
) -> Result<Vec<String>, String> {
    if cell.mode != "verify" {
        return Ok(Vec::new());
    }
    let directory = verification_report_path(results, cell, run_id)
        .parent()
        .expect("verification report has a parent")
        .join("verify-logs")
        .join("verify-1");
    if !directory.is_dir() {
        return Ok(Vec::new());
    }
    let mut run1 = None;
    let mut run2 = None;
    for entry in fs::read_dir(&directory).map_err(|e| {
        format!(
            "cannot read retained verify logs {}: {e}",
            directory.display()
        )
    })? {
        let entry = entry.map_err(|e| format!("cannot read retained verify-log entry: {e}"))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let slot = if name.starts_with("run1_log_") {
            Some(&mut run1)
        } else if name.starts_with("run2_log_") {
            Some(&mut run2)
        } else {
            None
        };
        let Some(slot) = slot else {
            continue;
        };
        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot inspect {}: {e}", entry.path().display()))?;
        let metadata = entry
            .metadata()
            .map_err(|e| format!("cannot inspect {}: {e}", entry.path().display()))?;
        if !file_type.is_file() || metadata.len() == 0 {
            return Err(format!(
                "retained verify-log capture {} is not a nonempty regular file",
                entry.path().display()
            ));
        }
        if slot
            .replace(entry.path().to_string_lossy().into_owned())
            .is_some()
        {
            return Err(format!(
                "retained verify-log directory {} contains duplicate {} captures",
                directory.display(),
                if name.starts_with("run1_log_") {
                    "run1"
                } else {
                    "run2"
                }
            ));
        }
    }
    if run1.is_some() != run2.is_some() {
        return Err(format!(
            "retained verify-log directory {} must contain exactly one nonempty run1 capture and one nonempty run2 capture",
            directory.display()
        ));
    }
    Ok(run1.into_iter().chain(run2).collect())
}

fn normalized_ptrace_golden(
    results: &Path,
    cell: &CellId,
    run_id: &str,
) -> Result<Option<String>, String> {
    if cell.mode != "verify" || cell.backend != "ptrace" {
        return Ok(None);
    }
    let directory = verification_report_path(results, cell, run_id)
        .parent()
        .expect("verification report has a parent")
        .join("verify-logs")
        .join("verify-1");
    let status_path = directory.join("normalized-ptrace-golden.status");
    let path = directory.join("normalized-ptrace-golden.log");
    if !status_path.exists() && !path.exists() {
        return Ok(None);
    }
    if !status_path.is_file() {
        return Err(format!(
            "ptrace golden-log output {} exists without its numeric status {}",
            path.display(),
            status_path.display()
        ));
    }
    let status_text = fs::read_to_string(&status_path)
        .map_err(|e| format!("cannot read {}: {e}", status_path.display()))?;
    let status = status_text.trim().parse::<i32>().map_err(|_| {
        format!(
            "{} contains nonnumeric log-diff exit `{}`",
            status_path.display(),
            status_text.trim()
        )
    })?;
    if status != 0 {
        return Err(format!(
            "ptrace golden-log normalization failed with exit {status}; see {}",
            directory.display()
        ));
    }
    if !path.is_file()
        || path
            .metadata()
            .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?
            .len()
            == 0
    {
        return Err(format!(
            "ptrace golden-log normalization reported success without a nonempty {}",
            path.display()
        ));
    }
    Ok(Some(path.to_string_lossy().into_owned()))
}

fn read_verification_report(
    results: &Path,
    cell: &CellId,
    run_id: &str,
) -> Result<Option<JsonValue>, String> {
    if !matches!(cell.mode.as_str(), "verify" | "replay") {
        return Ok(None);
    }
    let path = verification_report_path(results, cell, run_id);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read verification report {}: {e}", path.display()))?;
    let report: JsonValue = serde_json::from_str(&text)
        .map_err(|e| format!("invalid verification report {}: {e}", path.display()))?;
    let evidence: VerificationEvidence = serde_json::from_value(report.clone())
        .map_err(|e| format!("incomplete verification report {}: {e}", path.display()))?;
    match (evidence.verdict.as_str(), evidence.verified) {
        ("matched", true) | ("diverged" | "no_result", false) => {}
        (verdict, verified) => {
            return Err(format!(
                "inconsistent verification report {}: verdict={verdict} verified={verified}",
                path.display()
            ));
        }
    }
    if evidence.verdict != "no_result" && !evidence.comparison.is_object() {
        return Err(format!(
            "terminal verification report {} has no comparison object",
            path.display()
        ));
    }
    if !evidence.compared_log_messages.is_null() {
        let counts = evidence.compared_log_messages.as_object().ok_or_else(|| {
            format!(
                "verification report {} has invalid compared_log_messages",
                path.display()
            )
        })?;
        for side in ["left", "right"] {
            if counts.get(side).and_then(JsonValue::as_u64).is_none() {
                return Err(format!(
                    "verification report {} has no numeric {side} message count",
                    path.display()
                ));
            }
        }
    }
    if evidence.bitwise_parity && evidence.verdict != "matched" {
        return Err(format!(
            "verification report {} claims bitwise parity without a match",
            path.display()
        ));
    }
    if evidence.verdict != "diverged"
        && (evidence.first_divergent_scheduler_turn.0.is_some()
            || evidence.first_divergent_virtual_nanoseconds.0.is_some())
    {
        return Err(format!(
            "verification report {} records a divergence position without a divergent verdict",
            path.display()
        ));
    }
    Ok(Some(report))
}

fn summarize(root: &Path, results: &Path, allow_dirty_exact_cell: bool) -> Result<(), String> {
    let metadata_path = results.join("run.json");
    let metadata: RunMetadata = serde_json::from_str(
        &fs::read_to_string(&metadata_path)
            .map_err(|e| format!("cannot read {}: {e}", metadata_path.display()))?,
    )
    .map_err(|e| format!("invalid {}: {e}", metadata_path.display()))?;
    if metadata.schema != RUN_SCHEMA {
        return Err(format!("unsupported run schema {}", metadata.schema));
    }
    let current = git_output(root, &["rev-parse", "HEAD"])?;
    if current != metadata.hermit_sha {
        return Err(format!(
            "run belongs to {}, but checkout HEAD is {}",
            metadata.hermit_sha, current
        ));
    }
    if allow_dirty_exact_cell && (metadata.test.is_none() || metadata.backend.is_none()) {
        return Err("dirty pressure results are accepted only for one exact cell".into());
    }
    let expected = validate_run_contract(root, results, &metadata, allow_dirty_exact_cell)?;
    let runner_evidence = load_runner_evidence(results, &metadata.hermit_sha)?;

    let mut by_backend: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    let mut passing = Vec::new();
    let mut rows = Vec::new();
    for cell in &metadata.cells {
        for repetition in repetition_numbers(metadata.repetitions) {
            let slug = cell_run_slug(cell, repetition);
            let cell_dir = results.join("cells").join(&slug);
            let step_tag = format!("cell.{slug}");
            let runner = runner_evidence.get(&step_tag).copied().unwrap_or_default();
            let mut evidence_errors = Vec::new();
            let status_file = cell_dir.join("harness-status");
            let harness_status = if status_file.is_file() {
                match fs::read_to_string(&status_file) {
                    Ok(text) => {
                        let text = text.trim();
                        match text.parse::<i32>() {
                            Ok(status) => Some(status),
                            Err(_) => {
                                evidence_errors.push(format!(
                                    "{} contains nonnumeric harness exit `{text}`",
                                    status_file.display()
                                ));
                                None
                            }
                        }
                    }
                    Err(error) => {
                        evidence_errors.push(format!(
                            "cannot read harness exit {}: {error}",
                            status_file.display()
                        ));
                        None
                    }
                }
            } else {
                evidence_errors.push(format!(
                    "selected cell node wrote no attempt marker at {}",
                    status_file.display()
                ));
                None
            };
            let proven_oom = is_proven_oom_attempt(runner, harness_status);
            let proven_timeout = is_proven_timeout_attempt(runner, harness_status);
            let result_file = cell_dir.join("results.jsonl");
            let (outcome, row_valid, reason, error_kind) = if result_file.is_file() {
                match fs::read_to_string(&result_file) {
                    Ok(text) => {
                        let lines: Vec<_> = text
                            .lines()
                            .filter(|line| !line.trim().is_empty())
                            .collect();
                        if lines.len() != 1 {
                            evidence_errors.push(format!(
                                "{} contains {} nonempty rows; expected exactly one",
                                result_file.display(),
                                lines.len()
                            ));
                            ("NO_RESULT".to_string(), false, None, None)
                        } else {
                            match serde_json::from_str::<ResultRow>(lines[0]) {
                                Ok(row) => {
                                    let row_matches = result_row_matches_cell(
                                        &row,
                                        &slug,
                                        &metadata,
                                        cell,
                                        expected.get(cell).copied().unwrap_or(false),
                                        harness_status,
                                    );
                                    let runner_completed =
                                        runner_observed_terminal_attempt(runner, harness_status);
                                    if row_matches && (proven_oom || runner_completed) {
                                        (row.outcome, true, row.reason, row.error_kind)
                                    } else {
                                        evidence_errors.push(format!(
                                        "{} does not match the selected cell, harness exit, or retained runner result",
                                        result_file.display()
                                    ));
                                        ("NO_RESULT".to_string(), false, None, None)
                                    }
                                }
                                Err(error) => {
                                    evidence_errors.push(format!(
                                        "invalid result row {}: {error}",
                                        result_file.display()
                                    ));
                                    ("NO_RESULT".to_string(), false, None, None)
                                }
                            }
                        }
                    }
                    Err(error) => {
                        evidence_errors.push(format!(
                            "cannot read result row {}: {error}",
                            result_file.display()
                        ));
                        ("NO_RESULT".to_string(), false, None, None)
                    }
                }
            } else if !proven_oom && !proven_timeout {
                evidence_errors.push(format!("missing result row {}", result_file.display()));
                ("NO_RESULT".to_string(), false, None, None)
            } else {
                ("NO_RESULT".to_string(), false, None, None)
            };
            let verification = match read_verification_report(results, cell, &slug) {
                Ok(Some(report)) => Some(report),
                Ok(None)
                    if matches!(cell.mode.as_str(), "verify" | "replay")
                        && !proven_oom
                        && !proven_timeout =>
                {
                    evidence_errors.push(format!(
                        "missing verification report {}",
                        verification_report_path(results, cell, &slug).display()
                    ));
                    None
                }
                Ok(None) => None,
                Err(error) => {
                    evidence_errors.push(error);
                    None
                }
            };
            let verification_verdict = verification
                .as_ref()
                .and_then(|report| report.get("verdict"))
                .and_then(JsonValue::as_str);
            let verification_logs = match retained_verification_logs(results, cell, &slug) {
                Ok(logs) => logs,
                Err(error) => {
                    evidence_errors.push(error);
                    Vec::new()
                }
            };
            let normalized_ptrace_golden = match normalized_ptrace_golden(results, cell, &slug) {
                Ok(path) => path,
                Err(error) => {
                    evidence_errors.push(error);
                    None
                }
            };
            if cell.mode == "verify"
                && matches!(verification_verdict, Some("matched" | "diverged"))
                && verification_logs.len() != 2
            {
                evidence_errors.push(
                "terminal verify result must retain exactly one nonempty run1 log and one nonempty run2 log"
                    .into(),
            );
            }
            if cell.mode == "verify"
                && cell.backend == "ptrace"
                && matches!(verification_verdict, Some("matched" | "diverged"))
                && normalized_ptrace_golden.is_none()
                && !evidence_errors
                    .iter()
                    .any(|error| error.contains("golden-log normalization"))
            {
                evidence_errors
                    .push("terminal ptrace verify result has no normalized golden INFO log".into());
            }
            let result = classify_result(
                runner,
                harness_status,
                &outcome,
                row_valid,
                reason.as_deref(),
                &cell.mode,
                verification_verdict,
                verification_logs.len() == 2,
                evidence_errors.is_empty(),
            );
            *by_backend
                .entry(cell.backend.clone())
                .or_default()
                .entry(result.to_string())
                .or_default() += 1;
            if result == "pass" {
                passing.push(display_id(cell));
            }
            rows.push(json!({
                "cell": cell,
                "repetition": repetition,
                "harness_exit": harness_status,
                "outcome": outcome,
                "reason": reason,
                "error_kind": error_kind,
                "result_row_valid": row_valid,
                "result": result,
                "verification": verification,
                "verification_logs": verification_logs,
                "normalized_ptrace_golden": normalized_ptrace_golden,
                "evidence_errors": evidence_errors,
                "runner_seen": runner.seen,
                "runner_ok": runner.ok,
                "runner_timed_out": runner.timed_out,
                "runner_oom": runner.oom,
                "oom_proven_by_runner_and_attempt_marker": proven_oom,
                "timeout_proven_by_runner_and_attempt_marker": proven_timeout,
            }));
        }
    }
    if metadata.repetitions.is_some() {
        println!("# Repeated green-cell results");
    } else {
        println!("# Red-cell pressure-test results");
    }
    println!();
    println!(
        "Metric: current pre-basic-sanity manifest contract. Verify uses the legacy stripped comparison unless that cell's verification report says bitwise_parity=true; this is not the Milestone 2 strict-default metric."
    );
    println!();
    if metadata.source_tree_dirty {
        println!(
            "**Exploratory result from a dirty working tree: this cannot promote the scorecard.**"
        );
        println!();
    }
    println!(
        "| Backend | Pass | Determinism failure | Replay failure | Crash/error | Timeout | OOM | Infrastructure error | Total |"
    );
    println!("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |");
    let mut totals = [0usize; 8];
    for backend in ["ptrace", "dbt", "kvm", "sabre", "liteinst", "native"] {
        let counts = by_backend.get(backend).cloned().unwrap_or_default();
        let pass = counts.get("pass").copied().unwrap_or(0);
        let determinism = counts.get("determinism-failure").copied().unwrap_or(0);
        let replay = counts.get("replay-failure").copied().unwrap_or(0);
        let crash_error = counts.get("crash-error").copied().unwrap_or(0);
        let timeout = counts.get("timeout").copied().unwrap_or(0);
        let oom = counts.get("oom").copied().unwrap_or(0);
        let infrastructure = counts.get("infrastructure-error").copied().unwrap_or(0);
        let total = pass + determinism + replay + crash_error + timeout + oom + infrastructure;
        totals[0] += pass;
        totals[1] += determinism;
        totals[2] += replay;
        totals[3] += crash_error;
        totals[4] += timeout;
        totals[5] += oom;
        totals[6] += infrastructure;
        totals[7] += total;
        println!(
            "| `{backend}` | {pass} | {determinism} | {replay} | {crash_error} | {timeout} | {oom} | {infrastructure} | {total} |"
        );
    }
    println!(
        "| **Total** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** | **{}** |",
        totals[0], totals[1], totals[2], totals[3], totals[4], totals[5], totals[6], totals[7]
    );
    println!();
    println!(
        "Crash/error combines remaining nonzero harness exits. It includes signal-caused crashes when the shell reports a nonzero status; this runner does not yet distinguish the signal."
    );
    println!();
    if metadata.repetitions.is_none()
        && metadata.cells.len() == 1
        && matches!(metadata.cells[0].mode.as_str(), "verify" | "replay")
    {
        let verification = &rows[0]["verification"];
        if verification.is_object() {
            println!(
                "Exact-cell verification: verdict={} bitwise_parity={} comparison={}",
                verification["verdict"], verification["bitwise_parity"], verification["comparison"]
            );
        } else {
            println!(
                "Exact-cell verification: no trustworthy terminal report; see evidence_errors in summary.json."
            );
        }
        println!();
    }
    let repeated_result = if metadata.repetitions.is_some() {
        let result = if totals[0] == totals[7] {
            "passed every repetition"
        } else if totals[0] == 0 {
            "failed every repetition"
        } else {
            "flaky"
        };
        println!(
            "Repeated result: {}/{} passed; {result}.",
            totals[0], totals[7]
        );
        Some(result)
    } else {
        println!(
            "{} red cell(s) passed once; they are candidates for repeated confirmation, not automatic promotion.",
            passing.len()
        );
        for id in passing.iter().take(20) {
            println!("  PASS {id}");
        }
        None
    };

    let summary = json!({
        "schema": RUN_SCHEMA,
        "hermit_sha": metadata.hermit_sha,
        "detcore_tree": metadata.detcore_tree,
        "source_tree_dirty": metadata.source_tree_dirty,
        "mode": metadata.mode,
        "test": metadata.test,
        "backend": metadata.backend,
        "cell_timeout_seconds": metadata.cell_timeout_seconds,
        "sample": metadata.sample,
        "seed": metadata.seed,
        "unavailable_cells": metadata.unavailable_cells,
        "repetitions": metadata.repetitions,
        "repeated_result": repeated_result,
        "attempted": rows.len(),
        "pass_candidates": passing,
        "rows": rows,
    });
    let mut text = serde_json::to_string_pretty(&summary)
        .map_err(|e| format!("cannot serialize summary: {e}"))?;
    text.push('\n');
    fs::write(results.join("summary.json"), text)
        .map_err(|e| format!("cannot write summary.json: {e}"))?;
    println!("Summary: {}", results.join("summary.json").display());
    if totals[6] > 0 {
        return Err(format!(
            "{} selected cell run(s) produced no trustworthy result; these are harness/infrastructure errors, not compatibility evidence",
            totals[6]
        ));
    }
    if metadata.repetitions.is_some() && totals[0] != totals[7] {
        return Err(format!(
            "only {}/{} repeated green-cell checks passed; the retained summary classifies every non-pass",
            totals[0], totals[7]
        ));
    }
    Ok(())
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn display_id(cell: &CellId) -> String {
    format!(
        "{}/{}/{}/{}@{}",
        cell.lane, cell.category, cell.test, cell.mode, cell.backend
    )
}

fn self_test(root: &Path) -> Result<(), String> {
    let explicit_null = decode_budgets(
        br#"[{"test":"fixture/test","mode":"chaos","timeout_seconds":90,"attempts":null}]"#,
    )?;
    if explicit_null
        .get(&("fixture/test".into(), "chaos".into()))
        .is_none_or(|budget| budget.attempts.is_some())
    {
        return Err("explicit null chaos attempts must remain unavailable".into());
    }
    for (matrix, expected) in [
        (
            br#"[{"test":"fixture/test","mode":"chaos","timeout_seconds":90}]"#.as_slice(),
            "missing field `attempts`",
        ),
        (
            br#"[{"test":"fixture/test","mode":"verify","timeout_seconds":90,"attempts":null}]"#.as_slice(),
            "no attempt count for non-chaos mode",
        ),
        (
            br#"[{"test":"fixture/test","mode":"verify","timeout_seconds":1801,"attempts":1}]"#.as_slice(),
            "outside 1..=1800",
        ),
        (
            br#"[{"test":"fixture/test","mode":"verify","timeout_seconds":90,"attempts":1},{"test":"fixture/test","mode":"verify","timeout_seconds":91,"attempts":1}]"#.as_slice(),
            "conflicting execution budgets",
        ),
    ] {
        let error = decode_budgets(matrix).expect_err("invalid matrix budget must refuse");
        if !error.contains(expected) {
            return Err(format!(
                "invalid matrix budget refused for the wrong reason: {error:?}; expected {expected:?}"
            ));
        }
    }

    let manifest_budgets = load_budgets(root)?;
    let omitted_naked_runs = manifest_budgets
        .get(&("applications/timed-progress-bar".into(), "naked".into()))
        .ok_or("self-test manifest lost applications/timed-progress-bar naked budget")?;
    let explicit_naked_runs = manifest_budgets
        .get(&(
            "determinism-stress-c/producer-consumer".into(),
            "naked".into(),
        ))
        .ok_or("self-test manifest lost determinism-stress-c/producer-consumer naked budget")?;
    if omitted_naked_runs.attempts != Some(3) || explicit_naked_runs.attempts != Some(5) {
        return Err(format!(
            "pressure attempt counts diverge from the harness: omitted naked runs={:?} (want 3), explicit naked runs={:?} (want 5)",
            omitted_naked_runs.attempts, explicit_naked_runs.attempts
        ));
    }
    let seeded_chaos = manifest_budgets
        .get(&("determinism-stress/order-violation".into(), "chaos".into()))
        .ok_or("self-test manifest lost determinism-stress/order-violation chaos budget")?;
    let unavailable_chaos = manifest_budgets
        .get(&("applications/timed-progress-bar".into(), "chaos".into()))
        .ok_or("self-test manifest lost applications/timed-progress-bar chaos budget")?;
    if seeded_chaos.attempts != Some(64) || unavailable_chaos.attempts.is_some() {
        return Err(format!(
            "chaos attemptability diverges from the manifest: seeded={:?} (want 64), no-seed={:?} (want unavailable)",
            seeded_chaos.attempts, unavailable_chaos.attempts
        ));
    }
    let budget = CellBudget {
        timeout_seconds: 7,
        attempts: Some(3),
    };
    if outer_timeout(&budget)? != 98 {
        return Err(format!(
            "timeout derivation changed: expected 98, got {}",
            outer_timeout(&budget)?
        ));
    }
    if pressure_timeout(
        &CellBudget {
            timeout_seconds: 1800,
            attempts: Some(64),
        },
        None,
    )? != PRESSURE_CELL_TIMEOUT_SECONDS
    {
        return Err("pressure timeout did not cap a long repeated red cell".into());
    }
    if pressure_timeout(
        &CellBudget {
            timeout_seconds: 1800,
            attempts: Some(64),
        },
        Some(37),
    )? != 37
    {
        return Err("exact-cell pressure timeout did not apply the requested tighter cap".into());
    }
    let batch_without_liteinst: BTreeSet<_> = REQUIRED_BUILD_TAGS
        .into_iter()
        .filter(|tag| *tag != "build.liteinst_runtime_release")
        .collect();
    let lean_exact = BTreeSet::from(["build.runtime_release"]);
    let exact_runtime_backends_ok = ["ptrace", "kvm", "dbt", "sabre"]
        .into_iter()
        .all(|backend| required_build_tags(Some(("verify", backend)), false) == lean_exact);
    if !exact_runtime_backends_ok
        || !required_build_tags(Some(("naked", "native")), false).is_empty()
        || required_build_tags(Some(("verify", "liteinst")), true)
            != BTreeSet::from(REQUIRED_BUILD_TAGS)
        || required_build_tags(None, false) != batch_without_liteinst
        || required_build_tags(None, true) != BTreeSet::from(REQUIRED_BUILD_TAGS)
    {
        return Err(
            "selected-cell build closure lost a required node or built LiteInst for a sample without LiteInst"
                .into(),
        );
    }
    let non_liteinst_batch =
        selected_cell_dependencies(false, "verify", "ptrace", Some("prepare.fixture"));
    let liteinst_batch =
        selected_cell_dependencies(false, "verify", "liteinst", Some("prepare.fixture"));
    if non_liteinst_batch.contains(&"build.liteinst_runtime_release".to_string())
        || !liteinst_batch.contains(&"build.liteinst_runtime_release".to_string())
        || !selected_cell_dependencies(true, "naked", "native", None).is_empty()
        || selected_cell_dependencies(true, "verify", "ptrace", None)
            != ["build.runtime_release".to_string()]
        || selected_cell_dependencies(true, "verify", "liteinst", None)
            != ["build.liteinst_runtime_release".to_string()]
    {
        return Err(
            "selected-cell dependencies lost the LiteInst positive/negative build bracket".into(),
        );
    }
    let probe = "space ' quote";
    let quoted = shell_quote(probe);
    let output = Command::new("bash")
        .args(["-c", &format!("printf '%s' {quoted}")])
        .output()
        .map_err(|e| format!("cannot run quoting bracket: {e}"))?;
    if output.stdout != probe.as_bytes() {
        return Err("shell quoting did not round-trip".into());
    }

    let exact_cell_command = "printf '125\\n' > harness-status; status=0; \
        env HERMIT_BIN=\"$PWD/target/release/hermit\" ./ci/test_harness.sh run \
        --include-manual --test fixture --mode verify \
        --results results.in-progress.jsonl --junit junit.in-progress.xml || status=$?; \
        if test -e results.in-progress.jsonl; then \
        mv -- results.in-progress.jsonl results.jsonl || status=$?; fi; \
        if test -e junit.in-progress.xml; then \
        mv -- junit.in-progress.xml junit.xml || status=$?; fi; \
        printf '%s\\n' \"$status\" > harness-status; exit \"$status\"";
    let fixture_json = json!({
        "resource_caps": {"manifest_guest": 1},
        "steps": [
            {
                "group": "cell",
                "job": "fixture",
                "cmd": exact_cell_command,
                "deps": [],
                "timeout": 20,
                "cpu_timeout": 40,
                "hint": {
                    "resources": {"manifest_guest": 1},
                    "hard_mem_max_bytes": 1024
                }
            },
            {
                "group": "pressure",
                "job": "summarize",
                "cmd": "true",
                "deps": ["cell.fixture"],
                "timeout": 10,
                "cpu_timeout": 10,
                "hint": {"hard_mem_max_bytes": 1024}
            }
        ]
    });
    let fixture_text = serde_json::to_string(&fixture_json)
        .map_err(|e| format!("cannot serialize generated-DAG fixture: {e}"))?;
    let fixture = dag_from_json(&fixture_text)
        .map_err(|e| format!("cannot parse generated-DAG fixture: {e}"))?;
    let fixture_timeouts = BTreeMap::from([("cell.fixture".to_string(), 20)]);
    audit_dag(&fixture, 1, 100, &fixture_timeouts)
        .map_err(|e| format!("positive generated-DAG bracket failed: {e}"))?;
    let fixture_round_trip = dag_from_json(&dag_to_json(&fixture))
        .map_err(|e| format!("cannot reparse generated-DAG fixture: {e}"))?;
    assert_plan_round_trip(&fixture, &fixture_round_trip)
        .map_err(|e| format!("positive generated-DAG round-trip bracket failed: {e}"))?;
    let mut missing_memory_cap = fixture.clone();
    missing_memory_cap.steps[0].hint.hard_mem_max_bytes = None;
    if audit_dag(&missing_memory_cap, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step without a hard memory cap was accepted".into());
    }
    let mut missing_cpu_cap = fixture.clone();
    missing_cpu_cap.steps[0].cpu_timeout = 0;
    if audit_dag(&missing_cpu_cap, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step without an explicit CPU cap was accepted".into());
    }
    let mut missing_resource_cap = fixture.clone();
    missing_resource_cap.resource_caps.remove("manifest_guest");
    if audit_dag(&missing_resource_cap, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step whose named resource has no capacity was accepted".into());
    }
    let mut ungrantable_resource = fixture.clone();
    ungrantable_resource
        .resource_caps
        .insert("manifest_guest".into(), 0);
    if audit_dag(&ungrantable_resource, 1, 100, &fixture_timeouts).is_ok() {
        return Err("step whose named resource demand exceeds capacity was accepted".into());
    }
    let mut widened_cell_timeout = fixture.clone();
    widened_cell_timeout.steps[0].timeout = 21;
    if audit_dag(&widened_cell_timeout, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell wall timeout wider than its selected cap was accepted".into());
    }
    let mut disabled_fixture = fixture.clone();
    disabled_fixture.steps[0].cmd = exact_cell_command
        .replace("--include-manual", "--probe-disabled")
        .replace("--test fixture", "--test fixture --backend kvm");
    audit_dag(&disabled_fixture, 1, 100, &fixture_timeouts)
        .map_err(|e| format!("positive disabled-cell bracket failed: {e}"))?;
    let mut prepared_fixture = fixture.clone();
    prepared_fixture.steps[0].cmd = "printf '125\\n' > '/results/cells/fixture/harness-status'; \
         if ! test \"$(cat '/results/prepare/fixture/status' 2>/dev/null)\" = 0; then \
         printf '126\\n' > '/results/cells/fixture/harness-status'; exit 0; fi; \
         status=0; env ./ci/test_harness.sh run --include-manual --prebuilt \
         --test fixture --mode verify --results results.in-progress.jsonl \
         --junit junit.in-progress.xml || status=$?; \
         mv -- results.in-progress.jsonl results.jsonl || status=$?; \
         printf '%s\\n' \"$status\" > harness-status; exit \"$status\""
        .into();
    audit_dag(&prepared_fixture, 1, 100, &fixture_timeouts)
        .map_err(|e| format!("positive preparation-refusal bracket failed: {e}"))?;
    prepared_fixture.steps[0].cmd = prepared_fixture.steps[0]
        .cmd
        .replace("printf '126", "printf '0");
    if audit_dag(&prepared_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("prebuilt cell without the preparation-failure refusal was accepted".into());
    }
    let mut nested_timeout_fixture = fixture.clone();
    nested_timeout_fixture.steps[0].cmd = exact_cell_command.replace(
        "env HERMIT_BIN",
        "timeout --kill-after=10s 20s env HERMIT_BIN",
    );
    if audit_dag(&nested_timeout_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell with a nested wall timeout was accepted".into());
    }
    let mut swallowed_failure_fixture = fixture.clone();
    swallowed_failure_fixture.steps[0].cmd =
        exact_cell_command.replace("exit \"$status\"", "exit 0");
    if audit_dag(&swallowed_failure_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell command that hid its terminal status was accepted".into());
    }
    let mut direct_result_fixture = fixture.clone();
    direct_result_fixture.steps[0].cmd = exact_cell_command
        .replace("results.in-progress.jsonl", "results.jsonl")
        .replace("mv --", "cp --");
    if audit_dag(&direct_result_fixture, 1, 100, &fixture_timeouts).is_ok() {
        return Err("cell command without terminal result publication was accepted".into());
    }
    let mut missing_exact_selector = fixture;
    missing_exact_selector.steps[0].cmd = exact_cell_command.replace("--mode verify", "");
    if audit_dag(&missing_exact_selector, 1, 100, &fixture_timeouts).is_ok() {
        return Err("negative generated-DAG bracket accepted a cell without an exact mode".into());
    }

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("self-test clock failure: {e}"))?
        .as_nanos();
    let scratch = env::temp_dir().join(format!(
        "hermit-pressure-self-test-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&scratch).map_err(|e| {
        format!(
            "cannot create self-test directory {}: {e}",
            scratch.display()
        )
    })?;
    let scratch_cleanup = SelfTestDirectory::new(scratch.clone());
    require_empty_result_dir(&scratch)?;
    fs::write(scratch.join("old-row"), "stale\n")
        .map_err(|e| format!("cannot write self-test stale row: {e}"))?;
    if require_empty_result_dir(&scratch).is_ok() {
        return Err("nonempty pressure result directory was accepted".into());
    }
    fs::remove_file(scratch.join("old-row"))
        .map_err(|e| format!("cannot remove self-test stale row: {e}"))?;

    let unfiltered = pressure_cells(root, &CellSelection::default())?;
    let unavailable_id = unfiltered
        .unavailable
        .first()
        .ok_or("self-test needs at least one red chaos cell without seeds")?
        .id
        .clone();
    if unavailable_id.mode != "chaos"
        || unfiltered
            .selected
            .iter()
            .any(|tracked| tracked.id == unavailable_id)
    {
        return Err(
            "a red chaos cell without seeds entered the executable pressure population".into(),
        );
    }
    let unavailable_selection = CellSelection {
        test: Some(unavailable_id.test.clone()),
        mode: Some(unavailable_id.mode.clone()),
        backend: Some(unavailable_id.backend.clone()),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let unavailable_error = pressure_cells(root, &unavailable_selection)
        .err()
        .ok_or("an exact no-seed chaos cell was accepted for execution")?;
    if !unavailable_error.contains("no chaos seeds") {
        return Err(format!(
            "exact no-seed chaos refusal lost its actionable explanation: {unavailable_error}"
        ));
    }
    let exact_id = unfiltered
        .selected
        .iter()
        .find(|tracked| tracked.id.backend == "ptrace" && tracked.id.mode == "verify")
        .or_else(|| unfiltered.selected.first())
        .ok_or("self-test needs at least one red compatibility cell")?
        .id
        .clone();
    let exact_selection = CellSelection {
        test: Some(exact_id.test.clone()),
        mode: Some(exact_id.mode.clone()),
        backend: Some(exact_id.backend.clone()),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let exact_results = scratch.join("exact-plan");
    let exact_metadata = write_plan(
        root,
        &exact_results,
        &exact_results.join("dag.json"),
        &exact_selection,
    )?;
    if exact_metadata.cells != [exact_id] {
        return Err("generated exact-cell plan did not retain exactly its requested cell".into());
    }

    let sample_selection = CellSelection {
        sample: Some(2),
        seed: Some(7),
        run_timeout_seconds: Some(PRESSURE_RUN_TIMEOUT_SECONDS),
        ..CellSelection::default()
    };
    let sample_results = scratch.join("sample-plan");
    let sample_metadata = write_plan(
        root,
        &sample_results,
        &sample_results.join("dag.json"),
        &sample_selection,
    )?;
    if sample_metadata.cells.len() != 2 {
        return Err("generated sampled plan did not retain its requested two cells".into());
    }

    let profile_dir = scratch.join("runner-profile");
    fs::create_dir(&profile_dir)
        .map_err(|e| format!("cannot create self-test profile directory: {e}"))?;
    fs::write(
        profile_dir.join("step_profiles_fixture.csv"),
        "git_sha,step,ok,timed_out,cpu_timed_out,oom_kills\n\
         abc,cell.pass,true,false,false,0\n\
         abc,cell.oom,false,false,false,2\n\
         abc,cell.timeout,false,true,false,0\n\
         abc,cell.runner-failed,false,false,false,0\n\
         abc,\"cell,quoted\",true,false,false,0\n\
         other,cell.foreign,true,false,false,0\n\
         other,cell.foreign-oom,false,false,false,9\n",
    )
    .map_err(|e| format!("cannot write self-test runner profile: {e}"))?;
    let retained = load_runner_evidence(&scratch, "abc")?;
    if !retained
        .get("cell.pass")
        .is_some_and(|row| row.seen && row.ok)
        || !retained.get("cell.oom").is_some_and(|row| row.oom)
        || !retained
            .get("cell.timeout")
            .is_some_and(|row| row.timed_out)
        || !retained
            .get("cell.runner-failed")
            .is_some_and(|row| row.seen && !row.ok && !row.oom && !row.timed_out)
        || !retained
            .get("cell,quoted")
            .is_some_and(|row| row.seen && row.ok)
        || retained.contains_key("cell.foreign")
        || retained.contains_key("cell.foreign-oom")
    {
        return Err("retained runner evidence did not preserve pass/OOM/timeout identity".into());
    }
    if !reason_reports_timeout(Some(
        "test fixture/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: deadline reached (exit 124))",
    )) || !reason_reports_timeout(Some(
        "test fixture/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: SIGKILL after 10 s grace (exit 137))",
    )) || reason_reports_timeout(Some("deadline reached (exit 124)"))
        || reason_reports_timeout(Some("guest timed out"))
        || reason_reports_timeout(Some("verify exited with status 1"))
    {
        return Err("timeout failure bucketing lost its positive or negative bracket".into());
    }
    let runner_ok = RunnerEvidence {
        seen: true,
        ok: true,
        timed_out: false,
        oom: false,
    };
    let runner_oom = RunnerEvidence {
        oom: true,
        ..runner_ok
    };
    let runner_timeout = RunnerEvidence {
        ok: false,
        timed_out: true,
        ..runner_ok
    };
    let runner_failed = RunnerEvidence {
        ok: false,
        ..runner_ok
    };
    if !is_proven_oom_attempt(runner_oom, Some(INCOMPLETE_ATTEMPT_STATUS))
        || is_proven_oom_attempt(runner_oom, None)
        || is_proven_oom_attempt(runner_ok, Some(INCOMPLETE_ATTEMPT_STATUS))
    {
        return Err(
            "OOM proof did not require both an exact runner OOM row and numeric attempt marker"
                .into(),
        );
    }
    if !is_proven_timeout_attempt(runner_timeout, Some(INCOMPLETE_ATTEMPT_STATUS))
        || is_proven_timeout_attempt(runner_timeout, Some(124))
        || is_proven_timeout_attempt(runner_timeout, None)
        || is_proven_timeout_attempt(runner_ok, Some(INCOMPLETE_ATTEMPT_STATUS))
    {
        return Err(
            "timeout proof did not require both an exact runner timeout row and the incomplete-attempt marker"
                .into(),
        );
    }
    if !runner_observed_terminal_attempt(runner_ok, Some(0))
        || !runner_observed_terminal_attempt(runner_failed, Some(1))
        || runner_observed_terminal_attempt(runner_ok, Some(INCOMPLETE_ATTEMPT_STATUS))
        || runner_observed_terminal_attempt(runner_ok, Some(PREPARATION_FAILED_STATUS))
        || runner_observed_terminal_attempt(runner_timeout, Some(1))
    {
        return Err(
            "terminal runner evidence accepted an incomplete, preparation-failed, or runner-killed attempt"
                .into(),
        );
    }
    let classifications = [
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("matched"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(1),
            "FAIL",
            true,
            None,
            "verify",
            Some("diverged"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(1),
            "FAIL",
            true,
            None,
            "replay",
            Some("diverged"),
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(1),
            "FAIL",
            true,
            None,
            "verify",
            Some("no_result"),
            true,
            true,
        ),
        classify_result(
            runner_timeout,
            Some(INCOMPLETE_ATTEMPT_STATUS),
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            true,
            true,
        ),
        classify_result(
            runner_timeout,
            Some(INCOMPLETE_ATTEMPT_STATUS),
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            false,
            false,
        ),
        classify_result(
            runner_failed,
            Some(1),
            "FAIL",
            true,
            Some(
                "test fixture/verify/ptrace exceeded 1 s in attempt 1 (innermost E2E timeout: deadline reached (exit 124))",
            ),
            "verify",
            Some("no_result"),
            true,
            true,
        ),
        classify_result(
            runner_failed,
            Some(124),
            "FAIL",
            true,
            None,
            "naked",
            None,
            false,
            true,
        ),
        classify_result(
            runner_oom,
            Some(137),
            "FAIL",
            false,
            None,
            "verify",
            None,
            true,
            true,
        ),
        classify_result(
            runner_oom,
            None,
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            false,
            true,
        ),
        classify_result(
            runner_oom,
            Some(INCOMPLETE_ATTEMPT_STATUS),
            "NO_RESULT",
            false,
            None,
            "verify",
            None,
            false,
            false,
        ),
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("no_result"),
            true,
            true,
        ),
        classify_result(
            RunnerEvidence::default(),
            Some(124),
            "FAIL",
            false,
            Some("timed out"),
            "verify",
            None,
            true,
            true,
        ),
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("matched"),
            false,
            true,
        ),
        classify_result(
            runner_ok,
            Some(0),
            "PASS",
            true,
            None,
            "verify",
            Some("matched"),
            true,
            false,
        ),
    ];
    if classifications
        != [
            "pass",
            "determinism-failure",
            "replay-failure",
            "crash-error",
            "timeout",
            "infrastructure-error",
            "timeout",
            "crash-error",
            "oom",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
            "infrastructure-error",
        ]
    {
        return Err(format!(
            "failure bucketing changed unexpectedly: {classifications:?}"
        ));
    }
    let sample_a = CellId {
        lane: "portable".into(),
        category: "sample".into(),
        test: "sample/a".into(),
        mode: "verify".into(),
        backend: "ptrace".into(),
    };
    let sample_b = CellId {
        test: "sample/b".into(),
        ..sample_a.clone()
    };
    if sample_score(&sample_a, 42) == sample_score(&sample_b, 42)
        || sample_score(&sample_a, 42) == sample_score(&sample_a, 43)
    {
        return Err("seeded cell sampling lost its identity or seed sensitivity".into());
    }

    let sample_slug = base_cell_slug(&sample_a);
    let sample_metadata = RunMetadata {
        schema: RUN_SCHEMA,
        hermit_sha: "abc".into(),
        detcore_tree: "def".into(),
        source_tree_dirty: false,
        run_timeout_seconds: 60,
        mode: Some(sample_a.mode.clone()),
        test: Some(sample_a.test.clone()),
        backend: Some(sample_a.backend.clone()),
        cell_timeout_seconds: Some(20),
        sample: None,
        seed: None,
        unavailable_cells: 0,
        repetitions: None,
        cells: vec![sample_a.clone()],
    };
    let mut result_row = ResultRow {
        schema: 3,
        run_id: sample_slug.clone(),
        hermit_sha: sample_metadata.hermit_sha.clone(),
        source_tree_dirty: false,
        test: sample_a.test.clone(),
        category: sample_a.category.clone(),
        lane: sample_a.lane.clone(),
        mode: sample_a.mode.clone(),
        backend: Some(sample_a.backend.clone()),
        classification: "required".into(),
        outcome: "FAIL".into(),
        reason: None,
        error_kind: None,
    };
    if !result_row_matches_cell(
        &result_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("matching retained result-row identity was refused".into());
    }
    result_row.hermit_sha = "foreign".into();
    if result_row_matches_cell(
        &result_row,
        &sample_slug,
        &sample_metadata,
        &sample_a,
        true,
        Some(INCOMPLETE_ATTEMPT_STATUS),
    ) {
        return Err("foreign retained result-row identity was accepted".into());
    }

    if !retained_verification_logs(&scratch, &sample_a, &sample_slug)?.is_empty() {
        return Err("missing verify-log directory produced retained logs".into());
    }
    let verification_path = verification_report_path(&scratch, &sample_a, &sample_slug);
    let verification_directory = verification_path
        .parent()
        .expect("verification path has parent");
    let verify_log_directory = verification_directory.join("verify-logs").join("verify-1");
    fs::create_dir_all(&verify_log_directory)
        .map_err(|e| format!("cannot create verify-log self-test directory: {e}"))?;
    let run1_log = verify_log_directory.join("run1_log_fixture.log");
    let run2_log = verify_log_directory.join("run2_log_fixture.log");
    fs::write(&run1_log, "run one\n")
        .map_err(|e| format!("cannot write run1 verify-log fixture: {e}"))?;
    if retained_verification_logs(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("retained verify-log evidence accepted a missing run2 capture".into());
    }
    fs::write(&run2_log, "run two\n")
        .map_err(|e| format!("cannot write run2 verify-log fixture: {e}"))?;
    if retained_verification_logs(&scratch, &sample_a, &sample_slug)?.len() != 2 {
        return Err("one nonempty run1/run2 verify-log pair was refused".into());
    }
    let duplicate_run1 = verify_log_directory.join("run1_log_duplicate.log");
    fs::write(&duplicate_run1, "duplicate\n")
        .map_err(|e| format!("cannot write duplicate run1 fixture: {e}"))?;
    if retained_verification_logs(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("duplicate retained run1 verify-log capture was accepted".into());
    }
    fs::remove_file(&duplicate_run1)
        .map_err(|e| format!("cannot remove duplicate run1 fixture: {e}"))?;
    fs::write(&run2_log, "").map_err(|e| format!("cannot empty run2 verify-log fixture: {e}"))?;
    if retained_verification_logs(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("empty retained run2 verify-log capture was accepted".into());
    }
    fs::write(&run2_log, "run two\n")
        .map_err(|e| format!("cannot restore run2 verify-log fixture: {e}"))?;

    let golden_status = verify_log_directory.join("normalized-ptrace-golden.status");
    let golden_log = verify_log_directory.join("normalized-ptrace-golden.log");
    if normalized_ptrace_golden(&scratch, &sample_a, &sample_slug)?.is_some() {
        return Err("absent normalized ptrace golden produced an artifact".into());
    }
    fs::write(&golden_log, "canonical INFO\n")
        .map_err(|e| format!("cannot write normalized golden fixture: {e}"))?;
    if normalized_ptrace_golden(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("normalized ptrace golden without status was accepted".into());
    }
    fs::remove_file(&golden_log)
        .map_err(|e| format!("cannot remove normalized golden fixture: {e}"))?;
    fs::write(&golden_status, "0\n")
        .map_err(|e| format!("cannot write normalized golden status: {e}"))?;
    if normalized_ptrace_golden(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("normalized ptrace golden status without output was accepted".into());
    }
    fs::write(&golden_log, "canonical INFO\n")
        .map_err(|e| format!("cannot restore normalized golden fixture: {e}"))?;
    if normalized_ptrace_golden(&scratch, &sample_a, &sample_slug)?.is_none() {
        return Err("complete normalized ptrace golden output/status pair was refused".into());
    }
    fs::write(&golden_status, "not-a-status\n")
        .map_err(|e| format!("cannot mutate normalized golden status: {e}"))?;
    if normalized_ptrace_golden(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("nonnumeric normalized ptrace golden status was accepted".into());
    }

    fs::write(&verification_path, "{")
        .map_err(|e| format!("cannot write malformed verification fixture: {e}"))?;
    if read_verification_report(&scratch, &sample_a, &sample_slug).is_ok() {
        return Err("malformed existing verification report was accepted".into());
    }
    fs::remove_file(&verification_path)
        .map_err(|e| format!("cannot remove malformed verification fixture: {e}"))?;

    let invalid_profile = profile_dir.join("step_profiles_invalid.csv");
    fs::write(
        &invalid_profile,
        "git_sha,step,ok,timed_out,cpu_timed_out,oom_kills\nabc,cell.bad,Maybe,False,False,0\n",
    )
    .map_err(|e| format!("cannot write invalid self-test runner profile: {e}"))?;
    if load_runner_evidence(&scratch, "abc").is_ok() {
        return Err("malformed retained runner evidence was accepted".into());
    }
    scratch_cleanup.remove()?;
    println!(
        "compatibility pressure-test self-test: selection, manifest attempt budgets, build closure, sampling, timeout/OOM classification, generated-DAG, fresh-result, cleanup, retained-runner/result identity, verify-log, and normalized-golden brackets pass"
    );
    Ok(())
}
