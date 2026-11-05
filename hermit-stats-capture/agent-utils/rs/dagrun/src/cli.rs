//! Command-line interface for running, inspecting, and visualizing DAGs.

// Command-line interface for dagrun.
//
// Subcommands (matching `py/dagrun/cli.py`):
//   run --dag FILE    run a DAG (exit 0 iff every step passes)
//   list --dag FILE   list the steps
//   ascii --dag FILE  draw the DAG as ASCII art
//   dot --dag FILE    emit Graphviz DOT
//   json --dag FILE   re-emit the DAG as canonical JSON
//   yaml --dag FILE   re-emit the DAG as YAML
//   quickstart        print a self-contained getting-started guide
//   --userguide       print the full embedded user guide (the complete reference)
//
// `--dag FILE` auto-detects the input format by extension: `.yaml`/`.yml` load as YAML (which is
// ISOMORPHIC to the JSON schema — same model), everything else as JSON. `--dag -` reads JSON from
// stdin.
//
// `list`, `ascii`, `dot`, and `json` stdout is BYTE-IDENTICAL to the Python build; `--help`,
// `quickstart`, and `yaml` wording may differ (YAML byte-output is not cross-identical, only YAML
// *loading* is) but the structure and exit codes (0 / 1 / 2 / 3) match.
//
// Cgroup boxing is ON by default (this tool's primary purpose): `run` re-execs inside a
// transient `systemd-run --user --scope` and caps each step in its own child cgroup. When
// cgroup-v2 + a working systemd `--user` scope are unavailable the run ERRORS (exit 3);
// `--allow-cgroup-failure` downgrades to a best-effort UNBOXED run with a visible warning.
// `--perf-dir DIR` writes per-step + whole-run resource-usage CSVs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::capabilities::enforcement_manifest;
use crate::cgroup::{
    aggregate_slice_max_cpus, apply_specific_cores, attempt_scope_reexec, delegated_cgroup_root,
    delegated_resource_limits, enable_outer_oom_group, expected_outer_cpu_count,
    expected_outer_memory_max_bytes, expected_scope_runtime_max_s, install_scope_teardown,
    is_in_scope, nested_coordinator_restore_failures, observe_own_containment,
    outer_memory_max_bytes_capped, verify_scope_limits, verify_scope_runtime_max, CgroupManager,
    Cgroups, DelegatedResourceLimits, ScopeAttempt, DELEGATED_CGROUP_ENV, DELEGATED_UNBOXED_ENV,
    DIRECT_CGROUP_ENV, FORCE_ATTEMPT_ENV,
};
use crate::estimates::{
    apply_plan_to_config, build_plan_with_max_steps, feedback_identity,
    load_step_samples_for_workloads, load_step_speedups_for_workloads, plan_to_json, plan_to_text,
    step_speedups_from_buckets, write_scaling_model_for_workloads, BucketKey, Plan, Planner,
    Sample, StepSpeedup, DEFAULT_MIN_SAMPLES,
};
use crate::io::{dag_from_json, dag_from_path, dag_to_json, dag_to_yaml, DagJsonError};
use crate::memory_feedback::{
    apply_memory_admissions, load_memory_admissions, memory_admission_line, DEFAULT_MARGIN_PCT,
    DEFAULT_MIN_UNCENSORED_SAMPLES,
};
use crate::model::{
    step_classification, step_width_is_resizable, DagConfig, ResourceHint, Step, StepOutcome,
    DEFAULT_STEP_TIMEOUT, JOBS_ENV_ENV,
};
use crate::perflog::{append_step_profiles, append_step_timeseries, child_cpu_seconds, PerfWindow};
use crate::profile_capture::{
    capture_at_sweet_spot, select_capture_sweet_spot, CaptureConfig, IsolatedTrialRequest,
    IsolatedTrialResult, RunIsolatedTrial, SweetSpotSelection,
};
use crate::profile_enrich::container_core_budget;
use crate::profile_report;
use crate::scheduler::{
    cap_config_max_cpus, nested_run_refusal,
    run_dag_boxed_deadline_limited_with_resource_caps_and_timeseries,
    run_dag_boxed_limited_with_timeseries, validate_max_cpus_rewrite, BoxedCgroups,
};
use crate::sizing::{
    box_mem_budget_bytes, cpu_count, jobs_for_budget, parse_size,
    step_mem_cap_for_inner_jobs_optional, stress_control_floor_bytes, stress_copy_footprint_bytes,
    transitive_deps,
};
use crate::summary::{self, Summary, DEFAULT_MAX_BUCKETS, DEFAULT_RESERVOIR_K};
use crate::sweep::{
    coarse_widths, cumulative_width_grid, limit_topology, machine_topology, parse_target_duration,
    parse_widths, stable_topological_order, workload_digest, MachineTopology,
};
use crate::sync::{self, SyncBackend};
use crate::viz::{to_ascii, to_ascii_groups, to_dot, to_dot_groups, GROUP_BY_KEYS};
use crate::{PROG, VERSION};

/// Environment variable overriding the default profile-store location (Feature D). An explicit
/// `--perf-dir` still wins over this; `--no-profile` disables logging entirely.
const PROFILE_DIR_ENV: &str = "DAGRUN_PROFILE_DIR";

/// Default profile-store directory, RELATIVE TO THE CURRENT WORKING DIRECTORY, used when neither
/// `--perf-dir` nor `$DAGRUN_PROFILE_DIR` is set and `--no-profile` is absent. Created on
/// demand; runs and sweeps auto-append here so profiling data lands somewhere obvious.
const DEFAULT_PROFILE_DIR: &str = ".dagrun/profiles";

const CGROUP_SETUP_ENVIRONMENT_ERROR: &str = "ENVIRONMENT: managed cgroup scope could not \
quiesce and delegate per-step controllers; no DAG node started and no product build started";

/// The complete user guide embedded in the executable.
const USERGUIDE: &str = include_str!("embedded_userguide.md");

// --------------------------------------------------------------------------- colors

/// Minimal ANSI colorizer. Disabled for non-ttys and when `NO_COLOR` is set.
struct Palette {
    enabled: bool,
}

impl Palette {
    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\u{1b}[{code}m{text}\u{1b}[0m")
        } else {
            text.to_string()
        }
    }
    fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    fn green(&self, t: &str) -> String {
        self.wrap("32", t)
    }
    fn red(&self, t: &str) -> String {
        self.wrap("1;31", t)
    }
    fn yellow(&self, t: &str) -> String {
        self.wrap("33", t)
    }
    fn cyan(&self, t: &str) -> String {
        self.wrap("36", t)
    }
}

fn color_enabled() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

fn banner(c: &Palette) -> String {
    format!(
        "{} {}\n\
         Run a DAG of build/test steps concurrently and safely: dependency- and\n\
         resource-aware scheduling with eager-exit on first failure, memory-aware\n\
         concurrency, and DAG visualization.",
        c.bold(PROG),
        c.dim(&format!("v{VERSION}"))
    )
}

fn help_text(c: &Palette) -> String {
    let ex = |s: &str| c.cyan(s);
    format!(
        "{banner}\n\n\
         {usage}\n  {PROG} <command> [options]\n\n\
         {commands}\n\
         \x20 run        run a DAG (exit 0 iff every step passes)\n\
         \x20 box        box ONE command with --mem/--timeout/--cores (no DAG file)\n\
         \x20 sweep      target-time parallel-scaling sweep of one step or a whole DAG\n\
         \x20 plan       show learned estimates + the scheduled order (does NOT run anything)\n\
         \x20 list       list the steps\n\
         \x20 ascii      draw the DAG as ASCII art\n\
         \x20 dot        emit Graphviz DOT (pipe to `dot -Tsvg`)\n\
         \x20 json       re-emit the DAG as canonical JSON\n\
         \x20 yaml       re-emit the DAG as YAML\n\
         \x20 summary    inspect/build/merge portable profile summaries\n\
         \x20 pin-run    reserve collision-free cores and run one command\n\
         \x20 quickstart print a self-contained getting-started guide\n\
         \x20 capabilities print the enforcement-capability manifest\n\
         \x20 --userguide print the full embedded user guide (the complete reference)\n\n\
         {examples}\n\
         \x20 {e1}\n\
         \x20 {e2}\n\
         \x20 {e3}\n\
         \x20 {e6}\n\
         \x20 {e4}\n\
         \x20 {e5}\n\n\
         {profiling}\n",
        banner = banner(c),
        usage = c.bold("usage"),
        commands = c.bold("commands"),
        examples = c.bold("examples"),
        e1 = ex(&format!("{PROG} quickstart")),
        e2 = ex(&format!(
            "{PROG} box --mem 512M --timeout 30 --cores 2 -- ./probe.sh"
        )),
        e3 = ex(&format!("{PROG} run --dag dag.json --selected build.app")),
        e6 = ex(&format!(
            "{PROG} plan --dag dag.json --planner critical-path"
        )),
        e4 = ex(&format!(
            "{PROG} sweep --dag dag.json --step build.app --jobs 1..8"
        )),
        e5 = ex(&format!("{PROG} ascii --dag dag.json")),
        profiling = c.dim(
            "Profiling data auto-logs to ./.dagrun/profiles/ by default\n \
             (sweep: --output-dir, with --perf-dir as an alias; other commands: --perf-dir; \
             $DAGRUN_PROFILE_DIR also overrides; --no-profile disables)."
        ),
    )
}

fn quickstart(c: &Palette) -> String {
    let h = |s: &str| c.bold(s);
    let k = |s: &str| c.cyan(s);
    format!(
        "{banner}\n\n\
{i1}\n  cargo install dagrun\n  \
dagrun --help\n\n\
{i2}  {deps_note}\n  Save as dag.json:\n  {{\n    \"resource_caps\": {{\"browser\": 1}},\n    \"steps\": [\n      \
{{\"group\": \"build\", \"job\": \"app\", \"desc\": \"compile\", \"cmd\": \"echo build && sleep 0.2\"}},\n      \
{{\"group\": \"test\",  \"job\": \"unit\", \"desc\": \"unit tests\", \"cmd\": \"echo test && sleep 0.2\",\n        \"deps\": [\"build.app\"]}},\n      \
{{\"group\": \"e2e\",   \"job\": \"smoke\", \"desc\": \"browser smoke\", \"cmd\": \"echo e2e && sleep 0.2\",\n        \"deps\": [\"build.app\"], \"hint\": {{\"resources\": {{\"browser\": 1}}}}}}\n    ]\n  }}\n\n\
{i3}\n  {r1}\n  {r2}\n  {r3}\n\n\
{studies}\n  {s1}\n  {s2}\n  {s3}\n  {studies_note}\n\n\
{store}\n  {store_dir}\n  {store_note}\n\n\
{planning}\n  {pl1}\n  {pl2}\n  {pl3}\n  {plan_note}\n\n\
{schema}  {schema_note}\n  \
step:   group, job, desc, description, labels[], cmd, cmdtype, manifest{{lane,category,test?,mode?,backend?}}, result_manifests[], deps[], env{{}}, timeout, jobs_flag, jobs_env, networkonly, engine_only, delegated_children, hint{{}}\n  \
hint:   resources{{name:int}}, est_duration_s, rss_baseline_bytes, hard_mem_max_bytes,\n          classification(\"cpu-bound\"|\"latency-bound\"|\"light\"), preferred_inner_jobs\n  \
top:    include[{{path,namespace,after[]?}}], description, resource_caps{{name:int}}, mem_cap_factor, mem_cap_floor_bytes,\n          outer_mem_safety_factor, default_step_timeout, default_jobs_flag, default_jobs_env\n  \
desc = short label; description = long-form docs (often multi-line, great in YAML)\n  \
cmdtype: unknown (default) | make | cargo-build | cargo-test | cargo-nextest |\n          generic-dash-j-command | generic-with-flag; known types append width arguments or set\n          DAGRUN_EXTRA_ARGS for an unquoted placement in a compound command\n  \
jobs_flag: appended with a step preferred_inner_jobs; \"-j\"->\"-j 4\", \"-j%d\"->\"-j4\", \"--jobs=\"->\"--jobs=4\"\n  \
          empty/whitespace means a fixed self-managed width: it cannot be rewritten or swept\n  \
jobs_env: environment variable receiving the same admitted width; absent inherits default_jobs_env\n  \
          (normally resolved from ${JOBS_ENV_ENV})\n  \
yaml: --dag also accepts .yaml/.yml (isomorphic to JSON; allows comments + multi-line block-scalar descriptions); the `yaml` subcommand emits YAML\n  \
include: load-time namespaced file composition; optional after[] wires outer prerequisites\n\n\
delegated_children: opt into child dagrun; preserves cgroups or names an unboxed fallback\n\n\
{what}\n  \
- concurrent scheduling honoring deps + resource caps, ordered by the chosen --planner\n  \
  ({maxsteps} bounds active DAG steps; {maxcpus} caps each width + outer CPU bandwidth)\n  \
- learned est_duration / rss from the profile store override the DAG hints at plan time\n    (disable with --no-profile-feedback; inspect with the plan subcommand / --show-plan)\n  \
- a failing step fails the run (exit 1) and, by default, eager-cancels in-flight steps\n    ({keepgoing} continues launching independent ready steps; failed dependents are skipped)\n  \
- {maxmem} derives a conservative model-based active-step ceiling from RAM hints\n\n\
{note}  cgroup-v2 per-step boxing is ON by default; {acf} downgrades to a best-effort\n        unboxed run. Sweeps use {outputdir} ({perfalias} is an alias); other commands use\n        {perfdir} for profiling artifacts.\n\n\
{exits}  0 = all steps passed | 1 = a step failed | 2 = bad usage / bad DAG file | 3 = cgroup\n           boxing required but unavailable (use {acf})\n",
        banner = banner(c),
        i1 = h("1. Install"),
        i2 = h("2. Write a DAG (JSON)"),
        deps_note = c.dim("- a list of steps; each depends on others by \"group.job\" tag"),
        i3 = h("3. Look at it, then run it"),
        r1 = k(&format!("{PROG} list  --dag dag.json")),
        r2 = k(&format!("{PROG} ascii --dag dag.json")),
        r3 = k(&format!("{PROG} run   --dag dag.json")),
        studies = h("Profile & experiment with individual steps"),
        s1 = k(&format!("{PROG} run   --dag dag.json --profile        # per-step timing/memory table after the run")),
        s2 = k(&format!("{PROG} run   --dag dag.json --selected test.unit # run that step and all of its dependencies")),
        s3 = k(&format!("{PROG} sweep --dag dag.json --step build.app --jobs 1..8  # -j1..-j8 speedup table")),
        studies_note = c.dim(
            "--selected includes every dependency needed by the named steps. Add \
             --ignore-selected-deps only when those inputs are already present and must not run. \
             sweep passes each width through cmdtype, jobs_flag, and/or jobs_env and reports wall/user/sys/rss + speedup."
        ),
        store = h("Where profiling data lands (by default)"),
        store_dir = k("./.dagrun/profiles/   (created on demand, relative to CWD)"),
        store_note = c.dim(
            "Every run and sweep AUTO-LOGS resource-usage CSVs here; sweep overrides with \
             --output-dir (--perf-dir alias), while other commands use --perf-dir; \
             $DAGRUN_PROFILE_DIR is the shared fallback. Disable with --no-profile. The tool prints where it \
             appended (never silent). Consider gitignoring ./.dagrun/."
        ),
        planning = h("Smarter planning: learned estimates + the critical-path planner"),
        pl1 = k(&format!(
            "{PROG} plan  --dag dag.json                        # show est (+ source), rss, bottom-level, order"
        )),
        pl2 = k(&format!(
            "{PROG} plan  --dag dag.json --planner critical-path # order by longest remaining path"
        )),
        pl3 = k(&format!(
            "{PROG} run   --dag dag.json --planner critical-path --show-plan  # print the plan, then run it"
        )),
        plan_note = c.dim(
            // Hand-wrapped to ~90 columns (matching the Python quickstart) so the note reads
            // cleanly on a narrow terminal instead of as one long unbroken line. Each `\n  `
            // starts a new 2-space-indented line; the trailing `\` then eats the source newline.
            "The runner FEEDS the profile store back at plan time: a robust est_duration\n  \
             (contention-discounted MEDIAN of past elapsed_s) and an rss estimate (high\n  \
             percentile of past peak_bytes) OVERRIDE the DAG hint once enough samples exist,\n  \
             so est_duration_s need not be hand-authored and --max-mem sizing improves\n  \
             automatically. --planner greedy-lpt (default) launches the longest single step\n  \
             first; critical-path launches the highest bottom-level (longest remaining\n  \
             est-weighted path) first. When the store holds multiple inner-jobs widths for a\n  \
             step, plan/--show-plan also print a parallel-speedup section: the recommended\n  \
             inner_jobs (best wall before the diminishing-returns knee + within the core\n  \
             budget), achieved effective_cores, and the speedup curve.\n  \
             --no-profile-feedback ignores the store (DAG hints only)."
        ),
        schema = h("DAG schema"),
        schema_note = c.dim("(only group/job/cmd are required per step; everything else has defaults)"),
        what = h("What you get"),
        keepgoing = k("--keep-going"),
        maxsteps = k("--max-steps"),
        maxcpus = k("--max-cpus"),
        maxmem = k("run --max-mem 8G"),
        acf = k("--allow-cgroup-failure"),
        outputdir = k("sweep --output-dir DIR"),
        perfalias = k("--perf-dir"),
        perfdir = k("run --perf-dir DIR"),
        note = h("Note"),
        exits = h("Exit codes"),
    )
}

// --------------------------------------------------------------------------- per-subcommand help

/// Does this argument list request help (`-h`/`--help`)? The dispatcher checks this at the START of
/// each subcommand — before argument parsing — so `--help` prints a usage page and exits 0 instead of
/// tripping the unknown-argument arm.
fn wants_help(rest: &[String]) -> bool {
    rest.iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| argument == "-h" || argument == "--help")
}

fn pin_wants_help(rest: &[String]) -> bool {
    let mut index = 0;
    while index < rest.len() {
        let argument = &rest[index];
        if argument == "--" {
            return false;
        }
        if argument == "-h" || argument == "--help" {
            return true;
        }
        if argument == "--cores" || argument == "--tag" {
            index += 2;
            continue;
        }
        if argument.starts_with("--cores=") || argument.starts_with("--tag=") {
            index += 1;
            continue;
        }
        return false;
    }
    false
}

// Render a per-subcommand help page: a usage line, a one-line summary, then each accepted flag with
// a ONE-LINE description. Mirrors the Python argparse per-subcommand help.
fn render_subcommand_help(
    c: &Palette,
    usage: &str,
    summary: &str,
    flags: &[(&str, &str)],
) -> String {
    let width = flags.iter().map(|(f, _)| f.len()).max().unwrap_or(0);
    let mut out = format!(
        "{} {PROG} {usage}\n\n{summary}\n\n{}\n",
        c.bold("usage"),
        c.bold("options")
    );
    for (flag, desc) in flags {
        let padded = format!("{flag:<width$}");
        let mut lines = desc.lines();
        out.push_str(&format!(
            "  {}  {}\n",
            c.cyan(&padded),
            lines.next().unwrap_or_default()
        ));
        for line in lines {
            out.push_str(&format!("  {:width$}  {line}\n", ""));
        }
    }
    out
}

fn run_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "run --dag FILE [options]",
        "Run a DAG (exit 0 iff every step passes). Boxed per-step with cgroup-v2 by default.",
        &[
            (
                "--dag FILE",
                "DAG file to run; .yaml/.yml load as YAML, else JSON; files may use namespaced include fragments ('-' = self-contained JSON stdin only) [required]",
            ),
            (
                "-s, --max-steps N",
                "maximum active DAG steps; defaults to the effective --max-cpus budget; a bare -sN also works",
            ),
            (
                "-j, --max-cpus N",
                "outer CPU-bandwidth limit and maximum width of any one runner-controlled step (default: effective container/affinity budget tightened by the shared 90% slice); a nested run is clamped to its delegated parent's effective CPU envelope; a bare -jN also works",
            ),
            (
                "--cores/--cpuset/--pin K",
                "hard CPU PINNING, opt-in: reserve K least-busy free cores and require an exact cgroup cpuset; fail closed when unavailable; a nested request wider than its delegated parent's CPU envelope is refused",
            ),
            (
                "--max-mem SPEC",
                "RAM budget (e.g. 8G): becomes the outer scope's MemoryMax (it can tighten the derived host boundary, never widen it) and derives a conservative model-based --max-steps ceiling; with explicit --max-steps, the tighter value wins. A nested run inherits its delegated parent's finite ceiling as the default and upper bound, enforced at the nested aggregate root",
            ),
            (
                "--selected TAG[,TAG...]",
                "run the named step(s) and every dependency they require",
            ),
            (
                "--labels LABEL[,LABEL...]",
                "run every step carrying any named label and every dependency they require; labels are independent of group.job tags",
            ),
            (
                "--ignore-selected-deps",
                "valid only with --selected: run only the named steps and drop dependency edges outside the selection",
            ),
            (
                "--args STRING",
                "replace the opt-in {args} token in selected step commands",
            ),
            (
                "--stress N",
                "duplicate the graph into N disconnected components; --max-steps controls active copies, --max-cpus caps each width/shared bandwidth, and expansion is limited to 100,000 generated nodes",
            ),
            (
                "--perf-dir DIR",
                "write per-step + whole-run resource-usage CSVs into DIR",
            ),
            (
                "--profile-timeseries DURATION",
                "opt-in cgroup CPU/thread trace interval (50ms..10s); writes traces/<run_id>.csv",
            ),
            (
                "--no-profile",
                "disable the default auto-logging profile store for this run",
            ),
            (
                "--profile",
                "after the run, print a per-step profile (timing/memory) table",
            ),
            (
                "--planner NAME",
                "dispatch-ordering planner: greedy-lpt (default) | critical-path | cpa",
            ),
            ("--show-plan", "before running, print the scheduled plan"),
            (
                "--no-profile-feedback",
                "do NOT read the profile store to refine time/RAM estimates",
            ),
            (
                "--profile-memory-feedback",
                "OPT-IN: derive rss_baseline_bytes from the store's UNCENSORED peaks only",
            ),
            (
                "--profile-sync BACKEND",
                "download+upload the shared profile summary (for ephemeral CI)",
            ),
            (
                "--profile-sync-direction D",
                "both (default) | download | upload",
            ),
            (
                "-k, --keep-going",
                "after failure, continue independent work; skip failed dependents",
            ),
            (
                "--no-color",
                "suppress ANSI colour in failure output. The searchable DAGRUN STEP ERROR marker still prints: it is what a plain log is grepped for. Colour is also off automatically when NO_COLOR is set or stdout is not a terminal",
            ),
            (
                "--run-timeout SECONDS",
                "OUTER wall budget for the WHOLE run; cuts in-flight steps and still reports",
            ),
            (
                "--admission [WAIT_S]",
                "HOST-WIDE memory admission (opt-in): reserve --max-mem, or the finite inherited delegated memory ceiling, against a durable ledger every runner on the host shares. GRANT / QUEUE (says how many holders are ahead) / REFUSE (says the number to ask for). WAIT_S = how long to wait while queued (default 0 = report and exit 4; at most 86400)",
            ),
            (
                "--resource-caps-path FILE",
                "make resource_caps apply across runner processes through this existing state file; otherwise caps are process-local. $DAGRUN_RESOURCE_CAPS_PATH is the secondary route and the flag wins",
            ),
            (
                "cmdtype (DAG field)",
                "unknown (default) | make | cargo-build | cargo-test | cargo-nextest | generic-dash-j-command | generic-with-flag; known types append width arguments unless cmd places unquoted $DAGRUN_EXTRA_ARGS or ${DAGRUN_EXTRA_ARGS}",
            ),
            (
                "--allow-cgroup-failure",
                "if cgroup boxing is unavailable, run UNBOXED with a warning instead of erroring",
            ),
            (
                "--unsafe-no-cgroups",
                "DELIBERATELY skip cgroup boxing entirely (unsafe)",
            ),
            (
                "--allow-unwise-nest-dagruns",
                "allow a reviewed temporary nested dagrun exception; flatten the caller instead",
            ),
            (
                "--small-default-cap",
                "compatibility no-op (small caps are already on by default)",
            ),
            (
                "--cpu-timeout-multiplier FACTOR",
                "scale every step's canonical cpu_timeout by FACTOR on THIS platform \
                 (default 1.0 = no scaling); also $DAGRUN_CPU_TIMEOUT_MULTIPLIER",
            ),
            ("-v", "stream child output (repeatable)"),
            ("-q, --quiet", "quieter output"),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn sweep_help(c: &Palette) -> String {
    let summary = r#"Measure each selected step in isolation across inner-worker widths.

Two modes:
  Single-step dense mode (no --target-time): --step and --jobs are required.
  Target-time graph mode: visit selected nodes one at a time in stable topological
  order. Pass 1 always completes. A later pass starts only while time remains,
  and every started pass finishes even if that overruns the allowance.

Dependencies determine the visitation order; they are not run beside the measured
step. Each width is delivered through cmdtype, jobs_flag, and/or jobs_env.

Automatic widths (target-time mode with no --jobs):
  Pass 1 uses powers of two through the physical-core count, then the exact
  physical-core and logical-thread counts, limited by CPU affinity/cgroup quota.
  Example: 158 physical cores / 316 threads ->
  1,2,4,8,16,32,64,128,158,316.
  Later passes add integer midpoints of remaining gaps (for example, 48 between
  32 and 64) and rerun every width in the cumulative grid.

Output:
  Progress and speedup tables remain on the terminal. Profiling artifacts append
  to ./.dagrun/profiles/ by default, including a standalone interactive
  profile_report.html. --output-dir changes only that artifact-store location;
  it does not redirect terminal or child-process output.

Expensive profiler captures are opt-in and run only after the ordinary sweep has
selected an economic plateau width. They are fresh isolated executions at that
width and never enter the scaling model. In target-time mode they run after pass
1; their elapsed time counts against the allowance before refinement is considered."#;
    render_subcommand_help(
        c,
        "sweep --dag FILE [--target-time DURATION] [--step TAG] [--jobs WIDTHS] [options]",
        summary,
        &[
            (
                "--dag FILE",
                "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced include fragments ('-' = self-contained JSON stdin only) [required]",
            ),
            (
                "--step TAG",
                "measure only this group.job step; required without --target-time, optional otherwise",
            ),
            (
                "--jobs WIDTHS",
                "initial inner-parallelism widths: bare N means every width 1..N; LO..HI is an inclusive dense range.\nTarget mode also accepts sparse comma/range lists such as 1,2,4,8 or 1,2,4..8.\nOmit only in target mode to use the topology grid described above. Delegated sweeps refuse explicit widths beyond the inherited CPU envelope.",
            ),
            (
                "--target-time DURATION",
                "select graph-wide multi-pass mode with this soft wall-time allowance (bare seconds or ms/s/m/h suffix).\nPass 1 and every started pass finish; this is not a timeout.",
            ),
            (
                "--repeat K",
                "run every width K times; persist every sample and show the fastest wall time\nin the terminal table (default: 1)",
            ),
            (
                "--output-dir DIR",
                "sweep artifact directory: append whole-run and per-step CSVs, refresh\nscaling_model_*.json and profile_report.html after success, and place opt-in traces\nand profiler captures below DIR. Overrides $DAGRUN_PROFILE_DIR and ./.dagrun/profiles/.",
            ),
            (
                "--perf-dir DIR",
                "backward-compatible alias for --output-dir; it does not mean perf(1)\nand does not redirect terminal or child-process output",
            ),
            (
                "--report-html FILE",
                "write the standalone interactive report to FILE instead of\n<output-dir>/profile_report.html; failure at an explicitly requested FILE makes the sweep\nunsuccessful after preserving its measurements",
            ),
            (
                "--no-report",
                "do not generate the interactive HTML report after this sweep",
            ),
            (
                "--profile-timeseries DURATION",
                "OPT-IN: sample each trial's cgroup CPU/thread activity every DURATION\n(50ms..10s); requires active cgroup-v2 and writes traces/<run_id>.csv in the profile store",
            ),
            (
                "--perf-record",
                "OPT-IN: after the uninstrumented sweep, run one separate perf-record trial\nat each selected step's economic plateau width",
            ),
            (
                "--perf-window DURATION",
                "record only a centred perf window of this duration; implies --perf-record.\nWithout it, perf records the centred 80% of the expected trial wall time",
            ),
            (
                "--wprof-window DURATION",
                "OPT-IN, repeatable: run one separate centred wprof capture per occurrence.\nRepeated occurrences currently must use the same positive duration",
            ),
            (
                "--profiler-sudo",
                "explicitly prefix perf/wprof with 'sudo -n'; privilege escalation is never automatic",
            ),
            (
                "--no-profile",
                "write no local profile CSVs, model, report, time-series trace, or profiler capture",
            ),
            (
                "--allow-cgroup-failure",
                "run UNBOXED with a warning when cgroup-v2 is unavailable; cgroup CPU, memory,\nthrottling, and time-series evidence will be unavailable",
            ),
            (
                "--unsafe-no-cgroups",
                "DELIBERATELY run unboxed even when cgroup-v2 is available; profiling is less complete\nand --profile-timeseries cannot be used",
            ),
            (
                "-v",
                "-v shows each trial's captured final line; -vv streams child stdout/stderr",
            ),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn plan_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "plan --dag FILE [options]",
        "Show learned estimates + the scheduled order. Does NOT run anything.",
        &[
            (
                "--dag FILE",
                "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced include fragments ('-' = self-contained JSON stdin only) [required]",
            ),
            (
                "--planner NAME",
                "greedy-lpt (default) | critical-path | cpa",
            ),
            (
                "--max-mem SPEC",
                "RAM budget (e.g. 8G) used by the cpa planner",
            ),
            (
                "--max-steps N",
                "maximum active steps whose concurrent memory the cpa planner must fit",
            ),
            ("--format FORMAT", "text (default) | json"),
            ("--perf-dir DIR", "read the profile store from DIR"),
            (
                "--no-profile-feedback",
                "do NOT read the profile store to refine estimates",
            ),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn simple_help(c: &Palette, command: &str) -> String {
    let summary = match command {
        "list" => "List the steps (tag, classification, dependencies).",
        "ascii" => "Draw the DAG as ASCII art.",
        "dot" => "Emit Graphviz DOT (pipe to `dot -Tsvg`).",
        "json" => "Re-emit the DAG as canonical JSON.",
        "yaml" => "Re-emit the DAG as YAML.",
        _ => "Read a DAG and emit it.",
    };
    let dag_flag = (
        "--dag FILE",
        "DAG file to read; .yaml/.yml load as YAML, else JSON; files may use namespaced include fragments ('-' = self-contained JSON stdin only) [required]",
    );
    let help_flag = ("-h, --help", "show this help and exit");
    if matches!(command, "ascii" | "dot") {
        let usage = format!("{command} --dag FILE [--labels LABEL[,LABEL...]] [--group-by group]");
        return render_subcommand_help(
            c,
            &usage,
            summary,
            &[
                dag_flag,
                (
                    "--labels LABEL[,LABEL...]",
                    "show only steps carrying any named label, plus every dependency they require (the same selection as `run --labels`)",
                ),
                (
                    "--group-by group",
                    "collapse steps to one node per `group`, showing its step count; an edge per (upstream group -> group) pair shows how many step dependencies it merges",
                ),
                help_flag,
            ],
        );
    }
    let usage = format!("{command} --dag FILE");
    render_subcommand_help(c, &usage, summary, &[dag_flag, help_flag])
}

fn summary_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "summary <action> [options]",
        "Build / merge / plan / stats the mergeable profile summary.",
        &[
            (
                "build",
                "build a summary from a profile store (--perf-dir DIR, --out FILE, --reservoir-cap N)",
            ),
            (
                "merge",
                "merge one or more summary JSON files (--out FILE, --reservoir-cap N)",
            ),
            ("plan", "build a plan from a summary JSON and DAG"),
            ("stats", "print bucket/sample stats for a summary FILE"),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn summary_build_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "summary build [options]",
        "Build a summary JSON from a profile store for the current runner identity.",
        &[
            (
                "--perf-dir DIR",
                "read the profile-store CSV from DIR (otherwise use the configured default store)",
            ),
            ("--out FILE", "write JSON to FILE instead of stdout"),
            (
                "--reservoir-cap K",
                "maximum samples retained per (step, inner_jobs, workload_digest) bucket",
            ),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn summary_merge_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "summary merge FILE [FILE ...] [options]",
        "Merge one or more summary JSON files into one order-independent summary.",
        &[
            ("FILE [FILE ...]", "summary JSON files to merge [required]"),
            ("--out FILE", "write JSON to FILE instead of stdout"),
            (
                "--reservoir-cap K",
                "maximum samples retained per (step, inner_jobs, workload_digest) bucket after the merge",
            ),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn summary_plan_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "summary plan --summary FILE --dag FILE [options]",
        "Build a DAG plan from a portable profile summary.",
        &[
            ("--summary FILE", "summary JSON file [required]"),
            (
                "--dag FILE",
                "DAG file; .yaml/.yml load as YAML, else JSON; files may use namespaced include fragments ('-' = self-contained JSON stdin only) [required]",
            ),
            (
                "--planner NAME",
                "greedy-lpt (default) | critical-path | cpa",
            ),
            ("--max-mem SPEC", "RAM budget used by the cpa planner"),
            (
                "--max-steps N",
                "maximum active steps whose concurrent memory the cpa planner must fit",
            ),
            ("--format FORMAT", "text (default) | json"),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn summary_stats_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "summary stats FILE",
        "Print bucket and sample counts for one summary JSON file.",
        &[
            ("FILE", "summary JSON file [required]"),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

fn requested_summary_help(c: &Palette, rest: &[String]) -> Option<String> {
    if !wants_help(rest) {
        return None;
    }
    match rest.first().map(String::as_str) {
        Some("build") => Some(summary_build_help(c)),
        Some("merge") => Some(summary_merge_help(c)),
        Some("plan") => Some(summary_plan_help(c)),
        Some("stats") => Some(summary_stats_help(c)),
        Some("-h") | Some("--help") | None => Some(summary_help(c)),
        Some(_) => None,
    }
}

fn box_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "box [--mem SPEC] [--timeout SECS] [--cores K] [--label NAME] [--perf-dir DIR] \
         [--allow-cgroup-failure] [-q] -- CMD [ARGS...]",
        "Run ONE command under the same cgroup-v2 boxing a DAG step gets, without writing a DAG \
         file.",
        &[
            (
                "--mem SPEC",
                "RAM ceiling for the boxed command (e.g. 512M, 8G): both the outer scope's \
                 MemoryMax and the command's own inner memory.max",
            ),
            (
                "--timeout SECS",
                "WALL ceiling in seconds; the CPU ceiling is derived as SECS x K so the wall \
                 bound is the one that fires",
            ),
            (
                "--cores K",
                "CPU BANDWIDTH (cpu.max of K cores, and an outer budget of K). NOT the hard \
                 cpuset PIN that `run --cores` performs. Aliases: -j, --max-cpus",
            ),
            (
                "--label NAME",
                "name for the boxed step in output and evidence (default: the command's basename)",
            ),
            ("--perf-dir DIR", "write resource-usage CSVs into DIR"),
            (
                "--allow-cgroup-failure",
                "downgrade to a best-effort UNBOXED run instead of refusing (exit 3)",
            ),
            ("-q, --quiet", "suppress the per-step summary lines"),
            ("-- CMD [ARGS...]", "the command to box (put it after '--')"),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

/// Default per-command name when `box --label` is absent and the basename yields nothing usable.
const BOX_DEFAULT_LABEL: &str = "command";

/// Parse a `box` numeric flag, refusing zero and negatives by name rather than clamping them.
fn parse_positive_i64(raw: &str, flag: &str) -> Result<i64, String> {
    let value = raw
        .parse::<i64>()
        .map_err(|_| format!("{flag}: invalid int value: '{raw}'"))?;
    if value < 1 {
        return Err(format!("{flag}: must be >= 1"));
    }
    Ok(value)
}

#[derive(Debug)]
struct BoxArgs {
    mem: Option<String>,
    timeout: Option<i64>,
    cores: Option<i64>,
    label: Option<String>,
    perf_dir: Option<String>,
    allow_cgroup_failure: bool,
    quiet: bool,
    command: Vec<String>,
}

fn parse_box_args(rest: &[String]) -> Result<BoxArgs, String> {
    let mut a = BoxArgs {
        mem: None,
        timeout: None,
        cores: None,
        label: None,
        perf_dir: None,
        allow_cgroup_failure: false,
        quiet: false,
        command: Vec::new(),
    };
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let take_value = |inline: Option<String>, i: &mut usize| -> Result<String, String> {
            match inline {
                Some(v) => Ok(v),
                None => {
                    *i += 1;
                    rest.get(*i)
                        .cloned()
                        .ok_or_else(|| format!("expected one argument after {}", rest[*i - 1]))
                }
            }
        };
        match key.as_str() {
            "--" => {
                a.command.extend_from_slice(&rest[i + 1..]);
                break;
            }
            "--mem" => a.mem = Some(take_value(inline, &mut i)?),
            "--timeout" => {
                a.timeout = Some(parse_positive_i64(
                    &take_value(inline, &mut i)?,
                    "--timeout",
                )?)
            }
            "--cores" | "-j" | "--max-cpus" => {
                a.cores = Some(parse_positive_i64(&take_value(inline, &mut i)?, "--cores")?)
            }
            "--label" => a.label = Some(take_value(inline, &mut i)?),
            "--perf-dir" => a.perf_dir = Some(take_value(inline, &mut i)?),
            "--allow-cgroup-failure" => a.allow_cgroup_failure = true,
            "-q" | "--quiet" => a.quiet = true,
            other if other.starts_with('-') => {
                return Err(format!("unrecognized argument: {other}"));
            }
            _ => {
                a.command.extend_from_slice(&rest[i..]);
                break;
            }
        }
        i += 1;
    }
    Ok(a)
}

/// Shell-quote one argv element for a `bash -c` command line.
///
/// A step's `cmd` is handed to `bash -c`, so an unquoted join would let an argument containing a
/// space, a quote, a `;` or a `$(...)` become shell SYNTAX rather than an argument. Wrapping in
/// single quotes and escaping embedded single quotes makes each element survive as exactly one
/// word.
fn box_shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Build the singleton DAG that `box` runs, so ONE command needs no DAG file.
///
/// Deliberately a real [`DagConfig`] handed to the ordinary run path rather than a separate
/// execution route. `box` must be indistinguishable from hand-writing the equivalent
/// singleton-DAG file -- that is its entire value -- and the only way to guarantee that is for it
/// to be the same code after this function returns.
///
/// `--mem` ALSO LOWERS THE MODELED FLOOR, or the flag is unusable for the values anyone actually
/// types. `DagConfig::mem_cap_floor_bytes` defaults to 8 GiB: a lower bound on the modeled
/// worst-case footprint so that sizing an UNCHARACTERIZED graph never concludes "zero steps fit".
/// A boxed command is the opposite of uncharacterized -- `--mem` states its hard inner cap exactly
/// -- so leaving the floor at 8 GiB made the run's own budget check (`--max-mem`, which sizes
/// through [`jobs_for_budget`]) compare a 512 MiB budget against a fictional 8 GiB step and
/// REFUSE, for every value below the very default the flag was reached for in order to lower.
fn box_config(
    argv: &[String],
    label: &str,
    mem_bytes: Option<i64>,
    timeout_s: i64,
    cores: i64,
) -> DagConfig {
    let cmd = argv
        .iter()
        .map(|part| box_shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    DagConfig {
        steps: vec![Step {
            group: "box".to_string(),
            job: label.to_string(),
            desc: argv.join(" "),
            description: String::new(),
            labels: Vec::new(),
            cmd,
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: Vec::new(),
            env: BTreeMap::new(),
            hint: ResourceHint {
                hard_mem_max_bytes: mem_bytes,
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: timeout_s,
            // THE CPU CEILING IS DERIVED, NOT DEFAULTED. Left unset, the step would inherit the
            // deliberately tiny 10-second per-step CPU floor, which exists as a forcing function
            // for an UNDECLARED DAG node -- and would cut an honest boxed command short for a
            // reason its author never asked about. `--timeout x --cores` is the most CPU the
            // command could possibly consume inside its wall budget, so the wall bound is the one
            // that fires and the CPU guard stays a backstop.
            cpu_timeout: timeout_s.saturating_mul(cores),
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }],
        // cpu.max for the boxed command. Set on the CONFIG rather than as preferred_inner_jobs,
        // because the latter appends a `-j K` flag to the command -- correct for a build, wrong
        // for an arbitrary command that may not accept one.
        default_step_cpu_count: Some(cores),
        // Read from the model rather than copied, so `box` can only ever LOWER the floor.
        mem_cap_floor_bytes: match mem_bytes {
            None => DagConfig::default().mem_cap_floor_bytes,
            Some(requested) => DagConfig::default().mem_cap_floor_bytes.min(requested),
        },
        ..Default::default()
    }
}

fn cmd_box(a: &BoxArgs, c: &Palette) -> i32 {
    if a.command.is_empty() {
        eprintln!("{PROG}: box: no command given (use '-- CMD [ARGS...]')");
        return 2;
    }
    let label = match a.label.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => {
            let base = a.command[0]
                .rsplit('/')
                .next()
                .unwrap_or(BOX_DEFAULT_LABEL)
                .trim()
                .to_string();
            if base.is_empty() {
                BOX_DEFAULT_LABEL.to_string()
            } else {
                base
            }
        }
    };
    // The tag is `group.job`, so a dot in the job half would produce a tag that reads as a
    // different group. Replace rather than refuse: a label is a convenience, not a declaration.
    let label = label.replace('.', "-");

    let cores = a.cores.unwrap_or(1);
    let timeout_s = a.timeout.unwrap_or(DEFAULT_STEP_TIMEOUT);
    let mem_bytes = requested_max_mem_bytes(a.mem.as_deref());
    if a.mem.as_deref().is_some_and(|m| !m.is_empty()) && mem_bytes.is_none() {
        eprintln!(
            "{PROG}: box: --mem {:?} is not a positive size (e.g. 512M, 8G)",
            a.mem.as_deref().unwrap_or("")
        );
        return 2;
    }
    let cfg = box_config(&a.command, &label, mem_bytes, timeout_s, cores);

    let run_args = match box_run_args(a) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("{PROG} box: error: {msg}");
            return 2;
        }
    };
    cmd_run(&cfg, &run_args, c)
}

/// The `run` arguments a `box` invocation stands for.
///
/// EVERY KNOB NOT NAMED HERE COMES FROM `run`'s OWN DEFAULTS, obtained by parsing a bare `run`
/// invocation, rather than from a second list of defaults maintained here. A hand-copied list is
/// exactly the thing that drifts the moment `run` gains a flag, and the drift would be invisible:
/// `box` would keep working while quietly diverging from the singleton DAG file it claims to be
/// shorthand for.
///
/// Separate from [`cmd_box`] so the arguments can be READ without executing anything: `--mem`
/// becomes `--max-mem`, which is what the outer scope's `MemoryMax` is derived from, and that
/// half of the flag is otherwise observable only by creating a real systemd scope.
fn box_run_args(a: &BoxArgs) -> Result<RunArgs, String> {
    let cores = a.cores.unwrap_or(1);
    let mut run_args = parse_run_args(&[])?;
    run_args.dag = Some("-".to_string());
    run_args.max_steps = Some(1);
    run_args.max_cpus = Some(cores);
    run_args.max_cpus_source = Some("--max-cpus");
    run_args.max_mem = a.mem.clone();
    run_args.perf_dir = a.perf_dir.clone();
    run_args.allow_cgroup_failure = a.allow_cgroup_failure;
    run_args.quiet = a.quiet;
    Ok(run_args)
}

fn pin_run_help(c: &Palette) -> String {
    render_subcommand_help(
        c,
        "pin-run --cores K [--tag STR] -- CMD [ARGS...]",
        "Reserve disjoint cores, constrain a command subtree to them, and release on exit.",
        &[
            (
                "--cores K",
                "number of collision-free cores to reserve [required]",
            ),
            ("--tag STR", "label stored in the reservation ledger"),
            ("-- CMD [ARGS...]", "command to execute inside the core box"),
            ("-h, --help", "show this help and exit"),
        ],
    )
}

// --------------------------------------------------------------------------- rendering

fn render_list(cfg: &DagConfig, c: &Palette) -> String {
    if cfg.steps.is_empty() {
        return "(empty DAG)".to_string();
    }
    let width = cfg.steps.iter().map(|s| s.tag().len()).max().unwrap_or(1);
    let mut lines: Vec<String> = Vec::with_capacity(cfg.steps.len());
    for step in &cfg.steps {
        let tag = c.bold(&format!("{:<width$}", step.tag(), width = width));
        let cls = c.yellow(&format!("[{}]", step_classification(step).value()));
        let dep = if step.deps.is_empty() {
            String::new()
        } else {
            c.dim(&format!("  <- {}", step.deps.join(", ")))
        };
        lines.push(format!("{tag}  {cls} {}{dep}", step.desc));
    }
    lines.join("\n")
}

// --------------------------------------------------------------------------- dag loading

fn load(dag_arg: &str) -> Result<DagConfig, LoadError> {
    if dag_arg == "-" {
        // stdin has no filename to auto-detect from: default to JSON.
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| LoadError(format!("{e}")))?;
        return dag_from_json(&buf).map_err(|e| load_parse_error("<stdin>", e));
    }
    dag_from_path(Path::new(dag_arg)).map_err(|e| LoadError(e.to_string()))
}

struct LoadError(String);

fn load_parse_error(source: &str, error: DagJsonError) -> LoadError {
    let detail = error.0;
    let detail = detail.strip_prefix("<root>: ").unwrap_or(&detail);
    LoadError(format!("{source}: {detail}"))
}

// --------------------------------------------------------------------------- run subcommand

/// Largest CLI CPU-core budget that remains safe when converted to the fixed 100000-us cgroup
/// period used for per-step `cpu.max`.
const MAX_RUN_CPUS: i64 = i64::MAX / 100_000;
const MAX_STRESS_GENERATED_NODES: i64 = 100_000;
const MIN_PROFILE_TIMESERIES_INTERVAL: Duration = Duration::from_millis(50);
const MAX_PROFILE_TIMESERIES_INTERVAL: Duration = Duration::from_secs(10);

fn parse_profile_timeseries_duration(raw: &str) -> Result<Duration, String> {
    let duration = parse_target_duration(raw).map_err(|_| {
        format!("invalid --profile-timeseries '{raw}': expected seconds or an ms/s/m/h suffix")
    })?;
    if !(MIN_PROFILE_TIMESERIES_INTERVAL..=MAX_PROFILE_TIMESERIES_INTERVAL).contains(&duration) {
        return Err(format!(
            "invalid --profile-timeseries '{raw}': duration must be between 50ms and 10s"
        ));
    }
    Ok(duration)
}

fn parse_profiler_window_duration(raw: &str, flag: &str) -> Result<Duration, String> {
    let duration = parse_target_duration(raw).map_err(|_| {
        format!(
            "invalid {flag} '{raw}': expected a positive duration with an optional ms/s/m/h suffix"
        )
    })?;
    if duration.is_zero() {
        return Err(format!(
            "invalid {flag} '{raw}': duration must be greater than zero"
        ));
    }
    Ok(duration)
}

struct RunArgs {
    dag: Option<String>,
    max_steps: Option<i64>,
    max_cpus: Option<i64>,
    max_cpus_source: Option<&'static str>,
    cores: Option<i64>,
    max_mem: Option<String>,
    selected: Option<String>,
    labels: Option<String>,
    ignore_selected_deps: bool,
    passthrough_args: Option<String>,
    stress: i64,
    perf_dir: Option<String>,
    no_profile: bool,
    profile_timeseries: Option<Duration>,
    profile: bool,
    planner: String,
    show_plan: bool,
    no_profile_feedback: bool,
    /// OPT-IN censoring-aware memory feedback (see [`crate::memory_feedback`]).
    profile_memory_feedback: bool,
    profile_sync: Option<String>,
    profile_sync_direction: String,
    keep_going: bool,
    /// OUTER wall budget for the WHOLE run, in seconds (`None` = unbounded).
    run_timeout: Option<i64>,
    /// Host-wide memory admission (opt-in). `None` = off; `Some(wait_s)` = on, waiting that long
    /// while QUEUED.
    admission: Option<f64>,
    resource_caps_path: Option<String>,
    allow_cgroup_failure: bool,
    unsafe_no_cgroups: bool,
    allow_unwise_nest_dagruns: bool,
    small_default_cap: bool,
    cpu_timeout_multiplier: Option<f64>,
    verbosity: i64,
    quiet: bool,
}

/// Largest `--admission WAIT_S` this CLI will accept, in seconds (one day).
///
/// An UPPER bound, not only a lower one, because "finite and >= 0" admits 1e19 -- a number no
/// operator means and no CI job can outlive, and one this engine used to accept and then PANIC
/// on (exit 101, "overflow when adding duration to instant") while the paired engine ran the
/// same command normally. A validated input that then aborts is worse than a rejected one. A day
/// is longer than any real queue and short enough to be arithmetic.
const MAX_ADMISSION_WAIT_S: f64 = 86400.0;

/// Does this token read as a negative number, the way the paired engine's parser reads one?
///
/// There `-1` is taken as an option VALUE rather than a flag when the parser declares no
/// negative-number-like options, by the matcher `^-\d+$|^-\d*\.\d+$`. This is that expression,
/// spelled out: `-1e19` is deliberately NOT a match, because that matcher does not match it.
fn looks_like_negative_number(token: &str) -> bool {
    let Some(body) = token.strip_prefix('-') else {
        return false;
    };
    if body.is_empty() {
        return false;
    }
    match body.split_once('.') {
        None => body.bytes().all(|b| b.is_ascii_digit()),
        Some((whole, frac)) => {
            !frac.is_empty()
                && whole.bytes().all(|b| b.is_ascii_digit())
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
    }
}

fn parse_run_args(rest: &[String]) -> Result<RunArgs, String> {
    let mut a = RunArgs {
        dag: None,
        max_steps: None,
        max_cpus: None,
        max_cpus_source: None,
        cores: None,
        max_mem: None,
        selected: None,
        labels: None,
        ignore_selected_deps: false,
        passthrough_args: None,
        stress: 1,
        perf_dir: None,
        no_profile: false,
        profile_timeseries: None,
        profile: false,
        planner: Planner::GreedyLpt.value().to_string(),
        show_plan: false,
        no_profile_feedback: false,
        profile_memory_feedback: false,
        profile_sync: None,
        profile_sync_direction: "both".to_string(),
        keep_going: false,
        run_timeout: env_run_timeout(),
        admission: None,
        resource_caps_path: None,
        allow_cgroup_failure: false,
        unsafe_no_cgroups: false,
        allow_unwise_nest_dagruns: false,
        small_default_cap: false,
        cpu_timeout_multiplier: None,
        verbosity: 1,
        quiet: false,
    };
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        // Support both `--flag value` and `--flag=value`.
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let take_value = |inline: Option<String>, i: &mut usize| -> Result<String, String> {
            if let Some(v) = inline {
                Ok(v)
            } else {
                *i += 1;
                rest.get(*i)
                    .cloned()
                    .ok_or_else(|| format!("the argument {key} requires a value"))
            }
        };
        match key.as_str() {
            "--dag" => a.dag = Some(take_value(inline, &mut i)?),
            "-s" | "--max-steps" => {
                let v = take_value(inline, &mut i)?;
                let max_steps = v
                    .parse::<i64>()
                    .map_err(|_| format!("--max-steps: invalid int value: '{v}'"))?;
                if max_steps < 1 {
                    return Err("--max-steps: must be >= 1".to_string());
                }
                a.max_steps = Some(max_steps);
            }
            "-j" | "--max-cpus" => {
                let v = take_value(inline, &mut i)?;
                let source = if key == "-j" { "-j" } else { "--max-cpus" };
                set_run_max_cpus(&mut a, &v, source)?;
            }
            // Hidden 0.13 migration alias. `sweep --jobs` remains its distinct, documented
            // inner-width range; only `run --jobs N` reaches this arm.
            "--jobs" => {
                let v = take_value(inline, &mut i)?;
                set_run_max_cpus(&mut a, &v, "--jobs")?;
            }
            // `--cpuset`/`--pin` are discoverable aliases for `--cores` (identical semantics).
            "--cores" | "--cpuset" | "--pin" => {
                let v = take_value(inline, &mut i)?;
                let cores = v
                    .parse::<i64>()
                    .map_err(|_| format!("{key}: invalid int value: '{v}'"))?;
                if cores < 1 {
                    return Err(format!("{key}: must be >= 1"));
                }
                a.cores = Some(cores);
            }
            "--max-mem" => a.max_mem = Some(take_value(inline, &mut i)?),
            "--selected" => a.selected = Some(take_value(inline, &mut i)?),
            "--labels" => a.labels = Some(take_value(inline, &mut i)?),
            "--ignore-selected-deps" => {
                if inline.is_some() {
                    return Err(format!("unrecognized argument: {arg}"));
                }
                a.ignore_selected_deps = true;
            }
            "--args" => a.passthrough_args = Some(take_value(inline, &mut i)?),
            "--stress" => {
                let v = take_value(inline, &mut i)?;
                a.stress = v
                    .parse::<i64>()
                    .map_err(|_| format!("--stress: invalid int value: '{v}'"))?;
            }
            "--perf-dir" => a.perf_dir = Some(take_value(inline, &mut i)?),
            "--no-profile" => a.no_profile = true,
            "--profile-timeseries" => {
                let value = take_value(inline, &mut i)?;
                a.profile_timeseries = Some(parse_profile_timeseries_duration(&value)?);
            }
            "--profile" => a.profile = true,
            "--planner" => a.planner = validate_planner(take_value(inline, &mut i)?)?,
            "--show-plan" => a.show_plan = true,
            "--no-profile-feedback" => a.no_profile_feedback = true,
            "--profile-memory-feedback" => a.profile_memory_feedback = true,
            "--profile-sync" => a.profile_sync = Some(take_value(inline, &mut i)?),
            "--profile-sync-direction" => {
                let v = take_value(inline, &mut i)?;
                if !matches!(v.as_str(), "both" | "download" | "upload") {
                    return Err(format!(
                        "--profile-sync-direction: invalid value '{v}' (both|download|upload)"
                    ));
                }
                a.profile_sync_direction = v;
            }
            "-k" | "--keep-going" => a.keep_going = true,
            // Colour only. The DAGRUN STEP ERROR marker keeps printing: the string is MORE useful
            // in a plain log, not less, because a stripped transcript is where it gets grepped.
            "--no-color" => crate::scheduler::disable_colour(),
            "--cgroups" => return Err(
                "--cgroups has been removed (cgroup-v2 boxing is ON by default); drop the flag. \
                     Per-node resource limits are DAG fields (cpu_timeout, memory, pids)."
                    .to_string(),
            ),
            "--run-timeout" => {
                let v = take_value(inline, &mut i)?;
                let secs = v
                    .parse::<i64>()
                    .map_err(|_| format!("--run-timeout: invalid int value: '{v}'"))?;
                if secs <= 0 {
                    return Err(format!(
                        "--run-timeout: must be a positive number of seconds, got '{v}'"
                    ));
                }
                a.run_timeout = Some(secs);
            }
            "--admission" => {
                // Optional value, mirroring the Python `nargs="?"`: a bare `--admission` means
                // "do not wait", and only a token that is not itself a flag is taken as SECONDS.
                // A NEGATIVE NUMBER IS A VALUE, NOT A FLAG -- argparse consumes `-1` here (its
                // negative-number matcher, no negative-number-like options declared), so an
                // engine that left it unconsumed would report a different usage error for the
                // same command line. Same matcher, same token consumed, same message.
                let seconds = match inline {
                    Some(v) => v,
                    None => match rest.get(i + 1) {
                        Some(next)
                            if !next.starts_with('-') || looks_like_negative_number(next) =>
                        {
                            i += 1;
                            next.clone()
                        }
                        _ => "0".to_string(),
                    },
                };
                let parsed = seconds.parse::<f64>().map_err(|_| {
                    format!("--admission WAIT_S must be a number of seconds (got '{seconds}')")
                })?;
                // NaN must be refused too, hence the explicit finite check rather than a
                // negated comparison: `!(nan >= 0.0)` is true, but so is every other comparison
                // with NaN, and a wait budget that is not a number is a usage error either way.
                // The UPPER bound is refused in the same breath: see MAX_ADMISSION_WAIT_S.
                if !parsed.is_finite() || !(0.0..=MAX_ADMISSION_WAIT_S).contains(&parsed) {
                    return Err(format!(
                        "--admission WAIT_S must be a finite number of seconds in \
                         [0, {MAX_ADMISSION_WAIT_S}] (got '{seconds}')"
                    ));
                }
                a.admission = Some(parsed);
            }
            "--resource-caps-path" => {
                let path = take_value(inline, &mut i)?;
                if path.is_empty() {
                    return Err("--resource-caps-path requires a non-empty path".to_string());
                }
                a.resource_caps_path = Some(path);
            }
            "--allow-cgroup-failure" => a.allow_cgroup_failure = true,
            "--unsafe-no-cgroups" => a.unsafe_no_cgroups = true,
            "--allow-unwise-nest-dagruns" => a.allow_unwise_nest_dagruns = true,
            "--small-default-cap" => a.small_default_cap = true,
            "--cpu-timeout-multiplier" => {
                let v = take_value(inline, &mut i)?;
                a.cpu_timeout_multiplier = Some(
                    v.parse::<f64>()
                        .map_err(|_| format!("{key}: invalid float value: '{v}'"))?,
                );
            }
            "-v" => a.verbosity += 1,
            "-q" | "--quiet" => a.quiet = true,
            other => {
                // Handle bare `-sN` / `-jN` (no space/=).
                if let Some(n) = other.strip_prefix("-s") {
                    let max_steps = n
                        .parse::<i64>()
                        .map_err(|_| format!("--max-steps: invalid int value: '{n}'"))?;
                    if max_steps < 1 {
                        return Err("--max-steps: must be >= 1".to_string());
                    }
                    a.max_steps = Some(max_steps);
                } else if let Some(n) = other.strip_prefix("-j") {
                    set_run_max_cpus(&mut a, n, "-j")?;
                } else {
                    return Err(format!("unrecognized argument: {other}"));
                }
            }
        }
        i += 1;
    }
    if a.no_profile && a.profile_timeseries.is_some() {
        return Err("--profile-timeseries cannot be combined with --no-profile".to_string());
    }
    Ok(a)
}

fn set_run_max_cpus(a: &mut RunArgs, raw: &str, source: &'static str) -> Result<(), String> {
    let max_cpus = raw
        .parse::<i64>()
        .map_err(|_| format!("{source}: invalid int value: '{raw}'"))?;
    if max_cpus < 1 {
        return Err(format!("{source}: must be >= 1"));
    }
    if max_cpus > MAX_RUN_CPUS {
        return Err(format!("{source}: must be <= {MAX_RUN_CPUS}"));
    }
    if let Some(existing) = a.max_cpus {
        if existing != max_cpus {
            let existing_source = a.max_cpus_source.unwrap_or("an earlier CPU limit");
            if source == "--jobs" || existing_source == "--jobs" {
                return Err("--max-cpus and legacy --jobs disagree".to_string());
            }
            return Err(format!(
                "{source}: conflicts with {existing_source} ({max_cpus} != {existing})"
            ));
        }
    }
    a.max_cpus = Some(max_cpus);
    a.max_cpus_source.get_or_insert(source);
    Ok(())
}

/// Resolve the maximum total CPU-core budget for the run. The ambient container/affinity budget
/// is the default; an opt-in hard cpuset is always a tighter upper bound.
fn select_max_cpus(a: &RunArgs) -> i64 {
    select_max_cpus_bounded(a, None)
}

fn select_max_cpus_bounded(a: &RunArgs, inherited_max_cpus: Option<i64>) -> i64 {
    let requested = a
        .max_cpus
        .unwrap_or_else(|| container_core_budget().min(aggregate_slice_max_cpus()))
        .max(1);
    let requested = match a.cores {
        Some(cores) if requested > cores => {
            eprintln!(
                "{PROG}: --cores {cores} is tighter than --max-cpus {requested}; using total \
                 CPU-core budget {cores}"
            );
            cores
        }
        _ => requested,
    };
    match inherited_max_cpus {
        Some(parent) if requested > parent => {
            eprintln!(
                "{PROG}: requested/default --max-cpus {requested} exceeds inherited delegated \
                 CPU ceiling {parent}; using total CPU limit {parent}"
            );
            parent
        }
        _ => requested,
    }
}

fn inherited_delegated_limits() -> Result<Option<DelegatedResourceLimits>, String> {
    if cfg!(test) {
        // The repository's own Rust test binary may itself run as one delegated validation node.
        // That outer envelope is containment for the harness, not input to unit cases that
        // exercise fresh CLI semantics. Real delegated behavior is covered by integration
        // smokes, which build the ordinary (non-cfg(test)) library and binary.
        return Ok(None);
    }
    let Some(root) = delegated_cgroup_root()
        .map_err(|error| format!("invalid ${DELEGATED_CGROUP_ENV}: {error}"))?
    else {
        return Ok(None);
    };
    let limits = delegated_resource_limits(&root);
    let mut unknown = Vec::new();
    if !limits.cpu_known {
        unknown.push("cpu.max");
    }
    if !limits.memory_known {
        unknown.push("memory.max");
    }
    if unknown.is_empty() {
        Ok(Some(limits))
    } else {
        Err(format!(
            "delegated parent resource envelope has unreadable/malformed {}",
            unknown.join(", ")
        ))
    }
}

fn select_max_mem_bytes(a: &RunArgs, inherited_max_mem_bytes: Option<i64>) -> Option<i64> {
    let raw = a.max_mem.as_deref().filter(|value| !value.is_empty());
    let requested = requested_max_mem_bytes(raw);
    let Some(parent) = inherited_max_mem_bytes else {
        return requested;
    };
    if raw.is_some() && requested.is_none() {
        if raw.and_then(parse_size).is_none() {
            eprintln!(
                "{PROG}: could not parse --max-mem {:?}; ignoring that request but retaining \
                 inherited delegated memory ceiling {parent} bytes",
                raw.unwrap_or("")
            );
            return Some(parent);
        }
        // Preserve the existing non-positive refusal in select_max_steps_with_budget.
        return None;
    }
    let Some(requested) = requested else {
        eprintln!(
            "{PROG}: inherited delegated memory ceiling {parent} bytes is the effective default \
             --max-mem budget"
        );
        return Some(parent);
    };
    if requested > parent {
        eprintln!(
            "{PROG}: --max-mem {requested} bytes exceeds inherited delegated memory ceiling \
             {parent} bytes; using {parent} bytes"
        );
        Some(parent)
    } else {
        Some(requested)
    }
}

fn uses_inherited_memory_budget(a: &RunArgs, inherited_max_mem_bytes: Option<i64>) -> bool {
    let Some(parent) = inherited_max_mem_bytes else {
        return false;
    };
    let requested = requested_max_mem_bytes(a.max_mem.as_deref().filter(|value| !value.is_empty()));
    requested.is_none_or(|requested| requested > parent)
}

fn inherited_cores_error(a: &RunArgs, inherited_max_cpus: Option<i64>) -> Option<String> {
    let (Some(cores), Some(parent)) = (a.cores, inherited_max_cpus) else {
        return None;
    };
    (cores > parent).then(|| {
        format!(
            "--cores {cores} requests an exact reservation wider than the inherited delegated \
             CPU ceiling {parent}; refusing"
        )
    })
}

// Choose the outer active-step ceiling (`-s`). `--max-mem` independently derives a conservative
// model-based ceiling;
// when the caller supplied both, the tighter value wins and is announced.
fn apply_memory_step_ceiling(base: i64, memory_steps: i64) -> i64 {
    base.min(memory_steps)
}

#[cfg(test)]
fn select_max_steps(cfg: &DagConfig, a: &RunArgs, max_cpus: i64) -> i64 {
    select_max_steps_with_budget(cfg, a, max_cpus, None, false)
}

fn select_max_steps_with_budget(
    cfg: &DagConfig,
    a: &RunArgs,
    max_cpus: i64,
    effective_max_mem_bytes: Option<i64>,
    inherited_memory_budget: bool,
) -> i64 {
    let base = a.max_steps.unwrap_or(max_cpus);
    let max_mem = a.max_mem.as_deref().filter(|s| !s.is_empty());
    if max_mem.is_some() || effective_max_mem_bytes.is_some() {
        let parsed = effective_max_mem_bytes.or_else(|| max_mem.and_then(parse_size));
        match parsed {
            None => {
                let mm = max_mem.unwrap_or("");
                eprintln!(
                    "{PROG}: could not parse --max-mem '{mm}'; falling back to --max-steps \
                     {base}"
                );
                return base;
            }
            Some(budget) => {
                let mut sized_cfg = cfg.clone();
                if inherited_memory_budget {
                    sized_cfg.mem_cap_floor_bytes = sized_cfg.mem_cap_floor_bytes.min(budget);
                }
                let (memory_steps, footprint) = jobs_for_budget(&sized_cfg, budget);
                if memory_steps == 0 {
                    let budget_source = match max_mem.filter(|raw| parse_size(raw) == Some(budget))
                    {
                        Some(raw) => format!("--max-mem {raw}"),
                        None => format!("effective delegated --max-mem {budget} bytes"),
                    };
                    eprintln!(
                        "{PROG}: {budget_source}: REFUSED — minimum runnable footprint \
                         {footprint} bytes cannot fit safely within budget {budget} bytes"
                    );
                    return 0;
                }
                let selected = apply_memory_step_ceiling(base, memory_steps);
                let budget_source = match max_mem.filter(|raw| parse_size(raw) == Some(budget)) {
                    Some(raw) => format!("--max-mem {raw}"),
                    None => format!("effective delegated --max-mem {budget} bytes"),
                };
                eprintln!(
                    "{PROG}: {budget_source} -> modeled memory ceiling {memory_steps} active steps \
                     (worst-case {footprint} bytes fits budget {budget} bytes); base active-step \
                     ceiling {base}; final --max-steps {selected}"
                );
                let ncpu = cpu_count();
                let modeled = cfg.steps.iter().any(|s| {
                    s.skip_reason.is_none()
                        && (s.hint.hard_mem_max_bytes.is_some_and(|value| value > 0)
                            || s.hint.rss_baseline_bytes.is_some_and(|value| value > 0)
                            || cfg
                                .default_step_mem_cap_bytes
                                .is_some_and(|value| value > 0))
                });
                if memory_steps == ncpu && !modeled {
                    eprintln!(
                        "{PROG}: note: no runnable step has a positive hard/RSS/default memory \
cap, so the modeled footprint is only the mem_cap_floor_bytes floor ({} bytes) and {budget_source} did \
not throttle (modeled memory ceiling {memory_steps} = CPU count; final --max-steps {selected})",
                        sized_cfg.mem_cap_floor_bytes,
                    );
                }
                return selected;
            }
        }
    }
    base
}

/// Best-effort git SHA of the current working directory's repo (stamps perf-log rows only).
fn git_sha() -> String {
    match std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
    {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() {
                "unknown".to_string()
            } else {
                s
            }
        }
        _ => "unknown".to_string(),
    }
}

// Establish the two-level cgroup-v2 boxing that is this tool's PRIMARY purpose (mirrors the
// Python `_resolve_cgroup_manager`). Returns the manager to use (`None` = intentional UNBOXED
// run), or an `Err(exit_code)` the caller must return when boxing is required but unavailable.
// May re-exec this process into a systemd scope (never returns on success).
/// Env fallback for the outer run budget, so a wrapper that cannot edit the command line (a CI
/// job template, a systemd unit) can still bound the run.
fn env_run_timeout() -> Option<i64> {
    std::env::var("DAGRUN_RUN_TIMEOUT")
        .ok()
        .filter(|v| !v.is_empty())
        .and_then(|v| match v.parse::<i64>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                eprintln!(
                    "{PROG}: DAGRUN_RUN_TIMEOUT={v:?} is not a positive integer; \
                     ignoring"
                );
                None
            }
        })
}

/// How much longer than the runner's own budget the SCOPE is allowed to live.
///
/// The scope bound is a backstop for the runner itself wedging, so it must never be the thing that
/// fires in normal operation — the runner needs this window to terminate its steps, join its
/// readers, flush profile rows, and return a verdict. Sized as the larger of 60s and a tenth of
/// the budget, because reaping a large fan-out is not a constant-time operation.
fn scope_grace_s(run_timeout_s: i64) -> i64 {
    60.max(run_timeout_s / 10)
}

/// The `--max-mem` spec as an outer-scope ceiling in bytes, or `None` when it is absent,
/// unparseable, or NON-POSITIVE.
///
/// A bad spec is NOT an error here, and non-positive is treated exactly like unparseable:
/// `select_max_steps` already reports both by name and the run exits 2 before any step starts
/// (`--max-mem 0`: "REFUSED — minimum runnable footprint … cannot fit safely within budget 0
/// bytes"). Refusing again at scope bring-up would turn one typo into two different exit codes
/// depending on whether boxing was attempted — a run with `--allow-cgroup-failure` would exit 2
/// and the same command without it would exit 3.
///
/// SO THE SPEC IS DROPPED HERE AND REFUSED THERE; it is not accepted and it is not ignored.
/// `outer_memory_max_bytes_capped`'s own refusal of a non-positive request is a contract for
/// library callers, which this function is not one of; the end-to-end behaviour is pinned in
/// tests/max_mem_outer_scope_smoke.rs.
fn requested_max_mem_bytes(max_mem: Option<&str>) -> Option<i64> {
    max_mem
        .filter(|s| !s.is_empty())
        .and_then(parse_size)
        .filter(|bytes| *bytes > 0)
}

fn resolve_cgroups(
    allow_failure: bool,
    unsafe_no_cgroups: bool,
    max_cpus: Option<i64>,
    run_timeout_s: Option<i64>,
    max_mem_bytes: Option<i64>,
) -> Result<BoxedCgroups, i32> {
    if unsafe_no_cgroups {
        // Deliberate opt-out (--unsafe-no-cgroups): skip scope bring-up entirely and run unboxed
        // even where boxing is available. Distinct from --allow-cgroup-failure (a capability
        // fallback); logged loudly as a reviewable audit signal. Takes precedence over
        // allow_failure when both are set.
        eprintln!(
            "{PROG}: WARNING: DELIBERATELY UNBOXED via --unsafe-no-cgroups: per-step \
             memory/CPU-bandwidth/pids caps are NOT enforced; the per-step CPU-time budget uses only a best-effort procfs process-group floor. This is an explicit, reviewable opt-out of \
             cgroup resource boxing (not a capability fallback)."
        );
        return Ok(None);
    }
    let delegated = match delegated_cgroup_root() {
        Ok(root) => root,
        Err(error) => {
            eprintln!("{PROG}: ERROR: invalid ${DELEGATED_CGROUP_ENV}: {error}.");
            return Err(3);
        }
    };
    if let Some(root) = delegated {
        let mut manager =
            Cgroups::from_delegated_root_with_limits(root.clone(), max_cpus, max_mem_bytes);
        if manager.enabled() {
            eprintln!(
                "{PROG}: cgroup boxing ACTIVE in parent-owned delegated step root {}; the outer \
                 scheduler retains subtree teardown ownership.",
                root.display()
            );
            return Ok(Some(Arc::new(manager) as Arc<dyn CgroupManager>));
        }
        if let Err(error) = manager.restore_nested_coordinator_now() {
            eprintln!(
                "{PROG}: ERROR: failed nested cgroup setup also failed to restore the scheduler \
                 to its prior delegated cgroup ({error}); no step will be started."
            );
            return Err(3);
        }
        if allow_failure {
            eprintln!(
                "{PROG}: warning: delegated root {} could not be subdivided; running inside the \
                 parent-owned boundary without inner per-step cgroups \
                 (--allow-cgroup-failure).",
                root.display()
            );
            return Ok(None);
        }
        eprintln!(
            "{PROG}: ERROR: delegated root {} could not establish inner per-step cgroups.",
            root.display()
        );
        return Err(3);
    }
    if let Some(value) = std::env::var_os(DELEGATED_UNBOXED_ENV) {
        if value != "1" || std::env::var_os("DAGRUN_OUTER_RUN").is_none() {
            eprintln!(
                "{PROG}: ERROR: invalid ${DELEGATED_UNBOXED_ENV}: expected exact value '1' \
                 from an outer delegated_children step."
            );
            return Err(3);
        }
        eprintln!(
            "{PROG}: WARNING: reviewed uncontained nested execution via \
             ${DELEGATED_UNBOXED_ENV}=1; the outer scheduler has no cgroup subtree to delegate, \
             so this run uses process-group/procfs fallback teardown and accounting."
        );
        return Ok(None);
    }
    if is_in_scope() {
        // THE SENTINEL IS A PROMISE; GO AND LOOK BEFORE CLAIMING ANYTHING. `is_in_scope()` reads
        // an environment variable this process set for itself, and every "boxing ACTIVE" line
        // downstream used to rest on it. Observing the live pid inside the cgroup, from both the
        // kernel's view and the cgroup's own roster, is what turns the claim into a fact.
        // A PROMISED UNIT IS REQUIRED HERE, and demanding it is what makes the observation mean
        // something. The re-exec sets the sentinel and the unit name TOGETHER, so a sentinel with
        // no unit has no referent — and without one, "observed in some cgroup" is true of almost
        // every process on a cgroup-v2 host, which would wave through exactly the forged claim
        // this check exists to catch.
        // ONE OBSERVATION SITE, AND IT IS THE TYPED OUTCOME. Calling `observe_own_containment`
        // here directly would leave `ScopeAttempt::AlreadyInScope` with no consumer at all — the
        // variant would be unreachable from the binary, and mutating the enum's observation away
        // would not fail a single test. Measured: it did not. So the in-scope branch goes through
        // the same entry point as every other outcome.
        let attempt = attempt_scope_reexec(None, None, None);
        if !attempt.is_contained() {
            let msg = format!(
                "the in-scope sentinel is set but containment could NOT be observed: {}",
                attempt.describe()
            );
            if allow_failure {
                eprintln!("{PROG}: warning: {msg}; running UNBOXED (--allow-cgroup-failure).");
                return Ok(None);
            }
            eprintln!(
                "{PROG}: ERROR: {msg}. Refusing to report boxing on the strength of an \
                 environment variable."
            );
            return Err(3);
        }
        let expected_memory_max = expected_outer_memory_max_bytes();
        let expected_cpu_count = expected_outer_cpu_count();
        let cpu_request_matches = max_cpus.is_none() || expected_cpu_count == max_cpus;
        if expected_memory_max.is_none_or(|cap| {
            !cpu_request_matches
                || !enable_outer_oom_group()
                || !verify_scope_limits(cap, expected_cpu_count)
        }) {
            let msg = "outer scope MemoryMax/MemorySwapMax/memory.oom.group/cpu.max readback \
                       failed; the run is not safely contained";
            if allow_failure {
                eprintln!(
                    "{PROG}: warning: {msg}; running best-effort UNBOXED \
                     (--allow-cgroup-failure)."
                );
                return Ok(None);
            }
            eprintln!("{PROG}: ERROR: {msg}.");
            return Err(3);
        }
        // If an outer scope budget was requested, PROVE it is on the live unit. `-p
        // RuntimeMaxSec=N` on the command line is a request; the unit's own property is the fact.
        if let Some(expected) = expected_scope_runtime_max_s() {
            if !verify_scope_runtime_max(expected) {
                let msg = "outer scope RuntimeMaxSec readback failed; the run's outermost wall \
                           bound is NOT enforced";
                if allow_failure {
                    eprintln!("{PROG}: warning: {msg} (--allow-cgroup-failure).");
                } else {
                    eprintln!("{PROG}: ERROR: {msg}.");
                    return Err(3);
                }
            }
        }
        let mgr = Cgroups::new();
        if mgr.enabled() {
            install_scope_teardown();
            // NAME THE OBSERVED CGROUP, not the intention. A reader can now check the claim
            // against `/sys/fs/cgroup` themselves instead of taking the word ACTIVE for it.
            eprintln!(
                "{PROG}: cgroup boxing ACTIVE (two-level cgroup-v2 scope; per-step memory/CPU caps \
                 + setsid-proof teardown); containment OBSERVED: {}.",
                attempt.describe()
            );
            return Ok(Some(Arc::new(mgr) as Arc<dyn CgroupManager>));
        }
        if allow_failure {
            eprintln!(
                "{PROG}: warning: inside a scope but per-step cgroup setup failed; running \
                 best-effort UNBOXED (--allow-cgroup-failure)."
            );
            return Ok(None);
        }
        eprintln!(
            "{PROG}: ERROR: {CGROUP_SETUP_ENVIRONMENT_ERROR}; re-run with \
             --allow-cgroup-failure to run UNBOXED."
        );
        return Err(3);
    }
    // No systemd scope. That is one route to a cgroup being unavailable, NOT proof that
    // containment is impossible — the conflation that left every escapee unkillable. Where the
    // host allows it, a cgroup we create ourselves gives `cgroup.kill`, which reaches a setsid
    // escapee precisely because such a process changes session and pgid but not cgroup membership.
    //
    // OPT-IN, and deliberately so. Enabling containment where it is currently off changes every
    // lane at once; that is an owner decision. Default behaviour below is byte-for-byte what it
    // was. Set DAGRUN_DIRECT_CGROUP=1 to use it.
    if std::env::var(DIRECT_CGROUP_ENV).is_ok_and(|v| v == "1") {
        let mgr = Cgroups::direct();
        if mgr.enabled() {
            // SAY WHOSE CONTAINMENT THIS IS. The runner itself is NOT moved into this cgroup —
            // only each step is, at launch — so an observation of the RUNNER's pid would be the
            // wrong proof and claiming it would be worse than claiming nothing. State the route,
            // state where the runner actually sits, and leave the per-step proof to teardown.
            eprintln!(
                "{PROG}: cgroup teardown ACTIVE via direct cgroupfs (no systemd scope): steps are \
                 KILLABLE as a subtree, including setsid/double-fork escapees. Per-step \
                 memory/CPU caps are NOT enforced on this route — this is containment for \
                 teardown, not resource boxing. The RUNNER is not itself contained here: {}.",
                observe_own_containment(None).describe()
            );
            return Ok(Some(Arc::new(mgr) as Arc<dyn CgroupManager>));
        }
        eprintln!(
            "{PROG}: warning: {DIRECT_CGROUP_ENV}=1 but direct cgroupfs containment could not be \
             established; continuing with the behaviour below."
        );
    }
    if allow_failure {
        eprintln!(
            "{PROG}: warning: cgroup boxing not established (--allow-cgroup-failure); running \
             UNBOXED (process-group teardown, no per-step memory/CPU-bandwidth caps, and only a best-effort procfs CPU-time floor)."
        );
        // SAY WHICH BOUNDS SURVIVE THE FALLBACK, because "unboxed" has been read as "unbounded"
        // and that reading is how a run reached an external job kill. Per-step WALL budgets and
        // the outer run budget are enforced by the runner itself and still apply; the per-step
        // CPU-time budget and the scope's RuntimeMaxSec are cgroup/systemd features and do not.
        match run_timeout_s {
            Some(secs) => eprintln!(
                "{PROG}: unboxed run is STILL wall-bounded: per-step wall timeouts apply and the \
                 whole run is cut at {secs}s. Per-step CPU-time budgets use a best-effort \
                 procfs process-group floor; the scope-level RuntimeMaxSec backstop is NOT \
                 enforced without cgroups."
            ),
            None => eprintln!(
                "{PROG}: WARNING: no outer run budget is set (--run-timeout / \
                 DAGRUN_RUN_TIMEOUT), so nothing bounds the run as a whole; only \
                 per-step wall timeouts apply."
            ),
        }
        return Ok(None);
    }
    // Default: boxing is required -> re-exec into a transient systemd --user scope (never returns
    // on success).
    let Some(outer_memory_max) = outer_memory_max_bytes_capped(max_mem_bytes) else {
        eprintln!(
            "{PROG}: ERROR: cannot derive a positive outer MemoryMax from MemAvailable/\
             $DAGRUN_OUTER_MEMORY_MAX_BYTES/--max-mem; refusing an unbounded run."
        );
        return Err(3);
    };
    // SAY WHICH CEILING WON. --max-mem used to size the schedule and nothing else, so a run could
    // report a 20 GiB budget while its scope admitted 90% of the host. Naming the binding ceiling
    // is what makes the containment claim checkable against the live unit.
    if let Some(requested) = max_mem_bytes {
        if outer_memory_max == requested {
            eprintln!(
                "{PROG}: --max-mem is the outer scope ceiling: MemoryMax={outer_memory_max} bytes."
            );
        } else {
            eprintln!(
                "{PROG}: --max-mem {requested} bytes did not bind; the derived/environment \
                 boundary is smaller: MemoryMax={outer_memory_max} bytes."
            );
        }
    }
    // NAME WHAT ACTUALLY HAPPENED. This used to pick between two sentences from a bool, and on the
    // policy-skip path it chose "boxing was skipped (e.g. CI without a systemd --user scope)" — a
    // claim about a capability nothing had tested. Reporting the real outcome is the whole fix: the
    // exit code is unchanged, and so is the policy.
    let attempt = attempt_scope_reexec(
        Some(outer_memory_max),
        max_cpus,
        run_timeout_s.map(|s| s + scope_grace_s(s)),
    );
    match &attempt {
        ScopeAttempt::SkippedByPolicy { reason } => eprintln!(
            "{PROG}: ERROR: cgroup boxing was NOT ESTABLISHED and NOT TESTED: scope setup was \
             skipped by policy because ${reason} is set, so this run does not know whether boxing \
             is available here. Re-run with --allow-cgroup-failure to run UNBOXED, or with \
             {FORCE_ATTEMPT_ENV}=1 to probe instead of skipping."
        ),
        other => eprintln!(
            "{PROG}: ERROR: cgroup boxing could not be established: {}. Cgroup resource boxing is \
             this tool's primary purpose; re-run with --allow-cgroup-failure to run UNBOXED.",
            other.describe()
        ),
    }
    Err(3)
}

// --------------------------------------------------------------------------- profile store

/// Resolve the effective profile-store directory and a label for its source (Feature D).
///
/// Precedence: `--no-profile` disables logging (returns `(None, "disabled")`); otherwise an
/// explicit `--perf-dir` wins; otherwise `$DAGRUN_PROFILE_DIR`; otherwise the repo-local
/// default `./.dagrun/profiles/`. Auto-logging is ON by default.
fn resolve_profile_dir(perf_dir: Option<&str>, no_profile: bool) -> (Option<String>, &'static str) {
    if no_profile {
        return (None, "disabled");
    }
    if let Some(d) = perf_dir {
        if !d.is_empty() {
            return (Some(d.to_string()), "--perf-dir");
        }
    }
    if let Ok(env) = std::env::var(PROFILE_DIR_ENV) {
        if !env.is_empty() {
            return (Some(env), "env");
        }
    }
    (Some(DEFAULT_PROFILE_DIR.to_string()), "default")
}

fn require_timeseries_cgroups(
    command: &str,
    interval: Option<Duration>,
    cgroups: &BoxedCgroups,
) -> Result<(), i32> {
    if interval.is_some() && !matches!(cgroups, Some(manager) if manager.enabled()) {
        eprintln!(
            "{PROG}: {command}: --profile-timeseries requires active cgroup-v2 containment; no step was started"
        );
        return Err(3);
    }
    Ok(())
}

/// Validate a `--planner` value, returning the canonical string or a usage error.
fn validate_planner(value: String) -> Result<String, String> {
    match Planner::from_value(&value) {
        Some(p) => Ok(p.value().to_string()),
        None => Err(format!(
            "--planner: invalid choice: '{value}' (choose from greedy-lpt, critical-path, cpa)"
        )),
    }
}

// Apply censoring-aware profile memory feedback, reporting every decision.
//
// OFF unless the caller asked for it. Default feedback treats an authored `rss_baseline_bytes` as
// a floor: it may raise that value, but cannot shrink the next run's enforced cap from sparse
// history. This path is the separate opt-in allowed to lower the authored value, and only from
// enough uncensored observations plus a margin. Every step the store knows about is reported,
// including the ones that did NOT move and why, because otherwise "the cap did not change" and
// "the store had nothing usable to say" look identical from the outside.
//
// `cfg` is the config the ordinary plan has ALREADY been applied to and `authored` is the same
// config before that, which is what lets a decline restore the LARGER of the baseline its author
// wrote and the peak the store proves it has already reached. That also replaces a larger
// ordinary-feedback estimate only when this stricter path has enough evidence to do so.
//
// `--no-profile-feedback` turns the store reader off entirely, which makes this flag a no-op.
// That combination is legal but empty, and it is announced rather than obeyed in silence: a
// caller who asked for a learned cap by name and got the authored one has been told something
// untrue by omission.
fn apply_memory_feedback(
    cfg: &DagConfig,
    authored: &DagConfig,
    feedback_dir: Option<&str>,
    enabled: bool,
) -> DagConfig {
    if !enabled {
        return cfg.clone();
    }
    let baselines: BTreeMap<String, Option<i64>> = authored
        .steps
        .iter()
        .map(|s| (s.tag(), s.hint.rss_baseline_bytes))
        .collect();
    let Some(dir) = feedback_dir else {
        // Nothing to undo: with the reader off, the plan never learned a baseline either.
        eprintln!(
            "{PROG}: --profile-memory-feedback: --no-profile-feedback disables the profile-store \
             reader, so no estimate is derived and every authored hint is used as written"
        );
        return cfg.clone();
    };
    let admissions = load_memory_admissions(
        std::path::Path::new(dir),
        None,
        None,
        DEFAULT_MIN_UNCENSORED_SAMPLES,
        DEFAULT_MARGIN_PCT,
        None,
    );
    if admissions.is_empty() {
        eprintln!(
            "{PROG}: --profile-memory-feedback: no profile store for this machine/container \
             identity under {dir}; every authored hint is retained"
        );
        return apply_memory_admissions(cfg, &BTreeMap::new(), Some(&baselines));
    }
    let tags: std::collections::BTreeSet<String> = cfg.steps.iter().map(|s| s.tag()).collect();
    for tag in &tags {
        if let Some(admission) = admissions.get(tag) {
            eprintln!(
                "{}",
                memory_admission_line(PROG, admission, baselines.get(tag).copied().flatten())
            );
        }
    }
    apply_memory_admissions(cfg, &admissions, Some(&baselines))
}

// The directory the plan-time FEEDBACK reader loads the profile store from, or `None` when
// feedback is off. Independent of `--no-profile` (which governs WRITING). Mirrors Python's
// `_resolve_feedback_dir`.
fn resolve_feedback_dir(perf_dir: Option<&str>, no_feedback: bool) -> Option<String> {
    if no_feedback {
        return None;
    }
    if let Some(d) = perf_dir {
        if !d.is_empty() {
            return Some(d.to_string());
        }
    }
    if let Ok(env) = std::env::var(PROFILE_DIR_ENV) {
        if !env.is_empty() {
            return Some(env);
        }
    }
    Some(DEFAULT_PROFILE_DIR.to_string())
}

// Load the profile store (when feedback is on) and build the plan for `planner`. Mirrors Python's
// `_build_feedback_plan`. `core_budget` (`P`) bounds every displayed speedup recommendation and,
// together with `mem_budget`, drives the CPA allocator under `--planner cpa`.
fn build_feedback_plan(
    cfg: &DagConfig,
    feedback_dir: Option<&str>,
    planner: Planner,
    core_budget: Option<i64>,
    mem_budget: Option<i64>,
    max_steps: Option<i64>,
) -> Plan {
    match feedback_dir {
        Some(dir) => {
            let (mid, cc) = feedback_identity();
            let expected: HashMap<String, String> = cfg
                .steps
                .iter()
                .map(|step| {
                    (
                        step.tag(),
                        workload_digest(step, &cfg.default_jobs_flag, &cfg.default_jobs_env),
                    )
                })
                .collect();
            let samples = load_step_samples_for_workloads(Path::new(dir), &mid, &cc, &expected);
            let speedups = load_step_speedups_for_workloads(Path::new(dir), &mid, &cc, &expected);
            build_plan_with_max_steps(
                cfg,
                &samples,
                planner,
                DEFAULT_MIN_SAMPLES,
                &speedups,
                core_budget,
                mem_budget,
                max_steps,
            )
        }
        None => build_plan_with_max_steps(
            cfg,
            &std::collections::HashMap::new(),
            planner,
            DEFAULT_MIN_SAMPLES,
            &std::collections::HashMap::new(),
            core_budget,
            mem_budget,
            max_steps,
        ),
    }
}

// Build a plan whose learned estimates come from the mergeable SUMMARY (rather than a CSV store) —
// the reader half of the sync feature. Mirrors Python's `_build_plan_from_summary`.
fn build_plan_from_summary(
    cfg: &DagConfig,
    summary: &Summary,
    planner: Planner,
    core_budget: Option<i64>,
    mem_budget: Option<i64>,
    max_steps: Option<i64>,
) -> Plan {
    let expected: HashMap<String, String> = cfg
        .steps
        .iter()
        .map(|step| {
            (
                step.tag(),
                workload_digest(step, &cfg.default_jobs_flag, &cfg.default_jobs_env),
            )
        })
        .collect();
    let samples = summary::step_samples_from_summary_for_workloads(summary, &expected);
    let speedups = summary::step_speedups_from_summary_for_workloads(summary, None, &expected);
    build_plan_with_max_steps(
        cfg,
        &samples,
        planner,
        DEFAULT_MIN_SAMPLES,
        &speedups,
        core_budget,
        mem_budget,
        max_steps,
    )
}

// A summary built from THIS machine's local CSV store (its own not-yet-uploaded history), so a
// persistent box's local runs also seed the planner. Empty when feedback is off / the store is
// absent. Mirrors Python's `_local_store_summary`.
fn local_store_summary(feedback_dir: Option<&str>, mid: &str, cc: &str) -> Summary {
    match feedback_dir {
        Some(dir) => summary::summary_from_store(Path::new(dir), mid, cc, DEFAULT_RESERVOIR_K),
        None => summary::empty(mid, cc),
    }
}

/// DOWNLOAD the shared summary, MERGE it with this machine's local store, and build the plan from
/// the result. Returns `None` to fall back to the normal CSV-feedback plan (sync off / a backend
/// failure). A backend failure degrades LOUDLY without failing the run. Mirrors `_sync_seed_plan`.
#[allow(clippy::too_many_arguments)] // Mirrors the plan inputs without hiding distinct budgets.
fn sync_seed_plan(
    cfg: &DagConfig,
    backend: Option<&dyn SyncBackend>,
    feedback_dir: Option<&str>,
    planner: Planner,
    do_download: bool,
    core_budget: Option<i64>,
    mem_budget: Option<i64>,
    max_steps: Option<i64>,
) -> Option<Plan> {
    let backend = backend?;
    if !do_download {
        return None;
    }
    let (mid, cc) = feedback_identity();
    let downloaded = match backend.download(&mid, &cc) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "{PROG}: --profile-sync: download failed, planning from local store only ({e})"
            );
            return None;
        }
    };
    let local = local_store_summary(feedback_dir, &mid, &cc);
    let seed = match summary::merge(
        &downloaded,
        &local,
        DEFAULT_RESERVOIR_K,
        DEFAULT_MAX_BUCKETS,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{PROG}: --profile-sync: {e}; planning from local store only");
            return None;
        }
    };
    let (buckets, total, _largest) = summary::summary_stats(&seed);
    eprintln!(
        "{PROG}: --profile-sync: seeded planner from {} ({buckets} buckets, {total} samples for {mid}/{cc})",
        backend.describe()
    );
    Some(build_plan_from_summary(
        cfg,
        &seed,
        planner,
        core_budget,
        mem_budget,
        max_steps,
    ))
}

// Merge THIS run's per-step samples into the shared summary and publish them. Degrades LOUDLY on
// failure (a warning) so the run's exit code is preserved but the skip is never silent. Mirrors
// Python's `_sync_upload`.
fn rows_with_observation_ids(
    rows: &[std::collections::BTreeMap<String, String>],
    run_id: &str,
) -> Vec<std::collections::HashMap<String, String>> {
    rows.iter()
        .map(|row| {
            let mut copied: std::collections::HashMap<String, String> = row
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let observation_id = row
                .get("observation_id")
                .filter(|value| !value.is_empty())
                .or_else(|| row.get("run_id").filter(|value| !value.is_empty()))
                .map_or_else(|| run_id.to_string(), Clone::clone);
            copied.insert("observation_id".to_string(), observation_id);
            copied
        })
        .collect()
}

fn sync_upload(
    backend: &dyn SyncBackend,
    rows: &[std::collections::BTreeMap<String, String>],
    local_run_id: Option<&str>,
) {
    let (mid, cc) = feedback_identity();
    let minted_run_id;
    let observation_run_id = match local_run_id {
        Some(run_id) => run_id,
        None => {
            minted_run_id = crate::perflog::new_run_id();
            &minted_run_id
        }
    };
    let hash_rows = rows_with_observation_ids(rows, observation_run_id);
    let affinity = crate::estimates::affinity_width(&cc);
    let delta = summary::summary_from_rows(
        &hash_rows,
        &mid,
        &cc,
        affinity,
        DEFAULT_RESERVOIR_K,
        DEFAULT_MAX_BUCKETS,
    );
    match backend.publish(&delta, DEFAULT_RESERVOIR_K, DEFAULT_MAX_BUCKETS) {
        Ok(merged) => {
            let (buckets, total, largest) = summary::summary_stats(&merged);
            eprintln!(
                "{PROG}: --profile-sync: published this run's samples to {} (summary now {buckets} buckets, {total} samples, <= {largest}/bucket)",
                backend.describe()
            );
        }
        Err(e) => eprintln!("{PROG}: --profile-sync: upload failed ({e})"),
    }
}

// Resolve the planning budgets. A run passes its already-resolved `--max-cpus` value for every
// planner so `--show-plan` cannot recommend an inner width the outer cgroup will throttle. The
// standalone ordering-only planners retain `None` and do no cgroup/proc reads; CPA still needs a
// default machine budget for allocation. Memory affects CPA allocation only. Mirrors Python's
// `_planning_budgets`.
fn planning_budgets(
    planner: Planner,
    max_mem: Option<&str>,
    max_cpus: Option<i64>,
) -> (Option<i64>, Option<i64>) {
    planning_budgets_with_memory(planner, max_mem, max_cpus, None)
}

fn planning_budgets_with_memory(
    planner: Planner,
    max_mem: Option<&str>,
    max_cpus: Option<i64>,
    effective_max_mem_bytes: Option<i64>,
) -> (Option<i64>, Option<i64>) {
    let core_budget = match max_cpus {
        Some(value) => Some(value.max(1)),
        None if planner == Planner::Cpa => Some(container_core_budget().max(1)),
        None => None,
    };
    let mem_budget = (planner == Planner::Cpa)
        .then(|| {
            effective_max_mem_bytes
                .or_else(|| max_mem.filter(|s| !s.is_empty()).and_then(parse_size))
        })
        .flatten();
    (core_budget, mem_budget)
}

/// Print one visible line naming where profile CSVs were appended (No Silent Failure).
///
/// Lists EXACTLY the files this run/sweep wrote (the deterministic `store_paths` set, filtered to
/// files that exist), not a glob of the whole store — a persistent store also holds prior runs'
/// other-`container_class` CSVs for the same machine, and globbing would over-report files this run
/// never touched.
fn report_profile_written(perf_dir: &str, source: &str) {
    let mut csvs: Vec<String> = crate::perflog::store_paths(Path::new(perf_dir))
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| p.display().to_string())
        .collect();
    csvs.sort();
    if csvs.is_empty() {
        eprintln!("{PROG}: WARNING no profile CSVs were written under {perf_dir}");
        return;
    }
    if source == "--output-dir" {
        eprintln!("{PROG}: profile artifacts written under {perf_dir}:");
    } else if source == "--perf-dir" {
        eprintln!("{PROG}: perf CSVs written under {perf_dir}:");
    } else {
        let origin = match source {
            "default" => "default profile store".to_string(),
            "env" => format!("profile store (${PROFILE_DIR_ENV})"),
            other => format!("profile store ({other})"),
        };
        eprintln!(
            "{PROG}: profile data appended to the {origin} at {perf_dir} (override with --perf-dir \
             or ${PROFILE_DIR_ENV}; disable with --no-profile):"
        );
    }
    for path in csvs {
        eprintln!("  {path}");
    }
}

/// Refresh the inspectable derived model from the authoritative raw profile CSV.
fn refresh_scaling_model(perf_dir: &str, cfg: &DagConfig) {
    let machine = crate::perflog::machine_id();
    let container = crate::perflog::container_class();
    let expected: HashMap<String, String> = cfg
        .steps
        .iter()
        .map(|step| {
            (
                step.tag(),
                workload_digest(step, &cfg.default_jobs_flag, &cfg.default_jobs_env),
            )
        })
        .collect();
    let speedups =
        load_step_speedups_for_workloads(Path::new(perf_dir), &machine, &container, &expected);
    match write_scaling_model_for_workloads(
        Path::new(perf_dir),
        &machine,
        &container,
        &speedups,
        Some(&expected),
    ) {
        Ok(path) => eprintln!(
            "{PROG}: derived scaling model refreshed at {} (rebuildable from the profile CSVs)",
            path.display()
        ),
        Err(error) => eprintln!(
            "{PROG}: WARNING could not refresh derived scaling model under {perf_dir}: {error}"
        ),
    }
}

// --------------------------------------------------------------------------- selected steps

/// Split a comma-separated `group.job` tag list, dropping empty entries.
fn parse_tag_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Return the exactly named steps and, by default, every dependency they require.
///
/// `ignore_selected_deps` keeps only the named steps and drops dependency edges to steps outside
/// the selection because their outputs are assumed already present. Edges among selected steps are
/// preserved. Registration order is preserved in both modes, matching `run --selected`.
///
/// The source configuration is never modified. Its complete non-step policy is cloned into the
/// returned configuration. Returns an error when the request is empty or any tag is unknown.
pub fn select_steps_by_tags(
    cfg: &DagConfig,
    tags: &[String],
    ignore_selected_deps: bool,
) -> Result<DagConfig, String> {
    if tags.is_empty() {
        return Err("--selected requires at least one tag".to_string());
    }
    let by_tag: HashSet<String> = cfg.steps.iter().map(Step::tag).collect();
    let unknown: Vec<String> = tags
        .iter()
        .filter(|tag| !by_tag.contains(*tag))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        let mut known: Vec<String> = by_tag.iter().cloned().collect();
        known.sort();
        let known_s = if known.is_empty() {
            "(none)".to_string()
        } else {
            known.join(", ")
        };
        return Err(format!(
            "--selected: unknown step tag(s): {}. Known tags: {known_s}",
            unknown.join(", ")
        ));
    }
    let mut selected: HashSet<String> = tags.iter().cloned().collect();
    if !ignore_selected_deps {
        let dependencies = transitive_deps(&cfg.steps);
        for tag in tags {
            selected.extend(
                dependencies
                    .get(tag)
                    .expect("selected tags were validated against the DAG")
                    .iter()
                    .cloned(),
            );
        }
    }
    let mut new_cfg = cfg.clone();
    new_cfg.steps = cfg
        .steps
        .iter()
        .filter(|s| selected.contains(&s.tag()))
        .map(|s| {
            let mut step = s.clone();
            if ignore_selected_deps {
                step.deps.retain(|dep| selected.contains(dep));
            }
            step
        })
        .collect();
    Ok(new_cfg)
}

/// Return every step carrying any requested label and its dependency ancestry.
///
/// An empty label list selects no steps, preserving the original public-library behavior. The CLI
/// rejects an empty `--labels` value before calling this function.
pub fn select_steps_by_labels(cfg: &DagConfig, labels: &[String]) -> Result<DagConfig, String> {
    if labels.is_empty() {
        return Ok(cfg.with_steps(Vec::new()));
    }
    let known: HashSet<String> = cfg
        .steps
        .iter()
        .flat_map(|step| step.labels.iter().cloned())
        .collect();
    let unknown: Vec<String> = labels
        .iter()
        .filter(|label| !known.contains(*label))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        let mut known = known.into_iter().collect::<Vec<_>>();
        known.sort();
        let known = if known.is_empty() {
            "(none)".to_string()
        } else {
            known.join(", ")
        };
        return Err(format!(
            "--labels: unknown label(s): {}. Known labels: {known}",
            unknown.join(", ")
        ));
    }
    let tags = cfg
        .steps
        .iter()
        .filter(|step| labels.iter().any(|label| step.labels.contains(label)))
        .map(Step::tag)
        .collect::<Vec<_>>();
    select_steps_by_tags(cfg, &tags, false)
}

// --------------------------------------------------------------------------- --args / --stress

const ARGS_PLACEHOLDER: &str = "{args}";

/// Substitute passthrough arguments only in commands that explicitly opt in with `{args}`.
fn apply_passthrough_args(cfg: &DagConfig, args: Option<&str>) -> Result<DagConfig, String> {
    let declared = cfg
        .steps
        .iter()
        .any(|step| step.cmd.contains(ARGS_PLACEHOLDER));
    if args.is_some() && !declared {
        return Err(format!(
            "--args was given but no selected step declares the '{ARGS_PLACEHOLDER}' placeholder \
             in its cmd. Add {{args}} to the step's cmd where the extra args should go, or scope \
             the selection (--selected) to a step that accepts them."
        ));
    }
    if !declared {
        return Ok(cfg.clone());
    }
    let replacement = args.unwrap_or("");
    let mut out = cfg.clone();
    for step in &mut out.steps {
        if step.cmd.contains(ARGS_PLACEHOLDER) {
            step.cmd = step
                .cmd
                .replace(ARGS_PLACEHOLDER, replacement)
                .trim()
                .to_string();
        }
    }
    Ok(out)
}

fn stress_suffix(index: i64, count: i64) -> String {
    let width = count.to_string().len();
    format!("#{index:0width$}")
}

/// Duplicate a selected graph into disconnected copies with no edges between copies.
///
/// The top-level policy travels with the expanded step list by construction, via
/// [`DagConfig::with_steps`] — this is one of the two places in the product that rebuilds a
/// `DagConfig` around new steps, and writing it as a literal is how the dropped-field bug in
/// #21 scarce-resource-deadlock gets written every time. `resource_caps` is then cleared
/// DELIBERATELY and visibly: a stress expansion removes named-resource scheduling so `-j`
/// governs concurrency.
fn expand_stress(cfg: &DagConfig, n: i64) -> DagConfig {
    if n <= 1 {
        return cfg.clone();
    }
    let mut steps: Vec<Step> = Vec::new();
    for index in 1..=n {
        let suffix = stress_suffix(index, n);
        for original in &cfg.steps {
            let mut step = original.clone();
            step.job.push_str(&suffix);
            step.deps = step
                .deps
                .iter()
                .map(|dependency| format!("{dependency}{suffix}"))
                .collect();
            step.hint.resources.clear();
            steps.push(step);
        }
    }
    let mut out = cfg.with_steps(steps);
    out.resource_caps.clear();
    out
}

fn stress_expansion_guard(cfg: &DagConfig, n: i64) -> i32 {
    // Empty graphs count one control-plane unit per copy so an enormous n cannot burn time in the
    // expansion loop without creating nodes.
    let nodes_per_copy = i64::try_from(cfg.steps.len()).unwrap_or(i64::MAX).max(1);
    let generated = nodes_per_copy.saturating_mul(n);
    if generated <= MAX_STRESS_GENERATED_NODES {
        return 0;
    }
    eprintln!(
        "{PROG}: --stress {n}: REFUSED — expansion would create {generated} generated DAG \
         nodes/control units, exceeding safety limit {MAX_STRESS_GENERATED_NODES}; narrow \
         --selected or lower --stress"
    );
    2
}

fn stress_footprints(cfg: &DagConfig, n: i64, expanded: bool) -> (i64, i64) {
    let footprint = stress_copy_footprint_bytes(cfg, None);
    let total = if expanded {
        footprint.max(stress_control_floor_bytes(cfg, None).saturating_mul(n))
    } else {
        footprint.saturating_mul(n)
    };
    (footprint, total)
}

fn stress_memory_guard(
    cfg: &DagConfig,
    n: i64,
    expanded: bool,
    run_memory_budget_bytes: Option<i64>,
) -> i32 {
    let (footprint, total) = stress_footprints(cfg, n, expanded);
    if footprint == i64::MAX || total == i64::MAX {
        let subject = if expanded {
            "final planned expanded-graph"
        } else {
            "per-copy"
        };
        eprintln!(
            "{PROG}: --stress {n}: REFUSED — {subject} memory footprint is unbounded or \
             overflowed; declare finite positive per-step memory caps"
        );
        return 2;
    }
    let budget = [box_mem_budget_bytes(), run_memory_budget_bytes]
        .into_iter()
        .flatten()
        .min();
    let Some(budget) = budget else {
        if expanded {
            eprintln!(
                "{PROG}: --stress {n}: REFUSED — could not read the box memory budget for the \
                 final planned expanded graph; no step was started"
            );
            return 2;
        }
        eprintln!(
            "{PROG}: --stress {n}: WARNING could not read the box memory budget \
             (cgroup memory.max / MemAvailable); proceeding UNCHECKED with {n} x \
             {}/copy = {}. Watch for OOM.",
            human_bytes(Some(footprint)),
            human_bytes(Some(total))
        );
        return 0;
    };
    if expanded {
        if total > budget {
            eprintln!(
                "{PROG}: --stress {n}: REFUSED — final planned expanded graph needs {}, \
                 exceeding the box memory budget {}; no step was started",
                human_bytes(Some(total)),
                human_bytes(Some(budget)),
            );
            return 2;
        }
        return 0;
    }
    let max_safe = budget / footprint;
    if n > max_safe {
        eprintln!(
            "{PROG}: --stress {n}: REFUSED — {n} parallel copies would exceed the box memory budget.\n\
             \x20 requested copies:   {n}\n\
             \x20 per-copy footprint: {}\n\
             \x20 total needed:       {}\n\
             \x20 box memory budget:  {} (min of cgroup memory.max + MemAvailable)\n\
             \x20 max safe --stress:  {max_safe}\n\
             Re-run with --stress <= {max_safe} (cores are plentiful; memory is the binding \
             constraint), or lower the per-copy footprint via a tighter per-step \
             rss_baseline_bytes / hard_mem_max_bytes hint.",
            human_bytes(Some(footprint)),
            human_bytes(Some(total)),
            human_bytes(Some(budget)),
        );
        return 2;
    }
    eprintln!(
        "{PROG}: --stress {n}: OK — {n} x {}/copy = {} fits the box memory budget {} \
         (max safe {max_safe}); actual concurrency will be measured from child-process \
         lifetimes.",
        human_bytes(Some(footprint)),
        human_bytes(Some(total)),
        human_bytes(Some(budget)),
    );
    0
}

fn stress_guard_with_budget(cfg: &DagConfig, n: i64, budget: Option<i64>) -> i32 {
    stress_memory_guard(cfg, n, false, budget)
}

fn final_stress_guard_with_budget(cfg: &DagConfig, n: i64, budget: Option<i64>) -> i32 {
    stress_memory_guard(cfg, n, true, budget)
}

fn print_stress_report(
    rows: &[StepOutcome],
    n: i64,
    max_concurrent_steps: usize,
    max_steps: i64,
    max_cpus: i64,
    c: &Palette,
) {
    let mut groups: BTreeMap<String, Vec<(String, &StepOutcome)>> = BTreeMap::new();
    for outcome in rows {
        let (base, index) = match outcome.tag.rsplit_once('#') {
            Some((base, index)) => (base.to_string(), index.to_string()),
            None => (outcome.tag.clone(), String::new()),
        };
        groups.entry(base).or_default().push((index, outcome));
    }
    println!(
        "{}",
        c.bold(&format!("stress results ({n} generated graph copies):"))
    );
    for (base, mut items) in groups {
        items.sort_by(|left, right| left.0.cmp(&right.0));
        let passed = items.iter().filter(|(_, outcome)| outcome.ok).count();
        let total = items.len();
        let failed: Vec<String> = items
            .iter()
            .filter(|(_, outcome)| !outcome.ok && !outcome.aborted)
            .map(|(index, _)| index.clone())
            .collect();
        let aborted: Vec<String> = items
            .iter()
            .filter(|(_, outcome)| outcome.aborted)
            .map(|(index, _)| index.clone())
            .collect();
        let ratio = format!("{passed}/{total} passed");
        let styled = if passed == total {
            c.green(&ratio)
        } else {
            c.red(&ratio)
        };
        let mut detail = String::new();
        if !failed.is_empty() {
            detail.push_str(" — ");
            detail.push_str(&c.red(&format!(
                "{} FAILED: {}",
                failed.len(),
                failed
                    .iter()
                    .map(|index| format!("#{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        if !aborted.is_empty() {
            detail.push_str(" — ");
            detail.push_str(&c.yellow(&format!(
                "{} aborted: {}",
                aborted.len(),
                aborted
                    .iter()
                    .map(|index| format!("#{index}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        println!("  {base}: {styled}{detail}");
    }
    println!(
        "  maximum concurrent steps: {max_concurrent_steps} (--max-steps {max_steps}; --max-cpus \
         {max_cpus} CPU target/per-step ceiling)"
    );
}

// --------------------------------------------------------------------------- table rendering

/// Human-readable byte count (e.g. `3.5 GiB`); `-` when unknown.
fn human_bytes(n: Option<i64>) -> String {
    let mut value = match n {
        Some(v) => v as f64,
        None => return "-".to_string(),
    };
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 || unit == "TiB" {
            return if unit == "B" {
                format!("{} B", value as i64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{} B", n.unwrap_or(0))
}

/// Render a fixed-width table: the first column left-aligned, the rest right-aligned.
fn render_table(headers: &[String], rows: &[Vec<String>], c: &Palette) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt_row = |cells: &[String]| -> String {
        let mut parts: Vec<String> = Vec::with_capacity(cols);
        for (i, cell) in cells.iter().enumerate() {
            let w = widths[i];
            if i == 0 {
                parts.push(format!("{cell:<w$}"));
            } else {
                parts.push(format!("{cell:>w$}"));
            }
        }
        parts.join("  ")
    };
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    let mut out = vec![c.bold(&fmt_row(headers)), c.dim(&sep.join("  "))];
    for row in rows {
        out.push(fmt_row(row));
    }
    out.join("\n")
}

fn cell_secs(value: Option<&String>) -> String {
    match value.and_then(|s| s.parse::<f64>().ok()) {
        Some(v) => format!("{v:.3}"),
        None => "-".to_string(),
    }
}

fn cell_secs_from_usec(value: Option<&String>) -> String {
    match value.and_then(|s| s.parse::<i64>().ok()) {
        Some(v) => format!("{:.3}", v as f64 / 1e6),
        None => "-".to_string(),
    }
}

fn cell_bytes(value: Option<&String>) -> String {
    match value.and_then(|s| s.parse::<i64>().ok()) {
        Some(v) => human_bytes(Some(v)),
        None => "-".to_string(),
    }
}

/// Print the per-step profile table (Feature C) to stdout.
///
/// Columns: step | wall_s | user_s | sys_s | rss_hwm | oom | inner_jobs. `user_s`/`sys_s` come
/// from the per-step cgroup `cpu.stat` (present only under boxing) and `rss_hwm` from the step
/// cgroup `memory.peak`; each shows `-` when unavailable (an un-boxed run).
fn print_profile_table(rows: &[BTreeMap<String, String>], c: &Palette) {
    if rows.is_empty() {
        eprintln!("{PROG}: no per-step profile rows to show (nothing ran)");
        return;
    }
    let headers: Vec<String> = [
        "step",
        "wall_s",
        "user_s",
        "sys_s",
        "rss_hwm",
        "oom",
        "inner_jobs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut table: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        table.push(vec![
            row.get("step").cloned().unwrap_or_else(|| "?".to_string()),
            cell_secs(row.get("elapsed_s")),
            cell_secs_from_usec(row.get("cpu.user_usec")),
            cell_secs_from_usec(row.get("cpu.system_usec")),
            cell_bytes(row.get("peak_bytes")),
            row.get("oom_kills")
                .cloned()
                .unwrap_or_else(|| "0".to_string()),
            row.get("inner_jobs")
                .cloned()
                .unwrap_or_else(|| "-".to_string()),
        ]);
    }
    println!("{}", c.bold("per-step profile:"));
    println!("{}", render_table(&headers, &table, c));
}

// --------------------------------------------------------------------------- sweep

/// One measured single-step run at a given inner-parallelism width.
#[derive(Clone, Copy)]
struct SweepMeasure {
    wall_s: f64,
    user_s: f64,
    sys_s: f64,
    rss_hwm: Option<i64>,
    ok: bool,
    returncode: Option<i32>,
}

/// Provenance attached to rows produced by a target-time sweep. These are dynamic profile columns:
/// old readers ignore them, while experiment analysis can reconstruct which cumulative pass and
/// machine grid produced each sample.
struct SweepProfileMeta {
    sweep_id: String,
    pass: usize,
    sample: usize,
    repeat: i64,
    width_source: String,
    workload_digest: String,
    target_s: f64,
    topology: MachineTopology,
}

type CurrentSweepSamples = HashMap<String, Vec<(i64, SweepMeasure)>>;

fn remember_sweep_sample(
    samples: &mut CurrentSweepSamples,
    step: &str,
    inner_jobs: i64,
    measure: SweepMeasure,
) {
    samples
        .entry(step.to_string())
        .or_default()
        .push((inner_jobs, measure));
}

/// Fit capture widths from this invocation's uninstrumented samples only. Accumulated rows remain
/// valuable for the durable model, but must not silently choose where an explicitly requested
/// follow-up profiler run lands today.
fn current_sweep_speedups(samples: &CurrentSweepSamples) -> HashMap<String, StepSpeedup> {
    let mut buckets: HashMap<BucketKey, Vec<Sample>> = HashMap::new();
    for (step, observations) in samples {
        for (index, (inner_jobs, measure)) in observations.iter().enumerate() {
            let cpu_s = measure.user_s + measure.sys_s;
            buckets
                .entry((step.clone(), *inner_jobs))
                .or_default()
                .push(Sample {
                    elapsed_s: Some(measure.wall_s),
                    contention: 0.0,
                    cpu_s: Some(cpu_s),
                    effective_cores: (measure.wall_s > 0.0).then_some(cpu_s / measure.wall_s),
                    throttled_s: None,
                    peak_bytes: measure.rss_hwm,
                    uncensored_peak_bytes: measure.rss_hwm,
                    peak_floor_bytes: None,
                    observation_id: format!("current-sweep:{step}:{inner_jobs}:{index}"),
                    workload_digest: String::new(),
                });
        }
    }
    let core_budget = buckets.keys().map(|(_, width)| *width).max();
    step_speedups_from_buckets(&buckets, core_budget)
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|value| value.is_finite() && *value > 0.0);
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    })
}

fn current_sweep_selection(
    base: &Step,
    cfg: &DagConfig,
    observations: &[(i64, SweepMeasure)],
    git: &str,
) -> Result<SweetSpotSelection, String> {
    let tag = base.tag();
    let one_step = HashMap::from([(tag.clone(), observations.to_vec())]);
    let speedups = current_sweep_speedups(&one_step);
    let digest = workload_digest(base, &cfg.default_jobs_flag, &cfg.default_jobs_env);
    if let Some(speedup) = speedups.get(&tag) {
        return select_capture_sweet_spot(speedup, &digest, git);
    }

    let widths = observations
        .iter()
        .filter(|(width, measure)| *width > 0 && measure.wall_s.is_finite() && measure.wall_s > 0.0)
        .map(|(width, _)| *width)
        .collect::<HashSet<_>>();
    if widths.len() == 1 {
        let width = *widths.iter().next().expect("one width was checked");
        let wall_s = median(
            observations
                .iter()
                .filter(|(candidate, _)| *candidate == width)
                .map(|(_, measure)| measure.wall_s)
                .collect(),
        )
        .ok_or_else(|| format!("step '{tag}' has no positive wall measurement"))?;
        return Ok(SweetSpotSelection {
            step: tag,
            workload_digest: digest,
            inner_jobs: width,
            expected_wall_s: wall_s,
            baseline_inner_jobs: width,
            speedup: 1.0,
            model_wall_s: wall_s,
            raw_wall_s: Some(wall_s),
            source: "single-measured-width".to_string(),
            git_sha: git.to_string(),
        });
    }
    Err(format!(
        "step '{tag}' has insufficient valid sweep data for profiler capture"
    ))
}

fn sweep_capture_requested(args: &SweepArgs) -> bool {
    args.perf_record || !args.wprof_windows.is_empty()
}

fn capture_config(args: &SweepArgs, output_dir: &str) -> CaptureConfig {
    let mut config = CaptureConfig::new(output_dir);
    config.capture_perf = args.perf_record;
    config.perf_window_s = args.perf_window.map(|duration| duration.as_secs_f64());
    config.wprof_windows = i64::try_from(args.wprof_windows.len()).unwrap_or(i64::MAX);
    if let Some(duration) = args.wprof_windows.first() {
        config.wprof_window_s = duration.as_secs_f64();
    }
    if args.profiler_sudo {
        config.sudo = vec!["sudo".to_string(), "-n".to_string()];
    }
    config
}

/// Produce the exact guest command before placing a profiler executable in front of it. The
/// scheduler normally injects cmdtype/jobs channels immediately before spawn; the wrapper must
/// materialize those channels first or an appended `--jobs` would accidentally target perf.
fn profiler_wrapped_step(base: &Step, cfg: &DagConfig, request: &IsolatedTrialRequest) -> Step {
    let width = Some(request.inner_jobs);
    let mut guest_command =
        crate::model::command_with_inner_jobs(base, &cfg.default_jobs_flag, width);
    let jobs_env = crate::model::env_with_inner_jobs(base, &cfg.default_jobs_env, width);
    if let Some((name, value)) = &jobs_env {
        guest_command = format!(
            "{}{guest_command}",
            crate::scheduler::checked_jobs_env_export(name, value)
        );
    }
    let ready_path = box_shell_quote(&request.guest_launch_ready_path().to_string_lossy());
    let release_path = box_shell_quote(&request.guest_launch_release_path().to_string_lossy());
    let guest_command = format!(
        "if ! printf '%s\\n' \"$$\" > {ready_path}; then\n  \
         echo 'dagrun: profiler guest-launch notification failed' >&2\n  exit 125\nfi\n\
         if ! IFS= read -r _ < {release_path}; then\n  \
         echo 'dagrun: profiler guest-launch release failed' >&2\n  exit 125\nfi\n\
         {guest_command}"
    );
    let mut injected = request.env.clone();
    if let Some((name, _)) = &jobs_env {
        injected.remove(name);
    }
    if let Some((name, value)) = crate::model::cmdtype_env_with_inner_jobs(base, width) {
        injected.insert(name, value);
    }

    let mut argv = request.argv_prefix.clone();
    if !injected.is_empty() || jobs_env.is_some() {
        argv.push("env".to_string());
        // The outer wrapper opts out. Remove this channel again before its real inner Bash,
        // then inject it exactly once through the checked final guest guard.
        if let Some((name, _)) = &jobs_env {
            argv.extend(["-u".to_string(), name.clone()]);
        }
        argv.extend(
            injected
                .into_iter()
                .map(|(name, value)| format!("{name}={value}")),
        );
    }
    argv.extend(["bash".to_string(), "-c".to_string(), guest_command]);

    let mut wrapped = base.clone();
    wrapped.cmd = argv
        .iter()
        .map(|part| box_shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ");
    wrapped.cmdtype = crate::model::CmdType::Unknown;
    wrapped.jobs_flag = Some(String::new());
    wrapped.jobs_env = Some(String::new());
    wrapped.hint.preferred_inner_jobs = Some(request.inner_jobs);
    wrapped
}

#[allow(clippy::too_many_arguments)]
fn run_profiler_trial(
    base: &Step,
    cfg: &DagConfig,
    selected_inner_jobs: i64,
    request: &IsolatedTrialRequest,
    cgroups: &BoxedCgroups,
    git: &str,
    verbosity: i64,
) -> Result<IsolatedTrialResult, String> {
    if request.include_in_model {
        return Err("profiler trial unexpectedly requested model inclusion".to_string());
    }
    if request.step != base.tag() || request.inner_jobs != selected_inner_jobs {
        return Err("profiler callback received a mismatched step or width".to_string());
    }
    let step = profiler_wrapped_step(base, cfg, request);
    let measure = run_isolated_step(
        &step,
        cfg,
        Some(request.inner_jobs),
        request.inner_jobs,
        1,
        cgroups,
        None,
        git,
        verbosity,
        None,
        None,
    );
    Ok(IsolatedTrialResult {
        returncode: measure.returncode.unwrap_or(if measure.ok { 0 } else { 1 }),
        wall_s: measure.wall_s,
        detail: if measure.ok {
            String::new()
        } else {
            format!(
                "step '{}' failed under {}",
                request.step,
                request.kind.value()
            )
        },
    })
}

struct SweepProfilerRunner<'a> {
    base: &'a Step,
    cfg: &'a DagConfig,
    cgroups: &'a BoxedCgroups,
    git: &'a str,
    verbosity: i64,
    selected_inner_jobs: i64,
}

impl RunIsolatedTrial for SweepProfilerRunner<'_> {
    fn run_isolated_trial(
        &mut self,
        request: &IsolatedTrialRequest,
    ) -> Result<IsolatedTrialResult, String> {
        run_profiler_trial(
            self.base,
            self.cfg,
            self.selected_inner_jobs,
            request,
            self.cgroups,
            self.git,
            self.verbosity,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_requested_sweep_captures(
    args: &SweepArgs,
    cfg: &DagConfig,
    selected: &[usize],
    samples: &CurrentSweepSamples,
    cgroups: &BoxedCgroups,
    output_dir: &str,
    git: &str,
) -> Result<Vec<PathBuf>, String> {
    if !sweep_capture_requested(args) {
        return Ok(Vec::new());
    }
    let config = capture_config(args, output_dir);
    let mut manifests = Vec::new();
    for index in selected {
        let base = &cfg.steps[*index];
        if base.skip_reason.is_some() {
            continue;
        }
        let tag = base.tag();
        let Some(observations) = samples.get(&tag) else {
            // A fixed step can be deliberately omitted when its configured width exceeds the
            // effective machine budget. No process ran, so there is no evidence-backed capture
            // width and nothing to profile.
            continue;
        };
        let selection = current_sweep_selection(base, cfg, observations, git)?;
        println!(
            "sweep: {tag}: profiler capture at sweet-spot width {} (expected wall {:.3}s)",
            selection.inner_jobs, selection.expected_wall_s,
        );
        let runner = SweepProfilerRunner {
            base,
            cfg,
            cgroups,
            git,
            verbosity: args.verbosity,
            selected_inner_jobs: selection.inner_jobs,
        };
        match capture_at_sweet_spot(&selection, &config, runner) {
            Ok(manifest) => {
                println!(
                    "sweep: {tag}: profiler capture manifest: {}",
                    manifest.path.display()
                );
                manifests.push(manifest.path);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(manifests)
}

/// Parse a sweep width range: `"LO..HI"` or a bare `"N"` (meaning `1..N`).
fn parse_jobs_range(raw: &str) -> Result<(i64, i64), String> {
    let text = raw.trim();
    let (lo, hi) = match text.split_once("..") {
        Some((a, b)) => {
            let lo = a
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs range '{raw}': not an integer"))?;
            let hi = b
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs range '{raw}': not an integer"))?;
            (lo, hi)
        }
        None => {
            let n = text
                .parse::<i64>()
                .map_err(|_| format!("invalid --jobs '{raw}': not an integer"))?;
            (1, n)
        }
    };
    if lo < 1 || hi < lo {
        return Err(format!("invalid --jobs range '{raw}': need 1 <= LO <= HI"));
    }
    Ok((lo, hi))
}

/// Run ONE step at a fixed inner-parallelism width and measure it (see [`cmd_sweep`]).
#[allow(clippy::too_many_arguments)]
fn run_single_step(
    base: &Step,
    cfg: &DagConfig,
    inner_jobs: i64,
    cgroups: &BoxedCgroups,
    perf_dir: Option<&str>,
    git: &str,
    verbosity: i64,
    profile_timeseries_interval: Option<Duration>,
) -> SweepMeasure {
    run_isolated_step(
        base,
        cfg,
        Some(inner_jobs),
        inner_jobs,
        1,
        cgroups,
        perf_dir,
        git,
        verbosity,
        None,
        profile_timeseries_interval,
    )
}

/// Run a node alone, optionally overriding its guest-visible width.
#[allow(clippy::too_many_arguments)]
fn run_isolated_step(
    base: &Step,
    cfg: &DagConfig,
    inner_jobs: Option<i64>,
    max_cpus: i64,
    profile_outer_jobs: i64,
    cgroups: &BoxedCgroups,
    perf_dir: Option<&str>,
    git: &str,
    verbosity: i64,
    sweep_meta: Option<&SweepProfileMeta>,
    profile_timeseries_interval: Option<Duration>,
) -> SweepMeasure {
    let mut step = base.clone();
    step.deps.clear();
    if let Some(inner_jobs) = inner_jobs {
        step.hint.preferred_inner_jobs = Some(inner_jobs);
    }
    let mut one = cfg.clone();
    one.steps = vec![step];

    let (u0, s0) = child_cpu_seconds();
    let window = perf_dir.map(|d| PerfWindow::start(Path::new(d), git));
    let start = Instant::now();
    let mut result = run_dag_boxed_limited_with_timeseries(
        &one,
        1,
        max_cpus.max(1),
        false,
        verbosity,
        cgroups.clone(),
        profile_timeseries_interval,
    );
    let measured = start.elapsed().as_secs_f64();
    let (u1, s1) = child_cpu_seconds();
    let measured_user_s = (u1 - u0).max(0.0);
    let measured_sys_s = (s1 - s0).max(0.0);
    if let Some(w) = &window {
        w.finish(
            if result.ok { "pass" } else { "fail" },
            result.outcomes.len(),
            profile_outer_jobs.max(1),
        );
    }
    for row in &mut result.step_profile_rows {
        if row.get("user_s").is_none_or(|value| value.is_empty()) {
            row.insert("user_s".into(), format!("{measured_user_s:.3}"));
        }
        if row.get("sys_s").is_none_or(|value| value.is_empty()) {
            row.insert("sys_s".into(), format!("{measured_sys_s:.3}"));
        }
        if let Some(meta) = sweep_meta {
            row.insert("sweep_mode".into(), "target-time".into());
            row.insert("sweep_id".into(), meta.sweep_id.clone());
            row.insert("sweep_pass".into(), meta.pass.to_string());
            row.insert("sweep_sample".into(), meta.sample.to_string());
            row.insert("sweep_repeat".into(), meta.repeat.to_string());
            row.insert("sweep_width_source".into(), meta.width_source.clone());
            row.insert("workload_digest".into(), meta.workload_digest.clone());
            row.insert("sweep_target_s".into(), format!("{:.6}", meta.target_s));
            row.insert(
                "sweep_physical_cores".into(),
                meta.topology.physical_cores.to_string(),
            );
            row.insert(
                "sweep_logical_cpus".into(),
                meta.topology.logical_cpus.to_string(),
            );
        }
    }
    if let Some(meta) = sweep_meta {
        for row in &mut result.step_timeseries_rows {
            row.insert("sweep_mode".into(), "target-time".into());
            row.insert("sweep_id".into(), meta.sweep_id.clone());
            row.insert("sweep_pass".into(), meta.pass.to_string());
            row.insert("sweep_sample".into(), meta.sample.to_string());
            row.insert("sweep_repeat".into(), meta.repeat.to_string());
            row.insert("sweep_width_source".into(), meta.width_source.clone());
            row.insert("workload_digest".into(), meta.workload_digest.clone());
            row.insert("sweep_target_s".into(), format!("{:.6}", meta.target_s));
            row.insert(
                "sweep_physical_cores".into(),
                meta.topology.physical_cores.to_string(),
            );
            row.insert(
                "sweep_logical_cpus".into(),
                meta.topology.logical_cpus.to_string(),
            );
        }
    }
    if let Some(d) = perf_dir {
        let profile_run_id = crate::perflog::new_run_id();
        append_step_profiles(
            Path::new(d),
            &result.step_profile_rows,
            git,
            profile_outer_jobs.max(1),
            None,
            profile_enforcement_kind(cgroups.is_some()),
            "sweep",
            Some(&profile_run_id),
        );
        if profile_timeseries_interval.is_some() {
            if let Some(path) = append_step_timeseries(
                Path::new(d),
                &result.step_timeseries_rows,
                git,
                profile_outer_jobs.max(1),
                None,
                profile_enforcement_kind(cgroups.is_some()),
                "sweep",
                &profile_run_id,
            ) {
                eprintln!("[scheduler] step time-series written to {}", path.display());
            }
        }
    }
    let row = result.step_profile_rows.first();
    let wall_s = row
        .and_then(|r| r.get("elapsed_s"))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(measured);
    let rss_hwm = row
        .and_then(|r| r.get("peak_bytes"))
        .and_then(|s| s.parse::<i64>().ok());
    let returncode = result
        .outcomes
        .first()
        .and_then(|outcome| outcome.returncode)
        .and_then(|returncode| i32::try_from(returncode).ok());
    SweepMeasure {
        wall_s,
        user_s: measured_user_s,
        sys_s: measured_sys_s,
        rss_hwm,
        ok: result.ok,
        returncode,
    }
}

/// Profile provenance for a sweep execution. Sweep rows describe the containment that actually
/// wrapped the measured step, rather than inheriting the generic writer's unknown/default value.
fn profile_enforcement_kind(cgroup_manager_present: bool) -> &'static str {
    if cgroup_manager_present {
        "cgroup-v2"
    } else {
        "unboxed"
    }
}

/// Print the parallel-speedup sweep table (Feature B) to stdout.
fn print_sweep_table(
    step: &str,
    baseline_jobs: i64,
    measures: &[(i64, SweepMeasure)],
    c: &Palette,
) {
    let baseline_wall = measures.first().map(|(_, m)| m.wall_s).unwrap_or(0.0);
    let headers: Vec<String> = vec![
        "jobs".to_string(),
        "wall_s".to_string(),
        "user_s".to_string(),
        "sys_s".to_string(),
        "rss_hwm".to_string(),
        format!("speedup(vs j{baseline_jobs})"),
    ];
    let mut table: Vec<Vec<String>> = Vec::with_capacity(measures.len());
    for (jobs, m) in measures {
        let speedup = if m.wall_s > 0.0 {
            format!("{:.2}x", baseline_wall / m.wall_s)
        } else {
            "-".to_string()
        };
        table.push(vec![
            jobs.to_string(),
            format!("{:.3}", m.wall_s),
            format!("{:.3}", m.user_s),
            format!("{:.3}", m.sys_s),
            human_bytes(m.rss_hwm),
            speedup,
        ]);
    }
    println!("{}", c.bold(&format!("parallel-speedup sweep: {step}")));
    println!("{}", render_table(&headers, &table, c));
}

fn print_fixed_sweep_measure(step: &str, measure: SweepMeasure, c: &Palette) {
    let headers = vec![
        "jobs".to_string(),
        "wall_s".to_string(),
        "user_s".to_string(),
        "sys_s".to_string(),
        "rss_hwm".to_string(),
        "speedup".to_string(),
    ];
    let table = vec![vec![
        "fixed".to_string(),
        format!("{:.3}", measure.wall_s),
        format!("{:.3}", measure.user_s),
        format!("{:.3}", measure.sys_s),
        human_bytes(measure.rss_hwm),
        "-".to_string(),
    ]];
    println!("{}", c.bold(&format!("parallel-speedup sweep: {step}")));
    println!("{}", render_table(&headers, &table, c));
}

fn target_width_source(
    width: i64,
    initial_widths: &[i64],
    topology: MachineTopology,
    explicit: bool,
) -> String {
    if !initial_widths.contains(&width) {
        return "midpoint".to_string();
    }
    if explicit {
        return "explicit".to_string();
    }
    let mut sources = Vec::new();
    if width > 0 && (width & (width - 1)) == 0 && width <= topology.physical_cores {
        sources.push("power-of-two");
    }
    if width == topology.physical_cores {
        sources.push("physical-cores");
    }
    if width == topology.logical_cpus {
        sources.push("logical-threads");
    }
    if sources.is_empty() {
        sources.push("midpoint");
    }
    sources.join("+")
}

#[derive(Debug)]
struct SweepArgs {
    dag: Option<String>,
    step: Option<String>,
    jobs: Option<String>,
    target_time: Option<String>,
    repeat: i64,
    output_dir: Option<String>,
    perf_dir: Option<String>,
    no_profile: bool,
    report_html: Option<String>,
    no_report: bool,
    profile_timeseries: Option<Duration>,
    perf_record: bool,
    perf_window: Option<Duration>,
    wprof_windows: Vec<Duration>,
    profiler_sudo: bool,
    allow_cgroup_failure: bool,
    unsafe_no_cgroups: bool,
    verbosity: i64,
}

fn parse_sweep_args(rest: &[String]) -> Result<SweepArgs, String> {
    let mut a = SweepArgs {
        dag: None,
        step: None,
        jobs: None,
        target_time: None,
        repeat: 1,
        output_dir: None,
        perf_dir: None,
        no_profile: false,
        report_html: None,
        no_report: false,
        profile_timeseries: None,
        perf_record: false,
        perf_window: None,
        wprof_windows: Vec::new(),
        profiler_sudo: false,
        allow_cgroup_failure: false,
        unsafe_no_cgroups: false,
        verbosity: 0,
    };
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let take_value = |inline: Option<String>, i: &mut usize| -> Result<String, String> {
            if let Some(v) = inline {
                Ok(v)
            } else {
                *i += 1;
                rest.get(*i)
                    .cloned()
                    .ok_or_else(|| format!("the argument {key} requires a value"))
            }
        };
        match key.as_str() {
            "--dag" => a.dag = Some(take_value(inline, &mut i)?),
            "--step" => a.step = Some(take_value(inline, &mut i)?),
            "--jobs" => a.jobs = Some(take_value(inline, &mut i)?),
            "--target-time" => a.target_time = Some(take_value(inline, &mut i)?),
            "--repeat" => {
                let v = take_value(inline, &mut i)?;
                a.repeat = v
                    .parse::<i64>()
                    .map_err(|_| format!("--repeat: invalid int value: '{v}'"))?;
            }
            "--output-dir" => a.output_dir = Some(take_value(inline, &mut i)?),
            "--perf-dir" => a.perf_dir = Some(take_value(inline, &mut i)?),
            "--no-profile" => a.no_profile = true,
            "--report-html" => a.report_html = Some(take_value(inline, &mut i)?),
            "--no-report" => a.no_report = true,
            "--profile-timeseries" => {
                let value = take_value(inline, &mut i)?;
                a.profile_timeseries = Some(parse_profile_timeseries_duration(&value)?);
            }
            "--perf-record" => a.perf_record = true,
            "--perf-window" => {
                let value = take_value(inline, &mut i)?;
                a.perf_window = Some(parse_profiler_window_duration(&value, "--perf-window")?);
                a.perf_record = true;
            }
            "--wprof-window" => {
                let value = take_value(inline, &mut i)?;
                a.wprof_windows
                    .push(parse_profiler_window_duration(&value, "--wprof-window")?);
            }
            "--profiler-sudo" => a.profiler_sudo = true,
            "--allow-cgroup-failure" => a.allow_cgroup_failure = true,
            "--unsafe-no-cgroups" => a.unsafe_no_cgroups = true,
            "-v" => a.verbosity += 1,
            compact
                if compact.strip_prefix('-').is_some_and(|letters| {
                    letters.len() > 1 && letters.bytes().all(|b| b == b'v')
                }) =>
            {
                a.verbosity += i64::try_from(compact.len() - 1).unwrap_or(i64::MAX);
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
        i += 1;
    }
    if a.no_profile && a.profile_timeseries.is_some() {
        return Err("--profile-timeseries cannot be combined with --no-profile".to_string());
    }
    if a.output_dir
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("--output-dir must not be empty".to_string());
    }
    if a.perf_dir
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("--perf-dir must not be empty".to_string());
    }
    if let (Some(output), Some(perf)) = (&a.output_dir, &a.perf_dir) {
        if output != perf {
            return Err("--output-dir and --perf-dir disagree".to_string());
        }
    }
    if a.report_html
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("--report-html must not be empty".to_string());
    }
    if a.no_profile && a.report_html.is_some() {
        return Err("--report-html cannot be combined with --no-profile".to_string());
    }
    if a.no_report && a.report_html.is_some() {
        return Err("--report-html cannot be combined with --no-report".to_string());
    }
    if a.no_profile && (a.perf_record || !a.wprof_windows.is_empty()) {
        return Err(
            "profiler capture options cannot be combined with --no-profile; captures need an output directory"
                .to_string(),
        );
    }
    if a.profiler_sudo && !a.perf_record && a.wprof_windows.is_empty() {
        return Err(
            "--profiler-sudo requires --perf-record, --perf-window, or --wprof-window".to_string(),
        );
    }
    if let Some(first) = a.wprof_windows.first() {
        if a.wprof_windows.iter().any(|duration| duration != first) {
            return Err(
                "repeated --wprof-window durations must match; each occurrence requests another separate trial"
                    .to_string(),
            );
        }
    }
    Ok(a)
}

fn refresh_sweep_report(
    perf_dir: &str,
    cfg: &DagConfig,
    dag_arg: &str,
    report_html: Option<&str>,
    no_report: bool,
) -> bool {
    if no_report {
        return true;
    }
    let output = report_html
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(perf_dir).join(profile_report::PROFILE_REPORT_FILENAME));
    match profile_report::generate_report(
        cfg,
        Path::new(dag_arg),
        Path::new(perf_dir),
        &output,
        "dagrun profile history",
    ) {
        Ok(summary) => {
            eprintln!(
                "{PROG}: interactive profile report refreshed at {} ({} aggregate sample(s), {} time-series run(s))",
                output.display(),
                summary.aggregate_samples,
                summary.trace_series,
            );
            true
        }
        Err(error) => {
            eprintln!(
                "{PROG}: WARNING could not refresh interactive profile report at {}: {error}",
                output.display(),
            );
            false
        }
    }
}

fn sweep_memory_error(
    step: &Step,
    cfg: &DagConfig,
    width: i64,
    inherited_max_mem_bytes: Option<i64>,
) -> Option<String> {
    let parent = inherited_max_mem_bytes?;
    let cap = step_mem_cap_for_inner_jobs_optional(
        step,
        Some(width),
        cfg.mem_cap_factor,
        cfg.default_step_mem_cap_bytes,
    );
    if cap.is_some_and(|bytes| bytes <= parent) {
        return None;
    }
    let modeled = cap.map_or_else(|| "unbounded".to_string(), |bytes| format!("{bytes} bytes"));
    Some(format!(
        "step {:?} at width {width} has modeled memory cap {modeled}, exceeding inherited \
         delegated memory ceiling {parent} bytes",
        step.tag()
    ))
}

fn sweep_cpu_error(max_width: i64, inherited_max_cpus: Option<i64>) -> Option<String> {
    let parent = inherited_max_cpus?;
    (max_width > parent).then(|| {
        format!(
            "--jobs reaches {max_width}, exceeding inherited delegated CPU ceiling {parent}; \
             refusing physically throttled measurements"
        )
    })
}

fn selected_fixed_sweep_cpu_error(
    tag: &str,
    width: i64,
    effective_cpus: i64,
    selected_one: bool,
) -> Option<String> {
    (selected_one && width > effective_cpus).then(|| {
        format!(
            "selected fixed-width step '{tag}' requests {width} CPUs, exceeding the effective \
             {effective_cpus}-CPU budget; refusing an empty successful sweep"
        )
    })
}

fn target_sweep_step_resource_error(
    step: &Step,
    cfg: &DagConfig,
    max_width: i64,
    topology: MachineTopology,
    inherited_max_mem_bytes: Option<i64>,
    selected_one: bool,
) -> Option<String> {
    if step.skip_reason.is_some() {
        return None;
    }
    let resizable = step_width_is_resizable(step, &cfg.default_jobs_flag, &cfg.default_jobs_env);
    let width = if resizable {
        max_width
    } else {
        crate::model::effective_cpu_count(step, cfg.default_step_cpu_count)
            .unwrap_or(topology.logical_cpus)
            .max(1)
    };
    let tag = step.tag();
    if !resizable {
        if let Some(error) =
            selected_fixed_sweep_cpu_error(&tag, width, topology.logical_cpus, selected_one)
        {
            return Some(error);
        }
    }
    sweep_memory_error(step, cfg, width, inherited_max_mem_bytes)
        .map(|error| format!("{error}; refusing"))
}

/// Per-step parallel-speedup sweep (Feature B).
fn cmd_sweep(a: &SweepArgs, c: &Palette) -> i32 {
    if a.target_time.is_some() {
        return cmd_target_time_sweep(a, c);
    }
    let dag_arg = match &a.dag {
        Some(d) => d.clone(),
        None => {
            eprintln!("{PROG} sweep: error: the following arguments are required: --dag");
            return 2;
        }
    };
    let step_tag = match &a.step {
        Some(s) => s.clone(),
        None => {
            eprintln!("{PROG} sweep: error: the following arguments are required: --step");
            return 2;
        }
    };
    let jobs_spec = match &a.jobs {
        Some(j) => j.clone(),
        None => {
            eprintln!("{PROG} sweep: error: the following arguments are required: --jobs");
            return 2;
        }
    };
    let cfg = match load(&dag_arg) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{PROG}: {}", e.0);
            return 2;
        }
    };
    let inherited = match inherited_delegated_limits() {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("{PROG}: ERROR: {error}.");
            return 3;
        }
    };
    let inherited_max_cpus = inherited.and_then(|limits| limits.max_cpus);
    let inherited_max_mem_bytes = inherited.and_then(|limits| limits.memory_max_bytes);
    let by_tag: HashSet<String> = cfg.steps.iter().map(|s| s.tag()).collect();
    if !by_tag.contains(&step_tag) {
        let mut known: Vec<String> = by_tag.into_iter().collect();
        known.sort();
        let known_s = if known.is_empty() {
            "(none)".to_string()
        } else {
            known.join(", ")
        };
        eprintln!("{PROG}: sweep: unknown --step tag '{step_tag}'. Known tags: {known_s}");
        return 2;
    }
    let (lo, hi) = match parse_jobs_range(&jobs_spec) {
        Ok(range) => range,
        Err(e) => {
            eprintln!("{PROG}: sweep: {e}");
            return 2;
        }
    };
    if let Some(error) = sweep_cpu_error(hi, inherited_max_cpus) {
        eprintln!("{PROG}: sweep: {error}");
        return 2;
    }
    let repeat = a.repeat.max(1);
    let base_index = cfg
        .steps
        .iter()
        .position(|s| s.tag() == step_tag)
        .expect("tag presence checked above");
    let base = cfg.steps[base_index].clone();
    if !step_width_is_resizable(&base, &cfg.default_jobs_flag, &cfg.default_jobs_env) {
        eprintln!(
            "{PROG}: sweep: step '{step_tag}' offers no width channel, so --jobs cannot change \
             guest parallelism; set jobs_flag to the guest's worker-count option or \
             jobs_env/default_jobs_env to its worker-count environment variable"
        );
        return 2;
    }
    if let Some(error) = sweep_memory_error(&base, &cfg, hi, inherited_max_mem_bytes) {
        eprintln!("{PROG}: sweep: {error}; refusing");
        return 2;
    }

    // Cgroup boxing is ON by default here too (so the sweep measures under real boxing).
    let cgroups = match resolve_cgroups(
        a.allow_cgroup_failure,
        a.unsafe_no_cgroups,
        inherited_max_cpus,
        None,
        inherited_max_mem_bytes,
    ) {
        Ok(cg) => cg,
        Err(code) => return code,
    };
    if let Err(code) = require_timeseries_cgroups("sweep", a.profile_timeseries, &cgroups) {
        return code;
    }
    let explicit_profile_dir = a.output_dir.as_deref().or(a.perf_dir.as_deref());
    let (perf_dir, mut source) = resolve_profile_dir(explicit_profile_dir, a.no_profile);
    if a.output_dir.is_some() {
        source = "--output-dir";
    }
    let git = git_sha();

    let mut measures: Vec<(i64, SweepMeasure)> = Vec::new();
    let mut capture_samples = CurrentSweepSamples::new();
    for jobs in lo..=hi {
        let mut best: Option<SweepMeasure> = None;
        for _ in 0..repeat {
            let m = run_single_step(
                &base,
                &cfg,
                jobs,
                &cgroups,
                perf_dir.as_deref(),
                &git,
                a.verbosity,
                a.profile_timeseries,
            );
            if !m.ok {
                eprintln!(
                    "{PROG}: sweep: step '{step_tag}' FAILED at -j{jobs}; aborting the sweep"
                );
                if let Some(d) = perf_dir.as_deref() {
                    report_profile_written(d, source);
                }
                return 1;
            }
            remember_sweep_sample(&mut capture_samples, &step_tag, jobs, m);
            best = Some(match best {
                Some(b) if b.wall_s <= m.wall_s => b,
                _ => m,
            });
        }
        measures.push((jobs, best.expect("repeat >= 1")));
    }

    print_sweep_table(&step_tag, lo, &measures, c);
    if sweep_capture_requested(a) {
        let output_dir = perf_dir
            .as_deref()
            .expect("capture flags require an enabled profile directory");
        if let Err(error) = run_requested_sweep_captures(
            a,
            &cfg,
            &[base_index],
            &capture_samples,
            &cgroups,
            output_dir,
            &git,
        ) {
            eprintln!("{PROG}: sweep: profiler capture FAILED: {error}");
            report_profile_written(output_dir, source);
            refresh_scaling_model(output_dir, &cfg);
            refresh_sweep_report(
                output_dir,
                &cfg,
                &dag_arg,
                a.report_html.as_deref(),
                a.no_report,
            );
            return 1;
        }
    }
    if let Some(d) = perf_dir.as_deref() {
        report_profile_written(d, source);
        refresh_scaling_model(d, &cfg);
        let report_ok =
            refresh_sweep_report(d, &cfg, &dag_arg, a.report_html.as_deref(), a.no_report);
        if a.report_html.is_some() && !report_ok {
            return 1;
        }
    }
    0
}

/// Graph-wide, target-time parallel-scaling experiment.
///
/// The allowance is checked only here, between complete passes. Pass one is unconditional, and
/// once any later pass starts its complete cumulative grid is run for every selected resizable
/// node before the allowance is consulted again.
fn cmd_target_time_sweep(a: &SweepArgs, c: &Palette) -> i32 {
    let dag_arg = match &a.dag {
        Some(dag) => dag,
        None => {
            eprintln!("{PROG} sweep: error: the following arguments are required: --dag");
            return 2;
        }
    };
    let target =
        match parse_target_duration(a.target_time.as_deref().expect("target-time mode selected")) {
            Ok(target) => target,
            Err(error) => {
                eprintln!("{PROG}: sweep: {error}");
                return 2;
            }
        };
    let cfg = match load(dag_arg) {
        Ok(cfg) => cfg,
        Err(error) => {
            eprintln!("{PROG}: {}", error.0);
            return 2;
        }
    };
    let inherited = match inherited_delegated_limits() {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("{PROG}: ERROR: {error}.");
            return 3;
        }
    };
    let inherited_max_cpus = inherited.and_then(|limits| limits.max_cpus);
    let inherited_max_mem_bytes = inherited.and_then(|limits| limits.memory_max_bytes);

    let order = match stable_topological_order(&cfg.steps) {
        Ok(order) => order,
        Err(error) => {
            eprintln!("{PROG}: sweep: {error}");
            return 2;
        }
    };
    let selected: Vec<usize> = match a.step.as_deref() {
        Some(tag) => match cfg.steps.iter().position(|step| step.tag() == tag) {
            Some(index) => vec![index],
            None => {
                let mut known = cfg.steps.iter().map(Step::tag).collect::<Vec<_>>();
                known.sort();
                let known = known.join(", ");
                eprintln!(
                    "{PROG}: sweep: unknown --step tag '{tag}'. Known tags: {}",
                    if known.is_empty() { "(none)" } else { &known }
                );
                return 2;
            }
        },
        None => order,
    };

    let topology = limit_topology(
        machine_topology(),
        inherited_max_cpus.map_or_else(container_core_budget, |parent| {
            container_core_budget().min(parent)
        }),
    );
    let initial_widths = match a.jobs.as_deref() {
        Some(spec) => match parse_widths(spec) {
            Ok(widths) => widths,
            Err(error) => {
                eprintln!("{PROG}: sweep: {error}");
                return 2;
            }
        },
        None => coarse_widths(topology),
    };
    let max_width = initial_widths.iter().copied().max().unwrap_or(1);
    if let Some(error) = sweep_cpu_error(max_width, inherited_max_cpus) {
        eprintln!("{PROG}: sweep: {error}");
        return 2;
    }
    for index in &selected {
        let step = &cfg.steps[*index];
        if let Some(error) = target_sweep_step_resource_error(
            step,
            &cfg,
            max_width,
            topology,
            inherited_max_mem_bytes,
            a.step.is_some(),
        ) {
            eprintln!("{PROG}: sweep: {error}");
            return 2;
        }
    }
    let repeat = a.repeat.max(1);
    let cgroups = match resolve_cgroups(
        a.allow_cgroup_failure,
        a.unsafe_no_cgroups,
        inherited_max_cpus,
        None,
        inherited_max_mem_bytes,
    ) {
        Ok(cgroups) => cgroups,
        Err(code) => return code,
    };
    if let Err(code) = require_timeseries_cgroups("sweep", a.profile_timeseries, &cgroups) {
        return code;
    }
    let explicit_profile_dir = a.output_dir.as_deref().or(a.perf_dir.as_deref());
    let (perf_dir, mut profile_source) = resolve_profile_dir(explicit_profile_dir, a.no_profile);
    if a.output_dir.is_some() {
        profile_source = "--output-dir";
    }
    let git = git_sha();
    let target_s = target.as_secs_f64();
    let sweep_id = crate::perflog::new_run_id();
    let explicit_widths = a.jobs.is_some();
    let mut sample_number = 0usize;
    let mut capture_samples = CurrentSweepSamples::new();
    let has_resizable = selected.iter().any(|index| {
        let step = &cfg.steps[*index];
        step.skip_reason.is_none()
            && step_width_is_resizable(step, &cfg.default_jobs_flag, &cfg.default_jobs_env)
    });

    println!(
        "{}",
        c.bold(&format!(
            "parallel-scaling sweep: {} node(s), target {:.3}s, topology {}/{} physical/logical",
            selected.len(),
            target_s,
            topology.physical_cores,
            topology.logical_cpus,
        ))
    );

    let sweep_start = Instant::now();
    let mut pass = 1usize;
    let mut skipped_fixed = Vec::new();
    let completed_passes = loop {
        let grid = cumulative_width_grid(&initial_widths, pass);
        let grid_text = grid
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        println!(
            "sweep pass {pass} starting: cumulative widths [{}]{}",
            grid_text,
            if pass == 1 {
                " (mandatory minimum pass)"
            } else {
                ""
            }
        );
        let pass_start = Instant::now();

        for index in &selected {
            let base = &cfg.steps[*index];
            let tag = base.tag();
            if base.skip_reason.is_some() {
                if pass == 1 {
                    println!(
                        "sweep: {tag}: intentionally skipped by the DAG; no process was started"
                    );
                }
                continue;
            }
            if !step_width_is_resizable(base, &cfg.default_jobs_flag, &cfg.default_jobs_env) {
                if pass != 1 {
                    continue;
                }
                let max_cpus = crate::model::effective_cpu_count(base, cfg.default_step_cpu_count)
                    .unwrap_or(topology.logical_cpus)
                    .max(1);
                if max_cpus > topology.logical_cpus {
                    println!(
                        "sweep: {tag}: fixed configured width {max_cpus} exceeds the effective {}-CPU budget; skipping",
                        topology.logical_cpus,
                    );
                    skipped_fixed.push(tag);
                    continue;
                }
                println!(
                    "sweep: {tag}: no jobs_flag/jobs_env width channel; characterizing its configured width once (width {max_cpus})"
                );
                sample_number += 1;
                let meta = SweepProfileMeta {
                    sweep_id: sweep_id.clone(),
                    pass,
                    sample: sample_number,
                    repeat: 1,
                    width_source: "fixed".to_string(),
                    workload_digest: workload_digest(
                        base,
                        &cfg.default_jobs_flag,
                        &cfg.default_jobs_env,
                    ),
                    target_s,
                    topology,
                };
                let measure = run_isolated_step(
                    base,
                    &cfg,
                    None,
                    max_cpus,
                    1,
                    &cgroups,
                    perf_dir.as_deref(),
                    &git,
                    a.verbosity,
                    Some(&meta),
                    a.profile_timeseries,
                );
                if !measure.ok {
                    eprintln!(
                        "{PROG}: sweep: step '{tag}' FAILED at its configured width; aborting the sweep"
                    );
                    if let Some(dir) = perf_dir.as_deref() {
                        report_profile_written(dir, profile_source);
                    }
                    return 1;
                }
                remember_sweep_sample(&mut capture_samples, &tag, max_cpus, measure);
                print_fixed_sweep_measure(&tag, measure, c);
                continue;
            }

            let mut measures = Vec::with_capacity(grid.len());
            for jobs in &grid {
                let mut best: Option<SweepMeasure> = None;
                let width_source =
                    target_width_source(*jobs, &initial_widths, topology, explicit_widths);
                for repeat_number in 1..=repeat {
                    sample_number += 1;
                    let meta = SweepProfileMeta {
                        sweep_id: sweep_id.clone(),
                        pass,
                        sample: sample_number,
                        repeat: repeat_number,
                        width_source: width_source.clone(),
                        workload_digest: workload_digest(
                            base,
                            &cfg.default_jobs_flag,
                            &cfg.default_jobs_env,
                        ),
                        target_s,
                        topology,
                    };
                    let measure = run_isolated_step(
                        base,
                        &cfg,
                        Some(*jobs),
                        *jobs,
                        1,
                        &cgroups,
                        perf_dir.as_deref(),
                        &git,
                        a.verbosity,
                        Some(&meta),
                        a.profile_timeseries,
                    );
                    if !measure.ok {
                        eprintln!(
                            "{PROG}: sweep: step '{tag}' FAILED at -j{jobs} in pass {pass}; aborting the sweep"
                        );
                        if let Some(dir) = perf_dir.as_deref() {
                            report_profile_written(dir, profile_source);
                        }
                        return 1;
                    }
                    if pass == 1 {
                        remember_sweep_sample(&mut capture_samples, &tag, *jobs, measure);
                    }
                    best = Some(match best {
                        Some(previous) if previous.wall_s <= measure.wall_s => previous,
                        _ => measure,
                    });
                }
                measures.push((*jobs, best.expect("repeat >= 1")));
            }
            print_sweep_table(&tag, grid[0], &measures, c);
        }

        if pass == 1 && sweep_capture_requested(a) {
            let output_dir = perf_dir
                .as_deref()
                .expect("capture flags require an enabled profile directory");
            if let Err(error) = run_requested_sweep_captures(
                a,
                &cfg,
                &selected,
                &capture_samples,
                &cgroups,
                output_dir,
                &git,
            ) {
                eprintln!("{PROG}: sweep: profiler capture FAILED: {error}");
                report_profile_written(output_dir, profile_source);
                refresh_scaling_model(output_dir, &cfg);
                refresh_sweep_report(
                    output_dir,
                    &cfg,
                    dag_arg,
                    a.report_html.as_deref(),
                    a.no_report,
                );
                return 1;
            }
        }

        let pass_elapsed = pass_start.elapsed();
        let total_elapsed = sweep_start.elapsed();
        println!(
            "sweep pass {pass} complete in {:.3}s; total elapsed {:.3}s / target {:.3}s",
            pass_elapsed.as_secs_f64(),
            total_elapsed.as_secs_f64(),
            target_s,
        );
        if pass == 1 {
            println!(
                "minimum pass elapsed {:.3}s; target {:.3}s; overrun {:.3}s",
                pass_elapsed.as_secs_f64(),
                target_s,
                (pass_elapsed.as_secs_f64() - target_s).max(0.0),
            );
        }

        if total_elapsed >= target || !has_resizable {
            break pass;
        }
        pass += 1;
    };

    let elapsed_s = sweep_start.elapsed().as_secs_f64();
    let overrun_s = (elapsed_s - target_s).max(0.0);
    println!(
        "target-time sweep complete: {completed_passes} pass(es), {sample_number} sample(s), elapsed {:.3}s, target {:.3}s, overrun {:.3}s",
        elapsed_s, target_s, overrun_s,
    );
    if !skipped_fixed.is_empty() {
        println!(
            "sweep: fixed nodes skipped without samples because their configured widths exceed \
             the effective CPU budget: {}",
            skipped_fixed.join(", ")
        );
    } else if !has_resizable {
        println!(
            "sweep: no runnable selected node has a jobs_flag/jobs_env width channel; fixed nodes were handled once"
        );
    }
    if let Some(dir) = perf_dir.as_deref() {
        report_profile_written(dir, profile_source);
        refresh_scaling_model(dir, &cfg);
        let report_ok =
            refresh_sweep_report(dir, &cfg, dag_arg, a.report_html.as_deref(), a.no_report);
        if a.report_html.is_some() && !report_ok {
            return 1;
        }
    }
    0
}

// --------------------------------------------------------------------------- plan subcommand

struct PlanArgs {
    dag: Option<String>,
    planner: String,
    format: String,
    perf_dir: Option<String>,
    no_profile_feedback: bool,
    max_mem: Option<String>,
    max_steps: Option<i64>,
}

fn parse_plan_args(rest: &[String]) -> Result<PlanArgs, String> {
    let mut a = PlanArgs {
        dag: None,
        planner: Planner::GreedyLpt.value().to_string(),
        format: "text".to_string(),
        perf_dir: None,
        no_profile_feedback: false,
        max_mem: None,
        max_steps: None,
    };
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let take_value = |inline: Option<String>, i: &mut usize| -> Result<String, String> {
            if let Some(v) = inline {
                Ok(v)
            } else {
                *i += 1;
                rest.get(*i)
                    .cloned()
                    .ok_or_else(|| format!("the argument {key} requires a value"))
            }
        };
        match key.as_str() {
            "--dag" => a.dag = Some(take_value(inline, &mut i)?),
            "--planner" => a.planner = validate_planner(take_value(inline, &mut i)?)?,
            "--format" => {
                let v = take_value(inline, &mut i)?;
                if v != "text" && v != "json" {
                    return Err(format!(
                        "--format: invalid choice: '{v}' (choose from text, json)"
                    ));
                }
                a.format = v;
            }
            "--perf-dir" => a.perf_dir = Some(take_value(inline, &mut i)?),
            "--no-profile-feedback" => a.no_profile_feedback = true,
            "--max-mem" => a.max_mem = Some(take_value(inline, &mut i)?),
            "--max-steps" => {
                a.max_steps = Some(parse_positive_i64(
                    &take_value(inline, &mut i)?,
                    "--max-steps",
                )?)
            }
            other => return Err(format!("unrecognized argument: {other}")),
        }
        i += 1;
    }
    Ok(a)
}

/// Show the plan (per-step estimates + sources, critical path, scheduled order) without running.
fn cmd_plan(a: &PlanArgs) -> i32 {
    let dag_arg = match &a.dag {
        Some(d) => d.clone(),
        None => {
            eprintln!("{PROG} plan: error: the following arguments are required: --dag");
            return 2;
        }
    };
    let cfg = match load(&dag_arg) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{PROG}: {}", e.0);
            return 2;
        }
    };
    let planner = Planner::from_value(&a.planner).unwrap_or(Planner::GreedyLpt);
    let feedback_dir = resolve_feedback_dir(a.perf_dir.as_deref(), a.no_profile_feedback);
    let (core_budget, mem_budget) = planning_budgets(planner, a.max_mem.as_deref(), None);
    let plan = build_feedback_plan(
        &cfg,
        feedback_dir.as_deref(),
        planner,
        core_budget,
        mem_budget,
        a.max_steps,
    );
    if a.format == "json" {
        println!("{}", plan_to_json(&plan));
    } else {
        print!("{}", plan_to_text(&plan));
    }
    0
}

/// Exit code for a run that admission would not let start.
///
/// Distinct from 2 (bad usage) and 3 (cgroup boxing unavailable) on purpose: a scheduler that
/// retries should be able to tell "this host is busy, come back" from "this invocation is wrong",
/// without parsing prose.
const ADMISSION_EXIT_CODE: i32 = 4;

/// Reserve this run's memory against the host-wide ledger, or refuse to start.
///
/// Absent `--admission` this is a no-op: admission is opt-in because a durable cross-process
/// ledger changes WHEN a run may start, and that is not something to switch on underneath existing
/// callers. The returned reservation must be held for the life of the run.
fn apply_admission(
    a: &RunArgs,
    effective_max_mem_bytes: Option<i64>,
) -> Result<Option<crate::admission::MemoryReservation>, i32> {
    let Some(wait_s) = a.admission else {
        return Ok(None);
    };
    let Some(requested) =
        effective_max_mem_bytes.or_else(|| requested_max_mem_bytes(a.max_mem.as_deref()))
    else {
        // REQUIRED, not guessed. The only numbers available without --max-mem describe the whole
        // host, so guessing would reserve everything and turn admission into a global mutex --
        // which would look like it was working right up until it deadlocked a CI fleet.
        eprintln!(
            "{PROG}: run: --admission requires --max-mem or a finite inherited delegated memory \
             ceiling: admission reserves a NUMBER against a host-wide ledger."
        );
        return Err(2);
    };
    // ALREADY ADMITTED -- AND THIS RUN PROVES IT IS THE ONE ADMITTED. Boxing re-execs this
    // process into a systemd scope with `execvp`, which keeps the pid AND the /proc start time,
    // so the record this run wrote before the exec is still its own live reservation. Asking
    // again would count this run twice against the budget -- and on a tight budget the second
    // ask would QUEUE behind the first, i.e. the run would wait for itself until the wait ran
    // out.
    //
    // THE SENTINEL ALONE CANNOT ESTABLISH THAT. `systemd-run --setenv=DAGRUN_IN_SCOPE=1` sets it
    // for the whole scope, and every step inherits the runner's environment, so a runner invoked
    // as a STEP of a boxed run reads the same "1" while holding no reservation at all -- and
    // skipping on the flag alone would wave that nested run through with nothing reserved and no
    // verdict printed. So ask the LEDGER whether a live record is fingerprinted with this pid and
    // this /proc start time: only its own record licenses the skip. Anything else falls through
    // and admits normally.
    if crate::cgroup::is_in_scope() {
        match crate::admission::held_by_this_process(None) {
            Ok(own_bytes) if own_bytes > 0 => return Ok(None),
            Ok(_) => {}
            Err(error) => {
                eprintln!("{PROG}: run: admission ledger unusable: {error}");
                return Err(ADMISSION_EXIT_CODE);
            }
        }
    }
    let requested = u64::try_from(requested).unwrap_or(0);
    let poll_s = (wait_s / 4.0).clamp(0.25, 2.0);
    match crate::admission::admit(requested, "run", None, poll_s, wait_s, true) {
        Ok((_decision, Some(reservation))) => Ok(Some(reservation)),
        Ok((decision, None)) => {
            eprintln!("{PROG}: run: {}", decision.reason);
            if decision.verdict == crate::admission::Verdict::Queue && wait_s <= 0.0 {
                eprintln!(
                    "{PROG}: run: pass --admission SECONDS to wait for a slot instead of exiting."
                );
            }
            Err(ADMISSION_EXIT_CODE)
        }
        Err(error) => {
            eprintln!("{PROG}: run: admission ledger unusable: {error}");
            Err(ADMISSION_EXIT_CODE)
        }
    }
}

fn cmd_run(cfg: &DagConfig, a: &RunArgs, c: &Palette) -> i32 {
    // A selection includes its full dependency ancestry by default. Validate and select BEFORE
    // cgroup bring-up so a bad combination or tag fails fast (exit 2) without needing a scope.
    if a.selected.is_some() && a.labels.is_some() {
        eprintln!("{PROG}: run: --selected and --labels cannot be combined");
        return 2;
    }
    if a.ignore_selected_deps && a.selected.is_none() {
        eprintln!("{PROG}: run: --ignore-selected-deps requires --selected");
        return 2;
    }
    let filtered = if let Some(selected) = a.selected.as_deref() {
        let tags = parse_tag_list(selected);
        if tags.is_empty() {
            eprintln!("{PROG}: run: --selected requires at least one tag");
            return 2;
        }
        match select_steps_by_tags(cfg, &tags, a.ignore_selected_deps) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("{PROG}: {e}");
                return 2;
            }
        }
    } else if let Some(labels) = a.labels.as_deref() {
        let labels = parse_tag_list(labels);
        if labels.is_empty() {
            eprintln!("{PROG}: run: --labels requires at least one label");
            return 2;
        }
        match select_steps_by_labels(cfg, &labels) {
            Ok(f) => Some(f),
            Err(e) => {
                eprintln!("{PROG}: {e}");
                return 2;
            }
        }
    } else {
        None
    };
    let selected_subgraph = filtered
        .as_ref()
        .is_some_and(|selected| selected.steps.len() < cfg.steps.len());
    let selected: &DagConfig = filtered.as_ref().unwrap_or(cfg);

    // A command opts into passthrough by carrying the reserved `{args}` token.  Apply this before
    // stress expansion so every shard receives the same scoped command.
    let mut with_args = match apply_passthrough_args(selected, a.passthrough_args.as_deref()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{PROG}: run: {error}");
            return 2;
        }
    };

    let inherited = match inherited_delegated_limits() {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("{PROG}: ERROR: {error}.");
            return 3;
        }
    };
    let inherited_max_cpus = inherited.and_then(|limits| limits.max_cpus);
    let inherited_max_mem_bytes = inherited.and_then(|limits| limits.memory_max_bytes);
    if let Some(error) = inherited_cores_error(a, inherited_max_cpus) {
        eprintln!("{PROG}: run: {error}");
        return 2;
    }

    // Resolve the CPU target before stress sizing or cgroup bring-up. Stress must charge the
    // runner-controlled widths that can actually execute under this ceiling, while self-managed
    // commands which cannot be clamped must fail before sizing or expansion.
    let max_cpus = select_max_cpus_bounded(a, inherited_max_cpus);
    let effective_max_mem_bytes = select_max_mem_bytes(a, inherited_max_mem_bytes);
    let inherited_memory_budget = uses_inherited_memory_budget(a, inherited_max_mem_bytes);
    if let Some(budget) =
        effective_max_mem_bytes.filter(|_| selected_subgraph || inherited_memory_budget)
    {
        // A selected config retains the source DAG's non-step policy, including its conservative
        // whole-graph floor. That floor is not evidence that an omitted node consumes memory.
        // Bound it to this invocation's physical budget before every planner/stress/admission
        // preflight; the selected steps' own hard/default/RSS-derived caps remain unchanged and
        // still refuse a genuinely infeasible selection.
        with_args.mem_cap_floor_bytes = with_args.mem_cap_floor_bytes.min(budget);
    }
    if let Err(error) = validate_max_cpus_rewrite(&with_args, max_cpus) {
        eprintln!("{PROG}: run: {error}");
        return 2;
    }

    if a.stress < 1 {
        eprintln!("{PROG}: run: --stress N must be >= 1 (got {})", a.stress);
        return 2;
    }
    let stress_active = a.stress > 1;
    let stressed = if stress_active {
        let code = stress_expansion_guard(&with_args, a.stress);
        if code != 0 {
            return code;
        }
        // Clamp once before expansion so the guard sizes exactly what gets cloned. The later
        // post-plan clamp is then idempotent rather than warning for every generated copy.
        let stress_sized = cap_config_max_cpus(&with_args, max_cpus);
        let code = stress_guard_with_budget(&stress_sized, a.stress, effective_max_mem_bytes);
        if code != 0 {
            return code;
        }
        expand_stress(&stress_sized, a.stress)
    } else {
        with_args
    };

    // HOST-WIDE MEMORY ADMISSION, opt-in, and deliberately BEFORE cgroup bring-up. A run that is
    // going to wait must not be holding a systemd scope while it waits, and a run that is going to
    // be refused should not have created one at all. The reservation is kept alive for the rest of
    // this function, so the ledger reflects this run for exactly as long as it is on the machine.
    let _memory_admission = match apply_admission(a, effective_max_mem_bytes) {
        Ok(reservation) => reservation,
        Err(code) => return code,
    };

    // Cgroup boxing is ON by default (may re-exec into a systemd scope and not return). Bind the
    // same resolved CPU target to the outer scope's CPUQuota and each runner-controlled step's
    // width ceiling.
    let cgroups = match resolve_cgroups(
        a.allow_cgroup_failure,
        a.unsafe_no_cgroups,
        Some(max_cpus),
        a.run_timeout,
        effective_max_mem_bytes,
    ) {
        Ok(cg) => cg,
        Err(code) => return code,
    };
    if let Err(code) = require_timeseries_cgroups("run", a.profile_timeseries, &cgroups) {
        return code;
    }

    // Opt-in --cores K: constrain the whole run tree to K reserved cores. Apply it here,
    // after the boxing re-exec has settled (the re-exec'd in-scope child re-enters cmd_run and
    // applies it there) and BEFORE the scheduler spawns any worker thread or forks any step —
    // threads inherit the creator's affinity and forked steps inherit at fork, so an early
    // application covers the whole descendant tree. Only an exact cgroup cpuset is accepted;
    // process affinity is escapable and cannot enforce a collision-free reservation.
    let mut core_reservation = None;
    if let Some(k) = a.cores {
        match crate::reservation::acquire(k, "run", 0.3, None, &HashSet::new(), None) {
            Ok(mut reservation) => {
                if apply_specific_cores(&reservation.cores, &format!("--cores {k}")).is_none() {
                    eprintln!(
                        "{PROG}: --cores {k}: hard cgroup cpuset unavailable; refusing to run"
                    );
                    let _ = reservation.release();
                    return 3;
                }
                core_reservation = Some(reservation);
            }
            Err(error) => {
                eprintln!("{PROG}: --cores {k}: reservation failed: {error}");
                return 3;
            }
        }
    }

    // Make the budget visible in the plan as well as at execution. This caps authored hints and
    // the undeclared-step cpu.max default before CPA sees the graph, so --show-plan cannot display
    // a width that the scheduler later changes.
    let budgeted = cap_config_max_cpus(&stressed, max_cpus);
    let cfg = &budgeted;

    // Plan-time profile-store feedback: refine each step's est_duration_s
    // and rss_baseline_bytes from the recorded store, then pick the dispatch order for --planner.
    // The applied cfg (refined hints) feeds both memory-aware --max-steps sizing and the scheduler.
    let planner = Planner::from_value(&a.planner).unwrap_or(Planner::GreedyLpt);
    let feedback_dir = resolve_feedback_dir(a.perf_dir.as_deref(), a.no_profile_feedback);
    let (core_budget, mem_budget) = planning_budgets_with_memory(
        planner,
        a.max_mem.as_deref(),
        Some(max_cpus),
        effective_max_mem_bytes,
    );
    let planning_max_steps = Some(a.max_steps.unwrap_or(max_cpus));

    // Profile-artifact SYNC: parse the backend once; DOWNLOAD seeds the planner, UPLOAD (after the
    // run) publishes this run's samples. A malformed spec fails fast; a backend I/O failure degrades
    // LOUDLY without failing the run.
    let backend: Option<Box<dyn SyncBackend>> = match a.profile_sync.as_deref() {
        Some(spec) => match sync::parse_backend(spec) {
            Ok(b) => Some(b),
            Err(e) => {
                eprintln!("{PROG}: --profile-sync: {e}");
                return 2;
            }
        },
        None => None,
    };
    let do_download =
        backend.is_some() && matches!(a.profile_sync_direction.as_str(), "both" | "download");
    let do_upload =
        backend.is_some() && matches!(a.profile_sync_direction.as_str(), "both" | "upload");

    let plan = sync_seed_plan(
        cfg,
        backend.as_deref(),
        feedback_dir.as_deref(),
        planner,
        do_download && !a.no_profile_feedback,
        core_budget,
        mem_budget,
        planning_max_steps,
    )
    .unwrap_or_else(|| {
        build_feedback_plan(
            cfg,
            feedback_dir.as_deref(),
            planner,
            core_budget,
            mem_budget,
            planning_max_steps,
        )
    });
    if plan
        .allocation
        .as_ref()
        .is_some_and(|allocation| allocation.stop_reason == "infeasible-memory")
    {
        eprintln!(
            "{PROG}: run: CPA allocation is infeasible under --max-mem {}; the minimum runnable \
             footprint exceeds the memory budget",
            a.max_mem.as_deref().unwrap_or("")
        );
        return 2;
    }
    let mut applied = cap_config_max_cpus(&apply_plan_to_config(cfg, &plan), max_cpus);
    // Censoring-aware memory feedback (opt-in), applied AFTER the ordinary plan so it has the last
    // word on rss_baseline_bytes and BEFORE the memory-aware --max-steps sizing that reads it. It
    // is handed the PRE-plan config as well, so a step it declines to estimate goes back to its
    // authored baseline (or a larger proven floor) when this stricter path declines to estimate.
    applied = apply_memory_feedback(
        &applied,
        cfg,
        feedback_dir.as_deref(),
        a.profile_memory_feedback,
    );
    // Compatibility flag: the SMALL forcing-function caps are already active by default. Reassert
    // the same values so older callers keep working and announce that the flag is now redundant.
    if a.small_default_cap {
        applied.default_step_mem_cap_bytes = Some(crate::model::DEFAULT_SMALL_MEM_CAP_BYTES);
        applied.default_step_cpu_count = Some(crate::model::DEFAULT_SMALL_CPU_COUNT);
        applied.default_step_cpu_timeout = crate::model::DEFAULT_SMALL_CPU_TIMEOUT;
        eprintln!(
            "{PROG}: --small-default-cap is redundant (SMALL defaults are already active): \
             undeclared steps are boxed to (mem {} B / {} core / {} s CPU); declared per-step \
             hints still win",
            crate::model::DEFAULT_SMALL_MEM_CAP_BYTES,
            crate::model::DEFAULT_SMALL_CPU_COUNT,
            crate::model::DEFAULT_SMALL_CPU_TIMEOUT,
        );
    }
    // Per-platform CPU-budget scaling. Applied AFTER apply_plan_to_config so the planner never
    // sees (and cannot bake in) a platform-specific number: the graph and the plan stay canonical,
    // and only enforcement is scaled. Mirrors the Python CLI.
    let (cpu_multiplier, cpu_platform) =
        match crate::model::resolve_cpu_timeout_multiplier(a.cpu_timeout_multiplier) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("{PROG}: error: {e}");
                return 2;
            }
        };
    if cpu_multiplier != crate::model::DEFAULT_CPU_TIMEOUT_MULTIPLIER {
        applied.cpu_timeout_multiplier = cpu_multiplier;
        applied.cpu_timeout_platform = cpu_platform.clone();
        // Announce it: a scaled budget silently in force is exactly the invisible-policy problem
        // this mechanism exists to avoid.
        let label = if cpu_platform.is_empty() {
            String::new()
        } else {
            format!(" ({cpu_platform})")
        };
        eprintln!(
            "{PROG}: per-platform CPU-budget multiplier x{cpu_multiplier}{label} in effect; \
             every step's canonical cpu_timeout is scaled by it for enforcement on this platform"
        );
    }
    let cfg: &DagConfig = &applied;
    if a.show_plan {
        print!("{}", plan_to_text(&plan));
    }

    // Feedback and CPA may have raised RSS estimates or widened CPU-bound steps after the early
    // authored-hint preflight. Re-check the FINAL already-expanded graph directly -- multiplying
    // by stress again would double-count every generated copy. This is the last no-spawn barrier.
    if stress_active {
        let code = final_stress_guard_with_budget(cfg, a.stress, effective_max_mem_bytes);
        if code != 0 {
            return code;
        }
    }

    let max_steps = select_max_steps_with_budget(
        cfg,
        a,
        max_cpus,
        effective_max_mem_bytes,
        inherited_memory_budget,
    );
    if max_steps < 1 {
        return 2;
    }
    let verbosity = if a.quiet { 0 } else { a.verbosity };
    let resource_caps_path = a
        .resource_caps_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(crate::resource_caps::PATH_ENV)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        });

    let (perf_dir, source) = resolve_profile_dir(a.perf_dir.as_deref(), a.no_profile);
    let profile_run_id = perf_dir.as_ref().map(|_| crate::perflog::new_run_id());
    let git = git_sha();
    let window = perf_dir
        .as_deref()
        .map(|d| PerfWindow::start(Path::new(d), &git));
    let profile_enforcement = profile_enforcement_kind(matches!(
        &cgroups,
        Some(manager) if manager.enabled()
    ));

    let result = run_dag_boxed_deadline_limited_with_resource_caps_and_timeseries(
        cfg,
        max_steps,
        max_cpus,
        a.keep_going || stress_active,
        verbosity,
        cgroups,
        Some(plan.order.clone()),
        a.run_timeout,
        resource_caps_path,
        a.profile_timeseries,
    );
    let passed = result.outcomes.iter().filter(|o| o.ok).count();
    let failed = result
        .outcomes
        .iter()
        .filter(|o| !o.ok && !o.aborted)
        .count();
    let aborted = result.outcomes.iter().filter(|o| o.aborted).count();
    let verdict = if result.ok {
        c.green("PASS")
    } else {
        c.red("FAIL")
    };
    eprintln!(
        "{PROG}: {verdict} - {passed} passed, {failed} failed, {aborted} aborted, {} intentionally skipped, {} dependency-skipped, {} not launched in {:.1}s",
        result.intentional_skips.len(),
        result.skipped.len(),
        result.not_launched.len(),
        result.wall_s
    );
    if !result.not_launched.is_empty() {
        eprintln!("{PROG}: not launched: {}", result.not_launched.join(", "));
    }
    if let Some(line) = diagnostic_failure_summary(&result.outcomes) {
        eprintln!("{PROG}: {line}");
    }

    if let Some(d) = perf_dir.as_deref() {
        if let Some(w) = &window {
            w.finish(
                if result.ok { "pass" } else { "fail" },
                result.outcomes.len(),
                max_steps,
            );
        }
        append_step_profiles(
            Path::new(d),
            &result.step_profile_rows,
            &git,
            max_steps,
            None,
            profile_enforcement,
            "run",
            profile_run_id.as_deref(),
        );
        if a.profile_timeseries.is_some() {
            if let Some(run_id) = profile_run_id.as_deref() {
                if let Some(path) = append_step_timeseries(
                    Path::new(d),
                    &result.step_timeseries_rows,
                    &git,
                    max_steps,
                    None,
                    profile_enforcement,
                    "run",
                    run_id,
                ) {
                    eprintln!("[scheduler] step time-series written to {}", path.display());
                }
            }
        }
        report_profile_written(d, source);
    }

    if do_upload {
        if let Some(b) = backend.as_deref() {
            sync_upload(b, &result.step_profile_rows, profile_run_id.as_deref());
        }
    }

    if stress_active {
        print_stress_report(
            &result.outcomes,
            a.stress,
            result.max_concurrent_steps,
            max_steps,
            max_cpus,
            c,
        );
    }

    // Feature C: --profile prints a per-step profile table to stdout.
    if a.profile {
        print_profile_table(&result.step_profile_rows, c);
    }

    // Keep the reservation live through every child, profile write, and report.  Explicitly drop
    // it here so long-lived library callers release promptly instead of waiting for process exit.
    drop(core_reservation);
    if result.ok {
        0
    } else {
        1
    }
}

fn cmd_pin_run(rest: &[String]) -> i32 {
    let mut cores: Option<i64> = None;
    let mut tag = "pin-run".to_string();
    let mut command: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            command.extend_from_slice(&rest[i + 1..]);
            break;
        }
        let (key, inline) = match arg.split_once('=') {
            Some((key, value)) => (key, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(value) = inline.clone() {
                return Ok(value);
            }
            i += 1;
            rest.get(i)
                .cloned()
                .ok_or_else(|| format!("the argument {name} requires a value"))
        };
        match key {
            "--cores" => {
                let raw = match value("--cores") {
                    Ok(value) => value,
                    Err(error) => {
                        eprintln!("{PROG} pin-run: error: {error}");
                        return 2;
                    }
                };
                cores = match raw.parse::<i64>() {
                    Ok(value) if value >= 1 => Some(value),
                    Ok(_) => {
                        eprintln!("{PROG} pin-run: error: --cores must be >= 1");
                        return 2;
                    }
                    Err(_) => {
                        eprintln!("{PROG} pin-run: error: --cores: invalid int value: '{raw}'");
                        return 2;
                    }
                };
            }
            "--tag" => match value("--tag") {
                Ok(value) => tag = value,
                Err(error) => {
                    eprintln!("{PROG} pin-run: error: {error}");
                    return 2;
                }
            },
            other if other.starts_with('-') => {
                eprintln!("{PROG} pin-run: error: unrecognized argument: {other}");
                return 2;
            }
            _ => {
                command.extend_from_slice(&rest[i..]);
                break;
            }
        }
        i += 1;
    }
    let Some(k) = cores else {
        eprintln!("{PROG} pin-run: error: the following arguments are required: --cores");
        return 2;
    };
    if command.is_empty() {
        eprintln!("{PROG}: pin-run: no command given (use '-- CMD [ARGS...]')");
        return 2;
    }
    if tag.is_empty() {
        tag = "pin-run".to_string();
    }
    let mut reservation =
        match crate::reservation::acquire(k, &tag, 0.3, None, &HashSet::new(), None) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("{PROG}: pin-run: {error}");
                return 3;
            }
        };
    let code = crate::cpuset_allocator::run_reserved_hard(
        &reservation.cores,
        &command,
        &tag,
        &format!("{PROG}: pin-run"),
    );
    let _ = reservation.release();
    code
}

// --------------------------------------------------------------------------- entry

/// Run the CLI over `argv` (excluding the program name); returns the process exit code.
pub fn run(argv: &[String]) -> i32 {
    let restore_failures_before = nested_coordinator_restore_failures();
    let code = run_inner(argv);
    let restore_failures_after = nested_coordinator_restore_failures();
    let checked =
        restoration_checked_exit_code(code, restore_failures_before, restore_failures_after);
    if checked != code {
        eprintln!(
            "{PROG}: ERROR: nested scheduler cgroup membership could not be restored; refusing \
             to report a successful invocation"
        );
    }
    checked
}

fn restoration_checked_exit_code(code: i32, failures_before: u64, failures_after: u64) -> i32 {
    if code == 0 && failures_after > failures_before {
        3
    } else {
        code
    }
}

fn run_inner(argv: &[String]) -> i32 {
    let c = Palette {
        enabled: color_enabled(),
    };

    if argv.is_empty() {
        print!("{}", help_text(&c));
        return 0;
    }

    let command = argv[0].as_str();
    let rest = &argv[1..];

    match command {
        "--version" => {
            println!("{PROG} {VERSION}");
            0
        }
        "-h" | "--help" => {
            print!("{}", help_text(&c));
            0
        }
        "quickstart" => {
            println!("{}", quickstart(&c));
            0
        }
        "capabilities" => {
            // Machine-readable enforcement manifest, DERIVED from the guard registry rather than
            // typed out: byte-identical to the Python build and cross-checked, so an enforcement
            // guard in one build but not the other fails `cross`.
            println!("{}", enforcement_manifest());
            0
        }
        "--userguide" => {
            // Write the edition-specific embedded guide verbatim (no added/stripped newline).
            print!("{USERGUIDE}");
            0
        }
        "run" => {
            if wants_help(rest) {
                print!("{}", run_help(&c));
                return 0;
            }
            let a = match parse_run_args(rest) {
                Ok(a) => a,
                Err(msg) => {
                    eprintln!("{PROG} run: error: {msg}");
                    return 2;
                }
            };
            let dag_arg = match &a.dag {
                Some(d) => d.clone(),
                None => {
                    eprintln!("{PROG} run: error: the following arguments are required: --dag");
                    return 2;
                }
            };
            if let Some(refusal) = nested_run_refusal(a.allow_unwise_nest_dagruns) {
                eprintln!("{PROG}: run: {refusal}");
                return 2;
            }
            // A boxed run re-execs inside systemd. Do that before consuming a stdin DAG so the
            // in-scope child receives the untouched pipe. Explicit unboxed and direct-cgroup
            // modes do not re-exec, so they continue to load stdin directly.
            if dag_arg == "-"
                && !a.allow_cgroup_failure
                && !a.unsafe_no_cgroups
                && !is_in_scope()
                && std::env::var(DIRECT_CGROUP_ENV).as_deref() != Ok("1")
            {
                let max_cpus = select_max_cpus(&a);
                if let Err(code) = resolve_cgroups(
                    false,
                    false,
                    Some(max_cpus),
                    a.run_timeout,
                    requested_max_mem_bytes(a.max_mem.as_deref()),
                ) {
                    return code;
                }
            }
            let cfg = match load(&dag_arg) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("{PROG}: {}", e.0);
                    return 2;
                }
            };
            cmd_run(&cfg, &a, &c)
        }
        "box" => {
            if wants_help(rest) {
                print!("{}", box_help(&c));
                return 0;
            }
            let a = match parse_box_args(rest) {
                Ok(a) => a,
                Err(msg) => {
                    eprintln!("{PROG} box: error: {msg}");
                    return 2;
                }
            };
            cmd_box(&a, &c)
        }
        "sweep" => {
            if wants_help(rest) {
                print!("{}", sweep_help(&c));
                return 0;
            }
            let a = match parse_sweep_args(rest) {
                Ok(a) => a,
                Err(msg) => {
                    eprintln!("{PROG} sweep: error: {msg}");
                    return 2;
                }
            };
            cmd_sweep(&a, &c)
        }
        "plan" => {
            if wants_help(rest) {
                print!("{}", plan_help(&c));
                return 0;
            }
            let a = match parse_plan_args(rest) {
                Ok(a) => a,
                Err(msg) => {
                    eprintln!("{PROG} plan: error: {msg}");
                    return 2;
                }
            };
            cmd_plan(&a)
        }
        "summary" => {
            if let Some(help) = requested_summary_help(&c, rest) {
                print!("{help}");
                return 0;
            }
            cmd_summary(rest)
        }
        "pin-run" => {
            if pin_wants_help(rest) {
                print!("{}", pin_run_help(&c));
                return 0;
            }
            cmd_pin_run(rest)
        }
        "list" | "ascii" | "dot" | "json" | "yaml" => {
            if wants_help(rest) {
                print!("{}", simple_help(&c, command));
                return 0;
            }
            let view = if matches!(command, "ascii" | "dot") {
                parse_view_args(rest)
            } else {
                parse_simple_dag(rest).map(|dag| ViewArgs {
                    dag,
                    ..ViewArgs::default()
                })
            };
            let view = match view {
                Ok(v) => v,
                Err(msg) => {
                    eprintln!("{PROG} {command}: error: {msg}");
                    return 2;
                }
            };
            let dag_arg = match view.dag.clone() {
                Some(d) => d,
                None => {
                    eprintln!(
                        "{PROG} {command}: error: the following arguments are required: --dag"
                    );
                    return 2;
                }
            };
            let cfg = match load(&dag_arg) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("{PROG}: {}", e.0);
                    return 2;
                }
            };
            let cfg = match view.labels.as_deref() {
                Some(labels) => match select_steps_by_labels(&cfg, labels) {
                    Ok(selected) => selected,
                    Err(e) => {
                        eprintln!("{PROG}: {e}");
                        return 2;
                    }
                },
                None => cfg,
            };
            let grouped = view.group_by.is_some();
            match command {
                "list" => println!("{}", render_list(&cfg, &c)),
                "ascii" if grouped => print!("{}", to_ascii_groups(&cfg, None)),
                "ascii" => print!("{}", to_ascii(&cfg, None)),
                "dot" if grouped => print!("{}", to_dot_groups(&cfg, "dag", None)),
                "dot" => print!("{}", to_dot(&cfg, "dag", None)),
                "json" => println!("{}", dag_to_json(&cfg)),
                // dag_to_yaml already ends with a trailing newline.
                "yaml" => print!("{}", dag_to_yaml(&cfg)),
                _ => unreachable!(),
            }
            0
        }
        other => {
            eprintln!(
                "usage: {PROG} [-h] [--version] <command> ...\n\
                 {PROG}: error: argument <command>: invalid choice: '{other}' \
                 (choose from run, box, sweep, plan, summary, list, ascii, dot, json, yaml, pin-run, \
                 quickstart, capabilities)"
            );
            2
        }
    }
}

// --------------------------------------------------------------------------- summary subcommand

// The `summary` subcommand family: build / merge / plan / stats the mergeable profile summary.
// These primitives make the summary format inspectable and drivable from a script, and are what the
// cross-language differential exercises for byte-identical serialization + merge. Mirrors Python's
// `_cmd_summary`.
fn cmd_summary(rest: &[String]) -> i32 {
    let action = match rest.first() {
        Some(a) => a.as_str(),
        None => {
            eprintln!("{PROG}: summary: an action is required (build | merge | plan | stats)");
            return 2;
        }
    };
    let args = &rest[1..];
    match action {
        "build" => cmd_summary_build(args),
        "merge" => cmd_summary_merge(args),
        "plan" => cmd_summary_plan(args),
        "stats" => cmd_summary_stats(args),
        other => {
            eprintln!("{PROG}: summary: unknown action '{other}' (build | merge | plan | stats)");
            2
        }
    }
}

/// Parse `--key value` / `--key=value` flags plus positional args into (flags, positionals).
fn parse_flags(
    rest: &[String],
    value_keys: &[&str],
) -> Result<(HashMap<String, String>, Vec<String>), String> {
    let mut flags: HashMap<String, String> = HashMap::new();
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        if arg == "--" {
            positional.extend_from_slice(&rest[i + 1..]);
            break;
        }
        if let Some(key) = arg.strip_prefix("--") {
            let (k, inline) = match key.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (key.to_string(), None),
            };
            if value_keys.contains(&k.as_str()) {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        let value = rest
                            .get(i)
                            .ok_or_else(|| format!("the argument --{k} requires a value"))?;
                        if value.starts_with("--") {
                            return Err(format!("the argument --{k} requires a value"));
                        }
                        value.clone()
                    }
                };
                flags.insert(k, v);
            } else {
                return Err(format!("unrecognized argument: --{k}"));
            }
        } else {
            positional.push(arg.clone());
        }
        i += 1;
    }
    Ok((flags, positional))
}

fn summary_reservoir_cap(flags: &HashMap<String, String>) -> Result<usize, String> {
    let Some(raw) = flags.get("reservoir-cap") else {
        return Ok(DEFAULT_RESERVOIR_K);
    };
    let value = raw
        .parse::<i64>()
        .map_err(|_| format!("argument --reservoir-cap: invalid positive integer: '{raw}'"))?;
    if value < 1 {
        return Err("argument --reservoir-cap: must be >= 1".to_string());
    }
    usize::try_from(value)
        .map_err(|_| "argument --reservoir-cap: outside the supported range".to_string())
}

fn reject_summary_positionals(positionals: &[String]) -> Result<(), String> {
    if positionals.is_empty() {
        Ok(())
    } else {
        Err(format!("unrecognized arguments: {}", positionals.join(" ")))
    }
}

fn write_or_print(text: &str, out: Option<&String>) -> i32 {
    match out {
        Some(path) => match std::fs::write(path, format!("{text}\n")) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("{PROG}: summary: cannot write {path}: {e}");
                2
            }
        },
        None => {
            println!("{text}");
            0
        }
    }
}

fn cmd_summary_build(args: &[String]) -> i32 {
    let (flags, positionals) = match parse_flags(args, &["perf-dir", "out", "reservoir-cap"]) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{PROG} summary build: error: {e}");
            return 2;
        }
    };
    if let Err(error) = reject_summary_positionals(&positionals) {
        eprintln!("{PROG} summary build: error: {error}");
        return 2;
    }
    let cap = match summary_reservoir_cap(&flags) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{PROG} summary build: error: {error}");
            return 2;
        }
    };
    let perf_dir = flags
        .get("perf-dir")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    let feedback_dir = resolve_feedback_dir(perf_dir, false);
    let (mid, cc) = feedback_identity();
    let summary = match feedback_dir {
        Some(dir) => summary::summary_from_store(Path::new(&dir), &mid, &cc, cap),
        None => summary::empty(&mid, &cc),
    };
    write_or_print(
        &summary::to_json(&summary),
        flags.get("out").filter(|value| !value.is_empty()),
    )
}

fn cmd_summary_merge(args: &[String]) -> i32 {
    let (flags, files) = match parse_flags(args, &["out", "reservoir-cap"]) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{PROG} summary merge: error: {e}");
            return 2;
        }
    };
    let cap = match summary_reservoir_cap(&flags) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("{PROG} summary merge: error: {error}");
            return 2;
        }
    };
    if files.is_empty() {
        eprintln!("{PROG}: summary merge: need at least one file");
        return 2;
    }
    let mut summaries: Vec<Summary> = Vec::with_capacity(files.len());
    for file in &files {
        match std::fs::read_to_string(file) {
            Ok(text) => match summary::from_json(&text) {
                Ok(s) => summaries.push(s),
                Err(e) => {
                    eprintln!("{PROG}: summary merge: {file}: {e}");
                    return 2;
                }
            },
            Err(e) => {
                eprintln!("{PROG}: summary merge: cannot read {file}: {e}");
                return 2;
            }
        }
    }
    let first = &summaries[0];
    match summary::merge_all(
        &summaries,
        &first.machine_id,
        &first.container_class,
        cap,
        DEFAULT_MAX_BUCKETS,
    ) {
        Ok(merged) => write_or_print(
            &summary::to_json(&merged),
            flags.get("out").filter(|value| !value.is_empty()),
        ),
        Err(e) => {
            eprintln!("{PROG}: summary merge: {e}");
            2
        }
    }
}

fn cmd_summary_plan(args: &[String]) -> i32 {
    let (flags, positionals) = match parse_flags(
        args,
        &[
            "summary",
            "dag",
            "planner",
            "max-mem",
            "max-steps",
            "format",
        ],
    ) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{PROG} summary plan: error: {e}");
            return 2;
        }
    };
    if let Err(error) = reject_summary_positionals(&positionals) {
        eprintln!("{PROG} summary plan: error: {error}");
        return 2;
    }
    let planner = match flags.get("planner") {
        Some(raw) => match Planner::from_value(raw) {
            Some(value) => value,
            None => {
                eprintln!(
                    "{PROG} summary plan: error: argument --planner: invalid choice: '{raw}' "
                );
                return 2;
            }
        },
        None => Planner::GreedyLpt,
    };
    let output_format = match flags.get("format").map(String::as_str) {
        None | Some("text") => "text",
        Some("json") => "json",
        Some(raw) => {
            eprintln!("{PROG} summary plan: error: argument --format: invalid choice: '{raw}'");
            return 2;
        }
    };
    let summary_file = match flags.get("summary") {
        Some(f) => f,
        None => {
            eprintln!("{PROG} summary plan: error: --summary is required");
            return 2;
        }
    };
    let dag_file = match flags.get("dag") {
        Some(f) => f,
        None => {
            eprintln!("{PROG} summary plan: error: --dag is required");
            return 2;
        }
    };
    let summary = match std::fs::read_to_string(summary_file) {
        Ok(text) => match summary::from_json(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{PROG}: summary plan: {e}");
                return 2;
            }
        },
        Err(e) => {
            eprintln!("{PROG}: summary plan: cannot read {summary_file}: {e}");
            return 2;
        }
    };
    let cfg = match load(dag_file) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("{PROG}: {}", e.0);
            return 2;
        }
    };
    let (core_budget, mem_budget) =
        planning_budgets(planner, flags.get("max-mem").map(|s| s.as_str()), None);
    let max_steps = match flags.get("max-steps") {
        Some(raw) => match parse_positive_i64(raw, "--max-steps") {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("{PROG} summary plan: error: {error}");
                return 2;
            }
        },
        None => None,
    };
    let plan = build_plan_from_summary(&cfg, &summary, planner, core_budget, mem_budget, max_steps);
    if output_format == "json" {
        println!("{}", plan_to_json(&plan));
    } else {
        print!("{}", plan_to_text(&plan));
    }
    0
}

fn cmd_summary_stats(args: &[String]) -> i32 {
    let (_flags, files) = match parse_flags(args, &[]) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("{PROG} summary stats: error: {e}");
            return 2;
        }
    };
    let file = match files.as_slice() {
        [file] => file,
        [] => {
            eprintln!("{PROG} summary stats: error: a summary FILE is required");
            return 2;
        }
        [_, extra @ ..] => {
            eprintln!(
                "{PROG} summary stats: error: unrecognized arguments: {}",
                extra.join(" ")
            );
            return 2;
        }
    };
    let summary = match std::fs::read_to_string(file) {
        Ok(text) => match summary::from_json(&text) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("{PROG}: summary stats: {e}");
                return 2;
            }
        },
        Err(e) => {
            eprintln!("{PROG}: summary stats: cannot read {file}: {e}");
            return 2;
        }
    };
    let (buckets, total, largest) = summary::summary_stats(&summary);
    println!(
        "identity: {}/{}\nbuckets: {buckets}\ntotal_samples: {total}\nmax_bucket_samples: {largest}",
        summary.machine_id, summary.container_class
    );
    0
}

/// Parsed `ascii` / `dot` arguments. `list`, `json`, and `yaml` fill only `dag`.
#[derive(Debug, Default, PartialEq)]
struct ViewArgs {
    dag: Option<String>,
    /// Parsed, non-empty `--labels` list.
    labels: Option<Vec<String>>,
    /// Validated `--group-by` key (one of [`GROUP_BY_KEYS`]).
    group_by: Option<String>,
}

// `ascii` / `dot` arguments: `--dag`, `--labels`, and `--group-by`, each as `--flag VALUE` or
// `--flag=VALUE`. A repeated flag keeps its last value, as `--dag` always has.
fn parse_view_args(rest: &[String]) -> Result<ViewArgs, String> {
    let mut a = ViewArgs::default();
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        if !matches!(flag, "--dag" | "--labels" | "--group-by") {
            return Err(format!("unrecognized argument: {arg}"));
        }
        let value = match inline {
            Some(v) => v,
            None => {
                i += 1;
                // `--labels --dag F` must not read `--dag` as a label, and, as argparse does in
                // the Python edition, no option value may begin with `-`. The `--dag` value
                // keeps the long-standing take-next-argument behaviour of `parse_simple_dag`.
                rest.get(i)
                    .filter(|v| flag == "--dag" || !v.starts_with('-'))
                    .cloned()
                    .ok_or_else(|| format!("the argument {flag} requires a value"))?
            }
        };
        match flag {
            "--dag" => a.dag = Some(value),
            "--labels" => {
                let labels = parse_tag_list(&value);
                if labels.is_empty() {
                    return Err("--labels requires at least one label".to_string());
                }
                a.labels = Some(labels);
            }
            _ => {
                if !GROUP_BY_KEYS.contains(&value.as_str()) {
                    return Err(format!(
                        "argument --group-by: invalid choice: '{value}' (choose from {})",
                        GROUP_BY_KEYS.join(", ")
                    ));
                }
                a.group_by = Some(value);
            }
        }
        i += 1;
    }
    Ok(a)
}

/// Parse the `--dag FILE` argument for the read-only subcommands.
fn parse_simple_dag(rest: &[String]) -> Result<Option<String>, String> {
    let mut dag: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        match arg.split_once('=') {
            Some(("--dag", v)) => dag = Some(v.to_string()),
            _ if arg == "--dag" => {
                i += 1;
                dag = Some(
                    rest.get(i)
                        .cloned()
                        .ok_or_else(|| "the argument --dag requires a value".to_string())?,
                );
            }
            _ => return Err(format!("unrecognized argument: {arg}")),
        }
        i += 1;
    }
    Ok(dag)
}

/// One end-of-run line naming every step that reported non-blocking diagnostic test failures.
///
/// A diagnostic failure does not fail its step, so without this line a passing run would show no
/// trace of it at the bottom of the transcript. `None` when no step reported one.
fn diagnostic_failure_summary(outcomes: &[StepOutcome]) -> Option<String> {
    let per_step: Vec<(String, usize)> = outcomes
        .iter()
        .filter_map(|outcome| {
            let count = outcome
                .test_results
                .as_ref()?
                .iter()
                .filter(|result| result.is_diagnostic_failure())
                .count();
            (count > 0).then(|| (outcome.tag.clone(), count))
        })
        .collect();
    if per_step.is_empty() {
        return None;
    }
    let total: usize = per_step.iter().map(|(_, count)| count).sum();
    Some(format!(
        "{total} non-blocking diagnostic test failure(s) in {}",
        per_step
            .iter()
            .map(|(tag, count)| format!("{tag} ({count})"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostic_failure_summary_names_each_step_and_the_total() {
        use crate::test_results::{TestAttemptOutcome, TestAttemptResult, TestResult};
        let failed = || {
            vec![
                TestAttemptResult::new(1, TestAttemptOutcome::Failed, Some("exit 1".into()))
                    .unwrap(),
            ]
        };
        let mut a = StepOutcome::passed(
            "e2e.a".into(),
            1.0,
            String::new(),
            Some(0),
            Some(2),
            Some(0),
        );
        a.test_results = Some(vec![
            TestResult::diagnostic_failure("x".into(), failed(), "bounded".into()).unwrap(),
            TestResult::diagnostic_failure("y".into(), failed(), "bounded".into()).unwrap(),
        ]);
        let mut b = StepOutcome::passed(
            "e2e.b".into(),
            1.0,
            String::new(),
            Some(0),
            Some(1),
            Some(0),
        );
        b.test_results = Some(vec![TestResult::with_attempt_results(
            "z".into(),
            false,
            failed(),
        )
        .unwrap()]);
        assert_eq!(diagnostic_failure_summary(&[b.clone()]), None);
        assert_eq!(
            diagnostic_failure_summary(&[a, b]).as_deref(),
            Some("2 non-blocking diagnostic test failure(s) in e2e.a (2)")
        );
    }

    use super::*;

    fn tiny() -> DagConfig {
        dag_from_json(
            r#"{"steps":[{"group":"build","job":"app","cmd":"echo {args}"},{"group":"test","job":"unit","cmd":"true","deps":["build.app"]}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn a_wrong_document_error_names_the_path_contents_and_next_action() {
        let path = std::env::temp_dir().join(format!(
            "dagrun-wrong-document-{}-{}.yaml",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, "schema: 2\nbucket: example\ntest: []\n").unwrap();
        let error = load(path.to_str().unwrap()).unwrap_err().0;
        assert_eq!(
            error,
            format!(
                "{}: expected a dagrun DAG document with a top-level 'steps' list; found no \
                 'steps' key (top-level keys: 'bucket', 'schema', 'test'). This may be a \
                 different document type. Pass a dagrun DAG file, or run `dagrun quickstart` \
                 for the schema.",
                path.display()
            )
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cgroup_setup_failure_is_attributed_to_the_environment_before_any_node() {
        assert!(CGROUP_SETUP_ENVIRONMENT_ERROR.starts_with("ENVIRONMENT:"));
        assert!(CGROUP_SETUP_ENVIRONMENT_ERROR.contains("no DAG node started"));
        assert!(CGROUP_SETUP_ENVIRONMENT_ERROR.contains("no product build started"));
    }

    #[test]
    fn sync_upload_reuses_the_local_execution_identity() {
        let rows = vec![
            BTreeMap::from([("step".to_string(), "g.a".to_string())]),
            BTreeMap::from([("step".to_string(), "g.a".to_string())]),
        ];
        let stamped = rows_with_observation_ids(&rows, "batch");
        assert_eq!(stamped[0]["observation_id"], "batch");
        assert_eq!(stamped[1]["observation_id"], "batch");

        let rows = vec![BTreeMap::from([
            ("step".to_string(), "g.a".to_string()),
            ("run_id".to_string(), "persisted".to_string()),
        ])];
        assert_eq!(
            rows_with_observation_ids(&rows, "fallback")[0]["observation_id"],
            "persisted"
        );
    }

    #[test]
    fn passthrough_requires_declaration_and_substitutes() {
        let cfg = tiny();
        let applied = apply_passthrough_args(&cfg, Some("--case xyz")).unwrap();
        assert_eq!(applied.steps[0].cmd, "echo --case xyz");
        let selected = select_steps_by_tags(&cfg, &["test.unit".to_string()], true).unwrap();
        assert!(apply_passthrough_args(&selected, Some("x")).is_err());
    }

    #[test]
    fn selected_step_includes_every_dependency_it_requires() {
        let cfg = dag_from_json(
            r#"{"steps":[
                {"group":"package","job":"tarball","cmd":"true","deps":["test.unit"]},
                {"group":"test","job":"unit","cmd":"true","deps":["build.app"]},
                {"group":"build","job":"app","cmd":"true"}
            ]}"#,
        )
        .unwrap();

        let selected = select_steps_by_tags(&cfg, &["package.tarball".to_string()], false).unwrap();
        assert_eq!(
            selected.steps.iter().map(Step::tag).collect::<Vec<_>>(),
            ["package.tarball", "test.unit", "build.app"]
        );
        assert_eq!(selected.steps[0].deps, ["test.unit"]);
        assert_eq!(selected.steps[1].deps, ["build.app"]);
    }

    #[test]
    fn selected_dependencies_can_be_explicitly_ignored() {
        let cfg = tiny();
        let selected = select_steps_by_tags(&cfg, &["test.unit".to_string()], true).unwrap();
        assert_eq!(
            selected.steps.iter().map(Step::tag).collect::<Vec<_>>(),
            ["test.unit"]
        );
        assert!(selected.steps[0].deps.is_empty());
    }

    #[test]
    fn selected_run_memory_sizing_excludes_omitted_steps_but_keeps_dependencies() {
        let marker = std::env::temp_dir().join(format!(
            "dagrun-selected-memory-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let label_marker = std::env::temp_dir().join(format!(
            "dagrun-label-selected-memory-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&label_marker);
        let cfg = dag_from_json(
            &serde_json::json!({
                "mem_cap_floor_bytes": 8 * 1024_i64.pow(3),
                "steps": [
                    {
                        "group": "g",
                        "job": "large-dependency",
                        "cmd": "true",
                        "labels": ["all"],
                        "hint": {"hard_mem_max_bytes": 8 * 1024_i64.pow(3)},
                    },
                    {
                        "group": "g",
                        "job": "small",
                        "cmd": format!("touch {}", marker.display()),
                        "deps": ["g.large-dependency"],
                        "labels": ["all"],
                        "hint": {"rss_baseline_bytes": 256 * 1024_i64.pow(2)},
                    },
                    {
                        "group": "g",
                        "job": "label-small",
                        "cmd": format!("touch {}", label_marker.display()),
                        "labels": ["all", "small-only"],
                        "hint": {"rss_baseline_bytes": 256 * 1024_i64.pow(2)},
                    },
                ],
            })
            .to_string(),
        )
        .unwrap();
        let common = [
            "--selected=g.small",
            "--max-cpus=2",
            "--max-mem=1G",
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
            "--quiet",
        ]
        .map(String::from);

        let mut ignored = common.to_vec();
        ignored.push("--ignore-selected-deps".into());
        let ignored = parse_run_args(&ignored).unwrap();
        assert_eq!(cmd_run(&cfg, &ignored, &Palette { enabled: false }), 0);
        assert!(marker.exists());

        std::fs::remove_file(&marker).unwrap();
        let closed = parse_run_args(&common).unwrap();
        assert_eq!(cmd_run(&cfg, &closed, &Palette { enabled: false }), 2);
        assert!(!marker.exists());

        let mut all_tags = common.to_vec();
        all_tags[0] = "--selected=g.large-dependency,g.small,g.label-small".into();
        all_tags.push("--ignore-selected-deps".into());
        let all_tags = parse_run_args(&all_tags).unwrap();
        assert_eq!(cmd_run(&cfg, &all_tags, &Palette { enabled: false }), 2);
        assert!(!marker.exists());
        assert!(!label_marker.exists());

        let mut all_labels = common.to_vec();
        all_labels[0] = "--labels=all".into();
        let all_labels = parse_run_args(&all_labels).unwrap();
        assert_eq!(cmd_run(&cfg, &all_labels, &Palette { enabled: false }), 2);
        assert!(!marker.exists());
        assert!(!label_marker.exists());

        let mut partial_label = common.to_vec();
        partial_label[0] = "--labels=small-only".into();
        let partial_label = parse_run_args(&partial_label).unwrap();
        assert_eq!(
            cmd_run(&cfg, &partial_label, &Palette { enabled: false }),
            0
        );
        assert!(label_marker.exists());
        std::fs::remove_file(label_marker).unwrap();
    }

    #[test]
    fn labels_select_a_union_and_include_dependency_ancestry() {
        let cfg = dag_from_json(
            r#"{"steps":[
                {"group":"build","job":"app","cmd":"true"},
                {"group":"test","job":"unit","cmd":"true","labels":["quick","full"],"deps":["build.app"]},
                {"group":"test","job":"slow","cmd":"true","labels":["full"]},
                {"group":"privileged","job":"host","cmd":"true","labels":["super"]}
            ]}"#,
        )
        .unwrap();

        let quick = select_steps_by_labels(&cfg, &["quick".into()]).unwrap();
        assert_eq!(
            quick.steps.iter().map(Step::tag).collect::<Vec<_>>(),
            ["build.app", "test.unit"]
        );
        let union = select_steps_by_labels(&cfg, &["quick".into(), "super".into()]).unwrap();
        assert_eq!(
            union.steps.iter().map(Step::tag).collect::<Vec<_>>(),
            ["build.app", "test.unit", "privileged.host"]
        );
        let error = select_steps_by_labels(&cfg, &["test.unit".into()]).unwrap_err();
        assert!(error.contains("unknown label(s): test.unit"), "{error}");
        assert!(
            error.contains("Known labels: full, quick, super"),
            "{error}"
        );
    }

    /// A stress expansion is one of the two places the PRODUCT rebuilds a `DagConfig` around a
    /// new step list, and it is on the default `run` path (`--stress N`). The carry assertion is
    /// worth nothing if the only configs it ever compares are built inside a test, so apply it
    /// here: a lane's timeouts, caps and memory policy must survive the expansion, and only the
    /// two fields the expansion is FOR may move.
    #[test]
    fn the_stress_expansion_carries_the_whole_lane_policy_forward() {
        let mut cfg = tiny();
        cfg.description = "a real lane".to_string();
        cfg.resource_caps.insert("exclusive".to_string(), 1);
        cfg.mem_cap_factor = 1.5;
        cfg.mem_cap_floor_bytes = 4 * 1024i64.pow(3);
        cfg.outer_mem_safety_factor = 1.2;
        cfg.default_step_timeout = 600;
        cfg.default_jobs_flag = "--jobs {n}".to_string();
        cfg.default_step_cpu_count = Some(4);
        cfg.default_step_cpu_timeout = 120;
        cfg.cpu_timeout_multiplier = 2.0;
        cfg.cpu_timeout_platform = "github-hosted".to_string();

        let expanded = expand_stress(&cfg, 3);
        let named: Vec<String> = crate::model::dag_config_carry_diff(&cfg, &expanded)
            .iter()
            .map(|line| line.split(':').next().unwrap_or("").to_string())
            .collect();
        assert_eq!(named, vec!["steps", "resource_caps"], "{expanded:?}");
        assert_eq!(expanded.steps.len(), 6);
        assert_eq!(expanded.default_step_timeout, 600);
        assert_eq!(expanded.cpu_timeout_multiplier, 2.0);
    }

    #[test]
    fn stress_rewires_each_shard_internally() {
        let expanded = expand_stress(&tiny(), 3);
        assert_eq!(expanded.steps.len(), 6);
        assert_eq!(expanded.steps[0].tag(), "build.app#1");
        assert_eq!(expanded.steps[1].deps, vec!["build.app#1"]);
        assert_eq!(expanded.steps[5].deps, vec!["build.app#3"]);
    }

    #[test]
    fn stress_generation_removes_named_resource_serialization() {
        let cfg = dag_from_json(
            r#"{"resource_caps":{"exclusive":1},"steps":[{"group":"g","job":"j","cmd":"sleep 1","hint":{"resources":{"exclusive":1}}}]}"#,
        )
        .unwrap();
        let expanded = expand_stress(&cfg, 4);
        assert!(expanded.resource_caps.is_empty());
        assert!(expanded
            .steps
            .iter()
            .all(|step| step.hint.resources.is_empty()));
        let result = crate::scheduler::run_dag(&expanded, 4, true, 0);
        assert_eq!(result.outcomes.len(), 4);
        assert_eq!(result.max_concurrent_steps, 4);
    }

    #[test]
    fn stress_sizing_uses_the_run_cpu_ceiling_before_expansion() {
        let gib = 1024i64.pow(3);
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        cfg.steps[0].hint.rss_baseline_bytes = Some(gib);
        cfg.steps[0].hint.classification = crate::model::StepClass::CpuBound;
        cfg.steps[0].hint.preferred_inner_jobs = Some(128);
        cfg.steps[0].jobs_flag = Some("-j%d".into());
        cfg.mem_cap_factor = 1.0;
        cfg.mem_cap_floor_bytes = 0;

        assert_eq!(stress_copy_footprint_bytes(&cfg, None), 32 * gib);
        assert!(validate_max_cpus_rewrite(&cfg, 2).is_ok());
        let capped = cap_config_max_cpus(&cfg, 2);
        assert_eq!(capped.steps.len(), 1);
        assert_eq!(capped.steps[0].hint.preferred_inner_jobs, Some(2));
        assert_eq!(stress_copy_footprint_bytes(&capped, None), gib);
        let expanded = expand_stress(&capped, 2);
        assert_eq!(expanded.steps.len(), 2);
        assert!(expanded
            .steps
            .iter()
            .all(|step| step.hint.preferred_inner_jobs == Some(2)));
        let recapped = cap_config_max_cpus(&expanded, 2);
        assert!(recapped
            .steps
            .iter()
            .all(|step| step.hint.preferred_inner_jobs == Some(2)));
    }

    #[test]
    fn final_stress_footprint_does_not_multiply_an_expanded_graph_twice() {
        let gib = 1024i64.pow(3);
        let mut one = tiny();
        one.steps.truncate(1);
        one.steps[0].hint.rss_baseline_bytes = Some(gib);
        one.mem_cap_factor = 1.0;
        one.mem_cap_floor_bytes = 0;
        let expanded = expand_stress(&one, 2);

        assert_eq!(stress_footprints(&one, 2, false), (gib, 2 * gib));
        assert_eq!(stress_footprints(&expanded, 2, true), (2 * gib, 2 * gib));

        let mut tiny = one.clone();
        tiny.steps[0].hint.rss_baseline_bytes = None;
        tiny.steps[0].hint.hard_mem_max_bytes = Some(1);
        let tiny_expanded = expand_stress(&tiny, 2);
        assert_eq!(stress_footprints(&tiny, 2, false), (gib, 2 * gib));
        assert_eq!(stress_footprints(&tiny_expanded, 2, true), (gib, 2 * gib));
    }

    #[test]
    fn stress_expansion_guard_bounds_generated_nodes_before_allocation() {
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        assert_eq!(stress_expansion_guard(&cfg, MAX_STRESS_GENERATED_NODES), 0);
        assert_eq!(
            stress_expansion_guard(&cfg, MAX_STRESS_GENERATED_NODES + 1),
            2
        );
    }

    #[test]
    fn final_stress_guard_refuses_profile_raised_rss_before_spawn() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_final_stress_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("spawned");
        let (machine, container) = feedback_identity();
        let profile = dir.join(format!("step_profiles_{machine}_{container}.csv"));
        let csv = format!(
            "step,inner_jobs,elapsed_s,peak_bytes,ok,returncode,timed_out,oom_kills\n\
             build.app#1,1,1.0,{},True,0,False,0\n\
             build.app#2,1,1.0,{},True,0,False,0\n",
            i64::MAX,
            i64::MAX,
        );
        std::fs::write(&profile, csv).unwrap();

        let mut cfg = tiny();
        cfg.steps.truncate(1);
        cfg.steps[0].cmd = format!("touch {}", marker.display());
        cfg.steps[0].hint.rss_baseline_bytes = Some(1);
        cfg.steps[0].hint.preferred_inner_jobs = Some(1);
        cfg.steps[0].jobs_flag = Some(String::new());
        cfg.mem_cap_factor = 1.0;
        cfg.mem_cap_floor_bytes = 0;
        let args = parse_run_args(&[
            "--stress=2".into(),
            "--max-cpus=1".into(),
            format!("--perf-dir={}", dir.display()),
            "--no-profile".into(),
            "--unsafe-no-cgroups".into(),
            "--quiet".into(),
        ])
        .unwrap();
        let palette = Palette { enabled: false };

        assert_eq!(cmd_run(&cfg, &args, &palette), 2);
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_help_exposes_every_run_surface() {
        let palette = Palette { enabled: false };
        let help = run_help(&palette);
        for flag in [
            "--max-steps",
            "--max-cpus",
            "--selected",
            "--ignore-selected-deps",
            "--args",
            "--stress",
            "--cores",
            "--profile-sync",
            // `run` owns admission; `box` does not take it, and a help entry filed under the
            // wrong subcommand advertises a flag that subcommand will reject.
            "--admission",
            "--resource-caps-path",
        ] {
            assert!(help.contains(flag), "missing {flag} from run help");
        }
        assert!(
            !box_help(&palette).contains("--admission"),
            "box has no --admission; advertising it there is a promise the parser breaks"
        );
        assert!(!help.contains("--jobs"), "legacy alias must stay hidden");
        assert!(help.contains("maximum active DAG steps"));
        assert!(help.contains("outer CPU-bandwidth limit"));
        assert!(help.contains("maximum width of any one runner-controlled step"));
        assert!(help.contains("every dependency they require"));
        assert!(help.contains("run only the named steps"));
        assert!(help.contains("$DAGRUN_RESOURCE_CAPS_PATH"));
        for cmdtype in [
            "unknown",
            "make",
            "cargo-build",
            "cargo-test",
            "cargo-nextest",
            "generic-dash-j-command",
            "generic-with-flag",
        ] {
            assert!(
                help.contains(cmdtype),
                "missing cmdtype {cmdtype} from run help"
            );
        }
        assert!(help.contains("$DAGRUN_EXTRA_ARGS"));

        assert!(help.contains("--profile-timeseries"));
    }

    #[test]
    fn sweep_help_explains_modes_widths_and_profile_artifacts() {
        let help = sweep_help(&Palette { enabled: false });
        let normalized = help
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for phrase in [
            "single-step dense mode",
            "target-time graph mode",
            "one at a time in stable topological order",
            "pass 1 always completes",
            "every started pass finishes",
            "powers of two through the physical-core count",
            "physical-core and logical-thread counts",
            "1,2,4,8,16,32,64,128,158,316",
            "integer midpoints",
            "cmdtype, jobs_flag, and/or jobs_env",
            "./.dagrun/profiles/",
            "does not redirect terminal or child-process output",
            "scaling_model_*.json",
            "traces/<run_id>.csv",
            "persist every sample and show the fastest wall time",
            "-vv streams child stdout/stderr",
        ] {
            assert!(
                normalized.contains(phrase),
                "missing {phrase:?} from sweep help"
            );
        }
        assert!(help.contains("--jobs WIDTHS"));
        assert!(help.contains("--output-dir DIR"));
        assert!(help.contains("--report-html FILE"));
        assert!(help.contains("--no-report"));
        assert!(normalized.contains("backward-compatible alias for --output-dir"));
        assert!(help.contains("--profile-timeseries"));
        assert!(help.contains("--perf-record"));
        assert!(help.contains("--perf-window DURATION"));
        assert!(help.contains("--wprof-window DURATION"));
        assert!(help.contains("--profiler-sudo"));
        assert!(normalized.contains("never enter the scaling model"));
        assert!(!help.contains("--max-cpus"));
    }

    #[test]
    fn sweep_report_output_flags_validate_before_execution() {
        assert!(parse_sweep_args(&["--output-dir=   ".into()]).is_err());
        assert!(parse_sweep_args(&["--report-html=\t".into()]).is_err());
        assert_eq!(
            parse_sweep_args(&["--output-dir=one".into(), "--perf-dir=two".into(),])
                .err()
                .as_deref(),
            Some("--output-dir and --perf-dir disagree")
        );
        assert_eq!(
            parse_sweep_args(&["--no-profile".into(), "--report-html=report.html".into(),])
                .err()
                .as_deref(),
            Some("--report-html cannot be combined with --no-profile")
        );
        assert_eq!(
            parse_sweep_args(&["--no-report".into(), "--report-html=report.html".into(),])
                .err()
                .as_deref(),
            Some("--report-html cannot be combined with --no-report")
        );
        let args = parse_sweep_args(&[
            "--output-dir=artifacts".into(),
            "--perf-dir=artifacts".into(),
        ])
        .unwrap();
        assert_eq!(args.output_dir.as_deref(), Some("artifacts"));
        assert_eq!(args.perf_dir.as_deref(), Some("artifacts"));
    }

    #[test]
    fn sweep_profiler_flags_are_opt_in_and_validate_before_execution() {
        let args = parse_sweep_args(&[
            "--perf-window=350ms".into(),
            "--wprof-window=400ms".into(),
            "--wprof-window=400ms".into(),
            "--profiler-sudo".into(),
        ])
        .unwrap();
        assert!(args.perf_record);
        assert_eq!(args.perf_window, Some(Duration::from_millis(350)));
        assert_eq!(
            args.wprof_windows,
            [Duration::from_millis(400), Duration::from_millis(400)]
        );
        assert!(args.profiler_sudo);

        let differing =
            parse_sweep_args(&["--wprof-window=300ms".into(), "--wprof-window=500ms".into()])
                .unwrap_err();
        assert!(differing.contains("durations must match"));
        let disabled =
            parse_sweep_args(&["--perf-record".into(), "--no-profile".into()]).unwrap_err();
        assert!(disabled.contains("cannot be combined with --no-profile"));
        assert!(parse_sweep_args(&["--profiler-sudo".into()]).is_err());
        assert!(parse_sweep_args(&["--perf-window=0".into()]).is_err());
    }

    #[test]
    fn profiler_capture_width_is_fit_only_from_current_sweep_samples() {
        let measure = |wall_s: f64, cpu_s: f64| SweepMeasure {
            wall_s,
            user_s: cpu_s,
            sys_s: 0.0,
            rss_hwm: Some(1024),
            ok: true,
            returncode: Some(0),
        };
        let samples = HashMap::from([(
            "build.app".to_string(),
            vec![
                (1, measure(10.0, 10.0)),
                (4, measure(2.5, 10.5)),
                // Faster, but more than the model's 1.5x CPU-work guard permits.
                (8, measure(2.4, 16.0)),
            ],
        )]);
        let model = current_sweep_speedups(&samples);
        assert_eq!(model["build.app"].recommended_inner_jobs, 4);
    }

    #[test]
    fn one_width_sweep_has_an_explicit_capture_fallback() {
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        let observations = vec![
            (
                6,
                SweepMeasure {
                    wall_s: 3.0,
                    user_s: 5.0,
                    sys_s: 1.0,
                    rss_hwm: None,
                    ok: true,
                    returncode: Some(0),
                },
            ),
            (
                6,
                SweepMeasure {
                    wall_s: 5.0,
                    user_s: 6.0,
                    sys_s: 1.0,
                    rss_hwm: None,
                    ok: true,
                    returncode: Some(0),
                },
            ),
        ];
        let selection =
            current_sweep_selection(&cfg.steps[0], &cfg, &observations, "deadbeef").unwrap();
        assert_eq!(selection.inner_jobs, 6);
        assert_eq!(selection.expected_wall_s, 4.0);
        assert_eq!(selection.source, "single-measured-width");
    }

    #[test]
    fn perf_wrapper_keeps_the_width_on_the_guest_command() {
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        cfg.steps[0].cmd = "cargo build".to_string();
        cfg.steps[0].cmdtype = crate::model::CmdType::CargoBuild;
        cfg.steps[0].jobs_flag = None;
        cfg.steps[0].jobs_env = None;
        let request = IsolatedTrialRequest {
            trial_id: "perf-001".to_string(),
            step: "build.app".to_string(),
            inner_jobs: 8,
            kind: crate::profile_capture::CaptureKind::Perf,
            output_dir: PathBuf::from("/tmp/capture"),
            expected_wall_s: 1.0,
            window: crate::profile_capture::centered_capture_window(1.0, 0.4).unwrap(),
            argv_prefix: vec!["perf".to_string(), "record".to_string(), "--".to_string()],
            env: BTreeMap::new(),
            guest_launch: crate::profile_capture::GuestLaunchSignal::default(),
            include_in_model: false,
        };
        let wrapped = profiler_wrapped_step(&cfg.steps[0], &cfg, &request);
        assert!(wrapped
            .cmd
            .starts_with("'perf' 'record' '--' 'env' 'DAGRUN_EXTRA_ARGS=--jobs 8' 'bash' '-c'"));
        assert!(wrapped.cmd.contains("guest-launch-ready.fifo"));
        assert!(wrapped.cmd.contains("guest-launch-release.fifo"));
        assert!(wrapped.cmd.contains("cargo build --jobs 8"));
        assert_eq!(wrapped.cmdtype, crate::model::CmdType::Unknown);
        assert_eq!(wrapped.jobs_flag.as_deref(), Some(""));
        assert_eq!(wrapped.jobs_env.as_deref(), Some(""));
        assert_eq!(wrapped.hint.preferred_inner_jobs, Some(8));
    }

    fn jobs_env_profiled_boundary(channel: &str, width: i64, refused: bool, observe_startup: bool) {
        let root = std::env::temp_dir().join(format!(
            "dagrun-profile-jobs-env-{}-{}-{width}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir(&root).unwrap();
        let profiler = root.join("profiler");
        let guest = root.join("guest");
        let startup = root.join("startup");
        let hook = root.join("startup.sh");
        std::fs::write(
            &hook,
            format!(
                "printf '%s\\n' \"${{{channel}-unset}}\" >> {}\n",
                box_shell_quote(&startup.to_string_lossy())
            ),
        )
        .unwrap();
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        cfg.default_jobs_env = channel.to_string();
        cfg.steps[0].cmd = format!(
            "printf '%s' \"${{{channel}-}}\" > {}",
            box_shell_quote(&guest.to_string_lossy())
        );
        cfg.steps[0].jobs_flag = Some(String::new());
        cfg.steps[0].hint.preferred_inner_jobs = Some(width);
        cfg.steps[0]
            .env
            .insert(channel.to_string(), "99".to_string());
        let mut injected = BTreeMap::from([(channel.to_string(), "88".to_string())]);
        if observe_startup {
            injected.insert("BASH_ENV".to_string(), hook.to_string_lossy().into_owned());
        }
        let request = IsolatedTrialRequest {
            trial_id: "jobs-env".to_string(),
            step: cfg.steps[0].tag(),
            inner_jobs: width,
            kind: crate::profile_capture::CaptureKind::Perf,
            output_dir: root.clone(),
            expected_wall_s: 1.0,
            window: crate::profile_capture::centered_capture_window(1.0, 0.02).unwrap(),
            argv_prefix: vec![
                "/bin/bash".to_string(),
                "-c".to_string(),
                "printf started > \"$1\"; shift; exec \"$@\"".to_string(),
                "profiler".to_string(),
                profiler.to_string_lossy().into_owned(),
            ],
            env: injected,
            guest_launch: crate::profile_capture::GuestLaunchSignal::default(),
            include_in_model: false,
        };
        // Regular files exercise the real shell handshake without claiming controller/FIFO
        // timing or real perf coverage. The stand-in execs the actual inner command.
        std::fs::write(request.guest_launch_release_path(), "go\n").unwrap();
        let wrapped = profiler_wrapped_step(&cfg.steps[0], &cfg, &request);
        assert_eq!(wrapped.jobs_env.as_deref(), Some(""));
        assert_eq!(wrapped.jobs_flag.as_deref(), Some(""));
        let result =
            run_profiler_trial(&cfg.steps[0], &cfg, width, &request, &None, "", 0).unwrap();
        assert_eq!(std::fs::read_to_string(&profiler).unwrap(), "started");
        assert!(
            std::fs::read_to_string(request.guest_launch_ready_path())
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap()
                > 0
        );
        if refused {
            assert!(!result.ok());
            assert_eq!(result.returncode, 125);
            assert!(!guest.exists(), "profiler startup is not guest protection");
        } else {
            assert!(result.ok());
            assert_eq!(std::fs::read_to_string(&guest).unwrap(), width.to_string());
            if observe_startup {
                let observations = std::fs::read_to_string(&startup).unwrap();
                assert_eq!(observations.lines().next(), Some("unset"));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn jobs_env_profiled_valid_widths() {
        jobs_env_profiled_boundary("CARGO_BUILD_JOBS", 1, false, false);
        jobs_env_profiled_boundary("CARGO_BUILD_JOBS", 4, false, false);
    }

    #[test]
    fn jobs_env_profiled_lineno_refuses_guest() {
        jobs_env_profiled_boundary("LINENO", 1, true, false);
    }

    #[test]
    fn jobs_env_profiled_ifs_refuses_guest() {
        jobs_env_profiled_boundary("IFS", 1, true, false);
    }

    #[test]
    fn jobs_env_profiled_startup_precedes_injection() {
        jobs_env_profiled_boundary("WORKER_JOBS", 2, false, true);
    }

    #[test]
    fn profile_timeseries_interval_is_bounded_and_conflicts_with_no_profile() {
        assert_eq!(
            parse_run_args(&["--profile-timeseries=50ms".into()])
                .unwrap()
                .profile_timeseries,
            Some(Duration::from_millis(50))
        );
        assert_eq!(
            parse_sweep_args(&["--profile-timeseries".into(), "10s".into()])
                .unwrap()
                .profile_timeseries,
            Some(Duration::from_secs(10))
        );
        for invalid in ["49ms", "10.001s", "nope"] {
            assert!(
                parse_run_args(&[format!("--profile-timeseries={invalid}")]).is_err(),
                "run accepted {invalid}"
            );
            assert!(
                parse_sweep_args(&[format!("--profile-timeseries={invalid}")]).is_err(),
                "sweep accepted {invalid}"
            );
        }
        assert_eq!(
            parse_run_args(&["--profile-timeseries=100ms".into(), "--no-profile".into()])
                .err()
                .as_deref(),
            Some("--profile-timeseries cannot be combined with --no-profile")
        );
        assert_eq!(
            parse_sweep_args(&["--no-profile".into(), "--profile-timeseries=100ms".into()])
                .err()
                .as_deref(),
            Some("--profile-timeseries cannot be combined with --no-profile")
        );
    }

    #[test]
    fn run_timeseries_refuses_unboxed_before_spawning_a_step() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_timeseries_refusal_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("spawned");
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        cfg.steps[0].cmd = format!("touch {}", marker.display());
        let args = parse_run_args(&[
            "--unsafe-no-cgroups".into(),
            "--profile-timeseries=50ms".into(),
            format!("--perf-dir={}", dir.display()),
            "--quiet".into(),
        ])
        .unwrap();
        assert_eq!(cmd_run(&cfg, &args, &Palette { enabled: false }), 3);
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ordinary_run_profiles_share_one_run_id_and_name_the_unboxed_runner() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_run_profile_provenance_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let args = parse_run_args(&[
            "--unsafe-no-cgroups".into(),
            "--no-profile-feedback".into(),
            format!("--perf-dir={}", dir.display()),
            "--quiet".into(),
        ])
        .unwrap();
        assert_eq!(cmd_run(&tiny(), &args, &Palette { enabled: false }), 0);
        let profile = crate::perflog::store_paths(&dir)
            .into_iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("step_profiles_"))
            })
            .unwrap();
        let text = std::fs::read_to_string(profile).unwrap();
        let mut lines = text.lines();
        let header = crate::perflog::parse_csv_line(lines.next().unwrap());
        let rows: Vec<BTreeMap<String, String>> = lines
            .map(|line| {
                header
                    .iter()
                    .cloned()
                    .zip(crate::perflog::parse_csv_line(line))
                    .collect()
            })
            .collect();
        assert_eq!(rows.len(), 2);
        let run_id = &rows[0]["run_id"];
        assert!(!run_id.is_empty());
        assert!(rows.iter().all(|row| &row["run_id"] == run_id));
        assert!(rows
            .iter()
            .all(|row| row["profile_base_sha"] == row["git_sha"]));
        assert!(rows.iter().all(|row| row["enforcement_kind"] == "unboxed"));
        assert!(rows.iter().all(|row| row["runner_name"] == "run"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sweep_timeseries_refuses_unboxed_before_spawning_a_step() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_sweep_timeseries_refusal_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("spawned");
        let dag = dir.join("dag.json");
        let cfg = dag_from_json(&format!(
            r#"{{"steps":[{{"group":"g","job":"j","cmd":"touch {}","cmdtype":"generic-with-flag","jobs_flag":"--jobs"}}]}}"#,
            marker.display()
        ))
        .unwrap();
        std::fs::write(&dag, dag_to_json(&cfg)).unwrap();
        let args = parse_sweep_args(&[
            format!("--dag={}", dag.display()),
            "--step=g.j".into(),
            "--jobs=1".into(),
            "--unsafe-no-cgroups".into(),
            "--profile-timeseries=50ms".into(),
            format!("--perf-dir={}", dir.display()),
        ])
        .unwrap();
        assert_eq!(cmd_sweep(&args, &Palette { enabled: false }), 3);
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn successful_profiled_sweep_writes_interactive_report_by_default() {
        let root = std::env::temp_dir().join(format!(
            "dagrun_sweep_report_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dag = root.join("dag.json");
        let output = root.join("artifacts");
        std::fs::write(
            &dag,
            r#"{"steps":[{"group":"g","job":"j","cmd":"true","cmdtype":"generic-with-flag","jobs_flag":"--workers="}]}"#,
        )
        .unwrap();
        let args = parse_sweep_args(&[
            format!("--dag={}", dag.display()),
            "--step=g.j".into(),
            "--jobs=1".into(),
            "--unsafe-no-cgroups".into(),
            format!("--output-dir={}", output.display()),
        ])
        .unwrap();

        assert_eq!(cmd_sweep(&args, &Palette { enabled: false }), 0);
        let report = output.join(profile_report::PROFILE_REPORT_FILENAME);
        let html = std::fs::read_to_string(&report).unwrap();
        assert!(html.contains("id=\"dagrun-report-data\""));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn explicit_report_collision_fails_after_preserving_measurements() {
        let root = std::env::temp_dir().join(format!(
            "dagrun_sweep_report_collision_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dag = root.join("dag.json");
        let output = root.join("artifacts");
        let original = r#"{"steps":[{"group":"g","job":"j","cmd":"true","cmdtype":"generic-with-flag","jobs_flag":"--workers="}]}"#;
        std::fs::write(&dag, original).unwrap();
        let args = parse_sweep_args(&[
            format!("--dag={}", dag.display()),
            "--step=g.j".into(),
            "--jobs=1".into(),
            "--unsafe-no-cgroups".into(),
            format!("--output-dir={}", output.display()),
            format!("--report-html={}", dag.display()),
        ])
        .unwrap();

        assert_eq!(cmd_sweep(&args, &Palette { enabled: false }), 1);
        assert_eq!(std::fs::read_to_string(&dag).unwrap(), original);
        assert!(std::fs::read_dir(&output).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("step_profiles_")
        }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_report_collision_warns_without_failing_sweep() {
        let root = std::env::temp_dir().join(format!(
            "dagrun_sweep_default_report_collision_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let dag = root.join("dag.json");
        let output = root.join("artifacts");
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(
            &dag,
            r#"{"steps":[{"group":"g","job":"j","cmd":"true","cmdtype":"generic-with-flag","jobs_flag":"--workers="}]}"#,
        )
        .unwrap();
        let report = output.join(profile_report::PROFILE_REPORT_FILENAME);
        std::fs::write(&report, b"do not overwrite").unwrap();
        let args = parse_sweep_args(&[
            format!("--dag={}", dag.display()),
            "--step=g.j".into(),
            "--jobs=1".into(),
            "--unsafe-no-cgroups".into(),
            format!("--output-dir={}", output.display()),
        ])
        .unwrap();

        assert_eq!(cmd_sweep(&args, &Palette { enabled: false }), 0);
        assert_eq!(std::fs::read(&report).unwrap(), b"do not overwrite");
        assert!(std::fs::read_dir(&output).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("step_profiles_")
        }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resource_caps_path_flag_is_explicit_and_non_empty() {
        let parsed = parse_run_args(&[
            "--resource-caps-path".into(),
            "/tmp/resource-caps.json".into(),
        ])
        .unwrap();
        assert_eq!(
            parsed.resource_caps_path.as_deref(),
            Some("/tmp/resource-caps.json")
        );
        assert_eq!(
            parse_run_args(&["--resource-caps-path=".into()])
                .err()
                .as_deref(),
            Some("--resource-caps-path requires a non-empty path")
        );
    }

    #[test]
    fn ignoring_selected_dependencies_requires_a_selection() {
        let cfg = tiny();
        let args = parse_run_args(&["--ignore-selected-deps".into(), "--quiet".into()]).unwrap();
        let palette = Palette { enabled: false };

        assert_eq!(cmd_run(&cfg, &args, &palette), 2);
    }

    #[test]
    fn retired_only_flag_and_boolean_value_are_rejected() {
        assert_eq!(
            parse_run_args(&["--only".into(), "test.unit".into()])
                .err()
                .as_deref(),
            Some("unrecognized argument: --only")
        );
        assert_eq!(
            parse_run_args(&[
                "--selected=test.unit".into(),
                "--ignore-selected-deps=true".into()
            ])
            .err()
            .as_deref(),
            Some("unrecognized argument: --ignore-selected-deps=true")
        );
    }

    #[test]
    fn run_parser_separates_bare_max_steps_and_max_cpus_and_rejects_zero() {
        let parsed = parse_run_args(&[
            "--dag".into(),
            "dag.json".into(),
            "-s2".into(),
            "-j8".into(),
        ])
        .unwrap();
        assert_eq!(parsed.max_steps, Some(2));
        assert_eq!(parsed.max_cpus, Some(8));

        assert_eq!(
            parse_run_args(&["--max-steps=0".into()]).err().as_deref(),
            Some("--max-steps: must be >= 1")
        );
        assert_eq!(
            parse_run_args(&["--max-cpus=0".into()]).err().as_deref(),
            Some("--max-cpus: must be >= 1")
        );
        assert_eq!(
            parse_run_args(&["--max-cpus=9223372036854775807".into()])
                .err()
                .as_deref(),
            Some("--max-cpus: must be <= 92233720368547")
        );
    }

    #[test]
    fn admission_wait_is_bounded_above_so_a_validated_value_cannot_abort_the_run() {
        // 1e19 seconds used to pass this validator and then panic the process (exit 101,
        // "overflow when adding duration to instant") the moment `admit` built its deadline,
        // while the Python engine ran the same command line normally. Refuse it HERE, in words
        // both engines print, and name the ceiling literally: a test that recomputed it from
        // MAX_ADMISSION_WAIT_S would follow the constant anywhere it was moved.
        for absurd in ["1e19", "1e15", "99999999999999999999", "86400.001"] {
            let err = parse_run_args(&[format!("--admission={absurd}")])
                .err()
                .unwrap_or_else(|| panic!("--admission={absurd} must be refused"));
            assert_eq!(
                err,
                format!(
                    "--admission WAIT_S must be a finite number of seconds in [0, 86400] \
                     (got '{absurd}')"
                )
            );
        }
        assert_eq!(
            parse_run_args(&["--admission=nan".into()]).err().as_deref(),
            Some("--admission WAIT_S must be a finite number of seconds in [0, 86400] (got 'nan')")
        );
        assert_eq!(
            parse_run_args(&["--admission=eleven".into()])
                .err()
                .as_deref(),
            Some("--admission WAIT_S must be a number of seconds (got 'eleven')")
        );
        // The ceiling itself is accepted: the bound is inclusive, and a day-long queue is legal.
        assert_eq!(
            parse_run_args(&["--admission=86400".into()])
                .unwrap()
                .admission,
            Some(86400.0)
        );
        assert_eq!(
            parse_run_args(&["--admission".into()]).unwrap().admission,
            Some(0.0)
        );
    }

    #[test]
    fn admission_consumes_a_negative_number_the_way_argparse_does() {
        // Both engines parse the same command line, so both must agree on which token the flag
        // ATE. Python's argparse takes `-1` as the value (its negative-number matcher) and then
        // rejects it as out of range; an engine that left it unconsumed would report
        // "unrecognized argument" instead -- same exit code, different diagnosis of the typo.
        assert_eq!(
            parse_run_args(&["--admission".into(), "-1".into()])
                .err()
                .as_deref(),
            Some("--admission WAIT_S must be a finite number of seconds in [0, 86400] (got '-1')")
        );
        assert_eq!(
            parse_run_args(&["--admission".into(), "-0.5".into()])
                .err()
                .as_deref(),
            Some(
                "--admission WAIT_S must be a finite number of seconds in [0, 86400] (got '-0.5')"
            )
        );
        // A real flag is still a flag: `--admission --quiet` is a bare admission plus --quiet,
        // exactly as argparse's `nargs="?"` reads it.
        let parsed = parse_run_args(&["--admission".into(), "--quiet".into()]).unwrap();
        assert_eq!(parsed.admission, Some(0.0));
        assert!(parsed.quiet);
        // And `-1e19` is NOT a negative number to argparse, so it is not one here either.
        assert!(!looks_like_negative_number("-1e19"));
        assert!(!looks_like_negative_number("-"));
        assert!(!looks_like_negative_number("--quiet"));
        assert!(looks_like_negative_number("-1"));
        assert!(looks_like_negative_number("-.5"));
        assert!(looks_like_negative_number("-12.75"));
    }

    #[test]
    fn run_parser_accepts_hidden_jobs_alias_but_rejects_conflicting_limits() {
        let legacy = parse_run_args(&["--jobs=8".into()]).unwrap();
        assert_eq!(legacy.max_cpus, Some(8));

        let same = parse_run_args(&["--max-cpus=8".into(), "--jobs=8".into()]).unwrap();
        assert_eq!(same.max_cpus, Some(8));

        assert_eq!(
            parse_run_args(&["--max-cpus=8".into(), "--jobs=4".into()])
                .err()
                .as_deref(),
            Some("--max-cpus and legacy --jobs disagree")
        );
        assert_eq!(
            parse_run_args(&["--jobs=4".into(), "-j8".into()])
                .err()
                .as_deref(),
            Some("--max-cpus and legacy --jobs disagree")
        );
    }

    #[test]
    fn cores_and_memory_can_only_tighten_run_limits() {
        let mut args = parse_run_args(&[
            "--max-cpus".into(),
            "8".into(),
            "--cores".into(),
            "3".into(),
            "--max-steps".into(),
            "9".into(),
            "--max-mem".into(),
            "2G".into(),
        ])
        .unwrap();
        assert_eq!(select_max_cpus(&args), 3);

        let cfg = tiny();
        let memory_steps = jobs_for_budget(&cfg, parse_size("2G").unwrap()).0;
        assert_eq!(select_max_steps(&cfg, &args, 3), 9.min(memory_steps));

        args.max_mem = None;
        assert_eq!(select_max_steps(&cfg, &args, 3), 9);
    }

    #[test]
    fn delegated_parent_clamps_explicit_run_budgets_and_rejects_wide_cpuset() {
        let args = parse_run_args(&[
            "--max-cpus=8".into(),
            "--max-mem=8G".into(),
            "--cores=8".into(),
        ])
        .unwrap();
        assert_eq!(select_max_cpus_bounded(&args, Some(2)), 2);
        assert_eq!(
            select_max_mem_bytes(&args, Some(2 * 1024_i64.pow(3))),
            Some(2 * 1024_i64.pow(3))
        );
        assert_eq!(
            inherited_cores_error(&args, Some(2)).as_deref(),
            Some(
                "--cores 8 requests an exact reservation wider than the inherited delegated \
                 CPU ceiling 2; refusing"
            )
        );

        let defaults = parse_run_args(&[]).unwrap();
        assert_eq!(select_max_cpus_bounded(&defaults, Some(2)), 2);
        assert_eq!(
            select_max_mem_bytes(&defaults, Some(2 * 1024_i64.pow(3))),
            Some(2 * 1024_i64.pow(3))
        );
        assert!(uses_inherited_memory_budget(
            &defaults,
            Some(2 * 1024_i64.pow(3))
        ));
        assert!(uses_inherited_memory_budget(
            &args,
            Some(2 * 1024_i64.pow(3))
        ));
        let tighter = parse_run_args(&["--max-mem=1G".into()]).unwrap();
        assert!(!uses_inherited_memory_budget(
            &tighter,
            Some(2 * 1024_i64.pow(3))
        ));
        let invalid = parse_run_args(&["--max-mem=not-a-size".into()]).unwrap();
        assert_eq!(
            select_max_mem_bytes(&invalid, Some(2 * 1024_i64.pow(3))),
            Some(2 * 1024_i64.pow(3))
        );
        assert!(sweep_cpu_error(8, Some(2)).is_some());
        assert!(sweep_cpu_error(2, Some(2)).is_none());
        assert_eq!(
            selected_fixed_sweep_cpu_error("g.fixed", 8, 2, true).as_deref(),
            Some(
                "selected fixed-width step 'g.fixed' requests 8 CPUs, exceeding the effective \
                 2-CPU budget; refusing an empty successful sweep"
            )
        );
        assert!(selected_fixed_sweep_cpu_error("g.fixed", 8, 2, false).is_none());
        let cfg = tiny();
        assert!(sweep_memory_error(&cfg.steps[0], &cfg, 8, Some(1)).is_some());

        let skipped = dag_from_json(
            r#"{"steps":[{"group":"g","job":"skipped","cmd":"true","skip_reason":"empty-manifest-bucket","hint":{"hard_mem_max_bytes":8589934592}}]}"#,
        )
        .unwrap();
        assert_eq!(
            target_sweep_step_resource_error(
                &skipped.steps[0],
                &skipped,
                2,
                MachineTopology {
                    physical_cores: 2,
                    logical_cpus: 2,
                },
                Some(1024_i64.pow(3)),
                false,
            ),
            None,
            "a skipped step cannot violate an inherited budget it never consumes"
        );

        // The 8 GiB top-level modeling floor is not a claim that an uncharacterized step cannot
        // run in a deliberately smaller parent. Clamp only that run-level floor for inherited
        // admission; real per-step caps remain unchanged and still decide whether a step fits.
        assert_eq!(
            select_max_steps_with_budget(&cfg, &defaults, 2, Some(2 * 1024_i64.pow(3)), false,),
            0
        );
        assert_eq!(
            select_max_steps_with_budget(&cfg, &defaults, 2, Some(2 * 1024_i64.pow(3)), true,),
            2
        );
    }

    #[test]
    fn successful_cli_result_fails_closed_after_coordinator_restore_failure() {
        assert_eq!(restoration_checked_exit_code(0, 4, 5), 3);
        assert_eq!(restoration_checked_exit_code(0, 4, 4), 0);
        assert_eq!(
            restoration_checked_exit_code(1, 4, 5),
            1,
            "an existing command failure remains the primary exit status"
        );
    }

    #[test]
    fn an_absent_step_ceiling_defaults_to_the_resolved_cpu_budget() {
        // #36 dag-core-budget-split. `-s` and `-j` are separate controls, but ONE coupling
        // between them survives on purpose: an absent `-s` defaults to whatever `-j` resolved
        // to. That is the last place a CPU number silently decides DAG concurrency, so it is
        // pinned here rather than left to be rediscovered from the scheduler source. Python
        // pins the same default in tests/test_run_parallel_limits.py; this side had no test at
        // all, which is exactly the one-edition gap the differential exists to close.
        //
        // Every number below is written literally, never read back out of the flag it pins.
        let cfg = tiny();

        let explicit_cpus = parse_run_args(&["--max-cpus".into(), "3".into()]).unwrap();
        assert_eq!(explicit_cpus.max_steps, None);
        assert_eq!(select_max_cpus(&explicit_cpus), 3);
        assert_eq!(select_max_steps(&cfg, &explicit_cpus, 3), 3);

        // The hidden 0.13 alias must reach the same default, or a 0.13 script would keep its
        // CPU budget and silently lose its step ceiling.
        let legacy = parse_run_args(&["--jobs".into(), "7".into()]).unwrap();
        assert_eq!(legacy.max_steps, None);
        assert_eq!(select_max_cpus(&legacy), 7);
        assert_eq!(select_max_steps(&cfg, &legacy, 7), 7);

        // And an explicit `-s` is NOT the CPU budget: the two numbers stay apart.
        let both =
            parse_run_args(&["--max-cpus".into(), "3".into(), "-s".into(), "5".into()]).unwrap();
        assert_eq!(select_max_cpus(&both), 3);
        assert_eq!(select_max_steps(&cfg, &both, 3), 5);

        // The exact sentence the README and the user guide put in front of a user: `run -j200`
        // alone permits TWO HUNDRED active nodes. Pinned at that literal magnitude on purpose.
        // Every other value here and in the differential is 1, 3, 5 or 7, so a clamp on the
        // default -- `unwrap_or(max_cpus.min(8))` is the one-token version -- would leave the
        // whole suite green while falsifying the shipped sentence. 200 is not the host CPU
        // count, not a configured default, and not read back out of any constant.
        let two_hundred = parse_run_args(&["-j".into(), "200".into()]).unwrap();
        assert_eq!(two_hundred.max_steps, None);
        assert_eq!(select_max_cpus(&two_hundred), 200);
        assert_eq!(select_max_steps(&cfg, &two_hundred, 200), 200);
    }

    #[test]
    fn the_parsed_max_mem_flag_is_what_the_outer_scope_ceiling_is_built_from() {
        // #33: --max-mem used to feed the sizing model ONLY. This is the accessor scope bring-up
        // reads, so a rename that stopped feeding the scope would fail here rather than quietly
        // restoring a 90%-of-host boundary under the name of a 20 GiB budget.
        let args = parse_run_args(&["--max-mem".into(), "20G".into()]).unwrap();
        assert_eq!(
            requested_max_mem_bytes(args.max_mem.as_deref()),
            Some(20 * 1024i64.pow(3))
        );
        // Absent, empty and unparseable are all "no outer ceiling requested": select_max_steps
        // already names an unparseable spec, and refusing it twice would give one typo two exit
        // paths depending on which check ran first.
        let none = parse_run_args(&[]).unwrap();
        assert_eq!(requested_max_mem_bytes(none.max_mem.as_deref()), None);
        assert_eq!(requested_max_mem_bytes(Some("")), None);
        assert_eq!(requested_max_mem_bytes(Some("twenty gigs")), None);
        assert_eq!(requested_max_mem_bytes(Some("0")), None);
    }

    #[test]
    fn nonbinding_max_mem_cannot_loosen_default_max_steps_past_max_cpus() {
        assert_eq!(apply_memory_step_ceiling(2, 316), 2);
        assert_eq!(apply_memory_step_ceiling(2, 1), 1);
    }

    #[test]
    fn max_mem_refuses_when_one_step_cannot_fit() {
        let mut cfg = tiny();
        cfg.steps.truncate(1);
        cfg.steps[0].hint.rss_baseline_bytes = Some(2 * 1024i64.pow(3));
        cfg.mem_cap_factor = 1.0;
        cfg.mem_cap_floor_bytes = 0;
        cfg.outer_mem_safety_factor = 1.0;
        let args = parse_run_args(&["--max-mem=1G".into()]).unwrap();
        assert_eq!(select_max_steps(&cfg, &args, 8), 0);
    }

    #[test]
    fn run_core_budget_reaches_every_planner_but_memory_remains_cpa_only() {
        for planner in [Planner::GreedyLpt, Planner::CriticalPath, Planner::Cpa] {
            let (core_budget, mem_budget) = planning_budgets(planner, Some("2G"), Some(16));
            assert_eq!(core_budget, Some(16), "{planner:?}");
            if planner == Planner::Cpa {
                assert_eq!(mem_budget, Some(2 * 1024i64.pow(3)));
            } else {
                assert_eq!(mem_budget, None, "{planner:?}");
            }
        }

        // A standalone ordering-only `plan` has no run cgroup to constrain and must not invent a
        // budget. CPA still resolves its machine budget because allocation requires one.
        assert_eq!(
            planning_budgets(Planner::GreedyLpt, None, None),
            (None, None)
        );
        assert_eq!(
            planning_budgets(Planner::CriticalPath, None, None),
            (None, None)
        );
        assert!(planning_budgets(Planner::Cpa, None, None).0.is_some());
    }

    #[test]
    fn plan_parser_accepts_a_positive_max_steps_ceiling() {
        let parsed = parse_plan_args(&["--max-steps=3".into()]).unwrap();
        assert_eq!(parsed.max_steps, Some(3));
        assert_eq!(
            parse_plan_args(&["--max-steps=0".into()]).err().as_deref(),
            Some("--max-steps: must be >= 1")
        );
    }

    #[test]
    fn run_refuses_an_unrewritable_width_before_cgroup_setup() {
        let mut cfg = tiny();
        cfg.steps[0].hint.preferred_inner_jobs = Some(8);
        cfg.steps[0].jobs_flag = Some(String::new());
        let args = parse_run_args(&["--max-cpus=2".into()]).unwrap();
        let palette = Palette { enabled: false };

        assert_eq!(cmd_run(&cfg, &args, &palette), 2);
    }

    #[test]
    fn sweep_refuses_a_step_whose_guest_width_cannot_be_varied() {
        let mut cfg = tiny();
        cfg.default_jobs_flag.clear();
        cfg.steps[0].jobs_flag = None;
        let path = std::env::temp_dir().join(format!(
            "dagrun_sweep_empty_jobs_flag_{}_{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&path, dag_to_json(&cfg)).unwrap();
        let args = SweepArgs {
            dag: Some(path.display().to_string()),
            step: Some("build.app".to_string()),
            jobs: Some("1..2".to_string()),
            target_time: None,
            repeat: 1,
            output_dir: None,
            perf_dir: None,
            no_profile: true,
            report_html: None,
            no_report: false,
            profile_timeseries: None,
            perf_record: false,
            perf_window: None,
            wprof_windows: Vec::new(),
            profiler_sudo: false,
            allow_cgroup_failure: false,
            unsafe_no_cgroups: false,
            verbosity: 0,
        };
        let palette = Palette { enabled: false };

        assert_eq!(cmd_sweep(&args, &palette), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn profile_enforcement_tracks_cgroup_manager_presence() {
        assert_eq!(profile_enforcement_kind(true), "cgroup-v2");
        assert_eq!(profile_enforcement_kind(false), "unboxed");
    }

    #[test]
    fn summary_action_help_exposes_only_the_action_contract() {
        let palette = Palette { enabled: false };
        let cases = [
            (
                summary_build_help(&palette),
                "summary build",
                &["--perf-dir", "--out", "--reservoir-cap"][..],
                &["--summary", "--dag"][..],
            ),
            (
                summary_merge_help(&palette),
                "summary merge",
                &["FILE [FILE ...]", "--out", "--reservoir-cap"][..],
                &["--perf-dir", "--summary", "--dag"][..],
            ),
            (
                summary_plan_help(&palette),
                "summary plan",
                &[
                    "--summary",
                    "--dag",
                    "--planner",
                    "--max-mem",
                    "--max-steps",
                    "--format",
                ][..],
                &["--perf-dir", "--out", "--reservoir-cap"][..],
            ),
            (
                summary_stats_help(&palette),
                "summary stats",
                &["FILE"][..],
                &["--perf-dir", "--out", "--summary", "--dag"][..],
            ),
        ];
        for (help, usage, required, forbidden) in cases {
            assert!(help.contains(usage), "missing action usage {usage}");
            for token in required {
                assert!(help.contains(token), "missing {token} from {usage} help");
            }
            for token in forbidden {
                assert!(!help.contains(token), "unexpected {token} in {usage} help");
            }
        }
    }

    #[test]
    fn nested_summary_help_routes_by_action() {
        let palette = Palette { enabled: false };
        for action in ["build", "merge", "plan", "stats"] {
            let args = vec![action.to_string(), "--help".to_string()];
            let help = requested_summary_help(&palette, &args).expect("help requested");
            assert!(help.contains(&format!("summary {action}")));
            assert!(!help.contains("summary <action>"));
        }
        let top = vec!["--help".to_string()];
        let top_help = requested_summary_help(&palette, &top).expect("top-level summary help");
        assert!(top_help.contains("summary <action>"));
        assert!(top_help.contains("build a plan from a summary JSON and DAG"));
        assert!(top_help.contains("merge one or more summary JSON files"));
        assert!(!top_help.contains("plan a summary sync from a backend spec"));
    }

    #[test]
    fn summary_action_schemas_reject_invalid_invocations() {
        let cases = [
            vec!["summary", "build", "unexpected"],
            vec![
                "summary",
                "plan",
                "unexpected",
                "--summary",
                "missing-summary.json",
                "--dag",
                "missing-dag.json",
            ],
            vec!["summary", "stats", "one.json", "two.json"],
            vec!["summary", "merge"],
            vec!["summary", "build", "--reservoir-cap", "nope"],
            vec!["summary", "build", "--reservoir-cap", "0"],
            vec!["summary", "build", "--reservoir-cap", "9223372036854775808"],
            vec!["summary", "merge", "missing.json", "--reservoir-cap", "-1"],
            vec![
                "summary",
                "plan",
                "--summary",
                "missing-summary.json",
                "--dag",
                "missing-dag.json",
                "--planner",
                "unknown",
            ],
            vec![
                "summary",
                "plan",
                "--summary",
                "missing-summary.json",
                "--dag",
                "missing-dag.json",
                "--format",
                "xml",
            ],
            vec!["summary", "build", "--perf-dir"],
            vec!["summary", "build", "--", "--looks-like-an-option"],
            vec!["summary", "plan", "--summary", "--dag", "missing-dag.json"],
        ];
        for raw in cases {
            let args: Vec<String> = raw.into_iter().map(str::to_string).collect();
            assert_eq!(run(&args), 2, "accepted invalid invocation: {args:?}");
        }
    }

    // ---- #82 runner-box-subcommand --------------------------------------------------------

    fn box_args(argv: &[&str]) -> BoxArgs {
        parse_box_args(&argv.iter().map(|s| s.to_string()).collect::<Vec<_>>()).expect("parses")
    }

    #[test]
    fn argv_is_shell_quoted_element_by_element_not_joined() {
        // A step's `cmd` goes to `bash -c`, so a joined argv turns arguments into shell SYNTAX.
        let argv: Vec<String> = [
            "printf",
            "[%s]",
            "a b",
            "it's",
            "semi;colon",
            "$(echo pwned)",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let cfg = box_config(&argv, "probe", None, 30, 1);
        assert_eq!(
            cfg.steps[0].cmd,
            r#"'printf' '[%s]' 'a b' 'it'\''s' 'semi;colon' '$(echo pwned)'"#
        );
    }

    #[test]
    fn the_cpu_ceiling_is_derived_from_the_wall_ceiling_and_the_core_count() {
        // Left to default, the deliberately tiny 10-second per-step CPU floor -- a forcing
        // function for an UNDECLARED DAG node -- would cut an honest ad-hoc command short for a
        // reason its author never named.
        let argv = vec!["true".to_string()];
        let cfg = box_config(&argv, "probe", None, 30, 4);
        assert_eq!(cfg.steps[0].timeout, 30);
        assert_eq!(
            cfg.steps[0].cpu_timeout, 120,
            "30 wall-seconds across 4 cores is 120 CPU-seconds"
        );
    }

    #[test]
    fn cores_becomes_the_cgroup_width_and_not_a_synthetic_jobs_flag() {
        // `preferred_inner_jobs` would append `-j K` to a command that may not accept one.
        let argv = vec!["some-tool".to_string(), "--flag".to_string()];
        let cfg = box_config(&argv, "probe", None, 30, 3);
        assert_eq!(cfg.steps[0].hint.preferred_inner_jobs, None);
        assert!(!cfg.steps[0].cmd.contains("-j "));
        assert_eq!(cfg.default_step_cpu_count, Some(3));
    }

    #[test]
    fn mem_becomes_the_steps_own_hard_inner_cap() {
        let argv = vec!["true".to_string()];
        let cfg = box_config(&argv, "probe", Some(512 * 1024 * 1024), 30, 1);
        assert_eq!(
            cfg.steps[0].hint.hard_mem_max_bytes,
            Some(512 * 1024 * 1024)
        );
    }

    #[test]
    fn a_mem_below_the_modeled_floor_still_runs_the_command() {
        // 512 MiB is the value the guide, the help text and #82 all show. It has to WORK.
        //
        // `--mem` reaches the run as `--max-mem`, which is checked against the graph's MODELED
        // worst-case footprint. That model is floored at `mem_cap_floor_bytes` (8 GiB by default)
        // so sizing an uncharacterized graph never concludes "zero steps fit" -- and a boxed
        // command that states its own hard cap is not uncharacterized. With the floor left alone,
        // EVERY `--mem` below 8 GiB exited 2 with "REFUSED — minimum runnable footprint
        // 8589934592 bytes cannot fit safely within budget 536870912 bytes": the flag worked only
        // at or above the default it exists to lower.
        let dir = std::env::temp_dir().join(format!("dagrun_box_mem_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let argv: Vec<String> = [
            "box",
            "--allow-cgroup-failure",
            "--perf-dir",
            dir.to_str().unwrap(),
            "--mem",
            "512M",
            "--timeout",
            "30",
            "--",
            "true",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            run(&argv),
            0,
            "a 512 MiB box must RUN, not be refused as too small to hold a fictional 8 GiB step"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mem_is_also_the_ceiling_the_outer_scope_is_brought_up_with() {
        // The other half of "applied TWICE": the scope's own MemoryMax, not just the step's inner
        // cap. `requested_max_mem_bytes` is the exact accessor `cmd_run` uses to feed scope
        // bring-up, so reading the run arguments `box` builds through it is reading the number the
        // scope would be created with. Deleting the forwarding leaves this `None`.
        let a = box_args(&["--mem", "512M", "--", "true"]);
        let run_args = box_run_args(&a).expect("run's own defaults must parse");
        assert_eq!(
            requested_max_mem_bytes(run_args.max_mem.as_deref()),
            Some(512 * 1024 * 1024),
            "--mem must be the MemoryMax the outer scope is created with"
        );
    }

    #[test]
    fn box_flags_parse_in_both_spellings_and_the_command_starts_at_the_separator() {
        let a = box_args(&[
            "--mem=512M",
            "--timeout",
            "45",
            "-j",
            "2",
            "--label=probe",
            "-q",
            "--",
            "echo",
            "--not-a-flag",
        ]);
        assert_eq!(a.mem.as_deref(), Some("512M"));
        assert_eq!(a.timeout, Some(45));
        assert_eq!(a.cores, Some(2));
        assert_eq!(a.label.as_deref(), Some("probe"));
        assert!(a.quiet);
        assert_eq!(
            a.command,
            vec!["echo".to_string(), "--not-a-flag".to_string()]
        );
    }

    #[test]
    fn a_nonpositive_cores_or_timeout_is_refused_by_name_rather_than_clamped() {
        for flag in ["--cores", "--timeout"] {
            let err = parse_box_args(&[flag.to_string(), "0".to_string()])
                .expect_err("zero must be refused");
            assert!(err.contains(flag), "message was {err:?}");
            assert!(err.contains(">= 1"), "message was {err:?}");
        }
    }

    #[test]
    fn box_with_no_command_is_a_named_usage_error_not_a_boxed_nothing() {
        assert_eq!(run(&["box".to_string()]), 2);
    }

    #[test]
    fn a_bad_memory_spec_is_refused_rather_than_run_uncapped() {
        // Silently ignoring an unparseable `--mem` would run the command with NO cap at all --
        // the opposite of what the flag was reached for. `--allow-cgroup-failure` is here so a
        // regression FAILS rather than hanging on real cgroup bring-up.
        let argv: Vec<String> = [
            "box",
            "--allow-cgroup-failure",
            "--mem",
            "lots",
            "--",
            "true",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run(&argv), 2);
    }

    #[test]
    fn box_is_listed_in_the_top_level_help_and_accepted_as_a_command() {
        let c = Palette { enabled: false };
        assert!(help_text(&c).contains(" box "));
        assert_eq!(run(&["box".to_string(), "--help".to_string()]), 0);
    }
}
