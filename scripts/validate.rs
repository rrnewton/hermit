#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! validate.rs — Hermit's validation driver.
//!
//! This is the sole validation driver. Every production caller invokes it
//! directly; the former shell implementation has been removed. The repository-
//! root `validate.sh` is an audited reminder alias with no independent behavior.
//!
//! # Contract
//!
//! * **Everything runs as a `dagrun` node.** Preflight, the manifest
//!   gate, every CI-lane node, and every compatibility probe. The driver makes
//!   exactly one kind of call — `run_dag_boxed_deadline` (unbounded when no
//!   whole-run budget is supplied) — and never spawns a gate itself. See
//!   `lib/validate_plan.rs` for why that rule is load-bearing and for
//!   the measured evidence that an undeclared node is unboxed.
//! * **Boxing is fail-closed.** Default path re-execs into a transient
//!   `systemd --user` scope; if two-level cgroup-v2 boxing cannot be established
//!   the driver exits 3 rather than running unboxed.
//! * **Output is bounded by default.** Verbosity 1 prints O(1) lifecycle lines per
//!   DAG step. Verbosity 2 streams tagged step output, and verbosity 5 additionally
//!   carries the deepest test identity the runner can observe on every streamed line.
//!   Failures always print their complete captured detail at every level.
//! * **Every claim carries its conditions.** One ledger write point emits the
//!   profile, the executed/skipped/failed counts, commit anchoring, the tree hash,
//!   the toolchain, and the absolute durable log path together, so a downstream
//!   reader can never pair a bare `pass` with inferred coverage.
//! * **`HERMIT_DIR` is a USER-facing setting.** Validation never writes there.
//!   Run state goes to `target/validation/`, durable logs to `ignored/validate/`.
//!
//! # CLI
//!
//! Most of the flag surface preserves the former driver's CLI because in-tree
//! callers depend on it — notably
//! `ci/dag/portable.json`'s `test.strict_compat` node, which invokes
//! `./scripts/validate.rs --portable-strict-compat-only`, plus
//! `.github/workflows/validation-levels.yml`, three `Makefile` targets, and
//! `hermit-cli/tests/{analyze,rr_suite}.rs`. The inner dirty-tree and rebase-
//! freshness escape names its limited scope explicitly; its in-tree callers are
//! updated with it.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! serde_json = "1"
//! sha2 = "0.10"
//! libc = "0.2"
//! ```

// `serde_json::json!` expands one recursive macro level PER FIELD, and the ledger
// record is one literal carrying every qualification a reader needs. Keeping it a
// single literal is the point — it is what makes "the row states its own
// conditions" checkable by eye — so the limit is raised rather than the record
// split across statements where a field could be added on one path and not the
// other.
#![recursion_limit = "512"]

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

#[path = "lib/validate_corpus.rs"]
mod validate_corpus;

#[path = "lib/validate_envelope.rs"]
mod validate_envelope;

#[path = "lib/validate_history.rs"]
mod validate_history;

#[path = "lib/validate_cell_results.rs"]
mod validate_cell_results;

#[path = "lib/validate_plan.rs"]
mod validate_plan;

#[path = "lib/validate_receipt.rs"]
mod validate_receipt;

#[path = "lib/validate_runtime.rs"]
mod validate_runtime;

#[path = "lib/safe_ci_scope.rs"]
mod safe_ci_scope;

#[path = "lib/validate_super.rs"]
mod validate_super; // Normalizes and audits extracted Cargo tests/synthetic args onto nextest.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use dagrun::cgroup::aggregate_slice_max_cpus;
use dagrun::cgroup::is_in_scope;
use dagrun::model::DagConfig;
use dagrun::model::RunResult;
use dagrun::model::Step;
use dagrun::model::StepOutcome;
use dagrun::container_core_budget;
use dagrun::perflog::append_step_profiles;
use dagrun::scheduler::run_dag_boxed_deadline;
use dagrun::scheduler::steps_violating_run_timeout;
use dagrun::scheduler::BoxedCgroups;
use dagrun::scheduler::monotonic_now_ns;
use dagrun::scheduler::STEP_STARTED_MONOTONIC_NS_ENV;

use validate_plan::CompatMode;
use validate_plan::CompatDisposition;

/// Current receipt schema. Unknown scalar evidence is represented by explicit
/// nulls; optional collections stay type-safe by being omitted when inapplicable
/// or serialized as `[]` when positively known empty. A new writer must never
/// downgrade itself into the schema-4 grandfather.
const COVERAGE_LEDGER_SCHEMA_VERSION: i64 = 5;

/// Recorded in each row so a version-aware reader can tell which driver produced
/// it without inference.
const LEDGER_PRODUCER: &str = "hermit-validate-rs";

/// The Reverie-pin preflight node's tag. Named once so the plan that creates it
/// and the fail-closed assertion that requires it cannot drift apart.
const PIN_GATE_TAG: &str = "pre.reverie_pin";
const MANIFEST_AUDIT_COMMAND: &str = "target/debug/test-harness validate";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationStepIdentity {
    ManifestAudit,
    ManifestRun,
    Other,
}

fn validation_step_identity(step: &Step) -> ValidationStepIdentity {
    let manifest_group = step.group == "e2e" || step.group.ends_with("-e2e");
    if (step.group == "gate" && step.job == "manifest")
        || (manifest_group && step.job == "metadata")
    {
        ValidationStepIdentity::ManifestAudit
    } else if manifest_group && step.job.starts_with("manifest_") {
        ValidationStepIdentity::ManifestRun
    } else {
        ValidationStepIdentity::Other
    }
}

fn set_manifest_attempt(step: &mut Step, attempt: usize) {
    if validation_step_identity(step) == ValidationStepIdentity::ManifestRun
        || step.cmd.contains("target/debug/test-harness run ")
    {
        step.env.insert(
            validate_plan::E2E_ATTEMPT_ENV.into(),
            attempt.to_string(),
        );
    }
}

const LEDGER_ENV: &str = "HERMIT_VALIDATE_LEDGER";
const PARENT_ENV: &str = "DEV_HERMIT_PARENT";
const OWN_SCOPE_DEADLINE_ENV: &str = "HERMIT_VALIDATE_SCOPE_DEADLINE_MONOTONIC_NS";
const NESTED_SCOPE_SELF_TEST_ENV: &str = "HERMIT_VALIDATE_NESTED_SCOPE_SELF_TEST";
const SUMMARY_EPILOGUE_SELF_TEST_ENV: &str = "HERMIT_VALIDATE_SUMMARY_EPILOGUE_SELF_TEST";
const NESTED_SCOPE_OUTER: &str = "outer";
const NESTED_SCOPE_INNER: &str = "inner";
const NESTED_SCOPE_SIGNAL: &str = "signal";
const NESTED_INNER_STEP_S: i64 = 2;
const NESTED_INNER_RUN_S: i64 = 5;
const NESTED_OUTER_CHILD_STEP_S: i64 = 10;
const NESTED_OUTER_CHILD_RUN_S: i64 = 12;
const NESTED_SIGNAL_STEP_S: i64 = 5;
const NESTED_SIGNAL_RUN_S: i64 = 7;
const NESTED_SURVIVOR_STEP_S: i64 = 2;
const NESTED_SURVIVOR_RUN_S: i64 = 4;
const NESTED_SCOPE_RUNTIME_S: i64 = 30;
const NESTED_WRAPPER_TIMEOUT_S: i64 = 45;

/// Standalone-only in-repo ledger directory.
///
/// Admitted runs never write here: they send their HistoryRow to the parent's
/// canonical adapter. This fallback exists only for a checkout with no
/// dev-hermit parent and is deliberately not a qualifying receipt authority.
const LEDGER_DIR: &str = "ci/validate-ledger";

/// Fleet/team identity component of the shard name. Overridable so a different
/// team's runs land in a different shard rather than interleaving.
const LEDGER_TEAM_ENV: &str = "VALIDATE_LEDGER_TEAM";
const LEDGER_TEAM_DEFAULT: &str = "local";

// --------------------------------------------------------------------------- args

/// Validation level, mirroring `VALIDATION_LEVEL`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Quick,
    PortableOnly,
    Full,
    Super,
}

impl Level {
    fn parse(s: &str) -> Option<Level> {
        match s {
            "quick" => Some(Level::Quick),
            "portable-only" => Some(Level::PortableOnly),
            "full" => Some(Level::Full),
            "super" => Some(Level::Super),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Level::Quick => "quick",
            Level::PortableOnly => "portable-only",
            Level::Full => "full",
            Level::Super => "super",
        }
    }
}

/// A focused mode runs exactly one matrix/lane and exits. At most one may be
/// active, and none may combine with an explicit level — the same two-way
/// exclusion `validate.sh` enforces (validate.sh:360-367).
#[derive(Clone, PartialEq, Eq, Debug)]
enum Focused {
    StrictCompat,
    PortableStrictCompat,
    RrCompat,
    SabreCompat,
    E9patchCompat,
    LiteinstCompat,
    QemuL2,
    PrivilegedOnly,
    RequalifyCell { test: String, mode: String, backend: String },
    Only { lane: String, nodes: String },
    Selective { shallow: bool },
    /// `--envelope-only`, plus `--envelope-compare FILE` which is the same
    /// measurement followed by a monotonicity check (validate.sh:172-176).
    Envelope { baseline: Option<PathBuf> },
}

impl Focused {
    /// The `VALIDATION_PROFILE` string recorded in the ledger, matching
    /// validate.sh:381-392 so history for a profile stays continuous.
    fn profile(&self) -> String {
        match self {
            Focused::StrictCompat => "strict-compat-only".into(),
            Focused::PortableStrictCompat => "portable-strict-compat-only".into(),
            Focused::RrCompat => "rr-compat-only".into(),
            Focused::SabreCompat => "sabre-compat-only".into(),
            Focused::E9patchCompat => "e9patch-compat-only".into(),
            Focused::LiteinstCompat => "liteinst-compat-only".into(),
            Focused::QemuL2 => "qemu-l2-only".into(),
            Focused::PrivilegedOnly => "privileged-only".into(),
            Focused::RequalifyCell { .. } => "cell-requalification".into(),
            Focused::Only { lane, .. } => format!("only-{lane}"),
            Focused::Selective { .. } => "selective".into(),
            // Both spellings record ONE profile, matching validate.sh:382, so
            // envelope history stays continuous whether or not a baseline was
            // supplied.
            Focused::Envelope { .. } => "envelope-only".into(),
        }
    }
    /// `--all/--full-run` refuses to combine with any focused mode; this is the
    /// name used in that refusal message.
    fn cli_name(&self) -> &'static str {
        match self {
            Focused::StrictCompat => "strict-compat-only",
            Focused::PortableStrictCompat => "portable-strict-compat-only",
            Focused::RrCompat => "rr-compat-only",
            Focused::SabreCompat => "sabre-compat-only",
            Focused::E9patchCompat => "e9patch-compat-only",
            Focused::LiteinstCompat => "liteinst-compat-only",
            Focused::QemuL2 => "qemu-l2-only",
            Focused::PrivilegedOnly => "privileged-only",
            Focused::RequalifyCell { .. } => "requalify-cell",
            Focused::Only { .. } => "only",
            Focused::Selective { shallow } => {
                if *shallow {
                    "shallow-select"
                } else {
                    "selective"
                }
            }
            Focused::Envelope { baseline } => {
                if baseline.is_some() {
                    "envelope-compare"
                } else {
                    "envelope-only"
                }
            }
        }
    }
}

struct Args {
    level: Level,
    level_explicit: bool,
    focused: Option<Focused>,
    force_full: bool,
    baseline: Option<String>,
    skip_inner_dirty_working_tree_and_rebase_freshness_checks: bool,
    ignore_cache: bool,
    label_pr: bool,
    verbosity: i64,
    jobs: Option<i64>,
    keep_going: bool,
    allow_cgroup_failure: bool,
    /// Wall budget for the whole validate invocation, across lanes and retries.
    run_timeout: Option<i64>,
    merge_lanes: bool,
    reuse_parent_manifest_gate: bool,
    self_test: bool,
    show_plan: bool,
}

const SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION: &str =
    "--skip-inner-dirty-working-tree-and-rebase-freshness-checks";
const SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_ENV: &str =
    "VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS";

fn usage() -> &'static str {
    "Usage: ./scripts/validate.rs [LEVEL] [OPTIONS]\n\
     \n\
     Run Hermit's local validation suite. Every gate executes as a boxed\n\
     dagrun DAG node; nothing runs outside the runner.\n\
     \n\
     Levels:\n\
     \x20 quick            Core ptrace run/verify/record smoke tests; no alternate backends.\n\
     \x20 portable-only    Portable build, test, lint, format, and doc gates matching\n\
     \x20                  GitHub-managed portable CI; no PMU or namespace requirements.\n\
     \x20 full             quick plus the complete suite and DBI/KVM gates (default).\n\
     \x20 super            Repeat stress probes under moderate oversubscription.\n\
     \x20 --quick          Alias for the quick level.\n\
     \x20 --portable       Alias for the portable-only level.\n\
     \n\
     Focused gates (run one matrix/lane and exit):\n\
     \x20 --strict-compat-only          Run the blocking legacy stripped app matrix.\n\
     \x20 --portable-strict-compat-only Portable legacy stripped matrix with bounded diagnostics.\n\
     \x20 --rr-compat-only              Gate the known-passing record/replay matrix.\n\
     \x20 --sabre-compat-only           Gate the measured SaBRe matrix.\n\
     \x20 --e9patch-compat-only         Gate core + installed e9patch legacy stripped apps.\n\
     \x20 --liteinst-compat-only        Run the portable CI liteinst_strict test.\n\
     \x20 --qemu-l2-only                Run the heavyweight QEMU L2 boot.\n\
     \x20 --portable-only               No PMU/CPUID hardware required.\n\
     \x20 --privileged-only             PMU/CPUID-dependent tests only.\n\
     \x20 --requalify-cell TEST MODE BACKEND  Run one selected cell for schema-7 requalification.\n\
     \x20 --only <lane> <group.job>[,...]  Run those lane node(s) with their own\n\
     \x20                  declared caps; outside deps are dropped; preflight tags\n\
     \x20                  reuse validate's canonical preflight nodes.\n\
     \x20 --selective, --since-green    Only nodes affected since the last green baseline.\n\
     \x20 --shallow-select              Like --selective but pin the baseline to HEAD~1.\n\
     \x20 --baseline <sha>              Known-green baseline commit for --selective.\n\
     \x20 --envelope-only               Measure and emit the working-envelope vector (JSON + human).\n\
     \x20 --envelope-compare FILE       Measure, then fail if any count regressed below FILE.\n\
     \x20 --all, --full-run             Assert the COMPLETE suite explicitly.\n\
     \n\
     Other options:\n\
     \x20 --verbose        Verbosity level 2: stream tagged per-step output.\n\
     \x20 --verbosity N    Output level 1..5 (default 1; levels 3/4 currently equal 2;\n\
     \x20                  level 5 prefixes every streamed line with test identity).\n\
     \x20 --skip-inner-dirty-working-tree-and-rebase-freshness-checks\n\
     \x20                  Skip only scripts/validate.rs's dirty-working-tree and\n\
     \x20                  rebase-freshness checks; does not bypass ci-hub validate-lock\n\
     \x20                  admission. AGENTS SHOULD NOT USE THIS.\n\
     \x20 --label-pr       Publish a receipt and label the PR after a full green (default).\n\
     \x20 --no-label-pr    Disable the non-fatal receipt publication and label update.\n\
     \x20 --ignore-cache   Force a real run even on a tree-keyed cache hit.\n\
     \x20 -j N             Scheduler width (default: host_cpus/8, floor 2, cap 16).\n\
     \x20 --run-timeout SEC  Wall budget for the WHOLE invocation (across lanes and\n\
     \x20                  retries). On breach, in-flight nodes are cut and the run still\n\
     \x20                  reports instead of being killed externally. Also sets a later\n\
     \x20                  systemd-scope backstop. Env: HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS.\n\
     \x20 -k, --keep-going Do not eager-exit on the first failure.\n\
     \x20 --allow-cgroup-failure  Downgrade to an UNBOXED run instead of failing closed.\n\
     \x20 --merge-lanes    Fuse the portable and privileged lanes (the full default).\n\
     \x20 --sequential-lanes  Diagnostic fallback: run full lanes back to back.\n\
     \x20 --show-plan      Print the boxed DAG plan (nodes, caps, deps) and exit.\n\
     \x20 --self-test      Run inert policy/data brackets plus one bounded disposable\n\
     \x20                  nested-cgroup check, then exit.\n\
     \x20 -h, --help       Show this help and exit.\n\
     \n\
     Actual validation attempts end with exactly one machine-readable final line:\n\
     \x20 FINAL_VALIDATE_STATUS: PASSED          exit 0\n\
     \x20 FINAL_VALIDATE_STATUS: FAILED          exit 1\n\
     \x20 FINAL_VALIDATE_STATUS: COULD_NOT_RUN   exit 75\n\
     The line is validate's last output. Readers take the last occurrence and\n\
     require the exit code to agree; no line means validate died before reporting.\n\
     Help, --show-plan, and --probe-host-capability do not attempt validation and\n\
     therefore do not emit a final validate status.\n\
     \n\
     Environment: VALIDATE_LEVEL, VALIDATE_LABEL_PR,\n\
     VALIDATE_SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS,\n\
     VALIDATE_IGNORE_CACHE, VALIDATE_VERBOSITY, VALIDATE_VERBOSE, VALIDATE_FORCE_FULL,\n\
     HERMIT_VALIDATE_LEDGER, PR_NUMBER, SUPER_REPETITIONS, L4_REPS, ENVELOPE_JSON,\n\
     HERMIT_LAST_GREEN_SHA, CI_HUB_APPLY_LOCAL_LABEL, DEV_HERMIT_PARENT.\n\
     \n\
     HERMIT_VALIDATE_HOST_CAPABILITY_PRESENT=<name>[,<name>] asserts that this\n\
     machine HAS a declared host capability, so its nodes run without probing.\n\
     It is deliberately one-directional: it can only cause MORE nodes to run.\n\
     Nothing can force a capability ABSENT, because that would be a way to make\n\
     a node stop running without anyone measuring the machine.\n\
     \n\
     --probe-host-capability <name> reports this machine's verdict for one\n\
     capability as PRESENT|ABSENT plus the observation behind it, and exits.\n\
     It runs no gate. target/debug/test-harness calls it so a withheld manifest CELL\n\
     and a withheld DAG node are decided by the same probe."
}

fn env_flag(name: &str, want: &str) -> bool {
    std::env::var(name).map(|v| v == want).unwrap_or(false)
}

fn parse_verbosity(value: &str) -> Result<i64, u8> {
    match value.parse::<i64>() {
        Ok(v @ 1..=5) => Ok(v),
        _ => {
            eprintln!("validate: verbosity must be an integer from 1 through 5, got {value:?}");
            Err(2)
        }
    }
}

fn env_verbosity() -> Result<i64, u8> {
    match std::env::var("VALIDATE_VERBOSITY") {
        Ok(v) if !v.is_empty() => parse_verbosity(&v),
        _ => Ok(if env_flag("VALIDATE_VERBOSE", "1") { 2 } else { 1 }),
    }
}

fn parse_args() -> Result<Args, u8> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    parse_argv(&argv)
}

/// Argument parsing over an EXPLICIT argv.
///
/// Split out from [`parse_args`] so `--self-test` can exercise the real parser
/// on synthetic command lines without spawning a subprocess — a subprocess would
/// re-enter `main`, hit the dirty-tree and rebase-freshness gates, and turn a CLI
/// bracket into a test of the checkout's state instead of the flag surface.
fn parse_argv(argv: &[String]) -> Result<Args, u8> {
    let mut level = Level::Full;
    let mut level_explicit = false;
    if let Ok(v) = std::env::var("VALIDATE_LEVEL") {
        if !v.is_empty() {
            match Level::parse(&v) {
                Some(l) => {
                    level = l;
                    level_explicit = true;
                }
                None => {
                    eprintln!("validate: invalid VALIDATE_LEVEL: {v}");
                    return Err(2);
                }
            }
        }
    }
    let mut focused: Vec<Focused> = Vec::new();
    let verbosity = env_verbosity()?;
    let mut args = Args {
        level,
        level_explicit,
        focused: None,
        force_full: env_flag("VALIDATE_FORCE_FULL", "1"),
        baseline: None,
        skip_inner_dirty_working_tree_and_rebase_freshness_checks: env_flag(
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_ENV,
            "1",
        ),
        ignore_cache: env_flag("VALIDATE_IGNORE_CACHE", "1"),
        label_pr: !env_flag("VALIDATE_LABEL_PR", "0"),
        verbosity,
        jobs: None,
        keep_going: false,
        allow_cgroup_failure: false,
        run_timeout: None,
        merge_lanes: true,
        reuse_parent_manifest_gate: false,
        self_test: false,
        show_plan: false,
    };
    let mut shallow = false;
    let mut selective = false;
    let mut show_plan = false;
    let mut envelope = false;
    let mut envelope_baseline: Option<PathBuf> = None;

    let mut i = 0;
    let set_level = |args: &mut Args, l: Level| -> Result<(), u8> {
        if args.level_explicit {
            eprintln!("validate: choose only one validation level");
            return Err(2);
        }
        args.level = l;
        args.level_explicit = true;
        Ok(())
    };
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "quick" | "portable-only" | "full" | "super" => {
                set_level(&mut args, Level::parse(a).unwrap())?
            }
            "--quick" => set_level(&mut args, Level::Quick)?,
            "--portable" | "--portable-only" => set_level(&mut args, Level::PortableOnly)?,
            "--strict-compat-only" => focused.push(Focused::StrictCompat),
            "--portable-strict-compat-only" => focused.push(Focused::PortableStrictCompat),
            "--rr-compat-only" => focused.push(Focused::RrCompat),
            "--sabre-compat-only" => focused.push(Focused::SabreCompat),
            "--e9patch-compat-only" => focused.push(Focused::E9patchCompat),
            "--liteinst-compat-only" => focused.push(Focused::LiteinstCompat),
            "--qemu-l2-only" => focused.push(Focused::QemuL2),
            "--privileged-only" => focused.push(Focused::PrivilegedOnly),
            "--requalify-cell" => {
                let test = argv.get(i + 1).cloned().unwrap_or_default();
                let mode = argv.get(i + 2).cloned().unwrap_or_default();
                let backend = argv.get(i + 3).cloned().unwrap_or_default();
                if test.is_empty() || mode.is_empty() || backend.is_empty() {
                    eprintln!("validate: --requalify-cell needs TEST MODE BACKEND");
                    return Err(2);
                }
                focused.push(Focused::RequalifyCell { test, mode, backend });
                i += 3;
            }
            // `--envelope-only` and `--envelope-compare` are ONE mode in
            // validate.sh (both set ENVELOPE_MODE=only; the second merely adds a
            // baseline, validate.sh:172-176), so they accumulate into a single
            // Focused entry rather than colliding as two focused modes.
            "--envelope-only" => envelope = true,
            "--envelope-compare" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => {
                        envelope = true;
                        envelope_baseline = Some(PathBuf::from(v));
                    }
                    _ => {
                        eprintln!("validate: --envelope-compare needs a FILE");
                        return Err(2);
                    }
                }
            }
            "--show-plan" => show_plan = true,
            "--selective" | "--since-green" => selective = true,
            "--shallow-select" => {
                selective = true;
                shallow = true;
            }
            "--all" | "--full-run" => args.force_full = true,
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION => {
                args.skip_inner_dirty_working_tree_and_rebase_freshness_checks = true
            }
            "--ignore-cache" => args.ignore_cache = true,
            "--label-pr" => args.label_pr = true,
            "--no-label-pr" => args.label_pr = false,
            "--verbose" => args.verbosity = 2,
            "--verbosity" => {
                i += 1;
                args.verbosity = match argv.get(i) {
                    Some(v) => parse_verbosity(v)?,
                    None => {
                        eprintln!("validate: --verbosity needs a level from 1 through 5");
                        return Err(2);
                    }
                };
            }
            "--merge-lanes" => args.merge_lanes = true,
            "--sequential-lanes" => args.merge_lanes = false,
            // Internal nested-payload optimization. The outer full DAG has
            // already run the exact same manifest command and structurally
            // gates this node on it. The nested payload still reruns submodule
            // and Reverie-pin checks, so `reverie_pin_current` remains observed.
            "--reuse-parent-manifest-gate" => args.reuse_parent_manifest_gate = true,
            "--self-test" => args.self_test = true,
            "-k" | "--keep-going" => args.keep_going = true,
            "--allow-cgroup-failure" => args.allow_cgroup_failure = true,
            "--run-timeout" => {
                i += 1;
                match argv.get(i).and_then(|v| v.parse::<i64>().ok()) {
                    Some(v) if v > 0 => args.run_timeout = Some(v),
                    _ => {
                        eprintln!("validate: --run-timeout needs a positive number of SECONDS");
                        return Err(2);
                    }
                }
            }
            "--baseline" => {
                i += 1;
                match argv.get(i) {
                    Some(v) if !v.is_empty() => args.baseline = Some(v.clone()),
                    _ => {
                        eprintln!("validate: --baseline needs a SHA");
                        return Err(2);
                    }
                }
            }
            "-j" => {
                i += 1;
                match argv.get(i).and_then(|v| v.parse::<i64>().ok()) {
                    Some(n) if n > 0 => args.jobs = Some(n),
                    _ => {
                        eprintln!("validate: -j needs a positive integer");
                        return Err(2);
                    }
                }
            }
            "--only" => {
                let lane = argv.get(i + 1).cloned().unwrap_or_default();
                let nodes = argv.get(i + 2).cloned().unwrap_or_default();
                if lane.is_empty() || nodes.is_empty() {
                    eprintln!("validate: --only needs <lane> <group.job>[,<group.job>...]");
                    eprintln!("          e.g. ./scripts/validate.rs --only portable test.sabre_examples");
                    return Err(2);
                }
                focused.push(Focused::Only { lane, nodes });
                i += 2;
            }
            "-h" | "--help" => {
                println!("{}", usage());
                return Err(0);
            }
            other => {
                eprintln!("validate: unknown argument: {other} (try --help)");
                return Err(2);
            }
        }
        i += 1;
    }
    if selective {
        focused.push(Focused::Selective { shallow });
    }
    if envelope {
        focused.push(Focused::Envelope { baseline: envelope_baseline });
    }
    if focused.len() > 1 {
        eprintln!("validate: choose only one focused validation mode");
        return Err(2);
    }
    if args.level_explicit && !focused.is_empty() {
        eprintln!("validate: validation levels cannot be combined with focused validation modes");
        return Err(2);
    }
    args.show_plan = show_plan;
    args.focused = focused.pop();
    if args.reuse_parent_manifest_gate
        && (!matches!(args.focused, Some(Focused::PortableStrictCompat)) || args.label_pr)
    {
        eprintln!(
            "validate: --reuse-parent-manifest-gate is internal to the no-label \
             portable-strict payload of the full DAG"
        );
        return Err(2);
    }
    // `--privileged-only` and `--portable-only` are spelled as focused flags but
    // one of them is a LEVEL in validate.sh. Preserve that: --portable-only sets
    // the level, --privileged-only stays focused (validate.sh:169,189).
    if !force_full_policy_allows(
        args.force_full,
        args.level,
        args.focused.as_ref().map(|f| f.cli_name()),
    ) {
        eprintln!(
            "validate: --all/--full-run requires level full and forbids every focused or selective mode"
        );
        return Err(2);
    }
    if shallow && args.baseline.is_some() {
        eprintln!("validate: --shallow-select forces a HEAD~1 baseline; do not also pass --baseline");
        return Err(2);
    }
    Ok(args)
}

/// `force_full_policy_allows` (validate.sh:299): `--all` asserts the COMPLETE
/// suite, so it accepts only the unfocused `full` level.
fn force_full_policy_allows(force_full: bool, level: Level, focused: Option<&str>) -> bool {
    !force_full || (level == Level::Full && focused.is_none())
}

/// The environment marker only routes an invocation that already parsed the
/// explicit `--self-test` flag. An inherited or operator-supplied marker must
/// never turn an ordinary validation into a small passing probe.
fn nested_scope_probe_selected(self_test: bool, marker_present: bool) -> bool {
    self_test && marker_present
}

fn nested_scope_probe_requested() -> bool {
    std::env::var_os(NESTED_SCOPE_SELF_TEST_ENV).is_some()
}

/// Every local rung is strict, and the three sequential outer runs sum to
/// 12 + 7 + 4 = 23s, below the disposable scope's 30s and wrapper's 45s.
fn nested_scope_budgets_are_ordered() -> bool {
    NESTED_INNER_STEP_S < NESTED_INNER_RUN_S
        && NESTED_INNER_RUN_S < NESTED_OUTER_CHILD_STEP_S
        && NESTED_OUTER_CHILD_STEP_S < NESTED_OUTER_CHILD_RUN_S
        && NESTED_SIGNAL_STEP_S < NESTED_SIGNAL_RUN_S
        && NESTED_SURVIVOR_STEP_S < NESTED_SURVIVOR_RUN_S
        && NESTED_OUTER_CHILD_RUN_S + NESTED_SIGNAL_RUN_S + NESTED_SURVIVOR_RUN_S
            < NESTED_SCOPE_RUNTIME_S
        && NESTED_SCOPE_RUNTIME_S < NESTED_WRAPPER_TIMEOUT_S
}

fn nested_scope_probe_step(
    job: &str,
    cmd: String,
    mode: Option<&str>,
    timeout_s: i64,
) -> dagrun::model::Step {
    let mut step = step_with_caps(
        "safe_ci_scope_self_test", job, "Exercise nested per-step cgroup containment",
        cmd, Vec::new(), timeout_s, timeout_s, 512 * 1024 * 1024,
    );
    if let Some(mode) = mode {
        step.env.insert(NESTED_SCOPE_SELF_TEST_ENV.into(), mode.into());
    }
    step.env.insert("DAGRUN_NO_STEP_LOGS".into(), "1".into());
    step.hint.preferred_inner_jobs = Some(1);
    step.jobs_flag = Some(String::new());
    step
}

/// Total CPU cores the scheduler may hand out across concurrently running steps.
///
/// This is a DIFFERENT quantity from `-j`, which bounds how many steps run at
/// once, and the runner now takes both. Passing `None` defaults the CPU budget to
/// the active-step width, and the runner then refuses before any node starts if a
/// step declares a wider `preferred_inner_jobs` than the budget AND manages its own
/// concurrency (an empty `jobs_flag`) — because clamping such a step's cgroup quota
/// alone would leave its original worker count running inside a smaller box, which
/// is a slowdown disguised as a limit. Four nodes are in exactly that position:
/// `build.workspace` and `build.runtime_release` at 32, and
/// `e2e.manifest_backend_parity_c` and `e2e.manifest_c_programs` at 8. All four
/// bake their measured width into the command itself, so `-j` (host_cpus/8, floor
/// 2, cap 16) would refuse the entire run.
///
/// The value is the one the runner's own CLI defaults to: the ambient
/// container/affinity budget, tightened by the shared aggregate slice's quota. On a
/// host too small to satisfy a declared width the run still refuses, which is the
/// intended fail-closed behavior — the fix there is the step's declaration, not a
/// wider number here.
fn scheduler_cpu_budget() -> i64 {
    container_core_budget().min(aggregate_slice_max_cpus()).max(1)
}

fn run_one_nested_scope_probe_step(
    cgroups: BoxedCgroups,
    step: dagrun::model::Step,
    run_timeout_s: i64,
) -> Result<(), String> {
    let mut cfg = DagConfig {
        description: "real nested safe-ci scope self-test".into(),
        ..Default::default()
    };
    cfg.steps.push(step);
    let result = run_dag_boxed_deadline(
        &cfg, 1, true, 2, cgroups, None, Some(1), Some(run_timeout_s),
    );
    if !result.ok || result.run_timed_out || !result.skipped.is_empty()
        || result.outcomes.len() != 1 || !result.outcomes[0].ok
    {
        return Err(format!(
            "nested cgroup step did not pass exactly once: ok={} timed_out={} outcomes={:?} skipped={:?}",
            result.ok, result.run_timed_out, result.outcomes, result.skipped
        ));
    }
    Ok(())
}

fn run_one_nested_scope_signal_step(
    cgroups: BoxedCgroups,
    step: dagrun::model::Step,
    run_timeout_s: i64,
) -> Result<(), String> {
    let mut cfg = DagConfig {
        description: "real inherited-scope signal self-test".into(),
        ..Default::default()
    };
    cfg.steps.push(step);
    let result = run_dag_boxed_deadline(
        &cfg, 1, true, 2, cgroups, None, Some(1), Some(run_timeout_s),
    );
    if result.ok || result.run_timed_out || !result.skipped.is_empty()
        || result.outcomes.len() != 1
        || result.outcomes[0].ok
        || result.outcomes[0].returncode != Some(-libc::SIGTERM as i64)
    {
        return Err(format!(
            "nested signal step did not fail only by SIGTERM: ok={} timed_out={} \
             outcomes={:?} skipped={:?}",
            result.ok, result.run_timed_out, result.outcomes, result.skipped
        ));
    }
    Ok(())
}

/// Exercise outer-scope verification from a real scheduler `step-*` child,
/// then require that child to dispatch one further per-step cgroup.
fn run_nested_scope_probe() -> Result<String, String> {
    match std::env::var(NESTED_SCOPE_SELF_TEST_ENV).as_deref() {
        Ok(NESTED_SCOPE_OUTER) => {
            let cgroups = safe_ci_scope::resolve_cgroups(
                "safe-ci nested self-test outer", false, Some(NESTED_SCOPE_RUNTIME_S), true,
            ).map_err(|code| format!("outer cgroup setup refused with exit {code}"))?;
            let exe = std::env::current_exe()
                .map_err(|error| format!("cannot resolve self-test executable: {error}"))?;
            let exe = exe.to_str()
                .ok_or_else(|| "self-test executable path is not UTF-8".to_string())?;
            let command = format!("{} --self-test", validate_plan::shell_quote(exe));
            run_one_nested_scope_probe_step(
                cgroups.clone(),
                nested_scope_probe_step(
                    "outer_child", command.clone(), Some(NESTED_SCOPE_INNER),
                    NESTED_OUTER_CHILD_STEP_S,
                ),
                NESTED_OUTER_CHILD_RUN_S,
            )?;
            run_one_nested_scope_signal_step(
                cgroups.clone(),
                nested_scope_probe_step(
                    "signal_child", command, Some(NESTED_SCOPE_SIGNAL), NESTED_SIGNAL_STEP_S,
                ),
                NESTED_SIGNAL_RUN_S,
            )?;
            run_one_nested_scope_probe_step(
                cgroups,
                nested_scope_probe_step(
                    "surviving_sibling", "true".into(), None, NESTED_SURVIVOR_STEP_S,
                ),
                NESTED_SURVIVOR_RUN_S,
            )?;
            Ok(
                "outer scope observed the nested SIGTERM failure and then ran a boxed sibling"
                    .into(),
            )
        }
        Ok(NESTED_SCOPE_INNER) => {
            // This process inherited the outer scope's RuntimeMax; it did not
            // request a second systemd unit. Every other limit stays mandatory.
            let cgroups = safe_ci_scope::resolve_cgroups(
                "safe-ci nested self-test inner", false, None, false,
            ).map_err(|code| format!("nested cgroup setup refused with exit {code}"))?;
            run_one_nested_scope_probe_step(
                cgroups,
                nested_scope_probe_step(
                    "inner_child", "true".into(), None, NESTED_INNER_STEP_S,
                ),
                NESTED_INNER_RUN_S,
            )?;
            Ok("nested child verified the outer scope and dispatched its own boxed step".into())
        }
        Ok(NESTED_SCOPE_SIGNAL) => {
            // Remove validate's ordinary signal observer BEFORE resolve_cgroups.
            // The fixed inherited path installs no replacement, so SIGTERM
            // terminates only this child. The buggy path installs the outer-
            // scope teardown handler, which instead kills the disposable scope
            // and makes the bounded parent wrapper fail.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
            }
            let _cgroups = safe_ci_scope::resolve_cgroups(
                "safe-ci nested signal self-test", false, None, false,
            ).map_err(|code| format!("nested signal cgroup setup refused with exit {code}"))?;
            eprintln!("nested signal child resolved inherited cgroups; delivering SIGTERM");
            let raised = unsafe { libc::raise(libc::SIGTERM) };
            Err(format!("nested signal child survived SIGTERM (libc::raise rc={raised})"))
        }
        Ok(other) => Err(format!("unknown nested scope self-test mode {other:?}")),
        Err(error) => Err(format!("nested scope self-test mode is unavailable: {error}")),
    }
}

/// Launch the real nested topology in a bounded child. Clearing inherited
/// scope sentinels forces that child to establish and observe a fresh scope.
fn nested_scope_self_test() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot resolve self-test executable: {error}"))?;
    let output = Command::new("timeout")
        .arg("--kill-after=5s")
        .arg(format!("{NESTED_WRAPPER_TIMEOUT_S}s"))
        .arg(exe).arg("--self-test")
        .env(NESTED_SCOPE_SELF_TEST_ENV, NESTED_SCOPE_OUTER)
        .env("DAGRUN_FORCE_SCOPE_ATTEMPT", "1")
        .env("DAGRUN_NO_STEP_LOGS", "1")
        .env_remove("DAGRUN_IN_SCOPE")
        .env_remove("DAGRUN_SCOPE_UNIT")
        .env_remove("DAGRUN_EXPECTED_OUTER_MEMORY_MAX_BYTES")
        .env_remove("DAGRUN_EXPECTED_RUNTIME_MAX_SEC")
        .output()
        .map_err(|error| format!("cannot launch bounded nested scope self-test: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "real nested scope self-test failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status, String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for required in [
        "nested child verified the outer scope and dispatched its own boxed step",
        "safe-ci nested self-test inner: cgroup boxing ACTIVE",
        "outer cgroup audit at ",
        "safe_ci_scope_self_test.inner_child] ✓ PASS",
        "safe_ci_scope_self_test.signal_child] ✗ FAIL",
        "safe_ci_scope_self_test.surviving_sibling] ✓ PASS",
        "outer scope observed the nested SIGTERM failure and then ran a boxed sibling",
    ] {
        if !stdout.contains(required) && !stderr.contains(required) {
            return Err(format!(
                "real nested scope self-test exited successfully without required evidence \
                 {required:?}\nstdout:\n{stdout}\nstderr:\n{stderr}"
            ));
        }
    }
    Ok("safe-ci scope: real outer -> step child -> nested boxed step passed".into())
}

/// The literal flag that enables Hermit's strict execution mode.
///
/// Spelled out here, in the CONSUMER, on purpose. Deriving it from
/// `CompatMode::run_args` — the code being checked — would make the check agree
/// with whatever that code happens to emit, which is exactly the defect this
/// constant exists to catch: deleting `--strict` from the rendered plans left
/// `--self-test` exiting 0 and printing success, because nothing compared the
/// rendered command against an independently stated expectation.
///
/// This flag alone does NOT establish canonical L2 evidence. The modes checked
/// below use the legacy lossy `--verify` comparator and remain explicitly
/// below-L2 unless their argv also adopts `--verify-strict` and the surrounding
/// evidence policy is updated.
const STRICT_EXECUTION_FLAG: &str = "--strict";

/// Legacy below-L2 compatibility modes whose Hermit-option prefix must carry
/// [`STRICT_EXECUTION_FLAG`].
///
/// `CompatMode::Rr` is deliberately absent rather than overlooked: it renders
/// `record start --verify --verify-strict`, a different path with a different
/// evidence policy and no `--strict` marker, so folding it into this list would
/// assert something untrue about it.
const LEGACY_BELOW_L2_STRICT_MODES: [CompatMode; 4] = [
    CompatMode::Strict,
    CompatMode::PortableStrict,
    CompatMode::Sabre,
    CompatMode::E9patch,
];

/// Whether a rendered plan is missing the literal strict flag from Hermit's
/// option prefix.
///
/// The first `--` ends Hermit's options and begins the guest argv. A guest is
/// free to receive an argument spelled `--strict`; accepting that occurrence as
/// a Hermit option would make the policy check vacuous for exactly the malformed
/// command it is meant to refuse. A missing separator is malformed too.
fn strict_flag_missing_from(argv: &[String]) -> bool {
    let Some(guest_separator) = argv.iter().position(|arg| arg == "--") else {
        return true;
    };
    !argv[..guest_separator]
        .iter()
        .any(|arg| arg == STRICT_EXECUTION_FLAG)
}

/// Inert brackets for the policy predicate and the shell quoter.
///
/// These cannot launch a run or authorize a receipt — they only prove the
/// predicate refuses every non-qualifying case AND accepts the one qualifying
/// case, so it is not vacuously true. `validate.sh` ran the equivalent brackets
/// on every invocation (validate.sh:308); here they are a `--self-test` subcommand
/// so the cost is not paid on the hot path.
fn self_test() -> Result<(), String> {
    inner_freshness_skip_cli_bracket()?;

    // ---- known-fail-closed disposition, as a pure decision table ----
    //
    // The property under test is NOT "the listed rows get mentioned". It is that mentioning
    // them did not quietly excuse them. Each row below pins BOTH the message class and the
    // blocking verdict, so a future edit cannot improve the wording into an exemption.
    {
        use validate_plan::CompatDisposition as D;
        use validate_plan::classify_compat_outcome as classify;
        // (mode, ok, listed_failclosed, listed_diagnostic, expected, expected_blocking)
        let cases: &[(CompatMode, bool, bool, bool, D, bool)] = &[
            // PortableStrict: the whole point of this change. A listed failure is REPORTED and
            // STILL BLOCKS; a listed pass is reported as a stale expectation.
            (CompatMode::PortableStrict, false, true, false, D::KnownFailClosedBlocking, true),
            (CompatMode::PortableStrict, true, true, false, D::PassedButListedFailClosed, false),
            // ...and an UNLISTED failure is unaffected.
            (CompatMode::PortableStrict, false, false, false, D::Blocking, true),
            // Bounded portable diagnostics keep their existing nonblocking treatment.
            (CompatMode::PortableStrict, false, false, true, D::PortableDiagnostic, false),
            // Strict keeps its historical exemption, and only Strict has it.
            (CompatMode::Strict, false, true, false, D::KnownFailClosedExempt, false),
            (CompatMode::Strict, true, true, false, D::PassedButListedFailClosed, false),
            (CompatMode::Strict, false, false, false, D::Blocking, true),
            // No other mode consults either table: a failure blocks whatever the tables say.
            (CompatMode::Sabre, false, true, false, D::Blocking, true),
            (CompatMode::Sabre, false, false, true, D::Blocking, true),
            (CompatMode::Sabre, true, true, false, D::Passed, false),
            (CompatMode::E9patch, false, true, false, D::Blocking, true),
            (CompatMode::Rr, false, true, false, D::Blocking, true),
        ];
        for (mode, ok, listed, diag, want, want_blocking) in cases.iter().copied() {
            let got = classify(mode, ok, listed, diag);
            if got != want {
                return Err(format!(
                    "compat disposition for mode={mode:?} ok={ok} listed_failclosed={listed} \
                     listed_diagnostic={diag}: expected {want:?}, got {got:?}"
                ));
            }
            if got.is_blocking() != want_blocking {
                return Err(format!(
                    "compat disposition {got:?} (mode={mode:?} ok={ok} listed={listed}) must \
                     {} the run, but is_blocking() said {}",
                    if want_blocking { "BLOCK" } else { "not block" },
                    got.is_blocking()
                ));
            }
        }
    }

    // ---- the REAL summary consumer, against a PLANTED table ----
    //
    // Bound to the shipped `compat_summary_with_tables`, not a copy of its logic, so the two
    // cannot drift. The table is planted rather than real because the shipped
    // `known_failclosed()` holds ONE row today, which cannot express "one listed row blocks
    // while another listed row is exempt" in a single run -- and the answer to that is a
    // planted table in the bracket, never an invented row in production.
    {
        let planted_known: BTreeMap<&'static str, &'static str> = BTreeMap::from([
            ("listed_fails", "planted: refused by fail-closed --strict"),
            ("listed_passes", "planted: expected to be refused"),
        ]);
        let planted_diag: BTreeMap<&'static str, &'static str> =
            BTreeMap::from([("bounded_diag", "planted: bounded portable diagnostic")]);
        let row = |label: &str, ok: bool| StepOutcome {
            tag: format!("compat.{label}"),
            ok,
            duration_s: 0.0,
            summary: String::new(),
            executed_tests: None,
            filtered_tests: None,
            returncode: Some(if ok { 0 } else { 1 }),
            reason: String::new(),
            aborted: false,
        };
        let outcomes = vec![
            row("listed_fails", false),
            row("listed_passes", true),
            row("bounded_diag", false),
            row("unlisted_fails", false),
            row("plain_passes", true),
        ];
        let (_, _, blocking) = compat_summary_with_tables(
            CompatMode::PortableStrict,
            &outcomes,
            &planted_known,
            &planted_diag,
        );
        // THE LOAD-BEARING ASSERTION: a listed failure is still in the blocking set. If a future
        // change makes PortableStrict exempt listed rows the way Strict does, this fails.
        if !blocking.iter().any(|l| l == "listed_fails") {
            return Err(format!(
                "PortableStrict dropped a listed known-fail-closed row from the blocking set, \
                 which is the exemption this change exists to avoid: blocking={blocking:?}"
            ));
        }
        if !blocking.iter().any(|l| l == "unlisted_fails") {
            return Err(format!(
                "PortableStrict dropped an UNLISTED failure from the blocking set: \
                 blocking={blocking:?}"
            ));
        }
        if blocking.iter().any(|l| l == "bounded_diag") {
            return Err(format!(
                "a bounded portable diagnostic became blocking, changing prior policy: \
                 blocking={blocking:?}"
            ));
        }
        if blocking.iter().any(|l| l == "listed_passes" || l == "plain_passes") {
            return Err(format!("a PASSING row was reported as blocking: blocking={blocking:?}"));
        }
        // And the same planted table under Strict must exempt the listed failure, so the
        // bracket also pins that the two modes still differ.
        let (_, _, strict_blocking) = compat_summary_with_tables(
            CompatMode::Strict,
            &outcomes,
            &planted_known,
            &planted_diag,
        );
        if strict_blocking.iter().any(|l| l == "listed_fails") {
            return Err(format!(
                "Strict lost its historical exemption for a listed row: {strict_blocking:?}"
            ));
        }
    }


    // Strict-execution bracket for the legacy below-L2 compatibility modes.
    // `--strict` must be a Hermit option before the first guest `--`; these
    // modes still use lossy `--verify`, so this does not call them L2. The
    // bracket accepts the real prefix, rejects deletion, and rejects the subtle
    // goalpost move of putting the same spelling in guest argv.
    for mode in LEGACY_BELOW_L2_STRICT_MODES {
        let rendered = mode.run_args("whoami", "/tmp/nsswitch.conf");
        let guest_separator = rendered
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| format!("{mode:?} rendered no guest argv separator: {rendered:?}"))?;
        if strict_flag_missing_from(&rendered) {
            return Err(format!(
                "{mode:?} rendered a compatibility plan WITHOUT the literal \
                 {STRICT_EXECUTION_FLAG} in Hermit's option prefix, so the \
                 legacy below-L2 run would not use strict execution: {rendered:?}"
            ));
        }
        if rendered[..guest_separator]
            .iter()
            .any(|arg| arg == "--verify-strict")
            || !mode.display_name().contains("below-L2")
        {
            return Err(format!(
                "{mode:?} is governed by the legacy below-L2 policy, so its Hermit prefix must \
                 omit --verify-strict and its rendered description must say below-L2: \
                 argv={rendered:?}, description={:?}",
                mode.display_name()
            ));
        }
        let stripped: Vec<String> = rendered
            .iter()
            .filter(|arg| *arg != STRICT_EXECUTION_FLAG)
            .cloned()
            .collect();
        if stripped.len() == rendered.len() {
            return Err(format!(
                "{mode:?}: removing {STRICT_EXECUTION_FLAG} changed nothing, so the \
                 refusing direction below would be vacuous"
            ));
        }
        if !strict_flag_missing_from(&stripped) {
            return Err(format!(
                "the strict-flag check did not notice {STRICT_EXECUTION_FLAG} missing \
                 from a {mode:?} plan, so it cannot detect its deletion"
            ));
        }
        let stripped_separator = stripped
            .iter()
            .position(|arg| arg == "--")
            .ok_or_else(|| format!("{mode:?} rendered no guest argv separator: {rendered:?}"))?;
        let mut misplaced = stripped.clone();
        misplaced.insert(stripped_separator + 1, STRICT_EXECUTION_FLAG.into());
        if !strict_flag_missing_from(&misplaced) {
            return Err(format!(
                "the strict-flag check accepted {STRICT_EXECUTION_FLAG} after the first guest \
                 separator in a {mode:?} plan, so guest argv could forge the Hermit option: \
                 {misplaced:?}"
            ));
        }
        let no_separator: Vec<String> = rendered
            .iter()
            .filter(|arg| arg.as_str() != "--")
            .cloned()
            .collect();
        if !strict_flag_missing_from(&no_separator) {
            return Err(format!(
                "the strict-flag check accepted a {mode:?} plan with no guest argv separator: \
                 {no_separator:?}"
            ));
        }
    }

    let final_command = "  tail -F -- $'/tmp/holder run.log'".to_string();
    let mut refusal = RunSummary::refused(
        3,
        "self-test",
        "the per-checkout invocation lock",
        vec!["another validate is already running".into()],
    )
    .with_epilogue(vec![
        "watch the holder's live log with:".into(),
        final_command.clone(),
    ]);
    refusal.cpu_wall = Some((1.0, 0.1, 0.1));
    let rendered = run_summary_lines(&refusal, std::time::Instant::now());
    let final_status = format!("{FINAL_VALIDATE_STATUS_PREFIX}COULD_NOT_RUN");
    if rendered.last() != Some(&final_status)
        || rendered.get(rendered.len().saturating_sub(2)) != Some(&final_command)
        || rendered
            .iter()
            .filter(|line| line.starts_with(FINAL_VALIDATE_STATUS_PREFIX))
            .count()
            != 1
    {
        return Err(format!(
            "summary: refusal must end with exactly one final status after the holder command, got {:?}",
            rendered
        ));
    }
    let quoted = format!(
        "{FINAL_VALIDATE_STATUS_PREFIX}PASSED\nforeign output\n{FINAL_VALIDATE_STATUS_PREFIX}FAILED"
    );
    if final_validate_status_from_output(&quoted) != Ok(Some(FinalValidateStatus::Failed))
        || final_validate_status_from_output("ordinary output") != Ok(None)
        || final_validate_status_from_output("FINAL_VALIDATE_STATUS: MAYBE").is_ok()
    {
        return Err(
            "summary: final-status reader did not take the last occurrence, preserve absence, or reject an unknown value"
                .into(),
        );
    }
    for (verdict, word, exit_code) in [
        (Verdict::Pass, "PASSED", 0),
        (Verdict::Fail, "FAILED", 1),
        (Verdict::NoResult, "COULD_NOT_RUN", COULD_NOT_RUN_EXIT_CODE),
    ] {
        let summary = RunSummary::new(verdict, 222, "self-test", Vec::new());
        let lines = run_summary_lines(&summary, std::time::Instant::now());
        let expected = format!("{FINAL_VALIDATE_STATUS_PREFIX}{word}");
        if summary.exit_code != exit_code
            || lines.last().map(String::as_str) != Some(expected.as_str())
        {
            return Err(format!(
                "summary: {word} did not use fixed exit {exit_code} and the matching final line: exit={} lines={lines:?}",
                summary.exit_code
            ));
        }
    }
    let exe = std::env::current_exe()
        .map_err(|error| format!("summary: cannot resolve self-test executable: {error}"))?;
    let output = Command::new(exe)
        .arg("--self-test")
        .env(SUMMARY_EPILOGUE_SELF_TEST_ENV, "1")
        .output()
        .map_err(|error| format!("summary: cannot launch CLI output probe: {error}"))?;
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("summary: CLI output was not UTF-8: {error}"))?;
    if output.status.code() != Some(i32::from(COULD_NOT_RUN_EXIT_CODE))
        || stdout.lines().last() != Some(final_status.as_str())
        || stdout.lines().rev().nth(1) != Some(final_command.as_str())
    {
        return Err(format!(
            "summary: real refused CLI must exit {COULD_NOT_RUN_EXIT_CODE}, preserve the holder command, and end with {final_status:?}; status={} last={:?}",
            output.status,
            stdout.lines().last()
        ));
    }

    if !nested_scope_probe_selected(true, true)
        || nested_scope_probe_selected(false, true)
        || nested_scope_probe_selected(true, false)
        || nested_scope_probe_selected(false, false)
    {
        return Err(
            "nested scope probe dispatch did not require both --self-test and its internal marker"
                .into(),
        );
    }
    if !nested_scope_budgets_are_ordered() {
        return Err(format!(
            "nested scope self-test budgets are inverted: inner={NESTED_INNER_STEP_S}/\
             {NESTED_INNER_RUN_S}s outer-child={NESTED_OUTER_CHILD_STEP_S}/\
             {NESTED_OUTER_CHILD_RUN_S}s signal={NESTED_SIGNAL_STEP_S}/\
             {NESTED_SIGNAL_RUN_S}s survivor={NESTED_SURVIVOR_STEP_S}/\
             {NESTED_SURVIVOR_RUN_S}s scope={NESTED_SCOPE_RUNTIME_S}s \
             wrapper={NESTED_WRAPPER_TIMEOUT_S}s"
        ));
    }
    // CLI bracket: a real positive budget reaches the typed field, while zero,
    // negative, malformed, and missing values are all refused.
    let parsed = parse_argv(&["--run-timeout".into(), "600".into(), "--self-test".into()])
        .map_err(|code| format!("run-timeout parser refused 600s with exit {code}"))?;
    if parsed.run_timeout != Some(600) {
        return Err(format!(
            "run-timeout parser produced {:?}, expected 600s",
            parsed.run_timeout
        ));
    }
    for bad in ["0", "-1", "not-seconds"] {
        if parse_argv(&["--run-timeout".into(), bad.into(), "--self-test".into()]).is_ok() {
            return Err(format!("run-timeout parser accepted invalid value {bad:?}"));
        }
    }
    if parse_argv(&["--run-timeout".into()]).is_ok() {
        return Err("run-timeout parser accepted a missing value".into());
    }
    if scope_grace_s(600) != 60 || 600 + scope_grace_s(600) >= 720 {
        return Err("run-timeout scope backstop no longer satisfies 600 < 660 < 720".into());
    }
    // A node the scheduler NAMED in `not_launched` is accounted for; one it did not
    // name is a mystery. `not_launched` is neutral: dagrun also uses it when the
    // outer run budget expires, not only when fail-fast stops admission.
    {
        let unreported = vec![
            "e2e.manifest_applications".to_string(),
            "test.detcore_misc".to_string(),
        ];
        let named: BTreeSet<String> = ["e2e.manifest_applications".to_string()]
            .into_iter()
            .collect();
        let (skipped, unaccounted) = partition_unreported(&unreported, &named);
        if skipped != vec!["e2e.manifest_applications".to_string()] {
            return Err(format!(
                "a node named in not_launched must read as an accounted-for scheduler not-launched result, \
                 got {skipped:?}"
            ));
        }
        if unaccounted != vec!["test.detcore_misc".to_string()] {
            return Err(format!(
                "a node absent from not_launched must stay UNACCOUNTED FOR, got {unaccounted:?}"
            ));
        }
        if skipped.len() + unaccounted.len() != unreported.len() {
            return Err("partitioning unreported nodes must not drop any of them".into());
        }
        // The pre-fix behaviour: with nothing named, every node is still a mystery.
        let (none_named, all_unaccounted) = partition_unreported(&unreported, &BTreeSet::new());
        if !none_named.is_empty() || all_unaccounted.len() != 2 {
            return Err(
                "an empty not_launched must leave every unreported node unaccounted for".into(),
            );
        }
        // ...UNLESS the lane was refused before launching. A refusal empties all
        // four scheduler collections on purpose, so without this third state a
        // refusal would report every planned node as unaccounted for directly below
        // a refusal that states the reason -- the unaccounted signal failing exactly when loudest.
        if !scheduler_refused_before_launching(193, 0, 0, 0, 0) {
            return Err(
                "a lane that produced no outcome, dependency-skip, fail-fast skip or intentional \
                 skip must be recognised as refused before launching"
                    .into(),
            );
        }
        // A lane that ran and merely lost nodes is NOT a refusal, and must keep
        // reporting them as unaccounted for.
        if scheduler_refused_before_launching(54, 40, 0, 0, 0)
            || scheduler_refused_before_launching(54, 0, 7, 0, 0)
            || scheduler_refused_before_launching(54, 0, 0, 7, 0)
            || scheduler_refused_before_launching(54, 0, 0, 0, 7)
        {
            return Err(
                "a lane that produced outcomes, skips or not-launched entries is not a pre-flight \
                 refusal and must not be excused as one"
                    .into(),
            );
        }
        if scheduler_refused_before_launching(0, 0, 0, 0, 0) {
            return Err("an empty plan is not a refusal".into());
        }

        // Explanations describe the latest attempt, not an accumulated history. A
        // not-launched result followed by a retry refusal must read as refused; a later
        // completed retry must clear both non-run explanations.
        let retry_tag = "e2e.manifest_applications".to_string();
        let planned = vec![retry_tag.clone()];
        let mut latest_not_launched = BTreeSet::new();
        let mut latest_refused = BTreeSet::new();
        update_not_run_explanations(
            &planned,
            0,
            0,
            std::slice::from_ref(&retry_tag),
            0,
            &mut latest_not_launched,
            &mut latest_refused,
        );
        if !latest_not_launched.contains(&retry_tag) || latest_refused.contains(&retry_tag) {
            return Err("a scheduler not-launched result must be recorded for the latest attempt".into());
        }
        update_not_run_explanations(
            &planned,
            0,
            0,
            &[],
            0,
            &mut latest_not_launched,
            &mut latest_refused,
        );
        if latest_not_launched.contains(&retry_tag) || !latest_refused.contains(&retry_tag) {
            return Err(
                "a retry refusal must replace the earlier not-launched explanation".into(),
            );
        }
        update_not_run_explanations(
            &planned,
            1,
            0,
            &[],
            0,
            &mut latest_not_launched,
            &mut latest_refused,
        );
        if latest_not_launched.contains(&retry_tag) || latest_refused.contains(&retry_tag) {
            return Err("a completed retry must clear every non-run explanation".into());
        }
    }
    // The ceiling must stay strictly inside the budget the scheduler enforces at
    // EVERY remainder, not only at the nominal one. 441s is the measured remainder
    // that refused the strict-compat lane on 2026-08-25 while its gate ceiling was
    // a fixed 480s; 8s is the largest wall those nodes actually needed.
    // A 1s remainder admits no ceiling that is both usable and strictly smaller;
    // the epoch is over and the scheduler refusing is then correct.
    for remaining in [441_i64, 480, 600, 30, 2] {
        let ceiling = derived_wall_ceiling(remaining);
        if ceiling >= remaining {
            return Err(format!(
                "derived wall ceiling {ceiling}s does not fit inside a {remaining}s remainder, so \
                 the scheduler would refuse the lane instead of running it"
            ));
        }
        if ceiling < 1 {
            return Err(format!("derived wall ceiling {ceiling}s is not a usable budget"));
        }
    }
    if derived_wall_ceiling(441) >= 480 {
        return Err(
            "a 441s remainder must lower the 480s gate ceiling; leaving it fixed is what left \
             193 compat nodes unrun"
                .into(),
        );
    }
    let cold_compat = build_release_hermit_node("gate.manifest", "/tmp/target/release/hermit");
    if cold_compat.hint.preferred_inner_jobs != Some(8)
        || cold_compat.hint.classification != dagrun::model::StepClass::CpuBound
    {
        return Err("cold strict-compat release build lost its declared eight-job width".into());
    }
    let reused_compat = build_release_hermit_node(
        "gate.manifest",
        "/tmp/target/ci/hermit-strict",
    );
    if reused_compat.hint.preferred_inner_jobs.is_some()
        || reused_compat.hint.classification != dagrun::model::StepClass::Light
        || !reused_compat.cmd.starts_with("test -x ")
    {
        return Err("prebuilt strict-compat path stopped being a lightweight existence check".into());
    }
    if parse_git_depth(" 42\n")? != 42 {
        return Err("git-depth parser changed the measured value".into());
    }
    for bad in ["", "0", "-1", "not-a-depth", "1 2"] {
        if parse_git_depth(bad).is_ok() {
            return Err(format!("git-depth parser accepted invalid measurement {bad:?}"));
        }
    }
    let head = git_sha();
    let depth = measure_git_depth(&head)?;
    if depth == 0 {
        return Err("git-depth measurement accepted an impossible zero".into());
    }
    // The COMMAND-FAILURE branch, which the parser brackets above cannot reach.
    // `parse_git_depth` is only consulted when `git rev-list` exits zero, so a
    // parser that refuses every malformed string still says nothing about what
    // happens when the command itself fails -- and that is the case this field
    // exists for. Measured: `git rev-list --count 000...0` exits 128 with
    // "fatal: bad object", so this drives the `!output.status.success()` arm.
    // Without that arm the empty stdout would fall through to the parser and be
    // refused for the WRONG REASON, reporting a non-integer depth rather than a
    // failed command, so the assertion is on the message and not merely on
    // is_err().
    let absent = "0000000000000000000000000000000000000000";
    match measure_git_depth(absent) {
        Ok(depth) => {
            return Err(format!(
                "git-depth measurement invented {depth} for a commit that does not exist"
            ));
        }
        Err(error) => {
            if !error.contains("git rev-list --count") || !error.contains("failed with") {
                return Err(format!(
                    "git-depth refusal must name the failed command, not blame the parser: {error}"
                ));
            }
        }
    }
    // All three legitimate deadline sources share one pure precedence rule. The standalone boxed
    // re-exec must preserve D1 exactly; a scheduler epoch applies even when validate is top-level;
    // missing, future, and contradictory sources are refused.
    let now_ns = 10_000_000_000u64;
    let started_ns = 5_000_000_000u64;
    let allowance_ns = 600_000_000_000u64;
    let d1 = started_ns + allowance_ns;
    if deadline_from_sources(Some(600), true, false, None, None, now_ns).is_ok() {
        return Err("nested timeout accepted a missing scheduler-owned start epoch".into());
    }
    if deadline_from_sources(
        Some(600),
        true,
        false,
        Some(now_ns + 1),
        None,
        now_ns,
    )
    .is_ok()
    {
        return Err("nested timeout accepted a future scheduler-owned start epoch".into());
    }
    for nested in [false, true] {
        if deadline_from_sources(
            Some(600),
            nested,
            false,
            Some(started_ns),
            None,
            now_ns,
        )? != Some(d1)
        {
            return Err("scheduler epoch did not bind both top-level and nested deadlines".into());
        }
    }
    if deadline_from_sources(
        Some(600),
        true,
        true,
        Some(started_ns),
        Some(d1 - 1),
        now_ns,
    )? != Some(d1)
    {
        return Err("nested payload consumed its parent's scope deadline marker".into());
    }
    if deadline_from_sources(Some(600), false, true, None, Some(d1), now_ns)? != Some(d1) {
        return Err("boxed re-exec reset D1 instead of preserving it".into());
    }
    if deadline_from_sources(
        Some(600),
        false,
        true,
        Some(started_ns),
        Some(d1 + 1),
        now_ns,
    )
    .is_ok()
    {
        return Err("contradictory scheduler and scope deadline sources were accepted".into());
    }
    if deadline_from_sources(Some(600), false, false, None, Some(d1), now_ns)?
        != Some(now_ns + allowance_ns)
    {
        return Err("an out-of-scope marker forged deadline ownership".into());
    }
    let saved_scope_deadline = std::env::var_os(OWN_SCOPE_DEADLINE_ENV);
    for non_owner in [None, Some(""), Some("0"), Some("99"), Some("malformed")] {
        match non_owner {
            Some(v) => std::env::set_var(OWN_SCOPE_DEADLINE_ENV, v),
            None => std::env::remove_var(OWN_SCOPE_DEADLINE_ENV),
        }
        if owns_scope_request(Some(100)) {
            return Err(format!(
                "scope request ownership accepted non-owner marker {non_owner:?}"
            ));
        }
    }
    std::env::set_var(OWN_SCOPE_DEADLINE_ENV, "100");
    if !owns_scope_request(Some(100)) || owns_scope_request(None) {
        return Err("scope request ownership failed its exact positive bracket".into());
    }
    match saved_scope_deadline {
        Some(v) => std::env::set_var(OWN_SCOPE_DEADLINE_ENV, v),
        None => std::env::remove_var(OWN_SCOPE_DEADLINE_ENV),
    }

    // Positive: the one qualifying case must be ACCEPTED (guards against a
    // predicate that refuses everything and looks correct).
    if !force_full_policy_allows(true, Level::Full, None) {
        return Err("force-full: full/unfocused must be allowed".into());
    }
    if !force_full_policy_allows(false, Level::Quick, Some("rr-compat-only")) {
        return Err("force-full: inactive flag must allow anything".into());
    }
    // Negative: every non-full level and every focused mode must be REFUSED.
    for l in [Level::Quick, Level::PortableOnly, Level::Super] {
        if force_full_policy_allows(true, l, None) {
            return Err(format!("force-full: level {} must be refused", l.name()));
        }
    }
    for m in [
        "envelope-only",
        "strict-compat-only",
        "portable-strict-compat-only",
        "rr-compat-only",
        "sabre-compat-only",
        "e9patch-compat-only",
        "liteinst-compat-only",
        "qemu-l2-only",
        "privileged-only",
        "only",
        "selective",
        "shallow-select",
    ] {
        if force_full_policy_allows(true, Level::Full, Some(m)) {
            return Err(format!("force-full: focused mode {m} must be refused"));
        }
    }
    // Shell quoting: a corpus argv element must survive round-tripping through
    // `bash -c` byte-for-byte. A silent mangling here would change what the guest
    // runs while every count still looked right.
    for probe in [
        "plain",
        "with space",
        "single'quote",
        "$(command sub)",
        "back`tick`",
        "new\nline",
        r#"double"quote"#,
        "a;b|c&d",
        "",
    ] {
        let quoted = validate_plan::shell_quote(probe);
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!("printf '%s' {quoted}"))
            .output()
            .map_err(|e| format!("shell-quote bracket: cannot run bash: {e}"))?;
        let got = String::from_utf8_lossy(&out.stdout);
        if got != probe {
            return Err(format!("shell-quote bracket: {probe:?} round-tripped as {got:?}"));
        }
    }
    // Corpus tables must still match the counts the bash declared. This is the
    // drift guard for a MECHANICALLY EXTRACTED table: if someone edits a corpus
    // JSON without moving the corresponding ratchet, or vice versa, the extraction
    // has silently diverged from the numbers the gates are judged against.
    if validate_corpus::RR_PASSING_LABELS.len() != validate_corpus::RR_COMPAT_EXPECTED {
        return Err(format!(
            "R/R label set has {} rows, expected {}",
            validate_corpus::RR_PASSING_LABELS.len(),
            validate_corpus::RR_COMPAT_EXPECTED
        ));
    }
    let root = repo_root();
    let paths = validate_corpus::CorpusPaths {
        root_dir: "/nonexistent",
        real_compat_fixtures: "/nonexistent",
        validation_tmp_dir: "/nonexistent",
        shell_build_dir: "/nonexistent",
    };
    let count = |m: &str| -> Result<usize, String> {
        validate_corpus::load(&root, m, &paths).map(|r| r.len())
    };
    // Exact: these two matched their declared totals at extraction time, and that
    // exact agreement is the evidence the extraction was faithful.
    let strict = count("strict")?;
    if strict != validate_corpus::STRICT_COMPAT_TOTAL {
        return Err(format!(
            "strict corpus has {strict} rows, STRICT_COMPAT_TOTAL is {}",
            validate_corpus::STRICT_COMPAT_TOTAL
        ));
    }
    let sabre = count("sabre")?;
    if sabre != validate_corpus::SABRE_COMPAT_TOTAL {
        return Err(format!(
            "sabre corpus has {sabre} rows, SABRE_COMPAT_TOTAL is {}",
            validate_corpus::SABRE_COMPAT_TOTAL
        ));
    }
    // rr admits a superset and is filtered to the measured-passing labels; what
    // must hold is that every passing label is actually present to be measured.
    let rr_rows = validate_corpus::load(&root, "rr", &paths)?;
    let present: BTreeSet<&str> = rr_rows.iter().map(|r| r.label.as_str()).collect();
    let missing: Vec<&&str> = validate_corpus::RR_PASSING_LABELS
        .iter()
        .filter(|l| !present.contains(**l))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} R/R passing label(s) are absent from the rr corpus and could never be measured: {missing:?}",
            missing.len()
        ));
    }
    // e9patch admits a superset of its gated total (rows gate only when the
    // program is installed), so the invariant is >=, not ==.
    let e9 = count("e9patch")?;
    if e9 < validate_corpus::E9PATCH_COMPAT_TOTAL {
        return Err(format!(
            "e9patch corpus has {e9} rows, below E9PATCH_COMPAT_TOTAL {}",
            validate_corpus::E9PATCH_COMPAT_TOTAL
        ));
    }
    println!(
        "  corpora: strict={strict} sabre={sabre} rr={} (filtered to {}) e9patch={e9}",
        rr_rows.len(),
        validate_corpus::RR_COMPAT_EXPECTED
    );
    // Policy/data brackets are inert: none runs a gate, publishes a label,
    // writes the real ledger, or touches a PR. The one deliberate exception is
    // nested_scope_self_test: it uses a fresh disposable scope with strict
    // inner/run/scope/wrapper bounds, then proves a nested signal cannot stop it.
    for line in [
        safe_ci_scope::self_test()?,
        nested_scope_self_test()?,
        retry_timeout_bound_bracket(&root)?,
        scheduler_accounting_bracket()?,
        budget_reason_bracket()?,
        summary_listing_bracket()?,
        validate_super::self_test(&root)?,
        validate_envelope::self_test()?,
        validate_history::self_test()?,
        validate_receipt::self_test()?,
        validate_runtime::self_test()?,
    ] {
        println!("  {line}");
    }
    // The `--envelope-*` CLI shape is a CONTRACT with scripts/progress-report.sh
    // and the progress-rubric skill, so it is asserted rather than assumed.
    envelope_cli_bracket()?;
    verbosity_cli_bracket(&root)?;
    super_plan_bracket()?;
    // Completeness is what a self-certifying driver is least able to check about
    // itself, so its refusal predicate is bracketed here rather than assumed.
    verdict_refusal_bracket()?;
    host_capability_bracket(&root)?;
    coverage_schema_bracket()?;
    cell_results_schema_bracket()?;
    test_node_coverage_bracket()?;
    typed_libtest_count_bracket()?;
    ledger_gate_origin_bracket()?;
    requalification_plan_bracket(&root)?;
    validate_series_writer_bracket()?;
    no_result_propagation_bracket()?;
    possible_missing_artifact_bracket()?;
    selective_subset_bracket(&root)?;
    only_plan_bracket(&root)?;
    self_output_bracket()?;
    product_front_door_bracket()?;
    product_front_door_process_bracket()?;
    // ---- DAG-config carry + ungrantable-resource brackets -------------------
    // BOTH directions. A check that refuses everything would pass the negative
    // case alone, so the positive case (a real lane admits) is load-bearing.
    {
        let root = repo_root();
        for lane in ["portable", "privileged"] {
            let mut base = validate_plan::lane_config(&root, lane)?;
            base.default_jobs_env =
                format!("VALIDATE_CARRY_{}_JOBS", lane.to_ascii_uppercase());
            // POSITIVE: a real lane's own config must carry, and must be grantable.
            let steps = validate_plan::lane_nodes(&root, lane, "", "gate.manifest")?;
            let carried = validate_plan::config_from_base(&base, steps, "bracket");
            validate_plan::assert_config_carried(&base, &carried)
                .map_err(|e| format!("carry bracket: lane {lane} did not carry its config: {e}"))?;
            let mut missing_jobs_env = carried.clone();
            missing_jobs_env.default_jobs_env.clear();
            let jobs_env_error =
                validate_plan::assert_config_carried(&base, &missing_jobs_env)
                    .err()
                    .ok_or_else(|| {
                        format!("carry bracket: lane {lane} accepted a dropped default_jobs_env")
                    })?;
            if !jobs_env_error.contains("default_jobs_env") {
                return Err(format!(
                    "carry bracket: lane {lane} dropped jobs env but named {jobs_env_error}"
                ));
            }
            if base.resource_caps.is_empty() {
                return Err(format!("carry bracket: lane {lane} declares no resource_caps; \
                                    the bracket would be vacuous"));
            }
            let bad = validate_plan::ungrantable_resources(&carried);
            if !bad.is_empty() {
                return Err(format!(
                    "grantable bracket: lane {lane} carried its caps yet still reports {} \
                     ungrantable demand(s): {:?}", bad.len(), &bad[..bad.len().min(3)]));
            }
            // NEGATIVE: drop the caps exactly as the bug did -> must be REFUSED,
            // and must NAME the resource rather than sleeping on it.
            let mut stripped = carried.clone();
            stripped.resource_caps.clear();
            let starved = validate_plan::ungrantable_resources(&stripped);
            if starved.is_empty() {
                return Err(format!(
                    "grantable bracket: lane {lane} with resource_caps CLEARED reported nothing \
                     ungrantable -- the check is inert and would not have caught the stall"));
            }
            let named = base.resource_caps.keys().any(|r| starved.iter().any(|b| b.contains(r)));
            if !named {
                return Err(format!("grantable bracket: refusal for {lane} names no resource: {:?}",
                                   &starved[..starved.len().min(2)]));
            }
            // NEGATIVE 2: a dropped config must be DETECTED, not tolerated.
            let defaulted = validate_plan::config_from(carried.steps.clone(), "bracket");
            if validate_plan::assert_config_carried(&base, &defaulted).is_ok() {
                return Err(format!(
                    "carry bracket: lane {lane} rebuilt from Default::default() compared EQUAL to \
                     its file config -- the assertion cannot detect the bug it exists for"));
            }
            println!("  dag-config: {lane} carries {} cap(s), default_step_timeout={}s; \
cleared-caps refusal names {} starved step(s)",
                     base.resource_caps.len(), base.default_step_timeout, starved.len());
        }
    }
    // The full hot path is one fused DAG and pays the exact-tree manifest audit
    // once. Bracket the positive shape and both diagnostic escape hatches: a
    // sequential plan still exists, while the nested audit reuse is accepted
    // only for the no-label portable-strict payload.
    {
        let root = repo_root();
        let tmp = std::env::temp_dir().join(format!("validate-plan-selftest-{}", std::process::id()));
        let full_args = parse_argv(&["full".into(), "--no-label-pr".into()])
            .map_err(|rc| format!("full-plan bracket: parser refused positive form rc={rc}"))?;
        let mut full = build_plan(&root, &full_args, &tmp)?;
        if full.second.is_some() {
            return Err("full-plan bracket: default full plan is still sequential".into());
        }
        let manifest_nodes: Vec<String> = full
            .cfg
            .steps
            .iter()
            .filter(|s| validation_step_identity(s) == ValidationStepIdentity::ManifestAudit)
            .map(|s| s.tag())
            .collect();
        if manifest_nodes != vec!["gate.manifest"] {
            return Err(format!(
                "full-plan bracket: exact-tree manifest audit was not deduped to gate.manifest: {manifest_nodes:?}"
            ));
        }
        // The surviving audit must run AFTER the node that builds the binary it
        // invokes, and that builder must not wait on the audit. Losing this edge
        // in `dedupe_identical` made every cold full run die at
        // `exit 127: target/debug/test-harness: No such file or directory` with
        // 56 of 59 nodes skipped, and made every warm run audit the tree with a
        // stale binary. Asserted on the real full plan, not a fixture.
        let builder = "setup.manifest_plan";
        let find_deps = |tag: &str| {
            full.cfg
                .steps
                .iter()
                .find(|s| s.tag() == tag)
                .map(|s| s.deps.clone())
        };
        let audit_deps = find_deps("gate.manifest")
            .ok_or_else(|| "full-plan bracket: gate.manifest disappeared".to_string())?;
        let builder_deps = find_deps(builder)
            .ok_or_else(|| format!("full-plan bracket: {builder} disappeared"))?;
        if !audit_deps.iter().any(|d| d == builder) {
            return Err(format!(
                "full-plan bracket: gate.manifest does not depend on {builder}, so a cold run cannot build the binary it invokes: deps={audit_deps:?}"
            ));
        }
        if builder_deps.iter().any(|d| d == "gate.manifest") {
            return Err(format!(
                "full-plan bracket: {builder} still waits on gate.manifest, which is the cycle the dependency union must break: deps={builder_deps:?}"
            ));
        }
        let manifest_audit = full
            .cfg
            .steps
            .iter()
            .find(|step| validation_step_identity(step) == ValidationStepIdentity::ManifestAudit)
            .expect("manifest audit exists")
            .clone();
        let manifest_producer = full
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
            .ok_or("full-plan bracket: manifest-plan producer disappeared")?;
        if manifest_producer.cmd != validate_plan::MANIFEST_PLAN_BUILD_COMMAND
            || manifest_producer.deps != [PIN_GATE_TAG.to_string()]
            || manifest_producer.deps.iter().any(|dependency| dependency == "gate.manifest")
        {
            return Err(format!(
                "full-plan bracket: manifest-plan producer is not directly after the pin gate: cmd={} deps={:?}",
                manifest_producer.cmd, manifest_producer.deps
            ));
        }
        if manifest_audit.deps
            != [validate_plan::MANIFEST_PLAN_PRODUCER_TAG.to_string()]
        {
            return Err(format!(
                "full-plan bracket: manifest audit can run without its binary producer: deps={:?}",
                manifest_audit.deps
            ));
        }
        println!("  {}", manifest_producer_edge_bracket(&full.cfg)?);
        let mut wrong_invocation = vec![manifest_audit];
        wrong_invocation[0].cmd = "target/debug/test-harness validate --unexpected".into();
        if validation_step_identity(&wrong_invocation[0]) != ValidationStepIdentity::ManifestAudit
            || dedupe_identical(&mut wrong_invocation, "gate.manifest").is_ok()
        {
            return Err(
                "full-plan bracket: manifest-audit identity depended on command text or accepted an unexpected invocation"
                    .into(),
            );
        }
        let pin_nodes: Vec<String> = full
            .cfg
            .steps
            .iter()
            .filter(|s| s.cmd.contains("ci/run-reverie-pin-check.sh"))
            .map(|s| s.tag())
            .collect();
        if pin_nodes != vec![PIN_GATE_TAG] {
            return Err(format!(
                "full-plan bracket: pin authority was not deduped to the observed preflight: {pin_nodes:?}"
            ));
        }
        for required in ["test.strict_compat", "privileged-cpuid.faulting"] {
            if !full.cfg.steps.iter().any(|s| s.tag() == required) {
                return Err(format!("full-plan bracket: fused plan lost {required}"));
            }
        }
        let strict_before = full
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == "test.strict_compat")
            .expect("strict compatibility node exists")
            .clone();
        let executable = Path::new("/tmp/exact validate driver");
        if !reuse_validate_executable(&mut full, executable)? {
            return Err(
                "full-plan bracket: strict compatibility did not reuse the current validate executable"
                    .into(),
            );
        }
        let strict_after = full
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == "test.strict_compat")
            .expect("strict compatibility node remains after executable replacement");
        let mut strict_expected = strict_before.clone();
        strict_expected.cmd = strict_expected.cmd.replacen(
            " ./scripts/validate.rs --portable-strict-compat-only",
            " '/tmp/exact validate driver' --portable-strict-compat-only",
            1,
        );
        if format!("{strict_after:?}") != format!("{strict_expected:?}") {
            return Err(format!(
                "full-plan bracket: reusing the validate executable changed more than its command token:\n  before={strict_before:?}\n  after={strict_after:?}"
            ));
        }
        for argument in [
            "--portable-strict-compat-only",
            "--reuse-parent-manifest-gate",
            "--no-label-pr",
            "--verbose",
        ] {
            if strict_after.cmd.matches(argument).count() != 1 {
                return Err(format!(
                    "full-plan bracket: strict compatibility lost or duplicated {argument}: {}",
                    strict_after.cmd
                ));
            }
        }
        let mut corrupt = Plan::default();
        corrupt.cfg.steps.push(strict_before.clone());
        corrupt.cfg.steps[0].cmd = corrupt.cfg.steps[0]
            .cmd
            .replace(" ./scripts/validate.rs ", " ./changed-validate-driver ");
        if reuse_validate_executable(&mut corrupt, executable).is_ok() {
            return Err(
                "full-plan bracket: strict compatibility accepted an unaudited validate invocation"
                    .into(),
            );
        }
        let canonical_test_tags = test_nodes_of(&validate_plan::lane_config(&root, "portable")?);
        let fused_test_tags = test_nodes_of(&full.cfg);
        if fused_test_tags != canonical_test_tags {
            return Err(format!(
                "full-plan bracket: fused tags changed the receipt coverage denominator: \
                 canonical={canonical_test_tags:?}, fused={fused_test_tags:?}"
            ));
        }
        let portable_build = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "build.workspace")
            .ok_or("full-plan bracket: portable fat build disappeared")?;
        if !portable_build.cmd.contains("cargo build --workspace --all-targets")
            || !portable_build.cmd.contains("cargo build -p hermit")
            || !portable_build.cmd.contains("--bin hermit")
        {
            return Err("full-plan bracket: fat build does not finish the debug Hermit producer".into());
        }
        let artifact = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "build.e2e_artifact")
            .ok_or("full-plan bracket: verified E2E artifact publisher disappeared")?;
        if !artifact.cmd.contains("ci/publish-hermit-e2e-artifact.sh")
            || !artifact.cmd.ends_with(" target/install_pkg")
            || !["build.workspace", "build.runtime_release"]
                .iter()
                .all(|dep| artifact.deps.iter().any(|actual| actual == dep))
        {
            return Err(
                "full-plan bracket: E2E publisher is not a complete binary+resource barrier"
                    .into(),
            );
        }
        let manifest_consumers: Vec<_> = full
            .cfg
            .steps
            .iter()
            .filter(|s| validation_step_identity(s) == ValidationStepIdentity::ManifestRun)
            .collect();
        if manifest_consumers.is_empty() {
            return Err("full-plan bracket: no manifest consumers were inspected".into());
        }
        let manifest_tags = manifest_consumers
            .iter()
            .map(|step| step.tag())
            .collect::<BTreeSet<_>>();
        let scorecard_deps = full
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == "scorecard.compatibility")
            .ok_or("full-plan bracket: compatibility scorecard disappeared")?
            .deps
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        if scorecard_deps != manifest_tags {
            return Err(format!(
                "full-plan bracket: compatibility scorecard does not depend on every manifest result node: expected={manifest_tags:?}, actual={scorecard_deps:?}"
            ));
        }
        let mut results_paths = BTreeSet::new();
        let mut junit_paths = BTreeSet::new();
        let mut spelling_probe = (*manifest_consumers[0]).clone();
        spelling_probe.cmd = "changed invocation text".into();
        if validation_step_identity(&spelling_probe) != ValidationStepIdentity::ManifestRun {
            return Err(
                "full-plan bracket: manifest-run identity still depends on command text".into(),
            );
        }
        for consumer in manifest_consumers {
            let lane = if consumer.cmd.contains("--lane portable ") {
                "portable"
            } else if consumer.cmd.contains("--lane privileged ") {
                "privileged"
            } else {
                return Err(format!(
                    "full-plan bracket: {} manifest consumer has no exact lane selector: {}",
                    consumer.tag(), consumer.cmd
                ));
            };
            let result_path = format!(
                "\"$E2E_RESULT_ROOT/{lane}/{}/results.jsonl\"",
                consumer.job
            );
            let junit_path = format!(
                "\"$E2E_RESULT_ROOT/{lane}/{}/junit.xml\"",
                consumer.job
            );
            if consumer.cmd.matches("--results").count() != 1
                || !consumer.cmd.contains(&format!("--results {result_path}"))
                || !results_paths.insert(result_path)
            {
                return Err(format!(
                    "full-plan bracket: {} does not have one unique result path: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            if consumer
                .env
                .get(validate_plan::E2E_ATTEMPT_ENV)
                .map(String::as_str)
                != Some("1")
            {
                return Err(format!(
                    "full-plan bracket: {} does not declare initial E2E attempt 1: {:?}",
                    consumer.tag(), consumer.env
                ));
            }
            if consumer.cmd.matches("--junit").count() != 1
                || !consumer.cmd.contains(&format!("--junit {junit_path}"))
                || !junit_paths.insert(junit_path)
            {
                return Err(format!(
                    "full-plan bracket: {} does not have one unique JUnit path: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            if !consumer.cmd.starts_with("./ci/run-with-hermit-e2e-artifact.sh ") {
                return Err(format!(
                    "full-plan bracket: {} still consumes a mutable Hermit path: {}",
                    consumer.tag(), consumer.cmd
                ));
            }
            let producer = if consumer.cmd.contains("--lane portable ") {
                if !consumer.cmd.contains("--require-install") {
                    return Err(format!(
                        "full-plan bracket: portable consumer {} did not require the backend-resource bundle",
                        consumer.tag()
                    ));
                }
                "build.e2e_artifact"
            } else {
                "privileged-build.privileged_tests"
            };
            if !consumer.deps.iter().any(|d| d == producer) {
                return Err(format!(
                    "full-plan bracket: {} does not declare immutable artifact producer {producer}",
                    consumer.tag()
                ));
            }
        }
        let privileged_build = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "privileged-build.privileged_tests")
            .ok_or("full-plan bracket: privileged focused build disappeared")?;
        if !privileged_build
            .deps
            .iter()
            .any(|d| d == "build.e2e_artifact")
        {
            return Err(
                "full-plan bracket: privileged build can race the portable artifact producer"
                    .into(),
            );
        }
        if privileged_build.cmd.contains("cargo ")
            || !privileged_build
                .cmd
                .contains("verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path")
            || !privileged_build.cmd.contains("tests_misc-*")
        {
            return Err(
                "full-plan bracket: privileged build did not become an exact artifact assertion"
                    .into(),
            );
        }
        let cpuid = full
            .cfg
            .steps
            .iter()
            .find(|s| s.tag() == "privileged-cpuid.faulting")
            .ok_or("full-plan bracket: privileged CPUID node disappeared")?;
        if cpuid.cmd.contains("cargo ") || !cpuid.cmd.contains("rdrand_rdseed_is_masked") {
            return Err(
                "full-plan bracket: CPUID test does not directly execute the prebuilt binary"
                    .into(),
            );
        }
        let sequential_args = parse_argv(&[
            "full".into(),
            "--sequential-lanes".into(),
            "--no-label-pr".into(),
        ])
        .map_err(|rc| format!("full-plan bracket: sequential diagnostic refused rc={rc}"))?;
        if build_plan(&root, &sequential_args, &tmp)?.second.is_none() {
            return Err("full-plan bracket: --sequential-lanes did not preserve the fallback".into());
        }
        let nested_args = parse_argv(&[
            "--portable-strict-compat-only".into(),
            "--reuse-parent-manifest-gate".into(),
            "--no-label-pr".into(),
        ])
        .map_err(|rc| format!("full-plan bracket: nested positive form refused rc={rc}"))?;
        let mut nested = build_plan(&root, &nested_args, &tmp)?;
        let nested_before = format!("{:?}", nested.cfg.steps);
        if reuse_validate_executable(&mut nested, executable)?
            || format!("{:?}", nested.cfg.steps) != nested_before
        {
            return Err(
                "full-plan bracket: a plan without test.strict_compat was modified by executable reuse"
                    .into(),
            );
        }
        if nested.cfg.steps.iter().any(|s| s.tag() == "gate.manifest")
            || !nested.cfg.steps.iter().any(|s| s.tag() == PIN_GATE_TAG)
        {
            return Err(
                "full-plan bracket: nested reuse did not remove only manifest while retaining the pin gate"
                    .into(),
            );
        }
        if parse_argv(&[
            "--portable-strict-compat-only".into(),
            "--reuse-parent-manifest-gate".into(),
            // Make the rejected form explicit: frozen validation sets
            // VALIDATE_LABEL_PR=0 for the outer run, and a self-test must not
            // inherit that setting into the case meant to exercise labeling.
            "--label-pr".into(),
        ])
        .is_ok()
        {
            return Err("full-plan bracket: nested reuse accepted a label-capable invocation".into());
        }
        println!(
            "  full plan: {} fused node(s), 1 manifest-plan producer -> 1 exact-tree manifest audit + 1 pin authority; sequential fallback + nested no-label reuse bracketed",
            full.cfg.steps.len()
        );
    }

    Ok(())
}

/// Assert that the public option names only the two inner checks it skips.
///
/// The parent `ci-hub validate-lock` admission runs before this option reaches
/// `scripts/validate.rs`. The option therefore must not imply that it admits a
/// dirty or stale validation target through that earlier check.
fn inner_freshness_skip_cli_bracket() -> Result<(), String> {
    if parse_argv(&["--run-on-dirty-tree".into(), "--self-test".into()]).is_ok() {
        return Err("inner freshness skip: the misleading old option is still accepted".into());
    }
    let parsed = parse_argv(&[
        SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION.into(),
        "--self-test".into(),
    ])
    .map_err(|code| {
        format!(
            "inner freshness skip: parser refused {} with exit {code}",
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION
        )
    })?;
    if !parsed.skip_inner_dirty_working_tree_and_rebase_freshness_checks {
        return Err(format!(
            "inner freshness skip: {} did not select the two inner checks",
            SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION
        ));
    }

    let help = usage();
    for required in [
        SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
        SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_ENV,
        "Skip only scripts/validate.rs's dirty-working-tree and",
        "rebase-freshness checks; does not bypass ci-hub validate-lock",
        "admission. AGENTS SHOULD NOT USE THIS.",
    ] {
        if !help.contains(required) {
            return Err(format!(
                "inner freshness skip: help omitted required text {required:?}"
            ));
        }
    }
    for removed in ["--run-on-dirty-tree", "VALIDATE_RUN_ON_DIRTY_TREE"] {
        if help.contains(removed) {
            return Err(format!(
                "inner freshness skip: help still advertises misleading name {removed}"
            ));
        }
    }
    println!(
        "  inner freshness skip: new option accepted, old option refused, and help names only the two inner checks"
    );
    Ok(())
}

/// Execute the production manifest producer/audit dependency spine in both
/// directions. The positive case starts with no output and requires the
/// producer to create it before the audit runs. The negative case makes the
/// producer fail and requires the scheduler to dependency-skip the audit.
fn manifest_producer_edge_bracket(cfg: &DagConfig) -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!(
        "validate-manifest-producer-edge-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    std::fs::create_dir(&tmp)
        .map_err(|error| format!("manifest producer bracket: cannot create {}: {error}", tmp.display()))?;

    let result = (|| -> Result<(), String> {
        let required = [
            "pre.submodules",
            PIN_GATE_TAG,
            validate_plan::MANIFEST_PLAN_PRODUCER_TAG,
            "gate.manifest",
        ];
        let fixture = |producer_cmd: String, gate_cmd: String| -> Result<DagConfig, String> {
            let mut steps = Vec::new();
            for tag in required {
                let source = cfg
                    .steps
                    .iter()
                    .find(|step| step.tag() == tag)
                    .ok_or_else(|| format!("manifest producer bracket: production plan lost {tag}"))?;
                let mut step = step_with_caps(
                    &source.group,
                    &source.job,
                    "manifest producer dependency fixture",
                    match tag {
                        "pre.submodules" | PIN_GATE_TAG => "true".to_string(),
                        validate_plan::MANIFEST_PLAN_PRODUCER_TAG => producer_cmd.clone(),
                        "gate.manifest" => gate_cmd.clone(),
                        _ => unreachable!(),
                    },
                    source.deps.clone(),
                    30,
                    30,
                    64 * 1024 * 1024,
                );
                step.deps.retain(|dependency| required.contains(&dependency.as_str()));
                steps.push(step);
            }
            Ok(validate_plan::config_from(
                steps,
                "manifest producer dependency fixture",
            ))
        };

        let output = tmp.join("target/debug/test-harness");
        let gate_ran = tmp.join("gate-ran");
        let output_parent = output
            .parent()
            .ok_or_else(|| format!("manifest producer bracket: {} has no parent", output.display()))?;
        let positive = fixture(
            format!(
                "mkdir -p {parent} && printf '#!/bin/sh\\nexit 0\\n' > {output} && chmod +x {output}",
                parent = validate_plan::shell_quote(&output_parent.to_string_lossy()),
                output = validate_plan::shell_quote(&output.to_string_lossy()),
            ),
            format!(
                "test -x {output} && {output} && : > {gate_ran}",
                output = validate_plan::shell_quote(&output.to_string_lossy()),
                gate_ran = validate_plan::shell_quote(&gate_ran.to_string_lossy()),
            ),
        )?;
        let positive_result = run_lane_with_env_retries(
            &positive,
            2,
            true,
            0,
            None,
            &tmp.join("positive.log"),
            None,
            0,
            &BTreeMap::new(),
            false,
        );
        if !positive_result.complete
            || !positive_result.ok
            || positive_result.outcomes.len() != required.len()
            || !positive_result.skipped.is_empty()
            || !output.is_file()
            || !gate_ran.is_file()
        {
            return Err(format!(
                "manifest producer bracket: absent output was not produced before the gate: complete={} ok={} outcomes={:?} skipped={:?} output={} gate_ran={}",
                positive_result.complete,
                positive_result.ok,
                positive_result
                    .outcomes
                    .iter()
                    .map(|outcome| (outcome.tag.as_str(), outcome.ok))
                    .collect::<Vec<_>>(),
                positive_result.skipped,
                output.is_file(),
                gate_ran.is_file()
            ));
        }

        std::fs::remove_file(&output)
            .map_err(|error| format!("manifest producer bracket: cannot remove {}: {error}", output.display()))?;
        std::fs::remove_file(&gate_ran)
            .map_err(|error| format!("manifest producer bracket: cannot remove {}: {error}", gate_ran.display()))?;
        let negative = fixture(
            "exit 23".to_string(),
            format!(
                ": > {}",
                validate_plan::shell_quote(&gate_ran.to_string_lossy())
            ),
        )?;
        let negative_result = run_lane_with_env_retries(
            &negative,
            2,
            true,
            0,
            None,
            &tmp.join("negative.log"),
            None,
            0,
            &BTreeMap::new(),
            false,
        );
        let producer_failed = negative_result.outcomes.iter().any(|outcome| {
            outcome.tag == validate_plan::MANIFEST_PLAN_PRODUCER_TAG
                && !outcome.ok
                && outcome.returncode == Some(23)
        });
        if negative_result.complete
            || negative_result.ok
            || !producer_failed
            || negative_result.skipped != ["gate.manifest".to_string()]
            || gate_ran.exists()
        {
            return Err(format!(
                "manifest producer bracket: failed producer did not block the gate: complete={} ok={} producer_failed={producer_failed} outcomes={:?} skipped={:?} gate_ran={}",
                negative_result.complete,
                negative_result.ok,
                negative_result
                    .outcomes
                    .iter()
                    .map(|outcome| (outcome.tag.as_str(), outcome.ok, outcome.returncode))
                    .collect::<Vec<_>>(),
                negative_result.skipped,
                gate_ran.exists()
            ));
        }
        Ok(())
    })();

    let cleanup = std::fs::remove_dir_all(&tmp)
        .map_err(|error| format!("manifest producer bracket: cannot remove {}: {error}", tmp.display()));
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(
            "manifest producer edge: absent output built before gate; producer failure dependency-skips gate"
                .into(),
        ),
        (Err(problem), Ok(())) => Err(problem),
        (Ok(()), Err(cleanup_problem)) => Err(cleanup_problem),
        (Err(problem), Err(cleanup_problem)) => Err(format!(
            "{problem}; cleanup also failed: {cleanup_problem}"
        )),
    }
}

/// Bind the current schema to the evidence the row actually carries.
///
/// A missing or malformed coverage judgement stays explicit `null`. It must not
/// cause a new row to masquerade as a grandfathered schema-4 receipt.
fn ledger_schema_and_coverage(
    coverage: serde_json::Value,
) -> (i64, serde_json::Value) {
    let has_real_judgement = coverage
        .get("planned_test_nodes")
        .and_then(serde_json::Value::as_u64)
        .is_some_and(|planned| planned > 0);
    if has_real_judgement {
        (COVERAGE_LEDGER_SCHEMA_VERSION, coverage)
    } else {
        (COVERAGE_LEDGER_SCHEMA_VERSION, serde_json::Value::Null)
    }
}

fn ledger_schema_version(
    coverage_schema: i64,
    cell_results: Option<&validate_cell_results::RetainedCellResults>,
) -> i64 {
    cell_results
        .map(|results| results.schema_version)
        .unwrap_or(coverage_schema)
}

/// Two-sided producer bracket for [`ledger_schema_and_coverage`]. Inert: it
/// serializes no row and writes no ledger.
fn coverage_schema_bracket() -> Result<(), String> {
    let real = serde_json::json!({
        "planned_test_nodes": 4,
        "executed_test_nodes": 4,
        "zero_executed_nodes": [],
        "absent_nodes": [],
    });
    let (schema, carried) = ledger_schema_and_coverage(real.clone());
    if schema != COVERAGE_LEDGER_SCHEMA_VERSION || carried != real {
        return Err("coverage schema: a real judgement must be carried as schema 5".into());
    }

    for unresolved in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!({"planned_test_nodes": 0}),
        serde_json::json!({"planned_test_nodes": "4"}),
    ] {
        let (schema, carried) = ledger_schema_and_coverage(unresolved);
        if schema != COVERAGE_LEDGER_SCHEMA_VERSION || !carried.is_null() {
            return Err(
                "coverage schema: unresolved evidence must remain schema 5 with null coverage".into(),
            );
        }
    }
    println!(
        "  coverage schema: 1/1 real judgement -> schema 5; 4/4 unresolved shapes -> schema 5/null"
    );
    Ok(())
}

/// Bind the outer ledger version to the retained payload that defines its
/// shape. This makes reverting the payload version without changing the writer
/// fail in `--self-test`, rather than silently relabeling new evidence as an old
/// schema.
fn cell_results_schema_bracket() -> Result<(), String> {
    let retained = validate_cell_results::RetainedCellResults {
        schema_version: validate_cell_results::CELL_RESULTS_LEDGER_SCHEMA_VERSION,
        run_id: "schema-bracket".into(),
        evidence: serde_json::json!({}),
    };
    let current = ledger_schema_version(COVERAGE_LEDGER_SCHEMA_VERSION, Some(&retained));
    if current != 7 {
        return Err(format!(
            "cell-results schema: current payload must emit schema 7, got {current}"
        ));
    }
    if ledger_schema_version(COVERAGE_LEDGER_SCHEMA_VERSION, None)
        != COVERAGE_LEDGER_SCHEMA_VERSION
    {
        return Err("cell-results schema: a row without cell results changed schema".into());
    }
    println!("  cell-results schema: current payload -> schema 7; absent payload -> schema 5");
    Ok(())
}

/// Bracket the self-output classifier that decides whether the tree is dirty.
///
/// This predicate is load-bearing in a way that is easy to miss: `tree_dirty()`
/// feeds `commit_anchored`, which gates BOTH the tree-keyed cache and receipt
/// publication. When it was wrong, both features were inert and nothing said so
/// — every run simply recorded `commit_anchored: false` and re-ran. So each
/// listing SHAPE gets an explicit case, including the exact one that regressed:
/// a porcelain line whose leading status column has been eaten by a trim.
fn self_output_bracket() -> Result<(), String> {
    // MUST be excused (validate's own output, in every shape a caller emits).
    let excused = [
        (" M ci/validate-ledger/local.example-host.jsonl", "porcelain, modified, leading space intact"),
        ("M ci/validate-ledger/local.example-host.jsonl", "porcelain whose leading space a trim ate"),
        ("?? ci/validate-ledger/local.other.jsonl", "porcelain, untracked shard"),
        ("ci/validate-ledger/local.example-host.jsonl", "bare path (git diff --name-only)"),
        ("ignored/validate/validate-full-abc-1.log", "bare path, durable log"),
        (" M \"ci/validate-ledger/has space.jsonl\"", "porcelain, quoted path"),
        ("R  ci/validate-ledger/a.jsonl -> ci/validate-ledger/b.jsonl", "rename within the ledger dir"),
    ];
    for (line, why) in excused {
        if !line_is_self_output(line) {
            return Err(format!("self-output: {line:?} ({why}) must be excused as validate's own"));
        }
    }
    // MUST NOT be excused. A predicate that excused everything would satisfy the
    // list above and silently disable the dirty gate entirely.
    let foreign = [
        (" M scripts/validate.rs", "a real source change"),
        ("?? detcore/src/new_thing.rs", "a new untracked source file"),
        ("M  Cargo.lock", "a staged lockfile change"),
        ("scripts/lib/validate_plan.rs", "bare path, real source"),
        ("R  detcore/src/a.rs -> ci/validate-ledger/a.rs", "a source file MOVED into the ledger dir"),
        ("R  ci/validate-ledger/a.jsonl -> detcore/src/a.rs", "a ledger file moved OUT into source"),
        (" M ci/dag/portable.json", "a lane change under ci/, but not the ledger"),
        (" M ci/validate-ledger-notes.md", "a sibling whose name merely starts the same way"),
    ];
    for (line, why) in foreign {
        if line_is_self_output(line) {
            return Err(format!("self-output: {line:?} ({why}) must count as a DIRTY tree"));
        }
    }
    // LIVE invariant, independent of the synthetic shapes above: whatever this
    // checkout's real state is, no surviving entry may be validate's own output.
    // This is what actually catches a reintroduced trim, because it exercises
    // the real `git` invocation rather than a hand-written line.
    let mut live = 0usize;
    for args in [
        vec!["status", "--porcelain"],
        vec!["diff", "--name-only"],
        vec!["ls-files", "--others", "--exclude-standard"],
    ] {
        for line in foreign_porcelain(&args) {
            live += 1;
            if path_readings(&line).iter().any(|p| is_self_output(p)) {
                return Err(format!(
                    "self-output: `git {}` leaked validate's own output into the dirty set: {line:?}",
                    args.join(" ")
                ));
            }
        }
    }
    println!(
        "  self-output: {} own-output shape(s) excused, {} foreign change(s) still dirty, \
         {live} live entr(y/ies) from the real checkout all correctly classified",
        excused.len(),
        foreign.len()
    );
    Ok(())
}

/// Bracket the `--selective` subset builder against the REAL portable lane.
///
/// The dangerous failure here is silent under-running: a subset that drops a
/// node the selector asked for, or keeps a dangling dependency that makes the
/// runner skip a selected node. Both are checked against `ci/dag/portable.json`
/// itself rather than a fixture, because a fixture would not notice the lane
/// file changing shape underneath the selector.
fn selective_subset_bracket(root: &Path) -> Result<(), String> {
    let all = validate_plan::lane_nodes(root, "portable", "", "gate.manifest")?;
    let all_tags: BTreeSet<String> = all.iter().map(|s| s.tag()).collect();
    // Pick a node that has at least one intra-lane dependency, plus that
    // dependency, so the "keep both" and "prune the rest" behaviours are both
    // exercised on real data.
    let (child, parent) = all
        .iter()
        .find_map(|s| {
            s.deps.iter().find(|d| all_tags.contains(*d)).map(|d| (s.tag(), d.clone()))
        })
        .ok_or("selective bracket: ci/dag/portable.json has no intra-lane dependency to test")?;
    let keep: BTreeSet<String> = [child.clone(), parent.clone()].into_iter().collect();
    let sel = validate_plan::select_lane_nodes(all.clone(), &keep);
    // Positive: exactly the two named nodes survive, the kept edge survives, and
    // the manifest-gate edge (outside the lane) is NOT pruned.
    if sel.steps.len() != 2 {
        return Err(format!("selective bracket: kept {} node(s), expected 2", sel.steps.len()));
    }
    let kept_child = sel
        .steps
        .iter()
        .find(|s| s.tag() == child)
        .ok_or("selective bracket: the selected child node was dropped")?;
    if !kept_child.deps.contains(&parent) {
        return Err("selective bracket: a dependency inside the selected set must survive".into());
    }
    if sel.unknown_tags != Vec::<String>::new() {
        return Err(format!("selective bracket: unexpected unknown tags {:?}", sel.unknown_tags));
    }
    let root_node = sel.steps.iter().find(|s| s.tag() == parent).unwrap();
    if !root_node.deps.iter().all(|d| !all_tags.contains(d)) {
        return Err("selective bracket: an unselected lane dependency was left dangling".into());
    }
    // Negative: a tag the lane does not contain must be REPORTED, because that
    // means the selector and the DAG disagree and the subset is untrustworthy.
    let bogus: BTreeSet<String> =
        [parent.clone(), "no.such_node".to_string()].into_iter().collect();
    let sel2 = validate_plan::select_lane_nodes(all, &bogus);
    if sel2.unknown_tags != vec!["no.such_node".to_string()] {
        return Err(format!(
            "selective bracket: an unknown tag MUST be reported; got {:?}",
            sel2.unknown_tags
        ));
    }

    // Exercise the real selector, not a hand-written keep set. A flaky-tests
    // change reaches the chaos manifest cell through e2e.metadata and the
    // shipped setup.manifest_plan producer. The producer is valid selector
    // vocabulary even though plan composition satisfies it from preflight.
    let selector = Command::new(root.join("ci").join("select-tests.rs"))
        .args(["--files", "flaky-tests/Cargo.toml", "--format", "json"])
        .output()
        .map_err(|error| format!("selective bracket: cannot run real selector: {error}"))?;
    if !selector.status.success() {
        return Err(format!(
            "selective bracket: real selector failed: {}",
            String::from_utf8_lossy(&selector.stderr)
        ));
    }
    let selected_json: serde_json::Value = serde_json::from_slice(&selector.stdout)
        .map_err(|error| format!("selective bracket: real selector emitted invalid JSON: {error}"))?;
    let selected: BTreeSet<String> = selected_json["nodes"]
        .as_array()
        .ok_or("selective bracket: real selector JSON has no nodes array")?
        .iter()
        .filter_map(|node| node.as_str().map(str::to_string))
        .collect();
    for required in [
        validate_plan::MANIFEST_PLAN_PRODUCER_TAG,
        "e2e.metadata",
        "e2e.manifest_chaos_c",
    ] {
        if !selected.contains(required) {
            return Err(format!(
                "selective bracket: flaky-tests/Cargo.toml did not select required node {required}: {selected:?}"
            ));
        }
    }
    let raw = validate_plan::lane_nodes(root, "portable", "", "gate.manifest")?;
    let mut real = validate_plan::select_lane_nodes(raw, &selected);
    if !real.unknown_tags.is_empty() {
        return Err(format!(
            "selective bracket: real selector named unknown shipped tags before producer reuse: {:?}",
            real.unknown_tags
        ));
    }
    if !validate_plan::reuse_preflight_manifest_producer(
        &mut real.steps,
        "real selective result",
    )? {
        return Err(
            "selective bracket: real selector omitted its manifest-plan producer dependency"
                .into(),
        );
    }
    if real
        .steps
        .iter()
        .any(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
    {
        return Err("selective bracket: lane retained a duplicate manifest-plan producer".into());
    }
    let metadata = real
        .steps
        .iter()
        .find(|step| step.tag() == "e2e.metadata")
        .ok_or("selective bracket: real selector lost e2e.metadata")?;
    if !metadata
        .deps
        .iter()
        .any(|dependency| dependency == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
    {
        return Err(
            "selective bracket: e2e.metadata was not bound to the preflight manifest producer"
            .into(),
        );
    }

    // Exercise the same SelectDecision::Full branch used when no trustworthy
    // baseline exists. The complete shipped lane must also reuse preflight's
    // producer; otherwise composition creates two setup.manifest_plan nodes and
    // the lane copy still points back at gate.manifest.
    let full_lane = validate_plan::lane_nodes(root, "portable", "", "gate.manifest")?;
    let full_total = full_lane.len();
    let full_steps = apply_selective_decision(
        full_lane,
        full_total,
        SelectDecision::Full("no trustworthy green baseline (self-test)".into()),
    )?;
    let mut full_nodes = validate_plan::preflight_nodes(root, false);
    full_nodes.extend(full_steps);
    let submodules = full_nodes
        .iter()
        .find(|step| step.tag() == "pre.submodules")
        .ok_or("selective bracket: full/no-baseline fallback lost pre.submodules")?;
    if submodules.cmd
        != "./ci/verify-submodules.sh --self-test && ./ci/verify-submodules.sh"
        || submodules.cmd.contains("submodule update")
        || !submodules.deps.is_empty()
    {
        return Err(format!(
            "selective bracket: pre.submodules must self-test and verify before any repair: {submodules:?}"
        ));
    }
    let producer_count = full_nodes
        .iter()
        .filter(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
        .count();
    if producer_count != 1 {
        return Err(format!(
            "selective bracket: full/no-baseline fallback has {producer_count} manifest-plan producers, expected 1"
        ));
    }
    let producer = full_nodes
        .iter()
        .find(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
        .expect("exactly one producer exists");
    let gate = full_nodes
        .iter()
        .find(|step| step.tag() == "gate.manifest")
        .ok_or("selective bracket: full/no-baseline fallback lost gate.manifest")?;
    if producer.deps != [PIN_GATE_TAG.to_string()]
        || gate.deps != [validate_plan::MANIFEST_PLAN_PRODUCER_TAG.to_string()]
    {
        return Err(format!(
            "selective bracket: full/no-baseline producer ordering is wrong: producer={:?} gate={:?}",
            producer.deps, gate.deps
        ));
    }
    let tags: BTreeSet<String> = full_nodes.iter().map(|step| step.tag()).collect();
    if tags.len() != full_nodes.len() {
        return Err("selective bracket: full/no-baseline fallback contains duplicate tags".into());
    }
    let mut completed = BTreeSet::new();
    loop {
        let ready: Vec<String> = full_nodes
            .iter()
            .filter(|step| !completed.contains(&step.tag()))
            .filter(|step| step.deps.iter().all(|dependency| completed.contains(dependency)))
            .map(|step| step.tag())
            .collect();
        if ready.is_empty() {
            break;
        }
        completed.extend(ready);
    }
    if completed.len() != full_nodes.len() {
        let blocked: Vec<String> = full_nodes
            .iter()
            .filter(|step| !completed.contains(&step.tag()))
            .map(|step| step.tag())
            .collect();
        return Err(format!(
            "selective bracket: full/no-baseline fallback contains a dependency cycle or missing dependency: {blocked:?}"
        ));
    }
    println!(
        "  selective subset: kept {child} + its dep {parent} from the real portable lane \
         ({} edge(s) pruned); flaky-tests selector reused its manifest producer; \
         full/no-baseline fallback has one acyclic producer; 1 unknown-tag refusal",
        sel.pruned_edges
    );
    Ok(())
}

/// Bracket `--only` against the real portable lane and the real plan builder.
///
/// This specifically guards against the historical implementation: a synthetic
/// `shard.*` step that nested `ci/run-node.sh`, discarded the selected node's
/// resource contract, and duplicated the lane's manifest producer.
fn only_plan_bracket(root: &Path) -> Result<(), String> {
    let lane = "portable";
    let all = validate_plan::lane_nodes(root, lane, "", "gate.manifest")?;
    let all_tags: BTreeSet<String> = all.iter().map(|step| step.tag()).collect();
    let (child, parent) = all
        .iter()
        .find_map(|step| {
            step.deps
                .iter()
                .find(|dependency| all_tags.contains(*dependency))
                .map(|dependency| (step.clone(), dependency.clone()))
        })
        .ok_or("only bracket: portable lane has no intra-lane dependency")?;
    let parent_step = all
        .iter()
        .find(|step| step.tag() == parent)
        .cloned()
        .ok_or_else(|| format!("only bracket: selected parent {parent} is absent"))?;
    let selected_tags: BTreeSet<String> = [child.tag(), parent.clone()].into_iter().collect();
    let args = parse_argv(&[
        "--only".into(),
        lane.into(),
        format!("{parent},{}", child.tag()),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: CLI refused a valid selection with exit {code}"))?;
    let plan = build_plan(root, &args, &std::env::temp_dir().join("validate-only-plan"))?;
    if plan.selection_mode != "only" || plan.suite_complete || plan.second.is_some() {
        return Err(format!(
            "only bracket: focused plan authority changed: mode={} complete={} second={}",
            plan.selection_mode,
            plan.suite_complete,
            plan.second.is_some()
        ));
    }
    let tags: Vec<String> = plan.cfg.steps.iter().map(|step| step.tag()).collect();
    let actual_tags: BTreeSet<String> = tags.iter().cloned().collect();
    if actual_tags.len() != tags.len() {
        return Err(format!("only bracket: plan contains duplicate tags: {tags:?}"));
    }
    let mut expected_tags: BTreeSet<String> =
        validate_plan::preflight_nodes(root, has_cmd("with-proxy"))
            .iter()
            .map(|step| step.tag())
            .collect();
    expected_tags.extend(selected_tags.iter().cloned());
    if actual_tags != expected_tags {
        return Err(format!(
            "only bracket: plan did not contain exactly preflight plus requested nodes: expected={expected_tags:?} actual={actual_tags:?}"
        ));
    }
    if plan.cfg.steps.iter().any(|step| {
        step.group == "shard" || step.cmd.contains("ci/run-node.sh")
    }) {
        return Err("only bracket: selected node was wrapped in a nested runner".into());
    }
    for expected in [&parent_step, &child] {
        let actual = plan
            .cfg
            .steps
            .iter()
            .find(|step| step.tag() == expected.tag())
            .ok_or_else(|| format!("only bracket: selected node {} was dropped", expected.tag()))?;
        let mut expected_deps: Vec<String> = expected
            .deps
            .iter()
            .filter(|dependency| selected_tags.contains(*dependency))
            .cloned()
            .collect();
        if expected_deps.is_empty() {
            expected_deps.push("gate.manifest".into());
        }
        if actual.deps != expected_deps {
            return Err(format!(
                "only bracket: selected node {} has wrong transformed deps: expected={expected_deps:?} actual={:?}",
                actual.tag(),
                actual.deps
            ));
        }
        let mut normalized_expected = expected.clone();
        normalized_expected.deps = expected_deps;
        if format!("{actual:?}") != format!("{normalized_expected:?}") {
            return Err(format!(
                "only bracket: selected node {} did not retain its command/caps: expected={normalized_expected:?} actual={actual:?}",
                expected.tag()
            ));
        }
    }
    let actual_child = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == child.tag())
        .expect("checked above");
    if !actual_child.deps.contains(&parent) {
        return Err("only bracket: dependency inside the selection was dropped".into());
    }
    let base = validate_plan::lane_config(root, lane)?;
    validate_plan::assert_config_carried(&base, &plan.cfg)
        .map_err(|error| format!("only bracket: lane config was not carried: {error}"))?;

    let producer_args = parse_argv(&[
        "--only".into(),
        lane.into(),
        validate_plan::MANIFEST_PLAN_PRODUCER_TAG.into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: CLI refused producer selection with exit {code}"))?;
    let producer_plan = build_plan(
        root,
        &producer_args,
        &std::env::temp_dir().join("validate-only-producer-plan"),
    )?;
    let producer_count = producer_plan
        .cfg
        .steps
        .iter()
        .filter(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
        .count();
    if producer_count != 1 {
        return Err(format!(
            "only bracket: manifest producer appeared {producer_count} times, expected once"
        ));
    }
    let producer_tags: BTreeSet<String> = producer_plan
        .cfg
        .steps
        .iter()
        .map(|step| step.tag())
        .collect();
    let canonical_preflight = validate_plan::preflight_nodes(root, has_cmd("with-proxy"));
    let preflight_tags: BTreeSet<String> = canonical_preflight
        .iter()
        .map(|step| step.tag())
        .collect();
    if producer_tags != preflight_tags {
        return Err(format!(
            "only bracket: selecting the shared producer admitted non-preflight nodes: {producer_tags:?}"
        ));
    }
    let canonical_producer = canonical_preflight
        .iter()
        .find(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
        .ok_or("only bracket: canonical preflight omitted its manifest producer")?;
    let planned_producer = producer_plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == validate_plan::MANIFEST_PLAN_PRODUCER_TAG)
        .expect("counted exactly once above");
    if format!("{planned_producer:?}") != format!("{canonical_producer:?}") {
        return Err("only bracket: selected producer is not the canonical validate preflight node".into());
    }

    let unknown_args = parse_argv(&[
        "--only".into(),
        lane.into(),
        "no.such_node".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("only bracket: parser refused unknown-tag bracket with exit {code}"))?;
    let error = build_plan(
        root,
        &unknown_args,
        &std::env::temp_dir().join("validate-only-unknown-plan"),
    )
    .err()
    .ok_or("only bracket: unknown node tag was accepted")?;
    if !error.contains("unknown node tag") || !error.contains("Selectable tags") {
        return Err(format!(
            "only bracket: unknown-tag refusal omitted its diagnosis or choices: {error}"
        ));
    }
    println!(
        "  only plan: real node commands/caps retained, selected edge kept, outside edges dropped, producer unique"
    );
    Ok(())
}

/// Assert that `super` plans a complete, fully-boxed suite — and that the audit
/// which guarantees that would actually REFUSE an unboxed node.
///
/// The caps audit is the driver's own load-bearing guard: it is what makes
/// "boxing ACTIVE" true for every node rather than for the ones someone
/// remembered. A guard that never fires is indistinguishable from no guard, so
/// this brackets it on both sides with an inert synthetic node.
fn super_plan_bracket() -> Result<(), String> {
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("validate-super-plan-{}", std::process::id()));
    let args = parse_argv(&["super".to_string()])
        .map_err(|c| format!("super plan: the `super` level was REFUSED with exit {c}"))?;
    let plan = build_plan(&root, &args, &tmp)
        .map_err(|e| format!("super plan: could not build a plan: {e}"))?;
    // Positive: the audit must ACCEPT a real, fully-declared super plan.
    let undeclared = validate_plan::undeclared_nodes(&plan.cfg);
    if !undeclared.is_empty() {
        return Err(format!(
            "super plan: {} node(s) lack declared caps: {}",
            undeclared.len(),
            undeclared.join(", ")
        ));
    }
    let tags: BTreeSet<String> = plan.cfg.steps.iter().map(|s| s.tag()).collect();
    // One representative of each expansion the table names, so a lost synthetic
    // is caught here and not at 2am in the weekly run.
    for want in [
        "super.build_workspace",
        "super.build_release_hermit",
        "super.sqlite_veryquick_strict_determinism",
        "super.pmu_analyze_hello_race_stress_calibrated_skid",
        "superstress.ptrace_strict_verify_01",
        "superstress.kvm_available",
        "compatprep.fixtures",
        "compat.rustc",
    ] {
        if !tags.contains(want) {
            return Err(format!("super plan: node {want} is missing"));
        }
    }
    if !plan.super_mode {
        return Err("super plan: super_mode must be set so the stress table is printed".into());
    }
    // Negative: one node with no caps must be REFUSED by the same audit.
    let mut broken = validate_plan::config_from(
        vec![dagrun::model::Step {
            group: "bracket".into(),
            job: "uncapped".into(),
            desc: "inert fixture: declares no caps".into(),
            description: String::new(),
            cmd: "true".into(),
            deps: vec![],
            env: BTreeMap::new(),
            hint: Default::default(),
            networkonly: false,
            engine_only: false,
            timeout: 0,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }],
        "caps-audit negative bracket",
    );
    broken.default_step_cpu_timeout = 0;
    let refused = validate_plan::undeclared_nodes(&broken);
    if refused != vec!["bracket.uncapped".to_string()] {
        return Err(format!(
            "caps audit: an uncapped node MUST be refused; the audit returned {refused:?}"
        ));
    }
    println!(
        "  super plan: {} boxed node(s), all capped; caps audit bracketed 1 accept / 1 refusal",
        plan.cfg.steps.len()
    );
    Ok(())
}

fn verbosity_cli_bracket(root: &Path) -> Result<(), String> {
    let level = |args: &[&str]| -> Result<i64, String> {
        parse_argv(&args.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
            .map(|a| a.verbosity)
            .map_err(|code| format!("verbosity argv {args:?} refused with exit {code}"))
    };
    if level(&["--verbose"])? != 2 {
        return Err("verbosity: --verbose must select level 2".into());
    }
    for expected in 1..=5 {
        if level(&["--verbosity", &expected.to_string()])? != expected {
            return Err(format!("verbosity: --verbosity {expected} did not round-trip"));
        }
    }
    for bad in ["0", "6", "loud"] {
        if parse_verbosity(bad).is_ok() {
            return Err(format!("verbosity: invalid level {bad:?} was accepted"));
        }
    }
    let args = parse_argv(&["full".into(), "--no-label-pr".into()])
        .map_err(|code| format!("verbosity: full-plan argv refused with exit {code}"))?;
    let mut plan = build_plan(root, &args, &std::env::temp_dir().join("validate-verbosity-bracket"))?;
    let envelope = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "test.envelope_levels")
        .ok_or("verbosity: full plan lost test.envelope_levels")?;
    for fixture in [
        "run_probe true '/bin/true'",
        "run_probe echo '/bin/echo hermit-envelope'",
        "run_probe date '/bin/date -u +%Y'",
    ] {
        if !envelope.cmd.contains(fixture) {
            return Err(format!("verbosity: envelope lost stable identity fixture {fixture:?}"));
        }
    }
    if envelope.cmd.matches("\"$id\"").count() != 2 {
        return Err("verbosity: envelope START/END must use the same whitespace-free identity".into());
    }
    if envelope.cmd.matches("\"$id\" >&2").count() != 2
        || envelope.cmd.matches("</dev/null >&2").count() != 4
    {
        return Err(
            "verbosity: envelope markers and Hermit diagnostics must share stderr ordering".into(),
        );
    }
    propagate_verbosity(&mut plan, 5);
    let missing = plan
        .cfg
        .steps
        .iter()
        .chain(plan.second.iter().flat_map(|cfg| cfg.steps.iter()))
        .filter(|step| step.env.get("VALIDATE_VERBOSITY").map(String::as_str) != Some("5"))
        .count();
    if missing != 0 {
        return Err(format!("verbosity: {missing} DAG child(ren) lost level 5"));
    }
    Ok(())
}

/// Assert the `--envelope-only` / `--envelope-compare FILE` surface, and that it
/// actually plans the envelope measurement.
///
/// `scripts/progress-report.sh:102` runs `./scripts/validate.rs --envelope-only` and the
/// progress-rubric skill runs it with `ENVELOPE_JSON=...`. Those callers break
/// silently if the flag stops being accepted or starts meaning something else.
/// The parser and planner are exercised in-process, so the bracket measures the
/// FLAG SURFACE and not the checkout's cleanliness.
fn envelope_cli_bracket() -> Result<(), String> {
    let argv = |v: &[&str]| -> Vec<String> { v.iter().map(|s| s.to_string()).collect() };
    let root = repo_root();
    let tmp = std::env::temp_dir().join(format!("validate-envelope-cli-{}", std::process::id()));
    // Positive: both spellings must be ACCEPTED, select the envelope profile,
    // and produce a plan containing the L4 stress node — a parser that accepted
    // the flag and planned nothing would satisfy a weaker check.
    let mut accepted = 0usize;
    for v in [vec!["--envelope-only"], vec!["--envelope-compare", "/nonexistent-baseline.json"]] {
        let args = parse_argv(&argv(&v))
            .map_err(|c| format!("envelope CLI: `{v:?}` was REFUSED with exit {c}"))?;
        if !matches!(args.focused, Some(Focused::Envelope { .. })) {
            return Err(format!("envelope CLI: `{v:?}` did not select the envelope mode"));
        }
        let plan = build_plan(&root, &args, &tmp)
            .map_err(|e| format!("envelope CLI: `{v:?}` could not build a plan: {e}"))?;
        if plan.profile != "envelope-only" {
            return Err(format!("envelope CLI: `{v:?}` recorded profile {}", plan.profile));
        }
        let tags: BTreeSet<String> = plan.cfg.steps.iter().map(|s| s.tag()).collect();
        for want in ["envelope.build", "envelope.true_l4", "envelope.date_rr"] {
            if !tags.contains(want) {
                return Err(format!("envelope CLI: `{v:?}` planned no {want} node"));
            }
        }
        if !plan.force_keep_going {
            return Err("envelope CLI: the measurement must force keep-going".into());
        }
        if plan.nonblocking.len() != validate_envelope::PROBES.len() * validate_envelope::LEVELS.len()
        {
            return Err(format!(
                "envelope CLI: {} probe node(s) must be nonblocking, found {}",
                validate_envelope::PROBES.len() * validate_envelope::LEVELS.len(),
                plan.nonblocking.len()
            ));
        }
        // The build node must NOT be excused: it is the one gate in this profile.
        if plan.nonblocking.contains("envelope.build") {
            return Err("envelope CLI: the workspace build must stay BLOCKING".into());
        }
        // The measurement must never be answered from the tree-keyed cache: the
        // vector is an artifact consumers re-read, and with a baseline the
        // verdict depends on a file that is not part of the key.
        if plan.cacheable {
            return Err("envelope CLI: the envelope profile must NOT be cacheable".into());
        }
        accepted += 1;
    }
    // Negative: a missing FILE must be refused, not silently defaulted, and the
    // mode must not combine with a level, --all, or another focused mode.
    let mut refused = 0usize;
    for (why, v) in [
        ("--envelope-compare with no FILE", vec!["--envelope-compare"]),
        ("--envelope-only combined with a level", vec!["quick", "--envelope-only"]),
        ("--envelope-only combined with --all", vec!["--all", "--envelope-only"]),
        ("--envelope-only combined with another focused mode", vec!["--envelope-only", "--rr-compat-only"]),
    ] {
        if parse_argv(&argv(&v)).is_ok() {
            return Err(format!("envelope CLI: {why} must be REFUSED"));
        }
        refused += 1;
    }
    // Both spellings are ONE mode, so combining them is legal and the baseline
    // wins — this is the case validate.sh accepted (ENVELOPE_MODE=only twice).
    match parse_argv(&argv(&["--envelope-only", "--envelope-compare", "b.json"]))
        .map_err(|c| format!("envelope CLI: the two spellings must combine, got exit {c}"))?
        .focused
    {
        Some(Focused::Envelope { baseline: Some(_) }) => accepted += 1,
        other => return Err(format!("envelope CLI: combined spellings gave {other:?}")),
    }
    println!("  envelope CLI: {accepted} accepted form(s), {refused} refused misuse(s) (the \
              refusal messages above are expected)");
    Ok(())
}

// --------------------------------------------------------------------------- jobs

/// Default scheduler width, honoring the same runtime authority `validate.sh`
/// used (validate.sh:692-716) so both pick identical widths on the same host:
/// an explicit `CI_DAG_JOBS` is used EXACTLY (no clamp); otherwise the
/// host-adaptive `host_cpus/8`, floored at 2 and capped at 16.
///
/// The cap is measurement-backed, not a guess: on this 316-CPU box the portable
/// DAG measured CPU/wall ~2.6x at -j2 versus ~21.8x at -j16, and becomes
/// critical-path-bound near width 16. The same file also runs on GitHub's ~4-CPU
/// portable runner, where a flat 16 would schedule many multi-GiB nodes at once
/// and OOM a job that -j2 kept green.
fn default_jobs() -> i64 {
    if let Ok(v) = std::env::var("CI_DAG_JOBS") {
        if !v.is_empty() {
            if let Ok(n) = v.parse::<i64>() {
                if n > 0 {
                    return n;
                }
            }
            eprintln!("validate: CI_DAG_JOBS={v:?} is not a positive integer; using the host-adaptive default");
        }
    }
    let host = std::thread::available_parallelism().map(|n| n.get() as i64).unwrap_or(1);
    (host / 8).clamp(2, 16)
}

// --------------------------------------------------------------------------- boxing

/// How much longer than validate's own budget the scope may live.
///
/// The scope is only a backstop for the driver itself wedging. Validate needs
/// this later window to reap nodes and flush its rows, so it must not be the
/// level that normally fires. At the strict-compat 600s run budget this is 60s,
/// establishing the configured 600 < 660 portion of the nesting ladder.
fn scope_grace_s(run_timeout_s: i64) -> i64 {
    60.max(run_timeout_s / 10)
}

/// The wall ceiling every node must fit inside, DERIVED from the seconds left on
/// the run epoch rather than written beside the nominal budget.
///
/// The scheduler refuses any node whose declared wall is `>=` the budget it is
/// enforcing, and what it enforces is the REMAINDER, not the nominal figure. A
/// ceiling that is merely smaller than the nominal budget therefore inverts once
/// preparation has spent enough of the epoch. Keeping one grace band below the
/// remainder makes that inversion unreachable at any preparation time.
fn derived_wall_ceiling(remaining_s: i64) -> i64 {
    (remaining_s - scope_grace_s(remaining_s)).max(1)
}

fn owns_scope_request(deadline_ns: Option<u64>) -> bool {
    deadline_ns.is_some_and(|deadline| {
        std::env::var(OWN_SCOPE_DEADLINE_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            == Some(deadline)
    })
}

/// Establish two-level cgroup-v2 boxing, mirroring the runner's own
/// `resolve_cgroups` policy. Returns the manager (`None` = intentional unboxed
/// run) or `Err(exit_code)`. On the default path this re-execs into a transient
/// `systemd --user` scope and does not return on success.
fn resolve_cgroups(
    allow_failure: bool,
    run_timeout_s: Option<i64>,
    deadline_ns: Option<u64>,
) -> Result<BoxedCgroups, u8> {
    let owns_request = owns_scope_request(deadline_ns);
    if is_in_scope() && run_timeout_s.is_some() && !owns_request {
        eprintln!(
            "validate: inherited cgroup scope has no invocation-owned RuntimeMaxSec rung; \
             the anchored in-process deadline remains inside the enclosing DAG node limit"
        );
    }
    let scope_runtime_s = run_timeout_s.and_then(|run| {
        remaining_budget_s(deadline_ns).map(|remaining| remaining + scope_grace_s(run))
    });
    if !allow_failure {
        if let Some(deadline) = deadline_ns {
            std::env::set_var(OWN_SCOPE_DEADLINE_ENV, deadline.to_string());
        } else {
            std::env::remove_var(OWN_SCOPE_DEADLINE_ENV);
        }
    }
    safe_ci_scope::propagate_result(safe_ci_scope::resolve_cgroups(
        "validate",
        allow_failure,
        scope_runtime_s,
        owns_request,
    ))
}

// --------------------------------------------------------------------------- durable log

/// A live self-tee: everything written to fd 1/2 is duplicated into a durable
/// absolute log AND still shown on the terminal.
///
/// The receipt path must not depend on the launch path. A bare
/// `./scripts/validate.rs` with no `ci-hub validate-run` unit around it would
/// otherwise run, pass, and leave nothing on disk — indistinguishable from never
/// having run. Teeing here means the log exists whether the run came from
/// `validate-run`, `make validate`, or a bare invocation.
struct DurableLog {
    path: PathBuf,
    tee: std::process::Child,
    orig_stdout: i32,
    orig_stderr: i32,
}

impl DurableLog {
    fn finish(mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // Restoring fds 1/2 drops the last pipe write-ends, so tee sees EOF.
        unsafe {
            libc::dup2(self.orig_stdout, 1);
            libc::dup2(self.orig_stderr, 2);
            libc::close(self.orig_stdout);
            libc::close(self.orig_stderr);
        }
        let _ = self.tee.wait();
    }
}

/// Durable log path. Always ABSOLUTE — `verify_receipt.sh` (the merge gate)
/// requires the recorded path to start with `/`. Never under `HERMIT_DIR`: that
/// is a user-facing setting and validation must not write there.
fn durable_log_path(root: &Path, profile: &str, sha: &str) -> PathBuf {
    let dir = match std::env::var(PARENT_ENV) {
        Ok(p) if !p.is_empty() => PathBuf::from(p).join("ignored").join("validate"),
        _ => root.join("ignored").join("validate"),
    };
    let sha12: String = sha.chars().take(12).collect();
    let supplied = std::env::var("E2E_RUN_ID")
        .ok()
        .filter(|value| {
            !value.is_empty()
                && value
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._@:-".contains(c))
        });
    let run = supplied.unwrap_or_else(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        format!(
            "{}-{}-{nanos}",
            utc_now().replace([':', '-'], ""),
            std::process::id()
        )
    });
    durable_log_path_for_run(&dir, profile, &sha12, &run)
}

fn durable_log_path_for_run(dir: &Path, profile: &str, sha12: &str, run: &str) -> PathBuf {
    dir.join(format!("validate-{profile}-{sha12}-{run}.log"))
}

fn fallback_e2e_result_root(log_path: &Path, run: &std::ffi::OsStr) -> Result<PathBuf, String> {
    let log_dir = log_path
        .parent()
        .ok_or_else(|| format!("durable log has no parent: {}", log_path.display()))?;
    Ok(log_dir.join("e2e").join(run))
}

/// Give every real validate invocation its own durable E2E result directory.
///
/// `target/debug/test-harness` already emits one schema-4 row per cell, but its local
/// default is under the checkout. A canonical validate may run in a disposable
/// scratch tree, so those rows disappeared at cleanup. Deriving the fallback
/// from the durable log puts both artifacts under the same surviving root. A
/// caller such as ci-hub may still provide an explicit per-run location.
fn configure_e2e_result_root(
    root: &Path,
    log_path: &Path,
    temporary_build_root: &Path,
) -> Result<PathBuf, String> {
    let fallback_run = log_path
        .file_stem()
        .ok_or_else(|| format!("durable log has no file name: {}", log_path.display()))?
        .to_os_string();
    let run = std::env::var_os("E2E_RUN_ID")
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_run);
    let path = match std::env::var_os("E2E_RESULT_ROOT") {
        Some(value) if !value.is_empty() => {
            let supplied = PathBuf::from(value);
            if supplied.is_absolute() {
                supplied
            } else {
                root.join(supplied)
            }
        }
        _ => fallback_e2e_result_root(log_path, &run)?,
    };
    std::fs::create_dir_all(&path)
        .map_err(|e| format!("cannot create E2E result directory {}: {e}", path.display()))?;
    std::env::set_var("E2E_RESULT_ROOT", &path);
    // One full validate invokes the harness once per manifest bucket. Bind all
    // bucket rows to the durable validate identity instead of letting each
    // harness process mint a local timestamp. Schema-7 evidence is one complete
    // selected population, not a pool of unrelated bucket attempts.
    std::env::set_var("E2E_RUN_ID", &run);
    // The harness derives its prebuilt-fixture directory from RESULT_ROOT too,
    // but build products are not evidence and must not accumulate beside every
    // retained scorecard. Keep them in validate's ordinary disposable run
    // directory unless the caller deliberately supplied a build root.
    if std::env::var_os("E2E_BUILD_ROOT").is_none() {
        std::fs::create_dir_all(temporary_build_root).map_err(|e| {
            format!(
                "cannot create temporary E2E build directory {}: {e}",
                temporary_build_root.display()
            )
        })?;
        std::env::set_var("E2E_BUILD_ROOT", temporary_build_root);
    }
    Ok(path)
}

#[cfg(test)]
mod concurrent_validate_path_tests {
    use std::io::Write;
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn durable_outputs_reproduce_the_old_second_collision_and_separate_runs_now() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hermit-validate-output-collision-{}-{nanos}",
            std::process::id()
        ));
        let logs = root.join("ignored/validate");
        std::fs::create_dir_all(&logs).unwrap();
        let sha12 = "0123456789ab";

        // Removed behavior: second-resolution identity made both runs append to
        // one log and write one retained E2E tree.
        let old = logs.join(format!("validate-full-{sha12}-20260826T120000Z.log"));
        std::fs::write(&old, b"run-a\n").unwrap();
        let mut old_second = std::fs::OpenOptions::new().append(true).open(&old).unwrap();
        old_second.write_all(b"run-b\n").unwrap();
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "run-a\nrun-b\n");
        let old_e2e = logs.join("e2e/validate-full-0123456789ab-20260826T120000Z");
        std::fs::create_dir_all(&old_e2e).unwrap();
        std::fs::write(old_e2e.join("cell-results.jsonl"), b"run-a").unwrap();
        std::fs::write(old_e2e.join("cell-results.jsonl"), b"run-b").unwrap();
        assert_eq!(
            std::fs::read(old_e2e.join("cell-results.jsonl")).unwrap(),
            b"run-b"
        );

        let log_a = durable_log_path_for_run(&logs, "full", sha12, "validate-a");
        let log_b = durable_log_path_for_run(&logs, "full", sha12, "validate-b");
        assert_ne!(log_a, log_b);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_a)
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&log_b)
            .unwrap();
        assert!(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&log_a)
                .is_err(),
            "reusing one run identity must refuse rather than append"
        );
        let e2e_a = fallback_e2e_result_root(&log_a, std::ffi::OsStr::new("validate-a")).unwrap();
        let e2e_b = fallback_e2e_result_root(&log_b, std::ffi::OsStr::new("validate-b")).unwrap();
        assert_ne!(e2e_a, e2e_b);
        std::fs::create_dir_all(&e2e_a).unwrap();
        std::fs::create_dir_all(&e2e_b).unwrap();
        std::fs::write(e2e_a.join("cell-results.jsonl"), b"run-a").unwrap();
        std::fs::write(e2e_b.join("cell-results.jsonl"), b"run-b").unwrap();
        assert_eq!(
            std::fs::read(e2e_a.join("cell-results.jsonl")).unwrap(),
            b"run-a"
        );
        assert_eq!(
            std::fs::read(e2e_b.join("cell-results.jsonl")).unwrap(),
            b"run-b"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unavailable_checkout_lock_refuses_before_shared_target_output_is_written() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "hermit-validate-unavailable-lock-{}-{nanos}",
            std::process::id()
        ));
        let shared = root.join("old-target/shared-output");
        std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
        let (first_written_tx, first_written_rx) = mpsc::sync_channel(0);
        let (second_written_tx, second_written_rx) = mpsc::sync_channel(0);
        let first_path = shared.clone();
        let first = std::thread::spawn(move || {
            std::fs::write(first_path, b"run-a").unwrap();
            first_written_tx.send(()).unwrap();
            second_written_rx.recv().unwrap();
        });
        let second_path = shared.clone();
        let second = std::thread::spawn(move || {
            first_written_rx.recv().unwrap();
            std::fs::write(second_path, b"run-b").unwrap();
            second_written_tx.send(()).unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(
            std::fs::read(&shared).unwrap(),
            b"run-b",
            "the removed fail-open path allowed the second run to replace the first run's target output"
        );

        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join("target"), b"not a directory").unwrap();
        let error = match validate_runtime::acquire_invocation_lock(&checkout, "full", "abc") {
            validate_runtime::LockOutcome::Unavailable(error) => error,
            _ => panic!("an unusable target path must make the checkout lock unavailable"),
        };
        let summary = unavailable_invocation_lock_summary("full", error);
        assert_eq!(summary.verdict, Verdict::Refused);
        assert!(summary
            .detail
            .iter()
            .any(|line| line.contains("refusing rather than running two validates")));
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// Send all completed cells from one validate invocation to the parent series
/// writer as one batch. The harness appends retries to each bucket's existing
/// `results.jsonl`, so this reads the same durable attempt records used by the
/// terminal-verdict projection instead of maintaining another result file.
fn append_validate_series(
    parent: Option<&Path>,
    checkout: &Path,
    result_root: &Path,
    tree: &str,
) -> Result<bool, String> {
    let Some(parent) = parent else {
        return Ok(false);
    };
    let rows = validate_cell_results::all_result_rows(result_root)?;
    if rows.is_empty() {
        return Ok(false);
    }
    let run_id = std::env::var_os("E2E_RUN_ID")
        .filter(|value| !value.is_empty())
        .ok_or("E2E_RUN_ID is missing after completed cell rows were recorded")?;
    let script = parent.join("ci-hub/series/series.py");
    if !script.is_file() {
        return Err(format!(
            "{} does not exist; DEV_HERMIT_PARENT does not contain the series writer",
            script.display()
        ));
    }
    let mut child = Command::new("python3")
        .arg(&script)
        .arg("append-cells")
        .arg("--parent")
        .arg(parent)
        .arg("--checkout")
        .arg(checkout)
        .arg("--producer")
        .arg("validate")
        .arg("--run-id")
        .arg(&run_id)
        .arg("--tree")
        .arg(tree)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {}: {error}", script.display()))?;
    {
        use std::io::Write;
        let input = child
            .stdin
            .as_mut()
            .ok_or_else(|| format!("{} has no writable stdin", script.display()))?;
        for row in &rows {
            serde_json::to_writer(&mut *input, row)
                .map_err(|error| format!("cannot encode retained cell row: {error}"))?;
            input
                .write_all(b"\n")
                .map_err(|error| format!("cannot send retained cell row: {error}"))?;
        }
    }
    drop(child.stdin.take());
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot wait for {}: {error}", script.display()))?;
    if !output.status.success() {
        return Err(format!(
            "series writer refused {} retained cell row(s) from {}: {}",
            rows.len(),
            result_root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    eprintln!(
        "validate: per-cell series updated from {} retained row(s) under {}: {}",
        rows.len(),
        result_root.display(),
        String::from_utf8_lossy(&output.stdout).trim()
    );
    Ok(true)
}

/// Establish the self-tee. FAIL-CLOSED: any failure exits loudly rather than
/// running without a durable receipt. Must be called AFTER `resolve_cgroups`
/// (which re-execs), so the tee is set up once, in the final boxed process.
fn setup_durable_log(root: &Path, profile: &str, sha: &str) -> Result<DurableLog, u8> {
    use std::os::unix::io::AsRawFd;
    let path = durable_log_path(root, profile, sha);
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "validate: ERROR: cannot create durable-log dir {}: {e}. A run with no durable \
                 receipt is a silent no-result; refusing to proceed.",
                dir.display()
            );
            return Err(4);
        }
    }
    if let Err(e) = std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        eprintln!(
            "validate: ERROR: cannot reserve durable log {}: {e}. Refusing to append two runs to one path.",
            path.display()
        );
        return Err(4);
    }
    let mut tee = match Command::new("tee")
        .arg("-a")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "validate: ERROR: cannot spawn `tee` for {}: {e}. Refusing to run without a \
                 durable receipt.",
                path.display()
            );
            return Err(4);
        }
    };
    let (orig_stdout, orig_stderr, ok) = unsafe {
        let so = libc::dup(1);
        let se = libc::dup(2);
        let pipe_fd = tee.stdin.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
        let ok = so >= 0
            && se >= 0
            && pipe_fd >= 0
            && libc::dup2(pipe_fd, 1) >= 0
            && libc::dup2(pipe_fd, 2) >= 0;
        (so, se, ok)
    };
    if !ok {
        eprintln!("validate: ERROR: could not redirect stdout/stderr into the durable log.");
        let _ = tee.kill();
        return Err(4);
    }
    drop(tee.stdin.take());
    eprintln!("validate: durable log: {}", path.display());
    Ok(DurableLog { path, tee, orig_stdout, orig_stderr })
}

// --------------------------------------------------------------------------- git / host

fn sh(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn git_sha() -> String {
    sh("git", &["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into())
}

fn parse_git_depth(raw: &str) -> Result<u64, String> {
    let depth = raw
        .trim()
        .parse::<u64>()
        .map_err(|error| format!("git rev-list returned a non-integer depth {raw:?}: {error}"))?;
    if depth == 0 {
        return Err("git rev-list returned zero depth for a commit".into());
    }
    Ok(depth)
}

/// Measure the exact quantity carried by the historical `git_depth` field.
/// Failure is a refusal, never a fabricated zero or an omitted JSON key.
fn measure_git_depth(commit: &str) -> Result<u64, String> {
    let output = Command::new("git")
        .args(["rev-list", "--count", commit])
        .output()
        .map_err(|error| format!("cannot execute git rev-list --count {commit}: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!(
            "git rev-list --count {commit} failed with {}{}",
            output.status,
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        ));
    }
    parse_git_depth(&String::from_utf8_lossy(&output.stdout))
}

/// Content-addressed identity of exactly what validate builds and tests: the root
/// tree object. It hashes tracked file content AND submodule gitlink SHAs, but not
/// commit metadata — so a rebase or amend that leaves content byte-identical
/// yields the SAME tree. This, not the commit SHA, is the result-cache key.
fn git_tree() -> String {
    sh("git", &["rev-parse", "HEAD^{tree}"]).unwrap_or_else(|| "unknown".into())
}

fn repo_root() -> PathBuf {
    sh("git", &["rev-parse", "--show-toplevel"])
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Paths excluded from every dirtiness and anchoring judgement.
///
/// The ledger shard lives IN the repository, and validate is what writes it. If
/// it counted as dirt, validate would poison the very tree it just judged: the
/// next run would refuse on a dirty tree, and the tree hash — the result-cache
/// key — would change after every run, so a cache could never hit. Validate's own
/// output is not a source change, so it is excluded here rather than being
/// gitignored (the shards are meant to be committed and unioned across machines).
const SELF_OUTPUT_PREFIXES: &[&str] = &[LEDGER_DIR, "ignored/"];

/// True when `path` is inside (or equal to) one of validate's own output roots.
///
/// The match is on a PATH BOUNDARY, not a raw string prefix. A bare
/// `starts_with("ci/validate-ledger")` also swallowed siblings such as
/// `ci/validate-ledger-notes.md`, which would have been silently excused from the
/// dirty gate — the opposite of the failure it is meant to prevent, and exactly
/// the kind of "correlated proxy" match this driver is supposed to avoid.
fn is_self_output(path: &str) -> bool {
    SELF_OUTPUT_PREFIXES.iter().any(|p| {
        let root = p.trim_end_matches('/');
        path == root || path.starts_with(&format!("{root}/"))
    })
}

/// Every path a git listing line could be referring to.
///
/// The callers emit two different shapes — `git status --porcelain` prefixes each
/// path with a two-character status plus a space, while `git diff --name-only`
/// and `git ls-files` emit a bare path — and a rename line carries two paths.
/// Rather than guess which caller produced a line, every plausible reading is
/// derived and the classification asks whether ALL of them are validate's own
/// output.
///
/// **Do not reintroduce a fixed-offset strip.** Two bugs have now come from one:
/// stripping three characters unconditionally broke the bare-path callers
/// (turning `ci/validate-ledger/…` into `validate-ledger/…`), and the fix for
/// that still relied on the porcelain line keeping its leading status column —
/// which `sh()` trimmed off the FIRST line of the output. The measured effect of
/// the second bug: after any run, `git status --porcelain` returned exactly one
/// line, ` M ci/validate-ledger/<shard>.jsonl`, whose leading space `sh()` ate;
/// the 3-char strip then produced `i/validate-ledger/…`, no reading matched, and
/// `tree_dirty()` reported TRUE. Every subsequent ledger row was written with
/// `commit_anchored: false`, so the tree-keyed cache could never hit and a
/// receipt-backed label could never be published — both features inert, silently.
fn path_readings(line: &str) -> Vec<String> {
    let unquote = |s: &str| s.trim().trim_matches('"').to_string();
    let mut out = vec![unquote(line)];
    if let Some(rest) = porcelain_payload(line) {
        out.push(unquote(rest));
    }
    // Belt and braces for the exact bug this replaced: a porcelain line whose
    // leading status column was eaten by a trim reads as `M <path>`. Reading it
    // costs nothing (an extra reading can only WIDEN "self output", and the two
    // prefixes are specific paths) and it means a future accidental trim
    // degrades to "still classified correctly" instead of "cache silently off".
    const CODES: &[u8] = b"MADRCUT?!";
    let b = line.as_bytes();
    if b.len() > 2 && b[1] == b' ' && CODES.contains(&b[0]) {
        out.push(unquote(&line[2..]));
    }
    out
}

/// If `line` has a `git status --porcelain` `XY ` prefix, the text after it.
///
/// Both status characters are checked against git's actual code set rather than
/// just testing for a space at index 2, so an ordinary path that happens to
/// contain a space in its third position is not mistaken for a status prefix.
fn porcelain_payload(line: &str) -> Option<&str> {
    const CODES: &[u8] = b" MADRCUT?!";
    let b = line.as_bytes();
    if b.len() > 3 && b[2] == b' ' && CODES.contains(&b[0]) && CODES.contains(&b[1]) {
        Some(&line[3..])
    } else {
        None
    }
}

/// True when this listing line describes only validate's own output.
///
/// A rename (`R  old -> new`) counts as self-output only when BOTH sides are:
/// moving a source file INTO the ledger directory is a real change and must not
/// be excused.
fn line_is_self_output(line: &str) -> bool {
    let payload: &str = porcelain_payload(line).unwrap_or(line);
    if let Some((from, to)) = payload.split_once(" -> ") {
        let clean = |s: &str| s.trim().trim_matches('"').to_string();
        return is_self_output(&clean(from)) && is_self_output(&clean(to));
    }
    path_readings(line).iter().any(|p| is_self_output(p))
}

/// Entries from a git listing that are not validate's own output.
///
/// Reads git's stdout UNTRIMMED, because `git status --porcelain`'s leading
/// status column is significant and a global trim silently shifts the first
/// line's columns (see [`path_readings`]).
fn foreign_porcelain(args: &[&str]) -> Vec<String> {
    let Ok(out) = Command::new("git").args(args).output() else { return Vec::new() };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !line_is_self_output(l))
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// True when the tree differs from HEAD in any way validate did not itself cause.
fn tree_dirty() -> bool {
    !foreign_porcelain(&["status", "--porcelain"]).is_empty()
}

/// True when the WORKING TREE proper carries changes `git add` would capture.
/// This drives the hard gate, because staging or committing is the caller's
/// escape from it.
fn worktree_dirty() -> bool {
    let unstaged = !foreign_porcelain(&["diff", "--name-only"]).is_empty();
    unstaged || !foreign_porcelain(&["ls-files", "--others", "--exclude-standard"]).is_empty()
}

fn utc_now() -> String {
    sh("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".into())
}

fn epoch_now() -> i64 {
    sh("date", &["+%s"]).and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn has_cmd(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Locate the dev-hermit parent by walking up for a `.gitmodules` whose `hermit`
/// submodule path is `hermit` (validate.sh:19).
fn find_parent(root: &Path) -> Option<PathBuf> {
    let mut cur = root.to_path_buf();
    loop {
        if cur.join(".gitmodules").is_file() {
            if let Some(p) = sh(
                "git",
                &[
                    "-C",
                    cur.to_str()?,
                    "config",
                    "-f",
                    ".gitmodules",
                    "--get",
                    "submodule.hermit.path",
                ],
            ) {
                if p == "hermit" {
                    return Some(cur);
                }
            }
        }
        if !cur.pop() || cur.as_os_str().is_empty() {
            return None;
        }
    }
}

/// Whether this invocation is real product work in a dev-hermit workspace and
/// therefore needs canonical ci-hub admission.
///
/// A `.gitmodules` entry alone can describe a generic Hermit superproject. The
/// ci-hub directory is the dev-hermit boundary; once that boundary exists, a
/// missing or broken launcher is non-authorizing rather than an escape.
fn product_front_door_applies(
    parent_detected: bool,
    ci_hub_dir_present: bool,
    _nested: bool,
    show_plan: bool,
) -> bool {
    parent_detected && ci_hub_dir_present && !show_plan
}

/// Construct the refusal for an unadmitted product run. Production supplies
/// `canonically_admitted` only from [`canonical_validate_lock_admission`].
fn product_front_door_refusal(
    parent: &Path,
    root: &Path,
    commit: &str,
    requested_args: &str,
    ci_hub_launcher_available: bool,
    canonically_admitted: bool,
) -> Option<String> {
    if canonically_admitted {
        return None;
    }
    let ci_hub_path = parent.join("ci-hub/ci-hub");
    let ci_hub = validate_plan::shell_quote(&ci_hub_path.to_string_lossy());
    let checkout = validate_plan::shell_quote(&root.to_string_lossy());
    let remediation = if ci_hub_launcher_available {
        format!(
            "Run through the canonical launcher instead:\n\n  {ci_hub} validate-run --checkout \
             {checkout} --agent '<registered-agent-name>' --target {commit} -- {requested_args}"
        )
    } else {
        format!(
            "The canonical ci-hub launcher is unavailable at {ci_hub}. Repair or sync the parent \
             checkout, then invoke validation through ci-hub; do not run this driver directly."
        )
    };
    Some(format!(
        "validate: REFUSED — product validation inside dev-hermit must enter through canonical \
         ci-hub admission.\n\
         A naked run from {checkout} can consume the validation box without exact commit, host, \
         and live lock-owner ancestry authority, and can write an unadmitted ledger row.\n\
         \n\
         {remediation}"
    ))
}

/// Reproduce the caller's validated argv after ci-hub's `--` separator. An
/// empty argv means the driver's default full profile.
fn requested_validate_args() -> String {
    let args = std::env::args()
        .skip(1)
        .map(|arg| validate_plan::shell_quote(&arg))
        .collect::<Vec<_>>();
    if args.is_empty() {
        "full".into()
    } else {
        args.join(" ")
    }
}

/// `validation_slot_name` (validate.sh:37): which worktree slot this checkout is.
fn slot_name(root: &Path, parent: Option<&Path>) -> String {
    let Some(parent) = parent else { return "standalone".into() };
    let Ok(rel) = root.strip_prefix(parent) else { return "standalone".into() };
    let rel = rel.to_string_lossy();
    if rel == "hermit" {
        return "primary".into();
    }
    if let Some(rest) = rel.strip_prefix("worktrees/") {
        if let Some((slot, _)) = rest.split_once('/') {
            return slot.to_string();
        }
    }
    "standalone".into()
}

/// Classify the build-cache state BEFORE anything is built. Warm vs cold target/
/// dominates wall time, so the estimate and the ledger both record it.
fn cache_state(root: &Path) -> &'static str {
    let debug = root.join("target/debug/hermit").exists();
    let release = root.join("target/release/hermit").exists();
    match (debug, release) {
        (true, true) => "warm",
        (true, false) | (false, true) => "partial",
        (false, false) => "cold",
    }
}

// --------------------------------------------------------------------------- rebase freshness

/// Refuse to validate a head that is behind its upstream.
///
/// Owner directive: "ALWAYS rebase before validate; admission control should
/// ERROR if the base is out of date." The reason is not tidiness — a receipt is
/// keyed to a SHA, and while a stale head waits, `main` advances and the receipt
/// stops describing anything landable. Validating a stale base spends the
/// box-exclusive validate slot producing evidence that is already invalid.
///
/// Only ERRORS when the local `origin/main` ref genuinely contains commits this
/// head lacks. It does NOT fetch (that would make an offline run fail for a
/// network reason) and it does not fire when the ref is absent — an unknown base
/// is reported as unknown, never silently treated as fresh.
fn rebase_freshness(force: bool) -> Result<String, String> {
    if sh("git", &["rev-parse", "--verify", "--quiet", "refs/remotes/origin/main"]).is_none() {
        return Ok("base: origin/main not present locally; freshness UNKNOWN (not asserted)".into());
    }
    let counts = sh("git", &["rev-list", "--left-right", "--count", "origin/main...HEAD"])
        .unwrap_or_else(|| "0\t0".into());
    let mut it = counts.split_whitespace();
    let behind: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let ahead: i64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    if behind == 0 {
        return Ok(format!("base: up to date with origin/main (ahead {ahead}, behind 0)"));
    }
    let msg = format!(
        "HEAD is {behind} commit(s) BEHIND origin/main (ahead {ahead}).\n  \
         A receipt minted here is keyed to a SHA that main has already moved past, so it cannot \
         authorize a landing and will have to be rebuilt after the rebase it is missing.\n  \
         Rebase first:  git rebase origin/main\n  \
         To skip only scripts/validate.rs's dirty-working-tree and rebase-freshness checks, pass \
         --skip-inner-dirty-working-tree-and-rebase-freshness-checks. This does not bypass \
         ci-hub validate-lock admission."
    );
    if force {
        Ok(format!("base: STALE, {behind} behind origin/main — forced past the freshness gate"))
    } else {
        Err(msg)
    }
}

// --------------------------------------------------------------------------- plan

/// What the driver will execute, plus the accounting the ledger needs.
struct Plan {
    cfg: DagConfig,
    /// Second DAG run for a two-lane profile when lanes are NOT fused. Keeping
    /// them sequential is the faithful reproduction of `run_full_suite`, which
    /// runs `run_ci_manifest_lane portable` then `... privileged`.
    second: Option<DagConfig>,
    profile: String,
    selection_mode: &'static str,
    /// `test.*` nodes the profile PLANNED to run, for the coverage record.
    #[allow(dead_code)]
    planned_test_nodes: BTreeSet<String>,
    /// Set when this profile is a compatibility matrix, so the ratchet and the
    /// per-program summary are evaluated afterwards.
    compat: Option<CompatMode>,
    /// True only for a complete `full` plan, authorizing `gates_expected` to be
    /// derived from what ran (validate.sh:718).
    suite_complete: bool,
    /// True for the `super` stress suite, so its pass-rate table is printed and
    /// its verdict comes from the ratchet rather than the raw node count.
    super_mode: bool,
    /// Set for `--envelope-only`/`--envelope-compare`: the measurement is scored
    /// and emitted afterwards, and an optional baseline is enforced.
    envelope: Option<EnvelopePlan>,
    /// Tags whose failure must NOT turn the run red. This is how a MEASUREMENT
    /// (envelope probes) and a NEVER-BEFORE-MEASURED row (KVM/DBI stress) are
    /// kept out of the blocking verdict without hiding them from the report.
    /// Every member is named in the summary with the reason it is nonblocking.
    nonblocking: BTreeSet<String>,
    /// Forced on for the envelope profile, whose whole point is to measure every
    /// probe: an eager exit on the first probe failure would truncate the vector.
    force_keep_going: bool,
    /// Nodes withheld because this MACHINE provably cannot run them. Neither a
    /// pass nor a failure: each is reported by name and written to the ledger as
    /// a typed intentional skip whose reason is `host-inapplicable`, which the
    /// parent's separately-reviewed consumer allowlist does not admit, so a run
    /// carrying one does not qualify as landing authority.
    host_inapplicable: Vec<validate_plan::HostInapplicableNode>,
    /// May a prior passing record for this tree be reused instead of running?
    ///
    /// The tree-keyed cache is only sound when the run is a pure function of the
    /// tree. The envelope profile is neither: its verdict under
    /// `--envelope-compare FILE` depends on a BASELINE FILE that is not part of
    /// the key, and its purpose under `--envelope-only` is to (re)produce the
    /// `envelope.json` ARTIFACT that `scripts/progress-report.sh` then reads — a
    /// cache hit would answer a monotonicity question it never asked and leave
    /// the artifact unwritten. `validate.sh` cached it anyway (its cache gate at
    /// :655 runs before the `ENVELOPE_MODE` dispatch at :4877, with
    /// `VALIDATION_PROFILE=envelope-only`); that is a bug, not a contract.
    cacheable: bool,
    /// Exact selected population for a targeted schema-7 evidence row. This is
    /// deliberately separate from `suite_complete`: it may satisfy one open
    /// cell obligation but can never authorize a whole-run landing receipt.
    cell_evidence_expected: Option<Vec<serde_json::Value>>,
}

struct EnvelopePlan {
    reps: i64,
    baseline: Option<PathBuf>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            cfg: DagConfig::default(),
            second: None,
            profile: String::new(),
            selection_mode: "full",
            planned_test_nodes: BTreeSet::new(),
            compat: None,
            suite_complete: false,
            super_mode: false,
            envelope: None,
            nonblocking: BTreeSet::new(),
            force_keep_going: false,
            host_inapplicable: Vec::new(),
            cacheable: true,
            cell_evidence_expected: None,
        }
    }
}

/// Withhold every planned node this MACHINE provably cannot run, and say so.
///
/// Applied AFTER the plan is fully assembled — lane fusion, dedup and scorecard
/// attachment have already happened — so it matches the exact tags the runner
/// will see. Withholding is the only effect: nothing here can turn a node's
/// FAILURE into anything else, because a node that is not withheld runs and is
/// judged exactly as before.
///
/// An unknown capability name, or a retained node depending on a withheld one,
/// is an error that REFUSES the run. Substituting a different node set under the
/// requested profile name would be worse than refusing.
fn withhold_host_inapplicable(root: &Path, plan: &mut Plan) -> Result<(), String> {
    let requirements = validate_plan::host_capability_requirements(root)?;
    if requirements.is_empty() {
        return Ok(());
    }
    // Probe only what this plan actually needs, once per capability.
    let mut needed: BTreeSet<validate_plan::HostCapability> = BTreeSet::new();
    for cfg in std::iter::once(&plan.cfg).chain(plan.second.iter()) {
        for step in &cfg.steps {
            if let Some(capability) = requirements.get(&step.tag()) {
                needed.insert(*capability);
            }
        }
    }
    let mut absent: BTreeMap<validate_plan::HostCapability, String> = BTreeMap::new();
    for capability in needed {
        let verdict = validate_plan::probe_host_capability(capability);
        // Print PRESENT verdicts too: a reader must be able to see that the
        // question was asked and how it was answered, not just its consequences.
        println!(
            "Host capability {}: {} — {}",
            verdict.capability.value(),
            if verdict.present { "PRESENT" } else { "ABSENT" },
            verdict.evidence
        );
        if !verdict.present {
            absent.insert(capability, verdict.evidence);
        }
    }
    if absent.is_empty() {
        return Ok(());
    }
    let mut withheld = Vec::new();
    let mut apply = |cfg: &mut DagConfig| -> Result<(), String> {
        let steps = std::mem::take(&mut cfg.steps);
        let (keep, gone) =
            validate_plan::partition_host_inapplicable(steps, &requirements, &absent)?;
        cfg.steps = keep;
        withheld.extend(gone);
        Ok(())
    };
    apply(&mut plan.cfg)?;
    if let Some(second) = plan.second.as_mut() {
        apply(second)?;
    }
    plan.host_inapplicable = withheld;
    // A node can also lose its whole reason to exist WITHOUT declaring anything,
    // when every manifest cell it would run is withheld. That case is computed
    // from the live cell population, never declared; see
    // [`withhold_vacuous_manifest_nodes`].
    withhold_vacuous_manifest_nodes(root, plan, &absent)?;
    for node in &plan.host_inapplicable {
        println!(
            "HOST-INAPPLICABLE: {} will NOT RUN — this machine lacks {} ({}). This is NOT a pass \
             and carries NO coverage for what that node verifies; it is recorded in the ledger as \
             an intentional skip with reason '{}'.",
            node.tag,
            node.capability.value(),
            node.evidence,
            validate_plan::HOST_INAPPLICABLE_REASON
        );
    }
    Ok(())
}

// ------------------------------------------- a node whose whole bucket is gone
//
// hermit#2212 withholds a node that DECLARES a capability this machine lacks.
// hermit#2214 withholds a manifest CELL whose own `requires` declaration names
// one. Between them sits the case neither covers: a DAG node that declares
// nothing itself, but whose entire cell population is withheld at cell level, so
// it would spawn and have nothing at all to run. `target/debug/test-harness` refuses
// that with its vacuity guard — correctly, because `0/0` is not a passing
// population — which leaves the run incomplete rather than recorded.
//
// This is the third case, and it is deliberately the NARROWEST of the three.
//
// WHY IT CANNOT GENERALIZE INTO "SKIP ANYTHING INCONVENIENT":
//
//  1. It is COMPUTED, NEVER DECLARED. There is no list of withholdable nodes
//     anywhere; a node is withheld only when the live cell population it would
//     run is non-empty and every one of those cells is withheld. Adding ONE
//     runnable cell to the bucket un-withholds the node on the next run with no
//     code change, which is exactly the silent-swallow failure a hard-coded node
//     list would rot into.
//  2. IT ADDS NO NEW REASON TO WITHHOLD ANYTHING. Every input is already
//     established: the cell-level withholding of hermit#2214 (closed `requires`
//     vocabulary, one probeable token) and the probe of hermit#2212 (two
//     corroborating sources, absence only). This layer computes a conjunction
//     over decisions already made; it cannot withhold a cell that would
//     otherwise have run, so it cannot enlarge what is omitted by even one cell.
//  3. IT STILL NEVER READS THE NODE. Its inputs are the node's own command line,
//     the manifests, and that probe. No exit code, stderr, timeout or panic can
//     reach it, and the decision is taken before anything spawns.
//  4. AN EMPTY BUCKET IS NOT THIS CASE. `selected == 0` is the pre-existing
//     `empty-manifest-bucket` condition and is explicitly excluded, so this
//     mechanism can never absorb a bucket that simply has no cells.
//  5. IT FAILS CLOSED TOWARD RUNNING at every step. A command it cannot fully
//     model, a bucket missing from the audited required plan, malformed plan
//     metadata, or an uncertain capability probe all leave the node
//     RUNNING — where the harness's own vacuity guard still refuses a vacuous
//     pass.
//  6. THE DENOMINATOR STILL GOES UP. A node withheld here goes into
//     `plan.host_inapplicable` exactly like a declared one: added back into
//     `gates_expected`, named in the plan header, the cost table, the verdict
//     detail and the ledger row, and never written into `gates[]`.

/// One manifest bucket's cell accounting, exactly as `target/debug/test-harness`
/// counts it for the run that bucket's node would perform.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BucketCells {
    lane: String,
    category: String,
    /// Cells the bucket's node would select: one per (test, mode, backend).
    selected: usize,
    /// How many of those the harness would withhold as host-inapplicable.
    withheld: usize,
    /// Which capabilities did the withholding, sorted and deduplicated.
    capabilities: Vec<String>,
}

/// Would this bucket's node have NOTHING to run?
///
/// PURE, and the whole decision. `selected > 0` is load-bearing twice over: a
/// bucket with no cells at all is the pre-existing `empty-manifest-bucket`
/// condition and must not be absorbed here, and `0 == 0` would otherwise make
/// every empty bucket read as host-inapplicable — the exact vacuous accounting
/// this line of work exists to refuse.
///
/// `withheld == selected` rather than `withheld > 0`: one withheld cell in a
/// bucket that still has runnable cells leaves the node running, with the
/// withheld cell recorded by the harness. That is what makes adding a runnable
/// cell back un-withhold the node automatically.
fn bucket_runs_nothing(bucket: &BucketCells) -> bool {
    bucket.selected > 0 && bucket.withheld == bucket.selected
}

/// The `(lane, category)` a manifest bucket node runs, read off the node's OWN
/// command — the thing that actually selects the cells.
///
/// `None` when the command is not a manifest bucket run, when either output path
/// is missing or duplicated, or when it carries any token this function does not
/// model. THE WHITELIST IS THE POINT: `--results` and `--junit` are accepted only
/// as one value-bearing pair because they change storage, never selection. An
/// unmodelled or selection-affecting `--mode`, `--backend`, `--test`,
/// `--include-occasional`, or anything else means the cell set cannot be proven
/// equal to the bucket accounting, so the node is not a candidate and simply runs.
///
/// `--ci-only` is REQUIRED because the accounting is queried with `--ci-only`;
/// a node selecting a wider population must not be matched against a narrower
/// count.
fn manifest_bucket_of(cmd: &str) -> Option<(String, String)> {
    let tail = cmd.split_once("target/debug/test-harness run ")?.1;
    let tokens: Vec<&str> = tail.split_whitespace().collect();
    let mut lane: Option<String> = None;
    let mut category: Option<String> = None;
    let mut results = false;
    let mut junit = false;
    let mut ci_only = false;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i] {
            "--lane" => {
                if lane.is_some() {
                    return None;
                }
                lane = Some((*tokens.get(i + 1)?).to_string());
                i += 2;
            }
            "--category" => {
                if category.is_some() {
                    return None;
                }
                category = Some((*tokens.get(i + 1)?).to_string());
                i += 2;
            }
            "--results" => {
                if results || tokens.get(i + 1)?.starts_with("--") {
                    return None;
                }
                results = true;
                i += 2;
            }
            "--junit" => {
                if junit || tokens.get(i + 1)?.starts_with("--") {
                    return None;
                }
                junit = true;
                i += 2;
            }
            "--ci-only" => {
                if ci_only {
                    return None;
                }
                ci_only = true;
                i += 1;
            }
            // Tokens that change nothing about WHICH cells are selected.
            "--allow-empty" | "--prebuilt" => i += 1,
            // Anything else: unmodelled, so unproven, so not a candidate.
            _ => return None,
        }
    }
    if !ci_only || !results || !junit {
        return None;
    }
    Some((lane?, category?))
}


/// Read the exact checked-in required cell population and aggregate it by
/// manifest bucket for the already-resolved absent capabilities.
///
/// `test-harness audit-ci` regenerates this shape from the live YAML manifests
/// and compares it by normalized rows. Keeping the capability on the required
/// cell row means plan construction does not compile or run a second validation
/// driver before dagrun starts. A stale file still fails the mandatory manifest
/// gate, so it can never qualify a receipt.
fn read_bucket_cells(
    root: &Path,
    absent: &BTreeMap<validate_plan::HostCapability, String>,
) -> Result<Vec<BucketCells>, String> {
    let path = root.join("ci/expected-e2e-plan.json");
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
    )
    .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    let cells = document
        .get("cells")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{} has no cells array", path.display()))?;
    let mut buckets: BTreeMap<(String, String), BucketCells> = BTreeMap::new();
    for cell in cells {
        let lane = cell
            .get("lane")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{} contains a cell without a lane", path.display()))?;
        let category = cell
            .get("category")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("{} contains a cell without a category", path.display()))?;
        let bucket = buckets
            .entry((lane.to_string(), category.to_string()))
            .or_insert_with(|| BucketCells {
                lane: lane.to_string(),
                category: category.to_string(),
                selected: 0,
                withheld: 0,
                capabilities: Vec::new(),
            });
        bucket.selected += 1;
        let mut cell_absent = BTreeSet::new();
        if let Some(values) = cell.get("requires_host_capabilities") {
            let values = values.as_array().ok_or_else(|| {
                format!(
                    "{} contains a non-array requires_host_capabilities field",
                    path.display()
                )
            })?;
            for value in values {
                let name = value.as_str().ok_or_else(|| {
                    format!("{} contains a non-string host capability", path.display())
                })?;
                let capability = validate_plan::HostCapability::from_value(name).ok_or_else(|| {
                    format!(
                        "{} contains unknown host capability {name:?}",
                        path.display()
                    )
                })?;
                if absent.contains_key(&capability) {
                    cell_absent.insert(name.to_string());
                }
            }
        }
        if !cell_absent.is_empty() {
            bucket.withheld += 1;
            bucket.capabilities.extend(cell_absent);
        }
    }
    let mut out = buckets.into_values().collect::<Vec<_>>();
    for bucket in &mut out {
        bucket.capabilities.sort();
        bucket.capabilities.dedup();
    }
    Ok(out)
}

/// Withhold every planned manifest bucket node whose entire cell population is
/// withheld, and say so.
///
/// See the section comment above for why this cannot generalize. Called only
/// when at least one capability is ABSENT, so a machine that has everything
/// pays nothing and emits byte-identical output.
fn withhold_vacuous_manifest_nodes(
    root: &Path,
    plan: &mut Plan,
    absent: &BTreeMap<validate_plan::HostCapability, String>,
) -> Result<(), String> {
    let mut candidates: Vec<(String, String, String)> = Vec::new();
    for cfg in std::iter::once(&plan.cfg).chain(plan.second.iter()) {
        for step in &cfg.steps {
            if let Some((lane, category)) = manifest_bucket_of(&step.cmd) {
                candidates.push((step.tag(), lane, category));
            }
        }
    }
    if candidates.is_empty() {
        return Ok(());
    }
    let buckets = match read_bucket_cells(root, absent) {
        Ok(buckets) => buckets,
        Err(why) => {
            // FAIL CLOSED TOWARD RUNNING. Without the accounting there is no
            // proof that a bucket is empty of runnable cells, and an unproven
            // omission is worse than a node that runs and refuses itself.
            println!(
                "Host-inapplicable bucket accounting UNAVAILABLE ({why}); NO node was withheld \
                 and every planned manifest bucket node will run."
            );
            return Ok(());
        }
    };
    let by_bucket: BTreeMap<(&str, &str), &BucketCells> = buckets
        .iter()
        .map(|b| ((b.lane.as_str(), b.category.as_str()), b))
        .collect();

    let mut withheld: Vec<validate_plan::HostInapplicableNode> = Vec::new();
    for (tag, lane, category) in &candidates {
        // A bucket with no row selected NO cells at all. That is
        // `empty-manifest-bucket`, not host-inapplicable, and is left alone.
        let Some(bucket) = by_bucket.get(&(lane.as_str(), category.as_str())) else {
            continue;
        };
        if !bucket_runs_nothing(bucket) {
            continue;
        }
        // One typed record needs one capability. More than one means a second
        // probeable token was added without extending this record, so REFUSE
        // rather than pick: refusing is never the bar-lowering direction, and
        // this is unreachable while exactly one token has an absence proof.
        if bucket.capabilities.len() != 1 {
            return Err(format!(
                "manifest bucket {lane}/{category} has every cell withheld by {} capabilities \
                 ({}), and one host-inapplicable record names exactly one; extend the record \
                 before adding a second probeable `requires` token",
                bucket.capabilities.len(),
                bucket.capabilities.join(", ")
            ));
        }
        let name = &bucket.capabilities[0];
        let Some(capability) = validate_plan::HostCapability::from_value(name) else {
            return Err(format!(
                "manifest bucket {lane}/{category} was withheld by capability '{name}', which \
                 the driver's closed vocabulary does not know"
            ));
        };
        let evidence = absent.get(&capability).ok_or_else(|| {
            format!(
                "manifest bucket {lane}/{category} named capability '{name}' without an absent \
                 verdict; refusing inconsistent host-capability accounting"
            )
        })?;
        withheld.push(validate_plan::HostInapplicableNode {
            tag: tag.clone(),
            capability,
            evidence: format!(
                "all {} selected cell(s) of manifest bucket {lane}/{category} are \
                 host-inapplicable: {evidence}",
                bucket.selected
            ),
        });
    }
    if withheld.is_empty() {
        return Ok(());
    }
    let gone: BTreeSet<String> = withheld.iter().map(|n| n.tag.clone()).collect();
    let retained: Vec<(String, String, Vec<String>)> = std::iter::once(&plan.cfg)
        .chain(plan.second.iter())
        .flat_map(|cfg| cfg.steps.iter())
        .filter(|s| !gone.contains(&s.tag()))
        .map(|s| (s.tag(), s.cmd.clone(), s.deps.clone()))
        .collect();
    let (droppable, refusals) = classify_withheld_dependents(&retained, &gone);
    if !refusals.is_empty() {
        return Err(format!(
            "refusing to withhold a manifest bucket node that a NON-RESULT-CONSUMING node \
             depends on: {}; a machine incapability must not silently cascade into unrun work",
            refusals.join(", ")
        ));
    }
    let drop_edge: BTreeSet<(String, String)> = droppable.iter().cloned().collect();
    let apply = |cfg: &mut DagConfig| {
        cfg.steps.retain(|s| !gone.contains(&s.tag()));
        for step in cfg.steps.iter_mut() {
            let tag = step.tag();
            step.deps
                .retain(|d| !drop_edge.contains(&(tag.clone(), d.clone())));
        }
    };
    apply(&mut plan.cfg);
    if let Some(second) = plan.second.as_mut() {
        apply(second);
    }
    for (tag, dep) in &droppable {
        println!(
            "HOST-INAPPLICABLE: dropped result-consumer dependency edge {tag} -> {dep} — the \
             consumer still RUNS and judges the incomplete result set itself; it is not skipped."
        );
    }
    plan.host_inapplicable.extend(withheld);
    Ok(())
}

/// What to do about a RETAINED node that depends on a withheld manifest bucket
/// node. PURE, so both directions are bracketed with planted nodes.
///
/// A withheld bucket node produces per-cell results and nothing else, so a
/// dependent is a RESULT CONSUMER. Leaving the edge in place would strand that
/// consumer unrun — the cascade hermit#2212 refuses — so the edge is dropped and
/// the consumer RUNS and judges the incomplete result set for itself. That
/// measures MORE, not less: if it genuinely needed those results it fails and
/// the run is refused, which is the opposite of an excuse.
///
/// Only a result consumer qualifies. Any other dependent would be treating the
/// withheld node as a prerequisite whose removal cannot be justified from here,
/// so it is a REFUSAL, exactly like hermit#2212's declared-node case.
///
/// Returns the `(dependent, dependency)` edges that may be dropped, and the
/// refusals that must abort the run.
fn classify_withheld_dependents(
    retained: &[(String, String, Vec<String>)],
    gone: &BTreeSet<String>,
) -> (Vec<(String, String)>, Vec<String>) {
    let mut droppable = Vec::new();
    let mut refusals = Vec::new();
    for (tag, cmd, deps) in retained {
        // The withheld node's ONLY product is per-cell results under
        // `$E2E_RESULT_ROOT`; naming that root is what makes a dependent a
        // consumer of it rather than a consumer of some prerequisite effect.
        let consumes_results = cmd.contains("$E2E_RESULT_ROOT");
        for dep in deps {
            if !gone.contains(dep) {
                continue;
            }
            if consumes_results {
                droppable.push((tag.clone(), dep.clone()));
            } else {
                refusals.push(format!("{tag} depends on {dep}"));
            }
        }
    }
    (droppable, refusals)
}

fn test_nodes_of(cfg: &DagConfig) -> BTreeSet<String> {
    cfg.steps
        .iter()
        .map(|s| s.tag())
        .filter(|t| t.starts_with("test.") || t.contains(":test."))
        .collect()
}

/// Add one final node that checks the complete fresh result set and prints the
/// compatibility table. The existing bucket exits remain authoritative: in
/// particular, preserving their node identity keeps environmental retry and
/// precise failure attribution working exactly as before.
fn attach_compatibility_scorecard(
    steps: &mut Vec<dagrun::model::Step>,
    lanes: &[&str],
) -> Result<(), String> {
    let mut deps = Vec::new();
    for step in steps.iter() {
        if validation_step_identity(step) == ValidationStepIdentity::ManifestRun {
            deps.push(step.tag());
        }
    }
    if deps.is_empty() {
        return Err(format!(
            "cannot attach compatibility scorecard: no manifest result nodes in lanes {}",
            lanes.join(",")
        ));
    }
    deps.sort();
    steps.push(step_with_caps(
        "scorecard",
        "compatibility",
        "Verify fresh per-cell results and print the compatibility table",
        format!(
            "./ci/compat-envelope/scorecard.rs verify-results --results \"$E2E_RESULT_ROOT\" --lanes {}",
            lanes.join(",")
        ),
        deps,
        120,
        120,
        1024 * 1024 * 1024,
    ));
    Ok(())
}

/// Reuse the versioned nextest installer verbatim in focused plans instead of
/// copying its pinned version or network fallback into a second source.
fn nextest_setup_node(
    root: &Path,
    gate: &str,
) -> Result<dagrun::model::Step, String> {
    validate_plan::lane_nodes(root, "portable", "", gate)?
        .into_iter()
        .find(|step| step.tag() == "setup.nextest")
        .ok_or_else(|| "portable DAG lost setup.nextest".to_string())
}

/// Build the execution plan for the selected level/mode.
fn build_plan(root: &Path, args: &Args, tmp: &Path) -> Result<Plan, String> {
    let with_proxy = has_cmd("with-proxy");
    let pre = validate_plan::preflight_nodes(root, with_proxy);
    let gate = "gate.manifest";

    // Focused compatibility matrices.
    let compat_mode = match &args.focused {
        Some(Focused::StrictCompat) => Some(CompatMode::Strict),
        Some(Focused::PortableStrictCompat) => Some(CompatMode::PortableStrict),
        Some(Focused::SabreCompat) => Some(CompatMode::Sabre),
        Some(Focused::E9patchCompat) => Some(CompatMode::E9patch),
        Some(Focused::RrCompat) => Some(CompatMode::Rr),
        _ => None,
    };
    if let Some(mode) = compat_mode {
        let hermit_bin = std::env::var("STRICT_COMPAT_HERMIT_BIN")
            .unwrap_or_else(|_| root.join("target/release/hermit").to_string_lossy().into());
        let fixtures = root.join(format!("target/real-compat-fixtures-{}", std::process::id()));
        let nsswitch = tmp.join("e9patch-nsswitch.conf");
        let shell_build = tmp.join("shell-build");
        let paths = validate_corpus::CorpusPaths {
            root_dir: &root.to_string_lossy(),
            real_compat_fixtures: &fixtures.to_string_lossy(),
            validation_tmp_dir: &tmp.to_string_lossy(),
            shell_build_dir: &shell_build.to_string_lossy(),
        };
        let mut steps = pre;
        let compat_gate = if args.reuse_parent_manifest_gate {
            // The outer node is reachable only after its real gate.manifest
            // passed. Avoid rerunning that ~75 s exact-tree audit inside the
            // nested payload, but retain the cheap, independently observed
            // submodule and pin gates.
            steps.retain(|s| {
                s.tag() != gate && s.tag() != validate_plan::MANIFEST_PLAN_PRODUCER_TAG
            });
            PIN_GATE_TAG
        } else {
            gate
        };
        // The corpus needs a release Hermit and the functional fixtures; both are
        // DAG nodes so they are boxed and timed like everything else.
        steps.push(build_release_hermit_node(compat_gate, &hermit_bin));
        steps.push(prepare_fixtures_node("compatprep.fixtures", &fixtures));
        if mode == CompatMode::E9patch {
            steps.push(nsswitch_fixture_node(&nsswitch));
        }
        steps.extend(validate_plan::compat_nodes(
            root,
            mode,
            &hermit_bin,
            &nsswitch.to_string_lossy(),
            &paths,
            Some("compatprep.fixtures"),
        )?);
        let profile = args.focused.as_ref().unwrap().profile();
        let cfg = validate_plan::config_from(steps, &format!("compatibility matrix: {mode:?}"));
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            second: None,
            profile,
            selection_mode: "full",
            compat: Some(mode),
            ..Default::default()
        });
    }

    // Focused single-node mode: run the SELECTED lane node(s) as ordinary steps
    // of THIS run's own boxed DAG.
    //
    // This used to synthesise one `shard.<lane>_<node>` wrapper whose command was
    // `./ci/run-node.sh <lane> <nodes>`, i.e. a SECOND `dagrun`
    // underneath the one already driving this run. That nesting broke `--only`
    // outright, in two separate ways:
    //
    //   * The inner runner establishes and then reads back its OWN outer systemd
    //     scope. Inside validate's scope it does not own one, so the readback of
    //     MemoryMax/MemorySwapMax/memory.oom.group failed and it refused with
    //     "the run is not safely contained" — 0s, exit 3, before any work ran.
    //   * The wrapper invented caps (7200s wall / 7200s CPU / 16 GiB) in place of
    //     whatever the selected node actually declares, so a node budgeted at 900s
    //     in the full plan got eight hours here and `--run-timeout` below 7200 was
    //     refused outright.
    //
    // Selecting the real node directly cures both and STRENGTHENS containment:
    // the node now runs under this run's two-level boxing with per-step cgroups,
    // carrying its own declared wall/CPU/memory caps, exactly as it does in a full
    // run. That identity is the whole point of a reproducer — a focused rerun must
    // reproduce what the full run did to that node, budgets included.
    if let Some(Focused::Only { lane, nodes }) = &args.focused {
        let mut steps = pre;
        // Preflight tags already in the plan; naming one is satisfied by the
        // preflight itself and must not be looked up in the lane file.
        let preflight: BTreeSet<String> = steps.iter().map(|s| s.tag()).collect();
        // CARRY the lane's top-level config. `config_from` would substitute
        // DagConfig::default(), dropping resource_caps and default_step_timeout;
        // see config_from_base's note on the 14-minute 0%-CPU hang that caused.
        let base = validate_plan::lane_config(root, lane)?;
        let mut lane_steps = validate_plan::lane_nodes(root, lane, "", gate)?;
        // The lane is independently runnable, so it carries its own manifest-plan
        // producer. Validate's canonical preflight already carries the same tag
        // and deliberately owns that producer's validation-time cap. Reuse the
        // preflight node before filtering; otherwise `--only setup.manifest_plan`
        // creates a duplicate tag and a consumer selected with the producer keeps
        // an ambiguous edge. Every non-preflight selected node retains its lane cap.
        validate_plan::reuse_preflight_manifest_producer(
            &mut lane_steps,
            &format!("--only lane {lane}"),
        )?;
        let available: BTreeSet<String> = lane_steps.iter().map(|s| s.tag()).collect();

        let requested: Vec<String> = nodes
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .collect();
        if requested.is_empty() {
            return Err("--only needs at least one <group.job> node tag".into());
        }
        // Refuse an unknown tag HERE, naming what is selectable, instead of
        // letting it travel into a child process that reports it 90s later.
        let unknown: Vec<&String> = requested
            .iter()
            .filter(|t| !available.contains(*t) && !preflight.contains(*t))
            .collect();
        if !unknown.is_empty() {
            let mut known: Vec<&str> = available.iter().map(String::as_str).collect();
            known.extend(preflight.iter().map(String::as_str));
            known.sort_unstable();
            return Err(format!(
                "--only: unknown node tag(s) in lane {lane}: {}. Selectable tags: {}",
                unknown.iter().map(|t| t.as_str()).collect::<Vec<_>>().join(", "),
                known.join(", ")
            ));
        }
        let selected: BTreeSet<String> =
            requested.iter().filter(|t| available.contains(*t)).cloned().collect();
        let mut dropped: BTreeSet<String> = BTreeSet::new();
        for mut step in lane_steps.into_iter().filter(|s| selected.contains(&s.tag())) {
            // Same selection semantics run-node.sh documented for `run --only`:
            // edges to steps OUTSIDE the selection are dropped (their outputs are
            // assumed already built), edges AMONG the selection are preserved so a
            // selected sub-graph still runs in order.
            dropped.extend(step.deps.iter().filter(|d| !selected.contains(*d)).cloned());
            step.deps.retain(|d| selected.contains(d));
            if step.deps.is_empty() {
                step.deps.push(gate.to_string());
            }
            steps.push(step);
        }
        // SAY that this mode assumes an already-built tree, and name the build
        // edges it just dropped. Without this the mode is silent about its own
        // precondition. A fast exit 127 may mean one of those artifacts is absent,
        // but it can also mean a missing host tool or a command typo, so both the
        // pre-run and post-run diagnostics keep that distinction explicit.
        dropped.remove(gate);
        if dropped.is_empty() {
            eprintln!(
                "validate: --only runs the selected node(s) against the CURRENT tree; \
                 nothing they depend on is rebuilt first."
            );
        } else {
            eprintln!(
                "validate: --only assumes an already-built tree. Dropped {} dependency edge(s) \
                 whose outputs must ALREADY exist: {}. Build them first (or name them in the \
                 selection). A selected node exiting 127 in ~0s MAY indicate one is absent; \
                 inspect the node command too, because a missing host tool or typo is also 127.",
                dropped.len(),
                dropped.iter().cloned().collect::<Vec<_>>().join(", ")
            );
        }
        let cfg = validate_plan::config_from_base(&base, steps, "selected DAG node(s)");
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            second: None,
            profile: args.focused.as_ref().unwrap().profile(),
            selection_mode: "only",
            ..Default::default()
        });
    }

    // One-cell canonical requalification. The pressure runner already owns the
    // exact-cell build/run mechanics and resource declarations; validate wraps
    // it as one boxed gate and retains its typed result as schema 7. This plan
    // is never suite-complete and therefore cannot grant whole-run authority.
    if let Some(Focused::RequalifyCell { test, mode, backend }) = &args.focused {
        let matches = validate_cell_results::expected_plan(root)?
            .into_iter()
            .filter(|cell| {
                cell["test"] == *test && cell["mode"] == *mode && cell["backend"] == *backend
            })
            .collect::<Vec<_>>();
        let [identity] = matches.as_slice() else {
            return Err(format!(
                "--requalify-cell must name exactly one currently selected cell; found {} for {test}/{mode}/{backend}",
                matches.len()
            ));
        };
        // The outer validate publishes these rows after all attempts finish.
        // Suppress pressure-test's standalone publisher here so one physical
        // cell run cannot appear twice under two producer names.
        let command = format!(
            "env -u DEV_HERMIT_PARENT ./ci/compat-envelope/pressure-test.rs run --results \"$E2E_RESULT_ROOT\" \
             --test {} --mode {} --backend {} --repetitions 1 \
             --run-id-prefix \"$E2E_RUN_ID-pid$$\" --jobs 1",
            validate_plan::shell_quote(test),
            validate_plan::shell_quote(mode),
            validate_plan::shell_quote(backend),
        );
        let mut steps = pre;
        // The pressure runner performs its own exact manifest/scorecard check
        // after building test-harness. The ordinary gate.manifest assumes that
        // binary was already built by a lane node, which this focused path does
        // not have; retaining it would make every cold targeted run exit 127.
        steps.retain(|step| step.tag() != gate);
        let mut requalification = step_with_caps(
            "requalify",
            "cell",
            "Targeted canonical cell requalification",
            command,
            vec![PIN_GATE_TAG.into()],
            3600,
            7200,
            16 * 1024 * 1024 * 1024,
        );
        // The nested pressure plan may need its release-Hermit build, whose
        // declared worker width is eight. Giving the wrapper only the default
        // one CPU makes dagrun refuse before the selected cell can start.
        requalification.hint.preferred_inner_jobs = Some(8);
        // pressure-test owns the nested scheduler width through its explicit
        // `--jobs 1`. An ordinary `-j` jobs flag would make the outer runner
        // append `-j 8` to this command, which pressure-test does not accept.
        requalification.jobs_flag = Some(String::new());
        steps.push(requalification);
        let cfg = validate_plan::config_from(steps, "targeted cell requalification");
        return Ok(Plan {
            planned_test_nodes: BTreeSet::new(),
            cfg,
            second: None,
            profile: "cell-requalification".into(),
            selection_mode: "targeted",
            cacheable: false,
            cell_evidence_expected: Some(vec![identity.clone()]),
            ..Default::default()
        });
    }

    // Focused liteinst matrix (validate.sh:4815): three ordered gates.
    if matches!(args.focused, Some(Focused::LiteinstCompat)) {
        let mut steps = pre;
        steps.push(nextest_setup_node(root, gate)?);
        steps.push(step_with_caps("liteinst", "hermit_release", "Release Hermit for LiteInst compatibility",
            "cargo build --release --locked -p hermit --features third-party-backends".into(),
            vec![gate.to_string()], 1200, 3600, 16 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("liteinst", "runtime", "Release LiteInst runtime",
            "./scripts/stage-liteinst-runtime.sh release $PWD/target/release/libreverie_liteinst.so $PWD/target/liteinst-runtime-build".into(),
            vec!["liteinst.hermit_release".into()], 900, 1800, 8 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("liteinst", "strict", "Portable CI liteinst_strict",
            "HERMIT_LITEINST_TEST_BINARY=$PWD/target/release/hermit ./ci/run-nextest-counted.sh -p hermit --features third-party-backends --test liteinst_advanced -j 1".into(),
            vec!["liteinst.runtime".into(), "setup.nextest".into()], 900, 1800, 8 * 1024 * 1024 * 1024));
        let cfg = validate_plan::config_from(steps, "liteinst compatibility");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: args.focused.as_ref().unwrap().profile(), selection_mode: "full",
            ..Default::default() });
    }

    // Focused QEMU L2 boot (validate.sh:4860). Heavyweight; two ordered gates.
    if matches!(args.focused, Some(Focused::QemuL2)) {
        let mut steps = pre;
        steps.push(step_with_caps("qemu", "hermit_release", "Release Hermit for QEMU L2",
            "cargo build --release -p hermit --features third-party-backends".into(),
            vec![gate.to_string()], 3600, 7200, 16 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("qemu", "strict_l2_boot", "QEMU strict L2 boot (heavyweight)",
            "./tests/qemu-boot/strict_l2_test.sh".into(),
            vec!["qemu.hermit_release".into()], 1500, 3000, 16 * 1024 * 1024 * 1024));
        let cfg = validate_plan::config_from(steps, "QEMU L2 boot");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: args.focused.as_ref().unwrap().profile(), selection_mode: "full",
            ..Default::default() });
    }

    // `quick` is NOT "the portable lane" — it is seven specific smoke gates
    // (validate.sh:4583). Mapping it onto a lane would run a different, much
    // larger thing under the same name.
    if args.level == Level::Quick && args.focused.is_none() {
        let hermit = "target/debug/hermit";
        let marker = "hermit-validation-smoke";
        let run_args = "run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled";
        let mut steps = pre;
        steps.push(nextest_setup_node(root, gate)?);
        let mut add = |job: &str, desc: &str, cmd: String, deps: Vec<String>, t: i64, mem: i64| {
            steps.push(step_with_caps("quick", job, desc, cmd, deps, t, t * 2, mem));
        };
        add("build", "Build workspace", "cargo build --workspace --features third-party-backends".into(), vec![gate.into()], 3600, 16 * 1024 * 1024 * 1024);
        add("e2e_metadata", "Portable E2E metadata", "target/debug/test-harness validate".into(), vec!["quick.build".into()], 600, 4 * 1024 * 1024 * 1024);
        add("e2e_verify", "Portable ptrace E2E verification", "target/debug/test-harness run --lane portable --mode verify --backend ptrace --ci-only".into(), vec!["quick.build".into()], 1800, 8 * 1024 * 1024 * 1024);
        add("detcore_unit", "Detcore core unit tests", "./ci/run-nextest-counted.sh -p hermit-detcore --lib".into(), vec!["quick.build".into(), "setup.nextest".into()], 1800, 8 * 1024 * 1024 * 1024);
        add("run_smoke", "Hermit run smoke test",
            format!("out=$(timeout 30s {hermit} {run_args} -- /bin/echo {marker}) && test \"$out\" = {marker}"),
            vec!["quick.build".into()], 120, 4 * 1024 * 1024 * 1024);
        add("verify_smoke", "Hermit verify-mode smoke test",
            format!("timeout 30s {hermit} {run_args} --verify -- /bin/echo {marker}"),
            vec!["quick.build".into()], 120, 4 * 1024 * 1024 * 1024);
        add("record_replay_smoke", "Hermit record/replay smoke test",
            format!("timeout 30s {hermit} record start --verify -- /bin/echo {marker}"),
            vec!["quick.build".into()], 180, 4 * 1024 * 1024 * 1024);
        let cfg = validate_plan::config_from(steps, "quick smoke suite");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: "quick".into(), selection_mode: "full", ..Default::default() });
    }

    // The `super` stress/diagnostic suite (validate.sh:4702).
    if args.level == Level::Super && args.focused.is_none() {
        return super_plan(root, tmp, pre, gate);
    }

    // Working-envelope measurement (validate.sh:4173). A MEASUREMENT, not a
    // gate: probe failures lower a count and never abort, so keep-going is
    // forced and every probe node is nonblocking.
    if let Some(Focused::Envelope { baseline }) = &args.focused {
        let reps = validate_envelope::l4_reps();
        let hermit_bin = root.join("target/debug/hermit").to_string_lossy().into_owned();
        let mut steps = pre;
        steps.push(validate_envelope::build_node(gate));
        let probes = validate_envelope::nodes(&hermit_bin, reps, "envelope.build");
        let nonblocking: BTreeSet<String> = probes.iter().map(|s| s.tag()).collect();
        steps.extend(probes);
        let cfg = validate_plan::config_from(steps, "working-envelope measurement");
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            profile: "envelope-only".into(),
            envelope: Some(EnvelopePlan { reps, baseline: baseline.clone() }),
            nonblocking,
            force_keep_going: true,
            cacheable: false,
            ..Default::default()
        });
    }

    // Node-level `--selective` / `--since-green` (validate.sh:4421).
    if let Some(Focused::Selective { shallow }) = &args.focused {
        return selective_plan(root, args, pre, gate, *shallow);
    }

    // Lane-based profiles.
    let lanes: Vec<&str> = match (&args.focused, args.level) {
        (Some(Focused::PrivilegedOnly), _) => vec!["privileged"],
        (None, Level::PortableOnly) => vec!["portable"],
        (None, Level::Full) => vec!["portable", "privileged"],
        (_, _) => {
            return Err(format!(
                "no plan is defined for level={:?} focused={:?}; refusing to substitute another profile",
                args.level, args.focused
            ))
        }
    };
    let profile = match &args.focused {
        Some(f) => f.profile(),
        None => args.level.name().to_string(),
    };
    let selection_mode = match &args.focused {
        Some(Focused::Selective { .. }) => "selective",
        Some(Focused::Only { .. }) => "only",
        _ => "full",
    };

    if lanes.len() == 2 && !args.merge_lanes {
        // Faithful reproduction of run_full_suite: portable lane, then privileged.
        let mut a = pre.clone();
        a.extend(validate_plan::lane_nodes_reusing_manifest_producer(
            root, lanes[0], "", gate,
        )?);
        let mut b = validate_plan::lane_nodes(root, lanes[1], "", gate)?;
        // The second run repeats preflight-free; its nodes hang off nothing.
        for s in b.iter_mut() {
            s.deps.retain(|d| d != gate);
        }
        attach_compatibility_scorecard(&mut a, &["portable"])?;
        // The runs are sequential, so when this node runs the portable rows
        // already exist. Emit the same whole-scorecard answer as the fused
        // default rather than a second disconnected per-lane claim.
        attach_compatibility_scorecard(&mut b, &["portable", "privileged"])?;
        // Each lane carries ITS OWN loaded config. They genuinely differ --
        // portable default_step_timeout=600 vs privileged=120, and disjoint
        // resource_caps -- so there is no correct single merged value; running
        // them as two sequential DAGs lets each keep its own exactly.
        let base_a = validate_plan::lane_config(root, lanes[0])?;
        let base_b = validate_plan::lane_config(root, lanes[1])?;
        let cfg_a = validate_plan::config_from_base(&base_a, a, "portable lane");
        let cfg_b = validate_plan::config_from_base(&base_b, b, "privileged lane");
        for (base, derived, lane) in [(&base_a, &cfg_a, lanes[0]), (&base_b, &cfg_b, lanes[1])] {
            validate_plan::assert_config_carried(base, derived)
                .map_err(|e| format!("lane {lane}: DAG config was not carried: {e}"))?;
        }
        let mut planned = test_nodes_of(&cfg_a);
        planned.extend(test_nodes_of(&cfg_b));
        return Ok(Plan {
            cfg: cfg_a,
            second: Some(cfg_b),
            profile,
            selection_mode,
            planned_test_nodes: planned,
            suite_complete: args.level == Level::Full && args.focused.is_none(),
            // Validation is also the live compatibility measurement. Reusing
            // an older tree-keyed receipt would print no fresh per-cell table.
            cacheable: false,
            ..Default::default()
        });
    }

    let mut steps = pre;
    for lane in &lanes {
        // Keep the portable lane's shipped tags byte-identical: the
        // main-reachable receipt finalizer derives its coverage denominator
        // from those manifest tags. Prefix only the additional lane, which is
        // sufficient to disambiguate every collision in the fused graph.
        let prefix = if lanes.len() > 1 && *lane != "portable" {
            format!("{lane}-")
        } else {
            String::new()
        };
        steps.extend(validate_plan::lane_nodes_reusing_manifest_producer(
            root, lane, &prefix, gate,
        )?);
    }
    // Fusing lanes can duplicate identical work. In particular, the always-on
    // gate.manifest and both lane e2e.metadata nodes run the exact same
    // `test-harness validate` tree audit. Drop later duplicates and repoint
    // their dependents, so one full run pays that ~75 s audit exactly once. The
    // dedup is keyed by typed manifest-audit identity; an unexpected command is
    // refused explicitly instead of silently ceasing to match.
    let removed = dedupe_identical(&mut steps, gate)?;
    if !removed.is_empty() {
        eprintln!("validate: fused lanes; deduped {} identical node(s): {}", removed.len(), removed.join(", "));
    }
    if lanes.len() == 2 {
        // The artifact barrier waits for both initial Cargo producers, verifies
        // binary and resource identities, then publishes a content-addressed
        // bundle. Every later Cargo writer and manifest consumer runs only after
        // that barrier, so no writer can mutate either source during publication
        // and no consumer reads a mutable Cargo path afterward.
        let producer = "build.e2e_artifact";
        let debug_producer = "build.workspace";
        let consumer = "privileged-build.privileged_tests";
        let portable_build = steps
            .iter()
            .find(|s| s.tag() == debug_producer)
            .ok_or_else(|| format!("fused debug producer disappeared: {debug_producer}"))?;
        let expected_fat_build = "./ci/run-with-reverie-dbt-budget.sh cargo build --workspace --all-targets --features third-party-backends && CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit";
        if portable_build.cmd != expected_fat_build {
            return Err(format!(
                "fused debug producer command drifted; re-prove the artifact barrier: {}",
                portable_build.cmd
            ));
        }
        let artifact = steps
            .iter()
            .find(|s| s.tag() == producer)
            .ok_or_else(|| format!("fused artifact producer disappeared: {producer}"))?;
        let expected_artifact = "./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path target/install_pkg";
        if artifact.cmd != expected_artifact
            || ![debug_producer, "build.runtime_release"]
                .iter()
                .all(|dep| artifact.deps.iter().any(|actual| actual == dep))
        {
            return Err(format!(
                "fused artifact barrier drifted; re-prove binary+resource publication: {} deps={:?}",
                artifact.cmd, artifact.deps
            ));
        }
        let privileged_build = steps
            .iter_mut()
            .find(|s| s.tag() == consumer)
            .ok_or_else(|| format!("fused artifact consumer disappeared: {consumer}"))?;
        let expected_build = "CARGO_BUILD_JOBS=8 cargo build -p hermit --features third-party-backends --bin hermit && ./ci/publish-hermit-e2e-artifact.sh target/debug/hermit target/ci/hermit-e2e-artifacts target/ci/hermit-e2e-artifact.path && CARGO_BUILD_JOBS=8 cargo test -p hermit-detcore --test tests_misc --no-run";
        if privileged_build.cmd != expected_build {
            return Err(format!(
                "fused privileged build command drifted; re-prove that build.workspace is a superset: {}",
                privileged_build.cmd
            ));
        }
        if !privileged_build.deps.iter().any(|d| d == producer) {
            privileged_build.deps.push(producer.to_string());
            privileged_build.deps.sort();
        }
        // SELECT THE NEWEST, DO NOT REQUIRE EXACTLY ONE.
        //
        // This assertion used to end `test "$count" -eq 1`, and that made the
        // owner's `make validate` fail EVERY TIME while passing in every agent
        // worktree. Cargo writes one hash-suffixed `tests_misc-<hash>` per build
        // and never prunes the old ones, so the count is 1 only in a FRESH or
        // just-`cargo clean`ed tree. Measured 2026-08-10: 9 executables in
        // ~/work/dev-hermit/hermit versus 1 in a cleaned slot. `test 9 -eq 1`
        // exits 1 instantly and the shell builtin prints nothing -- which is
        // exactly the "0s, exit 1" with an empty detail block seen in both
        // failing runs at 2b38d8e6. It is not flaky and it is not a timeout:
        // once a working tree accumulates a second binary it can never pass
        // again. We validated only in clean clones, i.e. the one condition
        // where the defect cannot appear.
        //
        // Fixing the CHECK rather than the user's working directory is
        // deliberate: this must work in any checkout, including a dirty one,
        // and validate must not delete a developer's build artifacts.
        //
        // Newest-by-mtime is what cargo itself would run. Deliberately NOT
        // relaxed to `-ge 1`: the CPUID consumer below executes the binary it
        // selects, so "any one of nine" would let it silently test a STALE
        // artifact -- a check that passes while measuring the wrong thing,
        // which is worse than failing loudly. Zero binaries still fails.
        privileged_build.cmd = "./ci/verify-hermit-e2e-artifact.sh target/ci/hermit-e2e-artifact.path >/dev/null || exit 1; newest=\"\"; for f in target/debug/deps/tests_misc-*; do if [ -f \"$f\" ] && [ -x \"$f\" ] && { [ -z \"$newest\" ] || [ \"$f\" -nt \"$newest\" ]; }; then newest=\"$f\"; fi; done; test -n \"$newest\"".to_string();

        let cpuid = steps
            .iter_mut()
            .find(|s| s.tag() == "privileged-cpuid.faulting")
            .ok_or("fused prebuilt CPUID consumer disappeared")?;
        let expected_cpuid = "status=0; timeout --kill-after=5s 30s cargo test -p hermit-detcore --test tests_misc rdrand_rdseed_is_masked -- --exact || status=$?; if [ \"$status\" -eq 124 ] || [ \"$status\" -eq 137 ]; then printf 'test hermit-detcore/tests_misc::rdrand_rdseed_is_masked exceeded 30 s (innermost exact Cargo timeout: exit %s)\\n' \"$status\" >&2; fi; exit \"$status\"";
        if cpuid.cmd != expected_cpuid {
            return Err(format!(
                "fused CPUID command drifted; re-prove direct prebuilt invocation: {}",
                cpuid.cmd
            ));
        }
        // Same defect, same fix: `((${#bins[@]} == 1))` failed for exactly the
        // reason above, so this node could never run in a long-lived checkout
        // either. It EXECUTES the binary it picks, which is precisely why the
        // selection must be the NEWEST rather than an arbitrary survivor of a
        // `-ge 1` relaxation -- running a stale `tests_misc` would report a
        // CPUID verdict about an artifact that is not the one under test.
        cpuid.cmd = "newest=\"\"; for f in target/debug/deps/tests_misc-*; do if [ -f \"$f\" ] && [ -x \"$f\" ] && { [ -z \"$newest\" ] || [ \"$f\" -nt \"$newest\" ]; }; then newest=\"$f\"; fi; done; test -n \"$newest\"; timeout 30 \"$newest\" rdrand_rdseed_is_masked --exact".to_string();
    }
    attach_compatibility_scorecard(&mut steps, &lanes)?;
    // Fusing lanes means one config for both. Their default wall timeouts differ,
    // but every shipped/synthesized node has an explicit wall timeout and the
    // fail-closed undeclared-node audit below enforces that invariant. Therefore
    // the default is unreachable; retain the stricter value as defense in depth.
    // Resource caps are disjoint and merge cleanly.
    let bases: Vec<DagConfig> = lanes
        .iter()
        .map(|l| validate_plan::lane_config(root, l))
        .collect::<Result<_, _>>()?;
    let mut fused = bases[0].clone();
    for b in bases.iter().skip(1) {
        fused.default_step_timeout = fused.default_step_timeout.min(b.default_step_timeout);
        for (r, n) in &b.resource_caps {
            if let Some(prev) = fused.resource_caps.get(r) {
                if prev != n {
                    return Err(format!(
                        "--merge-lanes refused: resource {r} capped at {prev} and {n} by different lanes"
                    ));
                }
            }
            fused.resource_caps.insert(r.clone(), *n);
        }
    }
    let cfg = validate_plan::config_from_base(&fused, steps, "fused lanes");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        second: None,
        profile,
        selection_mode,
        suite_complete: args.level == Level::Full && args.focused.is_none(),
        // Every ordinary lane validation must produce fresh per-cell results;
        // a cache hit is valid landing evidence but is not a new measurement.
        cacheable: false,
        ..Default::default()
    })
}

/// Build the `super` plan from the mechanically extracted gate table.
///
/// Dependency policy — the bash ran all 32 rows strictly sequentially through
/// `run_check`, so ANY edge set that preserves the real prerequisites is a
/// faithful port and a strictly better schedule. The prerequisites are:
///   * the two build rows gate everything that needs a binary;
///   * `run_exact_detcore_cases` is FAIL-FAST within its group
///     (validate.sh:4514), reproduced by chaining those rows so a failure SKIPS
///     the rest instead of running them;
///   * the LevelDB test needs its fixture built first.
/// Everything else is independent and is allowed to overlap.
fn super_plan(
    root: &Path,
    tmp: &Path,
    pre: Vec<dagrun::model::Step>,
    gate: &str,
) -> Result<Plan, String> {
    let gates = validate_super::load_gates(root)?;
    let reps = validate_super::repetitions();
    let build_ws = "super.build_workspace".to_string();
    let build_rel = "super.build_release_hermit".to_string();
    let debug_bin = root.join("target/debug/hermit").to_string_lossy().into_owned();
    let release_bin = std::env::var("STRICT_COMPAT_HERMIT_BIN")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| root.join("target/release/hermit").to_string_lossy().into_owned());

    let mut steps = pre;
    steps.push(nextest_setup_node(root, gate)?);
    let mut nonblocking: BTreeSet<String> = BTreeSet::new();
    // `run_exact_detcore_cases` labels its rows "<group>: <case>"; consecutive
    // rows sharing a group prefix are one fail-fast family. Deriving the chain
    // from the label SHAPE keeps it correct if a case is added or removed.
    let family = |label: &str| label.split_once(": ").map(|(g, _)| g.to_string());
    let mut prev_family: Option<(String, String)> = None; // (family, previous tag)

    for g in &gates {
        let mut deps = match g.job.as_str() {
            "build_workspace" | "build_release_hermit" => vec![gate.to_string()],
            "full_leveldb_strict_determinism" => {
                vec!["super.build_pinned_leveldb_super_fixture".to_string()]
            }
            _ => vec![build_ws.clone()],
        };
        if g.argv.windows(3).any(|w| w == ["cargo", "nextest", "run"]) {
            deps.push("setup.nextest".to_string());
        }
        match g.synthetic.as_deref() {
            Some("portable_slow_strict_diagnostics") => {
                // The four PORTABLE_STRICT_SUPER_ONLY workloads, run with the
                // portable-strict flags after the shared functional fixtures are
                // prepared (validate.sh:4603).
                let fixtures = root.join(format!("target/real-compat-fixtures-{}", std::process::id()));
                steps.push(prepare_fixtures_node_dep("compatprep.fixtures", &fixtures, &build_rel));
                let only: BTreeSet<String> =
                    validate_corpus::portable_super_only().keys().map(|k| k.to_string()).collect();
                let shell_build = tmp.join("shell-build");
                let paths = validate_corpus::CorpusPaths {
                    root_dir: &root.to_string_lossy(),
                    real_compat_fixtures: &fixtures.to_string_lossy(),
                    validation_tmp_dir: &tmp.to_string_lossy(),
                    shell_build_dir: &shell_build.to_string_lossy(),
                };
                steps.extend(validate_plan::compat_nodes_for(
                    root,
                    CompatMode::PortableStrict,
                    &release_bin,
                    "",
                    &paths,
                    Some("compatprep.fixtures"),
                    Some(&only),
                    Some(g.wall()),
                )?);
            }
            Some("super_stress_suite") => {
                let stress =
                    validate_super::stress_nodes(&release_bin, &debug_bin, tmp, reps, &build_rel, &build_ws);
                steps.extend(stress);
                nonblocking.extend(validate_super::nonblocking_tags(reps));
            }
            Some("calibrated_analyze_tests") => {
                let mut deps = deps;
                deps.push("setup.nextest".to_string());
                steps.push(validate_super::calibrated_analyze_node(g, deps));
            }
            Some(other) => {
                return Err(format!(
                    "ci/super/gates.json row {} names an unknown synthetic expansion `{other}`; \
                     refusing to skip it silently",
                    g.job
                ))
            }
            None => {
                // Fail-fast chaining inside a `run_exact_detcore_cases` family. The edge
                // preserves the established serial order; the explicit family also preserves
                // eager cancellation if a later plan transformation makes two members runnable.
                let mut deps = deps;
                let declared_family = family(&g.label);
                if let Some(f) = declared_family.as_ref() {
                    if let Some((pf, ptag)) = &prev_family {
                        if pf == f {
                            deps = vec![ptag.clone()];
                        }
                    }
                    prev_family = Some((f.clone(), format!("super.{}", g.job)));
                } else {
                    prev_family = None;
                }
                let mut step = validate_super::gate_node(g, deps);
                step.fail_fast_family =
                    declared_family.map(|family| format!("super.{family}"));
                steps.push(step);
            }
        }
    }
    let cfg = validate_plan::config_from(steps, "super stress + diagnostic suite");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        profile: "super".into(),
        super_mode: true,
        nonblocking,
        ..Default::default()
    })
}

/// What `ci/select-tests.rs` decided, and what that means for the plan.
enum SelectDecision {
    /// No CI-relevant change: run nothing beyond preflight.
    Skip,
    /// Run exactly this dependency-closed node set.
    Nodes(BTreeSet<String>),
    /// Fail-safe: run the complete portable lane, for the stated reason.
    Full(String),
}

/// Apply one selector result while preserving the shipped lane as the tag
/// authority. Producer reuse happens once, after unknown-tag validation, so it
/// covers both dependency-closed subsets and fail-safe full-lane fallbacks.
fn apply_selective_decision(
    all: Vec<dagrun::model::Step>,
    total: usize,
    decision: SelectDecision,
) -> Result<Vec<dagrun::model::Step>, String> {
    let mut steps = match decision {
        SelectDecision::Skip => {
            println!(
                "Selective validation: no CI-relevant changes since baseline — nothing to run \
                 (0/{total} nodes). Preflight still ran; the ledger's coverage record will show \
                 zero planned test nodes, so this cannot be misread as a full pass."
            );
            Vec::new()
        }
        SelectDecision::Nodes(keep) => {
            let sel = validate_plan::select_lane_nodes(all, &keep);
            if !sel.unknown_tags.is_empty() {
                return Err(format!(
                    "select-tests.rs named {} node(s) absent from ci/dag/portable.json ({}); the \
                     selector and the DAG disagree, so refusing to run a subset derived from a \
                     stale mapping",
                    sel.unknown_tags.len(),
                    sel.unknown_tags.join(", ")
                ));
            }
            println!(
                "Selective validation: running {}/{total} portable DAG nodes ({} intra-lane \
                 dependency edge(s) pruned to the selected set):\n  {}",
                sel.steps.len(),
                sel.pruned_edges,
                keep.iter().cloned().collect::<Vec<_>>().join(" ")
            );
            sel.steps
        }
        SelectDecision::Full(why) => {
            println!("Selective validation: {why} — running the FULL portable lane.");
            all
        }
    };
    if validate_plan::reuse_preflight_manifest_producer(
        &mut steps,
        "selective portable lane",
    )? {
        println!("Selective validation: setup.manifest_plan is supplied by preflight.");
    }
    Ok(steps)
}

/// Ask `ci/select-tests.rs` what to run.
///
/// This is PLAN CONSTRUCTION, not a gate: the selector produces no verdict about
/// the tree, and its output is only used to choose which already-declared nodes
/// to schedule. Every failure mode — a nonzero exit, unparseable JSON, an empty
/// node set, or an unproducible coverage report — resolves to
/// [`SelectDecision::Full`], so the driver can only ever err toward running MORE
/// than the selector proved safe to omit (validate.sh:4416-4420).
fn ask_selector(root: &Path, baseline: Option<&str>) -> SelectDecision {
    let run = |format: &str| -> Option<String> {
        let mut c = Command::new(root.join("ci").join("select-tests.rs"));
        c.arg("--since-green");
        if let Some(b) = baseline {
            c.args(["--baseline", b]);
        }
        c.args(["--format", format]);
        let out = c.output().ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let Some(json_text) = run("json") else {
        return SelectDecision::Full("select-tests.rs failed".into());
    };
    let Ok(sel) = serde_json::from_str::<serde_json::Value>(&json_text) else {
        return SelectDecision::Full("select-tests.rs emitted unparseable JSON".into());
    };
    // A subset must never run without a human-auditable account of what it
    // dropped and why, so an unproducible report is treated as doubt.
    let report = run("human").unwrap_or_default();
    if report.trim().is_empty() {
        return SelectDecision::Full("could not produce the coverage report".into());
    }
    println!("----- selective coverage report (skipped nodes/shards/e2e cells + reasons) -----");
    println!("{}", report.trim_end());
    println!("-------------------------------------------------------------------------------");
    match sel.get("decision").and_then(|d| d.as_str()).unwrap_or("full") {
        "skip" => SelectDecision::Skip,
        "selective" => {
            let nodes: BTreeSet<String> = sel
                .get("nodes")
                .and_then(|n| n.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.to_string()).collect())
                .unwrap_or_default();
            if nodes.is_empty() {
                SelectDecision::Full("empty selected node set".into())
            } else {
                SelectDecision::Nodes(nodes)
            }
        }
        other => SelectDecision::Full(format!("decision={other}")),
    }
}

/// Build the `--selective` plan (validate.sh:4421).
fn selective_plan(
    root: &Path,
    args: &Args,
    pre: Vec<dagrun::model::Step>,
    gate: &str,
    shallow: bool,
) -> Result<Plan, String> {
    let commit_exists =
        |sha: &str| sh("git", &["cat-file", "-e", &format!("{sha}^{{commit}}")]).is_some()
            || Command::new("git")
                .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
    let baseline: Option<String> = if shallow {
        // --shallow-select pins the baseline to HEAD~1. A root commit has no
        // parent, so selection fails safe to the full lane (validate.sh:4369).
        sh("git", &["rev-parse", "--verify", "HEAD~1"])
    } else {
        let ledger = ledger_path(root);
        let rows = validate_history::read_rows(&ledger);
        let parent = find_parent(root);
        let slot = slot_name(root, parent.as_deref());
        validate_history::selective_baseline(&rows, args.baseline.as_deref(), &slot, &commit_exists)
    };
    match &baseline {
        Some(b) => println!("Selective validation: last-known-green baseline = {b}"),
        None => println!(
            "Selective validation: no trustworthy green baseline; running the FULL portable lane."
        ),
    }

    // Keep the shipped lane's complete tag vocabulary through selection. In
    // particular, the selector may return setup.manifest_plan as part of a
    // dependency-closed result; it is replaced by the preflight producer only
    // after unknown-tag validation below.
    let all = validate_plan::lane_nodes(root, "portable", "", gate)?;
    let total = all.len();
    let decision = match &baseline {
        Some(b) => ask_selector(root, Some(b)),
        None => SelectDecision::Full("no trustworthy green baseline".into()),
    };
    let steps = apply_selective_decision(all, total, decision)?;
    let mut nodes = pre;
    nodes.extend(steps);
    let cfg = validate_plan::config_from(nodes, "selective portable subset");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        profile: "selective".into(),
        selection_mode: "selective",
        ..Default::default()
    })
}

/// Remove later steps whose semantic work exactly matches an earlier step's,
/// and repoint every dependency onto the survivor. Returns the removed tags.
///
/// Most nodes require both job and command to match. Deliberate exceptions are
/// the manifest audit (different tags, byte-identical command/tree) and the
/// Reverie-pin authority (preflight passes `--repo`, lane nodes rely on the same
/// root cwd). The observed preflight node survives in both cases.
/// `gate_dep` is the tag `validate_plan::lane_nodes` injects onto every
/// dependency-less lane node to reproduce the fail-fast ordering. It is a
/// scheduling artifact rather than a data dependency, so a removed duplicate
/// never passes it on to its survivor: doing so would make `pre.reverie_pin`
/// inherit an edge to `gate.manifest` from the deduped `check.reverie_pin` and
/// invert the preflight.
fn dedupe_identical(steps: &mut Vec<Step>, gate_dep: &str) -> Result<Vec<String>, String> {
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut keep = Vec::with_capacity(steps.len());
    let mut removed = Vec::new();
    // (survivor tag, the removed duplicate's own dependencies)
    let mut inherited: Vec<(String, Vec<String>)> = Vec::new();
    for s in steps.drain(..) {
        let tag = s.tag();
        let key = if validation_step_identity(&s) == ValidationStepIdentity::ManifestAudit {
            if s.cmd != MANIFEST_AUDIT_COMMAND {
                return Err(format!(
                    "manifest-audit node {tag} has unexpected invocation: {}",
                    s.cmd
                ));
            }
            (
                "exact-tree-manifest-audit".to_string(),
                MANIFEST_AUDIT_COMMAND.to_string(),
            )
        } else if [
            "pre.reverie_pin",
            "check.reverie_pin",
            "privileged-check.reverie_pin",
        ]
        .contains(&tag.as_str())
            && s.cmd.contains("ci/run-reverie-pin-check.sh")
        {
            // The preflight spells the repository explicitly while lane nodes
            // rely on the same root cwd. They invoke the same single pin
            // authority; retaining the preflight observation also keeps
            // `reverie_pin_current` evidence-derived.
            (
                "reverie-pin-authority".to_string(),
                "current-repository-pin".to_string(),
            )
        } else {
            (s.job.clone(), s.cmd.clone())
        };
        match seen.get(&key) {
            Some(surv) => {
                remap.insert(s.tag(), surv.clone());
                // A duplicate's own dependencies are part of what made it
                // correct; they are unioned into the survivor below, never
                // dropped. See `inherited`.
                inherited.push((surv.clone(), s.deps.clone()));
                removed.push(s.tag());
            }
            None => {
                seen.insert(key, s.tag());
                keep.push(s);
            }
        }
    }
    // Repoint dependents of a removed node onto its survivor.
    for s in keep.iter_mut() {
        for d in s.deps.iter_mut() {
            if let Some(t) = remap.get(d) {
                *d = t.clone();
            }
        }
    }

    // Union each removed duplicate's OWN dependencies into its survivor.
    //
    // Discarding them is what made a cold `validate full` impossible. The
    // preflight `gate.manifest` and each lane's `e2e.metadata` run the identical
    // `test-harness validate` tree audit, but only the lane copy carries the edge
    // to `setup.manifest_plan`, which BUILDS `target/debug/test-harness`. The
    // preflight copy is pushed first so it always survived, its dependents were
    // repointed, and the builder edge was silently deleted. Measured on a cold
    // tree before this fix: `gate.manifest` died at node 3 of 59 with
    // `exit 127: target/debug/test-harness: No such file or directory`, skipping
    // the other 56, in 9.5 s. On a warm tree it did not fail -- it audited the
    // tree with whatever STALE binary an earlier build had left behind, which is
    // arguably worse, since the gate then vouches for a commit it never read.
    let mut gained: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (survivor, deps) in inherited {
        for dep in deps {
            // An inherited dependency may itself name a removed duplicate.
            let dep = remap.get(&dep).cloned().unwrap_or(dep);
            if dep != survivor && dep != gate_dep {
                gained.entry(survivor.clone()).or_default().insert(dep);
            }
        }
    }
    for s in keep.iter_mut() {
        if let Some(extra) = gained.get(&s.tag()) {
            s.deps.extend(extra.iter().cloned());
        }
    }

    // Break the reverse edge the union would otherwise close into a cycle.
    //
    // `validate_plan::lane_nodes` hangs every dependency-less lane node off the
    // manifest gate to reproduce the fail-fast ordering. That injection is wrong
    // for the gate's OWN producer: `setup.manifest_plan` ships with no
    // dependencies, so it acquires `gate.manifest`, and unioning the audit's
    // builder edge back in would make the two depend on each other. A node the
    // survivor now depends on cannot also depend on the survivor; the shipped
    // lane files declare no such edge, so the only one dropped here is that
    // injected fail-fast edge.
    let mut drop_edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (survivor, extra) in &gained {
        for dep in extra {
            drop_edges
                .entry(dep.clone())
                .or_default()
                .insert(survivor.clone());
        }
    }
    for s in keep.iter_mut() {
        if let Some(drop) = drop_edges.get(&s.tag()) {
            s.deps.retain(|d| !drop.contains(d));
        }
        s.deps.sort();
        s.deps.dedup();
    }

    // Fail closed on a cycle. Nothing downstream detects one: neither
    // `scripts/validate.rs` nor `dagrun` topologically checks the
    // graph, so a cycle would present as nodes that silently never run -- the
    // same "56 skipped" shape this function just stopped producing, but with no
    // failing node to name. Since unioning dependencies is precisely the
    // operation that can close a cycle, the check belongs here.
    if let Some(stuck) = first_dependency_cycle(&keep) {
        return Err(format!(
            "node deduplication produced a dependency cycle among: {}",
            stuck.join(", ")
        ));
    }

    *steps = keep;
    Ok(removed)
}

/// Return the tags that cannot be topologically ordered, or `None` when the
/// graph is a DAG. Dependencies naming absent nodes are ignored here; the
/// runner reports those separately.
fn first_dependency_cycle(steps: &[Step]) -> Option<Vec<String>> {
    let present: BTreeSet<String> = steps.iter().map(|s| s.tag()).collect();
    let mut pending: BTreeMap<String, BTreeSet<String>> = steps
        .iter()
        .map(|s| {
            let deps = s
                .deps
                .iter()
                .filter(|d| present.contains(*d))
                .cloned()
                .collect();
            (s.tag(), deps)
        })
        .collect();
    loop {
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(tag, _)| tag.clone())
            .collect();
        if ready.is_empty() {
            return (!pending.is_empty()).then(|| pending.keys().cloned().collect());
        }
        for tag in ready {
            pending.remove(&tag);
            for deps in pending.values_mut() {
                deps.remove(&tag);
            }
        }
    }
}

/// Heavy compatibility preparation is the innermost bound in the validation ladder:
///
/// `420 prep < 480 gate clamp < 600 whole run < 660 local scope < 720 node < 900 job`.
///
/// A 3600s preparation allowance inside a 900s job was unreachable by
/// construction. This bound fires while the scheduler can still name the node
/// and flush its profile row.
const COMPAT_DIAGNOSTIC_WALL_S: i64 = 420;

fn build_release_hermit_node(gate: &str, bin: &str) -> dagrun::model::Step {
    let default = bin.ends_with("target/release/hermit");
    let cmd = if default {
        "cargo build --release -p hermit --features third-party-backends".to_string()
    } else {
        // A caller-supplied binary is reused rather than rebuilt, but it must
        // exist: silently proceeding with a missing binary would fail every row
        // for a reason that has nothing to do with compatibility.
        format!("test -x {}", validate_plan::shell_quote(bin))
    };
    let mut step = step_with_caps(
        "compatprep",
        "hermit_release",
        "Release Hermit for compatibility",
        cmd,
        vec![gate.to_string()],
        COMPAT_DIAGNOSTIC_WALL_S,
        COMPAT_DIAGNOSTIC_WALL_S * 2,
        16 * 1024 * 1024 * 1024,
    );
    if default {
        // A fresh validation checkout has no target cache. Leaving this build
        // undeclared makes the runner box Cargo to one core; that measured
        // 420s and timed out before finishing at be4c0905. The full profile's
        // established eight-job release build completed in 80s on the same
        // host. Declare that width here instead of widening any timeout.
        step.hint.classification = dagrun::model::StepClass::CpuBound;
        step.hint.preferred_inner_jobs = Some(8);
    }
    step
}

fn prepare_fixtures_node(_tag: &str, fixtures: &Path) -> dagrun::model::Step {
    prepare_fixtures_node_dep(_tag, fixtures, "compatprep.hermit_release")
}

/// The functional-fixture prep node, with an explicit predecessor.
///
/// The `super` suite already builds a release Hermit under its own tag, so it
/// hangs the fixtures off THAT node instead of adding a second identical build.
fn prepare_fixtures_node_dep(
    _tag: &str,
    fixtures: &Path,
    dep: &str,
) -> dagrun::model::Step {
    step_with_caps(
        "compatprep",
        "fixtures",
        "Functional compatibility fixtures",
        format!(
            "./tests/compat/prepare_real_compat_fixtures.sh {}",
            validate_plan::shell_quote(&fixtures.to_string_lossy())
        ),
        vec![dep.to_string()],
        COMPAT_DIAGNOSTIC_WALL_S,
        COMPAT_DIAGNOSTIC_WALL_S,
        4 * 1024 * 1024 * 1024,
    )
}

/// `require_e9patch_artifacts`' files-only NSS fixture (validate.sh:4095): keeps
/// host identity-daemon races out of the e9patch compatibility measurement.
fn nsswitch_fixture_node(path: &Path) -> dagrun::model::Step {
    let entries = [
        "aliases", "automount", "ethers", "group", "gshadow", "hosts", "initgroups", "netgroup",
        "netmasks", "networks", "passwd", "protocols", "publickey", "rpc", "services", "shadow",
    ]
    .iter()
    .map(|k| format!("{k}: files"))
    .collect::<Vec<_>>()
    .join("\\n");
    step_with_caps(
        "compatprep",
        "nsswitch",
        "e9patch files-only NSS fixture",
        format!(
            "mkdir -p $(dirname {p}) && printf '{entries}\\n' > {p}",
            p = validate_plan::shell_quote(&path.to_string_lossy())
        ),
        vec![],
        60,
        30,
        512 * 1024 * 1024,
    )
}

// `shard_node` used to live here: it wrapped the selected node in a synthetic
// `shard.*` step whose command was `./ci/run-node.sh`, nesting a second
// dagrun under this one. See the `Focused::Only` branch in
// `build_plan` for why that broke `--only` and what replaced it. `ci/run-node.sh`
// itself is UNCHANGED and still serves the hosted GitHub fan-out, which really
// does need a standalone runner per shard job.

fn step_with_caps(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem: i64,
) -> dagrun::model::Step {
    dagrun::model::Step {
        group: group.into(),
        job: job.into(),
        desc: desc.into(),
        description: String::new(),
        cmd,
        deps,
        env: BTreeMap::new(),
        hint: dagrun::model::ResourceHint {
            rss_baseline_bytes: Some(mem),
            hard_mem_max_bytes: Some(mem),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        // Undeclared, as these nodes were before the runner grew the fields. See
        // validate_plan::node for why this is not `Some(vec![])`.
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    }
}

// --------------------------------------------------------------------------- reporting

/// A completed node uses EX_TEMPFAIL to say that it could not determine its
/// condition. This is deliberately the only nonzero code that is not a product
/// failure; every other nonzero remains loud.
const NO_RESULT_EXIT_CODE: i64 = 75;

fn outcome_is_no_result(outcome: &StepOutcome) -> bool {
    !outcome.aborted && outcome.returncode == Some(NO_RESULT_EXIT_CODE)
}

fn outcome_is_failure(outcome: &StepOutcome) -> bool {
    !outcome.ok && !outcome.aborted && !outcome_is_no_result(outcome)
}

/// The blocking-failure headline: the count and the names, from ONE collection.
///
/// ⚠️ THIS EXISTS BECAUSE THE COUNT AND THE LIST DISAGREED IN PRODUCTION. On the
/// owner's run at `4e168f2aa5b9` the verdict read `9 blocking failure(s):` and
/// then named EIGHT — the list was built with a bare `.take(8)` while the count
/// came from `.count()` on the same filter. The dropped node,
/// `test.sabre_examples`, is not an excused cell, so an operator who fixed the
/// eight they were shown would re-run into a red nobody had named.
///
/// Pure, and returns both halves together, so a caller cannot print one without
/// the other and `summary_listing_bracket` can pin every case without a DAG.
fn blocking_listing<'a>(
    outcomes: &'a [StepOutcome],
    nonblocking: &BTreeSet<String>,
    effective_failures: usize,
) -> (Vec<&'a str>, String) {
    let named: Vec<&str> = outcomes
        .iter()
        .filter(|o| outcome_is_failure(o) && !nonblocking.contains(&o.tag))
        .map(|o| o.tag.as_str())
        .collect();
    // A cap is defensible; a SILENT cap is not. Name the remainder as a number.
    const NAMED_CAP: usize = 12;
    let shown = named.len().min(NAMED_CAP);
    let elided = named.len() - shown;
    let mut listing = if named.is_empty() {
        String::new()
    } else {
        format!(
            ": {}{}",
            named[..shown].join(", "),
            if elided == 0 {
                String::new()
            } else {
                format!(" (+{elided} more, see the node table above)")
            }
        )
    };
    // ⚠️ A HEADLINE LARGER THAN ITS OWN LIST MEANS SOMETHING BLOCKING IS
    // UNCOUNTABLE FROM THIS SET, and that is how a timed-out node vanished from
    // this run. `effective_failures` legitimately exceeds `named` for the compat
    // and super profiles, which add their own blocking rows — so this does not
    // refuse, it SAYS SO. Silence was the only unacceptable option.
    if effective_failures > named.len() {
        listing.push_str(&format!(
            " ⚠️ {} counted blocking node(s) are NOT NAMEABLE from the failure set; \
             the node table is authoritative",
            effective_failures - named.len()
        ));
    }
    // ⚠️ AND THE OTHER DIRECTION, WHICH IS THE ONE THAT ACTUALLY BIT US: A NODE
    // THAT IS NOT OK AND IS NOT A `FAILURE` EITHER.
    //
    // On `4e168f2aa5b9` TEN nodes printed `✗ FAIL` and the headline said NINE.
    // The tenth, `privileged-e2e.manifest_backend_parity_c`, hit its 120s wall.
    // A budget kill is neither `ok` nor a `failure` by `outcome_is_failure`, and
    // its reason did not match the budget prefixes either, so it was invisible to
    // the failure count AND to `timed_out_nodes` — it had no class at all, and a
    // state with no value for it is a state that does not get reported.
    //
    // ⚠️ IT IS DELIBERATELY NOT FOLDED INTO THE FAILURE COUNT. A timeout is "ran
    // and produced no verdict", and calling it a failure would assert a product
    // claim the run never established — the same distinction, destroyed in the
    // other direction. It gets its OWN count and its OWN names, which is what
    // "not a pass, not a failure, and not nothing" requires.
    let unclassified: Vec<&str> = outcomes
        .iter()
        .filter(|o| !o.ok && !nonblocking.contains(&o.tag))
        .map(|o| o.tag.as_str())
        .filter(|tag| !named.contains(tag))
        .collect();
    if !unclassified.is_empty() {
        listing.push_str(&format!(
            " ⚠️ plus {} node(s) that did NOT pass and produced NO VERDICT (budget kill, \
             abort, or an unclassified exit) and are therefore in NEITHER the count above \
             nor any failure class: {}",
            unclassified.len(),
            unclassified.join(", ")
        ));
    }
    (named, listing)
}

/// Cap a refusal's item list and NAME THE REMAINDER.
///
/// ⚠️ A CAP IS DEFENSIBLE; A SILENT CAP IS NOT. Every caller here prints a count
/// taken from `.len()` and then the list. With more offenders than the cap, the
/// operator is told a number, shown fewer, and told nothing about the
/// difference -- so the refusal understates the very thing it exists to report,
/// and it understates it in the direction of "less work to do".
///
/// `blocking_listing` above fixed this shape at the blocking-failure headline
/// (hermit#2636). The class was three, not one: the two `RunSummary::refused(3, ..)`
/// sites for ungrantable resources and node-vs-whole-run budgets carried the same
/// bare `.take(8)`. This is the shared cap-and-declare those sites use, so the
/// next one cannot be added without going through a function whose whole purpose
/// is to state the remainder.
const REFUSAL_ITEM_CAP: usize = 8;

fn capped_refusal_items(items: Vec<String>) -> Vec<String> {
    let total = items.len();
    let shown = total.min(REFUSAL_ITEM_CAP);
    let elided = total - shown;
    let mut out: Vec<String> = items.into_iter().take(shown).collect();
    if elided != 0 {
        out.push(format!("  (+{elided} more not shown)"));
    }
    out
}

/// Pin the count-versus-enumeration invariant that broke on `4e168f2aa5b9`.
///
/// The regression it exists to catch is a cap that drops names silently. Case 2
/// is the exact production shape: NINE blocking failures, of which the old
/// `.take(8)` named eight and said nothing about the ninth.
fn summary_listing_bracket() -> Result<String, String> {
    let row = |tag: &str, ok: bool| StepOutcome {
        tag: tag.to_string(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        returncode: Some(if ok { 0 } else { 1 }),
        reason: String::new(),
        aborted: false,
    };
    let none: BTreeSet<String> = BTreeSet::new();

    // 1. Every failure is named when the set is small.
    let small: Vec<StepOutcome> = (0..3).map(|i| row(&format!("n{i}"), false)).collect();
    let (named, listing) = blocking_listing(&small, &none, 3);
    if named.len() != 3 || !listing.contains("n2") {
        return Err(format!("small set lost a name: {listing}"));
    }

    // 2. THE PRODUCTION CASE. Nine failures must yield nine names, not eight.
    let nine: Vec<StepOutcome> = (0..9).map(|i| row(&format!("f{i}"), false)).collect();
    let (named, listing) = blocking_listing(&nine, &none, 9);
    if named.len() != 9 {
        return Err(format!("nine failures produced {} names", named.len()));
    }
    for i in 0..9 {
        if !listing.contains(&format!("f{i}")) {
            return Err(format!("f{i} was counted but not named: {listing}"));
        }
    }

    // 3. Above the cap the remainder is STATED, never dropped in silence.
    let many: Vec<StepOutcome> = (0..15).map(|i| row(&format!("m{i}"), false)).collect();
    let (named, listing) = blocking_listing(&many, &none, 15);
    if named.len() != 15 || !listing.contains("(+3 more") {
        return Err(format!("cap did not declare its remainder: {listing}"));
    }

    // 4. A headline larger than its own list ANNOUNCES the gap. This is the
    //    timed-out-node shape: counted somewhere, nameable from nothing.
    let (_n, listing) = blocking_listing(&nine, &none, 10);
    if !listing.contains("NOT NAMEABLE") {
        return Err(format!("count exceeding the list was not announced: {listing}"));
    }

    // 5. THE OTHER PRODUCTION SHAPE: ten nodes not ok, nine classified as
    //    failures, one a budget kill. The tenth must be COUNTED AND NAMED in its
    //    own right, and must NOT be silently absorbed into the failure count.
    let mut ten = nine.clone();
    let mut killed = row("privileged-e2e.manifest_backend_parity_c", false);
    killed.aborted = true; // a budget kill: not ok, and not a `failure` either
    ten.push(killed);
    let (named, listing) = blocking_listing(&ten, &none, 9);
    if named.len() != 9 {
        return Err(format!("budget kill was absorbed into the failure count: {}", named.len()));
    }
    if !listing.contains("NO VERDICT") || !listing.contains("manifest_backend_parity_c") {
        return Err(format!("a node that did not pass went unreported: {listing}"));
    }

    // 6. A nonblocking row is excluded from BOTH halves, not just one.
    let excused: BTreeSet<String> = BTreeSet::from(["f0".to_string()]);
    let (named, listing) = blocking_listing(&nine, &excused, 8);
    if named.len() != 8 || listing.contains("f0") {
        return Err(format!("nonblocking row leaked into the list: {listing}"));
    }

    // 7. THE OTHER TWO INSTANCES OF THE SAME SHAPE. `capped_refusal_items` is
    //    what the two `RunSummary::refused(3, ..)` sites use; pin it here so the
    //    class is bracketed in one place rather than at each call site.
    //
    //    ⚠️ CONTROL FIRST, AND IT MUST NOT ELIDE. Without a case that stays
    //    whole, a helper that appended "(+N more)" unconditionally -- or one
    //    that dropped everything -- would satisfy every remaining assertion.
    let exactly_at_cap: Vec<String> =
        (0..REFUSAL_ITEM_CAP).map(|i| format!("  a{i}")).collect();
    let kept = capped_refusal_items(exactly_at_cap);
    if kept.len() != REFUSAL_ITEM_CAP || kept.iter().any(|l| l.contains("more not shown")) {
        return Err(format!(
            "a list exactly at the cap must be shown whole and unannotated: {kept:?}"
        ));
    }

    // 8. One over the cap: the remainder is STATED, and the arithmetic is right.
    let over: Vec<String> = (0..REFUSAL_ITEM_CAP + 1).map(|i| format!("  b{i}")).collect();
    let capped = capped_refusal_items(over);
    if capped.len() != REFUSAL_ITEM_CAP + 1 {
        return Err(format!("cap produced {} lines, want {}", capped.len(), REFUSAL_ITEM_CAP + 1));
    }
    if !capped.last().is_some_and(|l| l.contains("(+1 more not shown)")) {
        return Err(format!("one over the cap did not declare its remainder: {capped:?}"));
    }

    // 9. Well over the cap: the elided count is total-minus-shown, not a guess.
    let far_over: Vec<String> = (0..REFUSAL_ITEM_CAP + 7).map(|i| format!("  c{i}")).collect();
    let capped = capped_refusal_items(far_over);
    if !capped.last().is_some_and(|l| l.contains("(+7 more not shown)")) {
        return Err(format!("elided count is wrong: {capped:?}"));
    }
    //    And nothing above the cap is silently retained OR silently dropped: the
    //    shown items must be the FIRST ones, in order.
    if capped.first().map(String::as_str) != Some("  c0") {
        return Err(format!("cap did not keep the first items in order: {capped:?}"));
    }

    // 10. The empty case says nothing at all -- no "(+0 more)" noise.
    if !capped_refusal_items(Vec::new()).is_empty() {
        return Err("an empty list must produce no lines".to_string());
    }

    Ok("summary listing: count and enumeration agree across 10 cases (the 9-failure \
shape named in full, the cap states its remainder, a budget kill is counted \
and named without being folded into the failure count, and the two refusal \
sites' shared cap is whole at the cap, declares +1 and +7 above it, and is \
silent when empty)"
        .to_string())
}

fn ledger_gate_result(outcome: &StepOutcome) -> &'static str {
    if outcome.ok {
        "pass"
    } else if outcome_is_no_result(outcome) {
        "no_result"
    } else {
        "fail"
    }
}

fn ledger_run_results(
    exit_code: u8,
    failures: usize,
    no_results: usize,
    interrupted: bool,
) -> (&'static str, &'static str) {
    let raw = if exit_code == 0 && failures == 0 && no_results == 0 {
        "pass"
    } else {
        "fail"
    };
    let result = if failures > 0 {
        "fail"
    } else if interrupted
        || (exit_code == NO_RESULT_EXIT_CODE as u8 && no_results > 0)
    {
        "no_result"
    } else {
        raw
    };
    (raw, result)
}

fn completed_exit_code(
    effective_failures: usize,
    no_results: usize,
    run_timed_out: bool,
    unexplained_runner_failure: bool,
) -> u8 {
    if effective_failures > 0 || run_timed_out || unexplained_runner_failure {
        1
    } else if no_results > 0 {
        NO_RESULT_EXIT_CODE as u8
    } else {
        0
    }
}

/// Per-node cost table, built entirely from typed `StepOutcome` fields.
fn print_cost_table(
    outcomes: &[StepOutcome],
    skipped: &[String],
    host_inapplicable: &[validate_plan::HostInapplicableNode],
) {
    println!("\n=== per-node cost (dagrun) ===");
    println!("{:<44} {:>9}  {:<8} reason/returncode", "node", "seconds", "status");
    println!("{}", "-".repeat(84));
    let mut total = 0.0_f64;
    for o in outcomes {
        total += o.duration_s;
        let status = if o.ok {
            "ok"
        } else if o.aborted {
            "ABORTED"
        } else if outcome_is_no_result(o) {
            "NO_RESULT"
        } else {
            "FAIL"
        };
        let detail = if !o.reason.is_empty() {
            o.reason.clone()
        } else if let Some(rc) = o.returncode {
            if rc < 0 {
                format!("signal {}", -rc)
            } else {
                format!("rc {rc}")
            }
        } else {
            String::new()
        };
        println!("{:<44} {:>9.2}  {:<8} {}", o.tag, o.duration_s, status, detail);
    }
    println!("{}", "-".repeat(84));
    println!("{:<44} {:>9.2}  (sum of node wall)", "TOTAL", total);
    if !skipped.is_empty() {
        println!("\nskipped (dependency failed, never ran): {}", skipped.join(", "));
    }
    // Listed AFTER the TOTAL and outside the ok/FAIL/ABORTED column on purpose:
    // a host-inapplicable node has no status in that vocabulary. It did not
    // pass, and printing it in a table of statuses is how it would come to look
    // like one.
    for node in host_inapplicable {
        println!(
            "\nhost-inapplicable (NOT RUN, NOT a pass, no coverage): {} — this machine lacks {} \
             ({})",
            node.tag,
            node.capability.value(),
            node.evidence
        );
    }
}

/// Per-program compatibility summary, built from typed node outcomes rather than
/// a scraped TSV. Reproduces `print_compatibility_summary`'s category table.
fn print_compat_summary(mode: CompatMode, outcomes: &[StepOutcome]) -> (usize, usize, Vec<String>) {
    compat_summary_with_tables(
        mode,
        outcomes,
        &validate_corpus::known_failclosed(),
        &validate_corpus::portable_diagnostic(),
    )
}

/// The real summary body, with its two policy tables passed in.
///
/// Production calls it through [`print_compat_summary`] with the REAL tables, so nothing is
/// weakened; the tables are parameters purely so a bracket can exercise this exact code against
/// a planted table. That matters here because the shipped `known_failclosed()` currently holds a
/// single row, which is not enough to distinguish "listed and blocking" from "listed and
/// exempt" in one run -- and the fix for that must not be to add a fake row to production.
fn compat_summary_with_tables(
    mode: CompatMode,
    outcomes: &[StepOutcome],
    known: &BTreeMap<&'static str, &'static str>,
    diag: &BTreeMap<&'static str, &'static str>,
) -> (usize, usize, Vec<String>) {
    let mut per_cat: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut passed = 0usize;
    let mut measured = 0usize;
    let mut blocking_failures: Vec<String> = Vec::new();
    let mut measured_labels: BTreeSet<String> = BTreeSet::new();
    for o in outcomes {
        let Some(label) = o.tag.strip_prefix("compat.") else { continue };
        if outcome_is_no_result(o) {
            println!(
                "  NO_RESULT {label} could not determine its condition; excluded from the measured denominator"
            );
            continue;
        }
        let cat = validate_corpus::category_of(label);
        let e = per_cat.entry(cat).or_insert((0, 0));
        e.1 += 1;
        measured += 1;
        measured_labels.insert(label.to_string());
        if o.ok {
            e.0 += 1;
            passed += 1;
        }
        // ONE decision, read twice: once for what to print and once for whether the row
        // blocks. Before this the two were separate arms of the same `if`, which is how a
        // reporting change can silently become an exemption.
        // `display_name()` deliberately renders Strict and PortableStrict identically, but these
        // two modes treat a listed row in OPPOSITE ways, so the message must distinguish them or
        // the reader cannot tell an exemption from a blocking report.
        let mode_label = match mode {
            CompatMode::Strict => "--strict",
            CompatMode::PortableStrict => "--portable-strict",
            other => other.display_name(),
        };
        let disposition = validate_plan::classify_compat_outcome(
            mode,
            o.ok,
            known.contains_key(label),
            diag.contains_key(label),
        );
        match disposition {
            CompatDisposition::Passed => {}
            CompatDisposition::PassedButListedFailClosed => {
                println!(
                    "  WARN {label} passed but is listed as known fail-closed under {} \
                     ({}); the EXPECTATION is STALE -- drop it from the known-failure table",
                    mode_label,
                    known[label]
                );
            }
            CompatDisposition::KnownFailClosedExempt => {
                println!(
                    "  WARN {label} known fail-closed under --strict ({}; nonblocking)",
                    known[label]
                );
            }
            CompatDisposition::KnownFailClosedBlocking => {
                // Reported AND still blocking. Naming the reason is not excusing the failure:
                // the row is pushed onto `blocking_failures` below exactly as an unlisted
                // failure would be.
                println!(
                    "  FAIL {label} known fail-closed under {} ({}); STILL BLOCKING -- \
                     this mode does not exempt listed rows",
                    mode_label,
                    known[label]
                );
            }
            CompatDisposition::PortableDiagnostic => {
                println!("  WARN {label} is a bounded portable diagnostic: {}", diag[label]);
            }
            CompatDisposition::Blocking => {}
        }
        if disposition.is_blocking() {
            blocking_failures.push(label.to_string());
        }
    }
    // AUDIT: a listed row the selected corpus never measured. Such a row is silently carried
    // forever -- it can neither fail (nothing ran it) nor be reported stale (it never passed),
    // so the table grows entries no run can retire. Naming them is reporting only; it changes
    // no verdict.
    if matches!(mode, CompatMode::Strict | CompatMode::PortableStrict) {
        let unmeasured: Vec<&str> = known
            .keys()
            .copied()
            .filter(|label| !measured_labels.contains(*label))
            .collect();
        if !unmeasured.is_empty() {
            println!(
                "  WARN {} known fail-closed row(s) not measured by this corpus, so no run can \
                 confirm or retire them: {}",
                unmeasured.len(),
                unmeasured.join(", ")
            );
        }
    }
    println!("\nCOMPATIBILITY SUMMARY ({measured} measured programs, mode {})", mode.display_name());
    println!("{:<22} | {:>8} | {:>9}", "Category", "Programs", "passing");
    println!("{}", "-".repeat(46));
    for cat in validate_corpus::CATEGORIES {
        if let Some((p, m)) = per_cat.get(cat) {
            println!("{cat:<22} | {m:>8} | {:>9}", format!("{p}/{m}"));
        }
    }
    println!("{}", "-".repeat(46));
    println!("{:<22} | {measured:>8} | {:>9}", "TOTAL", format!("{passed}/{measured}"));
    println!("P/M means passing/measured; failures are M-P. Unmeasured rows are excluded from M.");
    if mode == CompatMode::Rr {
        // Name the rows deliberately EXCLUDED from the R/R ratchet. A denominator
        // that silently drops five known divergences reads as full coverage.
        let excluded = validate_corpus::rr_known_failures();
        println!(
            "R/R ratchet excludes {} program(s) measured to diverge on replay:",
            excluded.len()
        );
        for (label, why) in &excluded {
            println!("  - {label}: {why}");
        }
    }
    (passed, measured, blocking_failures)
}

/// Conditions that must FAIL a run whatever the ratchet's own arithmetic says,
/// each naming itself so the refusal is readable in the summary.
///
/// The defect this closes, measured 2026-08-08 on `--portable-strict-compat-only`
/// at hermit 0f90722a6: `compatprep.hermit_release` FAILED (it is only
/// `test -x <bin>`), all 188 `compat.*` rows were skipped as dependents, and the
/// run printed `✅ validate PASS (exit 0) — every blocking gate passed` over a
/// `COMPATIBILITY SUMMARY (0 measured programs)`. The cause was structural: for a
/// compat profile the verdict was `effective_failures = compat_blocking` ALONE, so
/// a failure in the build/prep/gate spine — precisely the thing that empties the
/// matrix — contributed nothing, and an empty matrix has no failing rows to count.
/// A ratchet may narrow WHICH measured rows are allowed to fail; it may never
/// decide whether any measurement happened.
///
/// Pure, so `--self-test` can bracket both directions without running a DAG.
fn verdict_refusals(
    compat_measured: Option<usize>,
    structural_failures: usize,
    executed_tests: Option<i64>,
) -> Vec<String> {
    let mut out = Vec::new();
    if structural_failures > 0 {
        out.push(format!(
            "{structural_failures} node(s) OUTSIDE the measured matrix failed; a spine failure \
             empties the matrix and can never be excused by the matrix's own ratchet"
        ));
    }
    // `Some(0)` is a MEASURED zero and is fatal; `None` is unknown and is handled
    // as a NON-VERDICT elsewhere. Conflating the two would turn every profile
    // that reports no count into a red.
    if compat_measured == Some(0) {
        out.push(
            "the compatibility matrix measured ZERO programs; an empty matrix is not a pass"
                .to_string(),
        );
    }
    if executed_tests == Some(0) {
        out.push(
            "ZERO tests executed; a run that executed nothing cannot certify anything".to_string(),
        );
    }
    out
}

/// Execution completeness applies after profile-specific failure policy. A
/// profile may allow a fully measured failing row, but no profile may turn a
/// partial run into exit zero.
fn exit_code_with_execution_completeness(exit_code: u8, execution_complete: bool) -> u8 {
    if execution_complete { exit_code } else { exit_code.max(1) }
}

/// Fast exit 127 is useful missing-artifact guidance only in `--only`, whose
/// documented contract deliberately drops build dependencies. It is never a
/// verdict override and is not inferred for full or other focused profiles.
fn possible_missing_artifact_nodes<'a>(
    selection_mode: &str,
    outcomes: &'a [StepOutcome],
) -> Vec<&'a str> {
    if selection_mode != "only" {
        return Vec::new();
    }
    outcomes
        .iter()
        .filter(|outcome| {
            !outcome.ok && outcome.returncode == Some(127) && outcome.duration_s < 5.0
        })
        .map(|outcome| outcome.tag.as_str())
        .collect()
}

/// Two-sided bracket for withholding a manifest bucket NODE whose entire cell
/// population is withheld.
///
/// Local and side-effect-free: it combines planted capability verdicts with the
/// checked-in manifests and production plan construction, but runs no probe or
/// scheduler and does not depend on which machine runs it. The load-bearing case
/// is UN-WITHHOLDING: give the same bucket one runnable cell back and the node
/// must run again, with no code change anywhere. That is the
/// difference between a computed decision and a hard-coded list of nodes that
/// would silently swallow a cell added later.
fn node_vacuity_bracket(root: &Path) -> Result<(), String> {
    let bucket = |selected: usize, withheld: usize| BucketCells {
        lane: "privileged".into(),
        category: "backend-parity-c".into(),
        selected,
        withheld,
        capabilities: if withheld > 0 {
            vec!["cpuid-faulting".into()]
        } else {
            Vec::new()
        },
    };

    // POSITIVE — the bucket's only cell is withheld, so its node has nothing at
    // all left to run.
    if !bucket_runs_nothing(&bucket(1, 1)) {
        return Err("node vacuity: a bucket whose every selected cell is withheld must \
                    withhold its node"
            .into());
    }
    // ...and the same at a larger size, so the rule is not "exactly one cell".
    if !bucket_runs_nothing(&bucket(78, 78)) {
        return Err("node vacuity: an all-withheld bucket of 78 cells must withhold its node".into());
    }

    // THE UN-WITHHOLDING PROOF. One runnable cell in the bucket and the node
    // runs, however many of its siblings are withheld. Nothing is edited to make
    // this happen: the same predicate over a changed cell population produces
    // the opposite answer.
    for (selected, withheld) in [(2usize, 1usize), (79, 78), (100, 99)] {
        if bucket_runs_nothing(&bucket(selected, withheld)) {
            return Err(format!(
                "node vacuity: a bucket with {} runnable cell(s) left ({selected} selected, \
                 {withheld} withheld) must still RUN its node; withholding it would silently \
                 swallow the runnable cells",
                selected - withheld
            ));
        }
    }

    // NEGATIVE — nothing withheld at all.
    for selected in [1usize, 5, 78] {
        if bucket_runs_nothing(&bucket(selected, 0)) {
            return Err("node vacuity: a bucket with nothing withheld must run its node".into());
        }
    }

    // NEGATIVE, AND SEPARATELY LOAD-BEARING — an EMPTY bucket is the
    // pre-existing `empty-manifest-bucket` condition, not this one. Without the
    // `selected > 0` guard, `0 == 0` would make every empty bucket read as
    // host-inapplicable and quietly inflate the omission count.
    if bucket_runs_nothing(&bucket(0, 0)) {
        return Err("node vacuity: a bucket that selected NO cells is empty-manifest-bucket, \
                    never host-inapplicable"
            .into());
    }

    // THE NODE-TO-BUCKET BINDING, from the node's own command. A command with
    // any selection token this function does not model must NOT be matched
    // against the bucket accounting.
    let mut transformed =
        validate_plan::lane_nodes(root, "privileged", "privileged-", "gate.manifest")?;
    let withheld_tag = "privileged-e2e.manifest_backend_parity_c";
    let shipped = transformed
        .iter()
        .find(|step| step.tag() == withheld_tag)
        .ok_or("node vacuity: transformed lane lost privileged backend-parity-c bucket")?
        .cmd
        .clone();
    if manifest_bucket_of(&shipped)
        != Some(("privileged".to_string(), "backend-parity-c".to_string()))
    {
        return Err(format!(
            "node vacuity: the shipped bucket command must bind to its bucket; got {:?}",
            manifest_bucket_of(&shipped)
        ));
    }
    let unmodelled = [
        // A narrower selection than the accounting was taken with.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --mode verify --results r --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --backend ptrace --results r --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --test backend-parity-c/cpuid-probe --results r --junit j",
        // A WIDER selection than the accounting was taken with.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --include-occasional --results r --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --results r --junit j",
        // Output paths are required exactly once, and each must have a value.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r1 --results r2 --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r --junit j1 --junit j2",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results --junit j",
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --results r --junit",
        // Unknown tokens remain fail-closed even with the required output pair.
        "target/debug/test-harness run --lane privileged --category backend-parity-c --ci-only --future-selector value --results r --junit j",
        // Not a bucket run at all.
        "target/debug/test-harness validate",
        "target/debug/test-harness build --lane privileged --ci-only --allow-empty",
        "cargo test -p hermit-detcore",
    ];
    for cmd in unmodelled {
        if manifest_bucket_of(cmd).is_some() {
            return Err(format!(
                "node vacuity: {cmd:?} selects a cell population this function cannot prove \
                 equal to the bucket accounting and must NOT be a withholding candidate"
            ));
        }
    }

    // THE CHECKED-IN ACCOUNTING — the required plan itself carries the host
    // capability metadata generated from the live YAML manifests. This must be
    // enough to withhold the one current privileged bucket without compiling or
    // invoking another validation driver before dagrun starts.
    let absent = BTreeMap::from([(
        validate_plan::HostCapability::CpuidFaulting,
        "planted absence".to_string(),
    )]);
    let parsed = read_bucket_cells(root, &absent)?;
    let privileged = parsed
        .iter()
        .find(|bucket| bucket.lane == "privileged" && bucket.category == "backend-parity-c")
        .ok_or("node vacuity: required plan lost the privileged backend-parity-c bucket")?;
    if privileged.selected != 1
        || !bucket_runs_nothing(privileged)
        || privileged.capabilities != vec!["cpuid-faulting".to_string()]
    {
        return Err(format!(
            "node vacuity: checked-in host-capability accounting is wrong: {privileged:?}"
        ));
    }
    let present = read_bucket_cells(root, &BTreeMap::new())?;
    if present.iter().any(|bucket| bucket.withheld != 0) {
        return Err(
            "node vacuity: no bucket may be withheld when no capability is absent".into(),
        );
    }

    // INTEGRATION: exercise the production transformed command, checked-in
    // bucket accounting, withholding mutation, and scorecard edge handling
    // together. This catches drift between the command producer and parser.
    attach_compatibility_scorecard(&mut transformed, &["privileged"])?;
    let scorecard_tag = "scorecard.compatibility";
    let before: BTreeMap<String, Vec<String>> = transformed
        .iter()
        .map(|step| (step.tag(), step.deps.clone()))
        .collect();
    if !before
        .get(scorecard_tag)
        .is_some_and(|deps| deps.iter().any(|dep| dep == withheld_tag))
    {
        return Err(
            "node vacuity: actual compatibility scorecard did not depend on the transformed \
             host-inapplicable bucket"
                .into(),
        );
    }
    let mut expected = before.clone();
    if expected.remove(withheld_tag).is_none() {
        return Err("node vacuity: transformed plan lost the expected bucket".into());
    }
    let scorecard_deps = expected
        .get_mut(scorecard_tag)
        .ok_or("node vacuity: transformed plan lost the compatibility scorecard")?;
    let scorecard_deps_before = scorecard_deps.len();
    scorecard_deps.retain(|dep| dep != withheld_tag);
    if scorecard_deps.len() + 1 != scorecard_deps_before {
        return Err("node vacuity: expected exactly one scorecard edge to the bucket".into());
    }
    let mut actual_plan = Plan {
        cfg: DagConfig {
            steps: transformed,
            ..Default::default()
        },
        ..Default::default()
    };
    withhold_vacuous_manifest_nodes(root, &mut actual_plan, &absent)?;
    let after: BTreeMap<String, Vec<String>> = actual_plan
        .cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step.deps.clone()))
        .collect();
    if after != expected {
        return Err(format!(
            "node vacuity: actual withholding changed more than the bucket and its scorecard \
             edge: expected={expected:?} actual={after:?}"
        ));
    }
    if actual_plan.host_inapplicable.len() != 1
        || actual_plan.host_inapplicable[0].tag != withheld_tag
        || actual_plan.host_inapplicable[0].capability
            != validate_plan::HostCapability::CpuidFaulting
    {
        return Err(format!(
            "node vacuity: actual transformed plan did not withhold exactly {withheld_tag}: {:?}",
            actual_plan.host_inapplicable
        ));
    }

    // THE RETAINED DEPENDENT. A result consumer keeps running with the edge
    // dropped; ANY other dependent refuses the whole run rather than having a
    // prerequisite quietly removed from under it.
    let gone: BTreeSet<String> = ["privileged-e2e.manifest_backend_parity_c".to_string()]
        .into_iter()
        .collect();
    let consumer = (
        "scorecard.compatibility".to_string(),
        "./ci/compat-envelope/scorecard.rs verify-results --results \"$E2E_RESULT_ROOT\" \
         --lanes portable,privileged"
            .to_string(),
        vec![
            "privileged-e2e.manifest_backend_parity_c".to_string(),
            "e2e.manifest_util_c".to_string(),
        ],
    );
    let prerequisite = (
        "test.something".to_string(),
        "cargo nextest run -p hermit-detcore".to_string(),
        vec!["privileged-e2e.manifest_backend_parity_c".to_string()],
    );
    let unrelated = (
        "lint.rustfmt".to_string(),
        "cargo fmt --all -- --check".to_string(),
        vec!["quick.build".to_string()],
    );
    let (droppable, refusals) =
        classify_withheld_dependents(&[consumer.clone(), unrelated.clone()], &gone);
    if droppable
        != vec![(
            "scorecard.compatibility".to_string(),
            "privileged-e2e.manifest_backend_parity_c".to_string(),
        )]
        || !refusals.is_empty()
    {
        return Err(format!(
            "node vacuity: exactly the result consumer's edge to the withheld node may be \
             dropped; got droppable={droppable:?} refusals={refusals:?}"
        ));
    }
    let (droppable, refusals) =
        classify_withheld_dependents(&[prerequisite.clone(), unrelated.clone()], &gone);
    if !droppable.is_empty() || refusals.len() != 1 {
        return Err(format!(
            "node vacuity: a NON-result-consuming dependent must REFUSE the run, never have its \
             prerequisite silently removed; got droppable={droppable:?} refusals={refusals:?}"
        ));
    }
    // Nothing withheld: no edge is touched and nothing refuses.
    let (droppable, refusals) =
        classify_withheld_dependents(&[consumer, prerequisite, unrelated], &BTreeSet::new());
    if !droppable.is_empty() || !refusals.is_empty() {
        return Err("node vacuity: with nothing withheld, no dependency edge may change".into());
    }

    println!(
        "  node vacuity: 2 withheld / 7 not-withheld (3 un-withholding, 3 nothing-withheld, \
         1 empty-bucket), 1 transformed command bound / 15 refused, accounting parser 1 good / \
         3 malformed, dependents 1 edge-dropped / 1 refusal / 1 inert, actual plan 1 bucket \
         withheld / 1 scorecard edge dropped / 0 other changes"
    );
    Ok(())
}

/// Two-sided bracket for the host-capability withholding decision.
///
/// Inert: it plants capability verdicts instead of probing, so it exercises the
/// decision on BOTH the "machine cannot run it" and the "machine can run it"
/// side without depending on which machine is running the bracket. The
/// load-bearing case is NEGATIVE 2: a node that is merely BROKEN must never be
/// withheld, whatever is absent.
fn host_capability_bracket(root: &Path) -> Result<(), String> {
    use validate_plan::HostCapability;
    let step = |group: &str, job: &str, deps: Vec<String>| dagrun::model::Step {
        group: group.into(),
        job: job.into(),
        desc: String::new(),
        description: String::new(),
        cmd: "true".into(),
        deps,
        env: BTreeMap::new(),
        hint: dagrun::model::ResourceHint::default(),
        networkonly: false,
        engine_only: false,
        timeout: 10,
        cpu_timeout: 10,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    };
    let requirements: BTreeMap<String, HostCapability> =
        [("cpuid.faulting".to_string(), HostCapability::CpuidFaulting)].into_iter().collect();
    let absent: BTreeMap<HostCapability, String> =
        [(HostCapability::CpuidFaulting, "planted".to_string())].into_iter().collect();
    let present: BTreeMap<HostCapability, String> = BTreeMap::new();
    let plan = || {
        vec![
            step("cpuid", "faulting", vec!["build.privileged_tests".into()]),
            step("test", "detcore_unit", vec!["build.privileged_tests".into()]),
            step("build", "privileged_tests", vec![]),
        ]
    };

    // POSITIVE — the declaring node, and only it, is withheld when the machine
    // provably lacks the capability.
    let (keep, gone) = validate_plan::partition_host_inapplicable(plan(), &requirements, &absent)?;
    if gone.len() != 1 || gone[0].tag != "cpuid.faulting" {
        return Err(format!(
            "host capability: exactly cpuid.faulting must be withheld, got {:?}",
            gone.iter().map(|n| n.tag.clone()).collect::<Vec<_>>()
        ));
    }
    if keep.len() != 2 {
        return Err("host capability: withholding one node must not remove any other".into());
    }

    // NEGATIVE 1 — with the capability present, nothing is withheld. Without
    // this the mechanism could be a blanket omission rather than a predicate.
    let (keep, gone) = validate_plan::partition_host_inapplicable(plan(), &requirements, &present)?;
    if !gone.is_empty() || keep.len() != 3 {
        return Err("host capability: a capable machine must run every planned node".into());
    }

    // NEGATIVE 2 — THE ONE THAT MATTERS. A node that declares NO capability is
    // never withheld, whatever is absent. This is what stops the mechanism from
    // being usable to excuse a node that is merely broken: a broken node has no
    // declaration, so it still runs, still fails, and is still refused.
    let undeclared: BTreeMap<String, HostCapability> = BTreeMap::new();
    let (keep, gone) = validate_plan::partition_host_inapplicable(plan(), &undeclared, &absent)?;
    if !gone.is_empty() || keep.len() != 3 {
        return Err(
            "host capability: an undeclared node was withheld; an absent capability must not \
             excuse a node that never claimed to need it"
                .into(),
        );
    }

    // NEGATIVE 3 — withholding a node that a RETAINED node depends on is a
    // refusal, not a silent cascade of unrun work.
    let mut dependent = plan();
    dependent.push(step("e2e", "needs_cpuid", vec!["cpuid.faulting".into()]));
    if validate_plan::partition_host_inapplicable(dependent, &requirements, &absent).is_ok() {
        return Err(
            "host capability: withholding a node with a retained dependent must REFUSE".into()
        );
    }

    // VOCABULARY — closed on both sides of the parse.
    if HostCapability::from_value("cpuid-faulting") != Some(HostCapability::CpuidFaulting) {
        return Err("host capability: the shipped capability name must parse".into());
    }
    if HostCapability::from_value("cpuid_faulting").is_some()
        || HostCapability::from_value("anything-at-all").is_some()
    {
        return Err("host capability: an unrecognized capability name must NOT parse".into());
    }

    // NON-VACUITY — the shipped DAG really does declare the requirement this
    // bracket is about, and every declaration in every lane parses. A bracket
    // that passed against an empty declaration set would prove nothing.
    let shipped = validate_plan::host_capability_requirements(root)?;
    if shipped.get("privileged-cpuid.faulting") != Some(&HostCapability::CpuidFaulting)
        || shipped.get("cpuid.faulting") != Some(&HostCapability::CpuidFaulting)
    {
        return Err(format!(
            "host capability: ci/dag/privileged.json must declare cpuid.faulting as requiring \
             cpuid-faulting under both the bare and fused tag; got {shipped:?}"
        ));
    }

    // THE PROBE'S OWN CONJUNCTION, bracketed with planted observations so it is
    // checked on a machine of either kind. Exactly ONE combination may read as
    // absent; every form of doubt must run the node.
    let absent_cases = [
        // (syscall, /proc/cpuinfo advertises cpuid_fault, must read absent)
        (Err(libc::ENODEV), Some(false), true),
        // The kernel accepted it: present however cpuinfo reads.
        (Ok(()), Some(false), false),
        (Ok(()), Some(true), false),
        // The two sources DISAGREE — doubt, so the node runs.
        (Err(libc::ENODEV), Some(true), false),
        // /proc/cpuinfo unreadable — doubt, so the node runs.
        (Err(libc::ENODEV), None, false),
        // A different errno is doubt about the PROBE, not proof about the
        // machine. EPERM is what a restricted sandbox returns.
        (Err(libc::EPERM), Some(false), false),
        (Err(libc::EINVAL), Some(false), false),
        // The fork/waitpid probe could not be completed at all.
        (Err(0), Some(false), false),
    ];
    for (syscall, advertised, want_absent) in absent_cases {
        if validate_plan::cpuid_faulting_absent(syscall, advertised) != want_absent {
            return Err(format!(
                "host capability: cpuid-faulting absence for (syscall={syscall:?}, \
                 cpuinfo={advertised:?}) must be {want_absent}; only a corroborated ENODEV may \
                 read as absent and every other shape must run the node"
            ));
        }
    }

    // The same conjunction for KVM, and the same rule: only a corroborated
    // ENOENT may read as absent. ⚠️ For this capability "doubt runs the node" is
    // only safe because the node also asserts its executed-test COUNT -- every
    // `run_kvm_` test self-guards on /dev/kvm and returns early, so a wrongly-run
    // node would report silent passes rather than a loud failure.
    let kvm_absent_cases: &[(Result<(), i32>, Option<bool>, bool)] = &[
        // The only shape that may read as absent: no device AND no vmx/svm.
        (Err(libc::ENOENT), Some(false), true),
        // The device opened: present, whatever /proc/cpuinfo says.
        (Ok(()), Some(false), false),
        (Ok(()), Some(true), false),
        // The two sources DISAGREE -- doubt, so the node runs.
        (Err(libc::ENOENT), Some(true), false),
        // /proc/cpuinfo unreadable -- doubt, so the node runs.
        (Err(libc::ENOENT), None, false),
        // A restricted sandbox or a permissions problem is doubt about the
        // PROBE, not proof the machine lacks KVM.
        (Err(libc::EACCES), Some(false), false),
        (Err(libc::EPERM), Some(false), false),
        (Err(libc::EBUSY), Some(false), false),
    ];
    for (open, advertised, want_absent) in kvm_absent_cases {
        if validate_plan::kvm_absent(*open, *advertised) != *want_absent {
            return Err(format!(
                "host capability: kvm absence for (open={open:?}, cpuinfo={advertised:?}) must be \
                 {want_absent}; only a corroborated ENOENT may read as absent and every other \
                 shape must run the node"
            ));
        }
    }

    node_vacuity_bracket(root)?;

    // The one override can only force PRESENT; nothing forces ABSENT.
    let verdict = validate_plan::probe_host_capability(HostCapability::CpuidFaulting);
    println!(
        "  host capability: 1 withheld / 3 not-withheld (capable, undeclared, dependent-refusal), \
         probe conjunction 1 absent / 7 present-on-doubt, vocabulary closed, shipped DAG declares \
         it; this machine's cpuid-faulting probe says {} ({})",
        if verdict.present { "PRESENT" } else { "ABSENT" },
        verdict.evidence
    );
    Ok(())
}

/// Two-sided bracket for [`verdict_refusals`]. Inert: no DAG, no ledger, no
/// label, no PR — it exercises the decision function with planted counts only.
fn verdict_refusal_bracket() -> Result<(), String> {
    // POSITIVE 1 — the exact shape measured on 2026-08-08 must fire, and must
    // fire for BOTH reasons rather than collapsing into one.
    let observed = verdict_refusals(Some(0), 1, Some(20));
    if observed.len() != 2 {
        return Err(format!(
            "verdict: the observed fail-open shape (0 measured, 1 spine failure, 20 executed) \
             must trip 2 refusals, tripped {}: {observed:?}",
            observed.len()
        ));
    }
    // POSITIVE 2 — zero executed tests alone, with nothing else wrong.
    if verdict_refusals(None, 0, Some(0)).len() != 1 {
        return Err("verdict: zero executed tests must refuse on its own".into());
    }
    // POSITIVE 3 — a spine failure alone, with a fully measured matrix, still
    // refuses: 187/187 passing rows do not excuse a failed prep node.
    if verdict_refusals(Some(187), 1, Some(862)).len() != 1 {
        return Err("verdict: a spine failure must refuse even with a full matrix".into());
    }
    // NEGATIVE 1 — a genuinely complete run must stay inert, or the gate is a
    // blanket red rather than a predicate.
    let clean = verdict_refusals(Some(187), 0, Some(862));
    if !clean.is_empty() {
        return Err(format!("verdict: a complete run must NOT refuse, got {clean:?}"));
    }
    // NEGATIVE 2 — unknown counts are not a measured zero.
    if !verdict_refusals(None, 0, None).is_empty() {
        return Err("verdict: unknown counts must not be read as a measured zero".into());
    }
    println!(
        "  verdict refusals: 3 positive(s) fire (0-measured+spine, 0-executed, spine-with-full-matrix), \
         2 negative(s) inert (complete run, unknown counts)"
    );
    Ok(())
}

fn human_duration(secs: f64) -> String {
    let x = secs.round() as i64;
    let (h, m, s) = (x / 3600, (x % 3600) / 60, x % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// A positive-integer env override, or `None` when unset/empty/invalid.
fn env_positive(name: &str) -> Option<i64> {
    let v = std::env::var(name).ok()?;
    if v.is_empty() {
        return None;
    }
    match v.parse::<i64>() {
        Ok(n) if n > 0 => Some(n),
        _ => {
            eprintln!("validate: {name}={v:?} is not a positive integer; ignoring");
            None
        }
    }
}

/// Lower every node's wall ceiling to at most `cap`.
fn clamp_wall(plan: &mut Plan, cap: i64) {
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        for s in cfg.steps.iter_mut() {
            s.timeout = s.timeout.min(cap);
        }
    }
}

/// Lower every node's CPU budget to at most `cap`, including the DAG-level
/// default that shipped lane nodes inherit.
fn clamp_cpu(plan: &mut Plan, cap: i64) {
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        cfg.default_step_cpu_timeout = if cfg.default_step_cpu_timeout > 0 {
            cfg.default_step_cpu_timeout.min(cap)
        } else {
            cap
        };
        for s in cfg.steps.iter_mut() {
            s.cpu_timeout = if s.cpu_timeout > 0 { s.cpu_timeout.min(cap) } else { cap };
        }
    }
}

/// Give every validation node an explicit fail-fast family.
///
/// A node that already names a shared family keeps it. Every other node gets its own tag, so its
/// failure still blocks true dependents through the DAG edges without cancelling unrelated work.
/// The runner's global eager-exit default remains available to every graph that does not opt in.
fn assign_fail_fast_families(plan: &mut Plan) {
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        for step in &mut cfg.steps {
            if step.fail_fast_family.is_none() {
                step.fail_fast_family = Some(step.tag());
            }
        }
    }
}

fn propagate_verbosity(plan: &mut Plan, verbosity: i64) {
    let value = verbosity.to_string();
    for step in &mut plan.cfg.steps {
        step.env.insert("VALIDATE_VERBOSITY".into(), value.clone());
    }
    if let Some(second) = &mut plan.second {
        for step in &mut second.steps {
            step.env.insert("VALIDATE_VERBOSITY".into(), value.clone());
        }
    }
}

/// Run the nested strict-compatibility payload with this already-compiled
/// validation executable.
///
/// The shipped lane remains independently runnable through
/// `./scripts/validate.rs`.  In a full validation, however, that spelling
/// enters rust-script a second time from the fresh checkout and recompiles this
/// same driver inside the one-core `test.strict_compat` step before any of its
/// 193 nodes can start.  Replacing only the executable token preserves every
/// argument, environment assignment, dependency, resource, timeout, and nested
/// check.
fn reuse_validate_executable(
    plan: &mut Plan,
    executable: &Path,
) -> Result<bool, String> {
    const INVOCATION: &str = " ./scripts/validate.rs --portable-strict-compat-only";
    let replacement = format!(
        " {} --portable-strict-compat-only",
        validate_plan::shell_quote(&executable.to_string_lossy())
    );
    let mut found = false;
    for cfg in std::iter::once(&mut plan.cfg).chain(plan.second.iter_mut()) {
        for step in &mut cfg.steps {
            if step.tag() != "test.strict_compat" {
                continue;
            }
            if found {
                return Err("more than one test.strict_compat node was planned".into());
            }
            if step.cmd.matches(INVOCATION).count() != 1 {
                return Err(format!(
                    "test.strict_compat no longer has the audited nested validate invocation: {}",
                    step.cmd
                ));
            }
            step.cmd = step.cmd.replacen(INVOCATION, &replacement, 1);
            found = true;
        }
    }
    Ok(found)
}

fn reuse_current_validate_executable(plan: &mut Plan) -> Result<bool, String> {
    let has_strict_compat = std::iter::once(&plan.cfg)
        .chain(plan.second.iter())
        .flat_map(|cfg| cfg.steps.iter())
        .any(|step| step.tag() == "test.strict_compat");
    if !has_strict_compat {
        return Ok(false);
    }
    let executable = std::env::current_exe().map_err(|error| {
        format!(
            "cannot resolve this validate executable for test.strict_compat: {error}"
        )
    })?;
    reuse_validate_executable(plan, &executable)
}

// --------------------------------------------------------------------------- interruption

/// Set from a signal handler when the operator stops the run.
static INTERRUPTED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn on_stop_signal(sig: i32) {
    // Async-signal-safe: a relaxed atomic store and nothing else.
    INTERRUPTED.store(sig, std::sync::atomic::Ordering::SeqCst);
}

/// Install SIGINT/SIGTERM/SIGHUP handlers so an operator stop is DISTINGUISHABLE
/// from a run that finished.
///
/// **The ledger records every COMPLETE run — and a timeout IS complete.**
/// A gate that blew its wall or CPU budget produced a real, reproducible result
/// about the tree: it is written, and `timed_out_nodes` says so. An operator
/// pressing Ctrl-C learned nothing about the product, so it is a NO-RESULT and
/// no row is appended at all. Recording interrupts would salt the ledger with
/// rows whose `fail` means "someone stopped it", and every consumer that counts
/// reds — the drain report, the flake classifier, the newest-green frontier —
/// would have to learn to subtract them.
///
/// This is a deliberate change from `validate.sh`, which appended a row with
/// `result: no_result` on a stop. That row was never useful and had to be
/// filtered by every reader; not writing it is strictly simpler.
fn install_stop_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, on_stop_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_stop_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_stop_signal as *const () as libc::sighandler_t);
    }
}

/// The stopping signal's BARE name (`INT`/`TERM`/`HUP`), not `SIGINT`.
///
/// The bare form is the ledger's `interruption_signal` value and is what
/// `scripts/test_validate_stop_paths.py` asserts
/// (`sig.name.removeprefix("SIG")`). Prose call sites print `SIG{name}`.
fn interrupted_by() -> Option<&'static str> {
    match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        libc::SIGINT => Some("INT"),
        libc::SIGTERM => Some("TERM"),
        libc::SIGHUP => Some("HUP"),
        _ => Some("signal"),
    }
}

// --------------------------------------------------------------- lane execution

/// One node's record of ONE attempt, kept even after a later attempt replaced it.
///
/// WHY THIS TYPE EXISTS — a measured erasure, not a hypothetical one.
///
/// The retry loop below keeps a `BTreeMap<tag, StepOutcome>` and overwrites the
/// entry on every retry. That map is what becomes `LaneResult::outcomes` and
/// therefore the ledger's `gates[]` array, so a node that FAILED on attempt 1
/// and PASSED on attempt 2 is written to the ledger as a plain `pass` with no
/// surviving trace. Counted on the live ledger under `ledger/hermit/`:
/// **67 runs retried a node, 17 of them ended `pass`, and in all 17 the
/// `gates[]` array records zero failures.** The scalar `env_block_retries`
/// says a retry happened SOMEWHERE in the lane; it never names the node, the
/// attempt, or the reason, so no per-node flake rate can be computed from it.
///
/// Keeping the attempts as their own rows is what makes a green that needed two
/// attempts distinguishable from a green that needed one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptExecution {
    /// The child spawned and wait produced its exit status.
    Completed,
    /// No child result exists: unreported, aborted, spawn failure, or supervisor failure.
    Unknown,
}

impl AttemptExecution {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Unknown => "unknown",
        }
    }
}

/// Typed execution state for a scheduler outcome.
///
/// `reported && !aborted` is insufficient: spawn failures and supervisor
/// crashes deliberately publish a non-aborted failure row, but both say the
/// step's own result is UNKNOWN and carry no child exit status.
fn outcome_execution(outcome: &StepOutcome) -> AttemptExecution {
    if !outcome.aborted && outcome.returncode.is_some() {
        AttemptExecution::Completed
    } else {
        AttemptExecution::Unknown
    }
}

#[derive(Clone)]
struct NodeAttempt {
    /// The node's `group.job` tag.
    tag: String,
    /// 1-based ordinal. Attempt 1 is the first scheduler pass.
    attempt: usize,
    /// The scheduler's safety verdict when it reported this attempt. Pair this
    /// with `execution`: spawn/supervisor failures report `Some(false)` so the
    /// run fails closed, while the step's own result remains UNKNOWN.
    ok: Option<bool>,
    /// Whether a completion payload arrived at all. False is the
    /// verdict-not-recorded case that a re-run cannot distinguish after the
    /// fact unless it is written down here.
    reported: bool,
    returncode: Option<i64>,
    /// The runner's typed failure reason for THIS attempt; `""` when it passed
    /// or was never reported. A later attempt never overwrites it.
    reason: String,
    duration_s: f64,
    aborted: bool,
    /// Whether a child actually executed through a collected exit status.
    execution: AttemptExecution,
    /// Why this attempt's failure was judged retry-eligible, in the same words
    /// the retry line prints. `None` when it was not retried — because it
    /// passed, because nothing classified it, or because the budget ran out.
    retry_class: Option<String>,
    /// The environmental signature found in this failed attempt's own detail
    /// region. This is kept separately from `retry_class`: a classified attempt
    /// may never execute again, and that distinction is the UNCONFIRMED verdict.
    environmental_class: Option<String>,
    /// Whether this attempt's own round emitted a detail region, even if that
    /// region carried no environmental signature. Without this bit, "banner
    /// gone" is indistinguishable from "no new evidence was captured".
    detail_observed: bool,
}

fn attempt_is_no_result(attempt: &NodeAttempt) -> bool {
    attempt.execution == AttemptExecution::Completed
        && !attempt.aborted
        && attempt.returncode == Some(NO_RESULT_EXIT_CODE)
}

fn attempt_result(attempt: &NodeAttempt) -> Option<&'static str> {
    if attempt.execution != AttemptExecution::Completed {
        None
    } else if attempt.ok == Some(true) {
        Some("pass")
    } else if attempt_is_no_result(attempt) {
        Some("no_result")
    } else if attempt.ok == Some(false) {
        Some("fail")
    } else {
        None
    }
}

fn attempt_is_failure(attempt: &NodeAttempt) -> bool {
    attempt_result(attempt) == Some("fail")
}

/// This node's next attempt ordinal: one more than however many attempts of it
/// are already recorded.
///
/// The ordinal is PER NODE, not per retry round. A node that is dependency-
/// skipped in round 1 and runs for the first time in round 2 has made ONE
/// attempt, not two — labelling it `attempt 2` would put it in `retried_nodes`
/// and make a node that never failed look like a flake, which is the same
/// over-counting this record exists to prevent in the other direction.
fn next_attempt_ordinal(attempts: &[NodeAttempt], tag: &str) -> usize {
    attempts.iter().filter(|a| a.tag == tag).count() + 1
}

/// Record one attempt the scheduler REPORTED.
fn reported_attempt(outcome: &StepOutcome, attempt: usize) -> NodeAttempt {
    NodeAttempt {
        tag: outcome.tag.clone(),
        attempt,
        ok: Some(outcome.ok),
        reported: true,
        returncode: outcome.returncode,
        reason: outcome.reason.clone(),
        duration_s: outcome.duration_s,
        aborted: outcome.aborted,
        execution: outcome_execution(outcome),
        retry_class: None,
        environmental_class: None,
        detail_observed: false,
    }
}

/// Record one attempt for which NO completion payload arrived. Every observation
/// field stays absent rather than defaulting to a zero, because a fabricated
/// `exit 0`/`0.0s` here would read exactly like a node that ran and passed.
fn unreported_attempt(tag: String, attempt: usize) -> NodeAttempt {
    NodeAttempt {
        tag,
        attempt,
        ok: None,
        reported: false,
        returncode: None,
        reason: "no completion payload was reported for this node".into(),
        duration_s: 0.0,
        aborted: false,
        execution: AttemptExecution::Unknown,
        retry_class: None,
        environmental_class: None,
        detail_observed: false,
    }
}

/// The exact attempt whose failed detail region is represented by `by_tag`.
///
/// A later scheduler round can add an unreported or aborted row without adding
/// a new detail region. Selecting merely "latest by tag" would then attach the
/// old reported failure's signature to an attempt that produced no evidence.
fn latest_reported_failure_mut<'a>(
    attempts: &'a mut [NodeAttempt],
    tag: &str,
) -> Option<&'a mut NodeAttempt> {
    attempts.iter_mut().rev().find(|attempt| {
        attempt.tag == tag && attempt.reported && attempt_is_failure(attempt)
    })
}

/// Attach one round's detail observation to the exact reported failure it came from.
fn stamp_attempt_detail(attempts: &mut [NodeAttempt], tag: &str, class: Option<&str>) {
    if let Some(attempt) = latest_reported_failure_mut(attempts, tag) {
        attempt.detail_observed = true;
        attempt.environmental_class = class.map(str::to_string);
    }
}

/// The first later attempt that demonstrably executed and completed.
///
/// A retry round, an unreported row, or an aborted scheduler outcome is not an
/// execution result and cannot confirm or refute anything.
fn actual_rerun_after<'a>(
    attempts: &'a [NodeAttempt],
    classified: &NodeAttempt,
) -> Option<&'a NodeAttempt> {
    attempts
        .iter()
        .filter(|attempt| {
            attempt.tag == classified.tag
                && attempt.attempt > classified.attempt
                && attempt.execution == AttemptExecution::Completed
                && attempt.ok.is_some()
                && !attempt_is_no_result(attempt)
        })
        .min_by_key(|attempt| attempt.attempt)
}

/// Settle one classified attempt from the attempt ledger itself.
fn environmental_assessment(
    attempts: &[NodeAttempt],
    classified: &NodeAttempt,
) -> Option<(validate_runtime::EnvBlockVerdict, Option<validate_runtime::RefutedShape>)> {
    // NO GOALPOST LOWERING: this derives evidence after execution. It does not
    // alter retry eligibility, the terminal StepOutcome, LaneResult::ok, or
    // failure counts. Unknown execution can only tighten completeness and refuse
    // a receipt that lacked evidence; a label can never turn a RED into a pass.
    let original = classified.environmental_class.as_deref()?;
    let rerun = actual_rerun_after(attempts, classified);
    let rerun_result = rerun.and_then(|attempt| attempt.ok);
    let verdict = validate_runtime::EnvBlockVerdict::settle(rerun_result);
    let shape = if verdict == validate_runtime::EnvBlockVerdict::Refuted
        && rerun.is_some_and(|attempt| attempt.detail_observed)
    {
        Some(validate_runtime::RefutedShape::of(
            original,
            rerun.and_then(|attempt| attempt.environmental_class.as_deref()),
        ))
    } else {
        None
    };
    Some((verdict, shape))
}

/// One lane's terminal state after any environmental retries.
struct LaneResult {
    outcomes: Vec<StepOutcome>,
    skipped: Vec<String>,
    /// Every attempt of every node, in the order they were reported. A node run
    /// once contributes exactly one row, so this is a superset of `outcomes`
    /// rather than a parallel structure that can disagree with it.
    attempts: Vec<NodeAttempt>,
    /// Every non-intentional planned node completed with a collected child exit
    /// status, and the whole-run clock did not cut the lane short. Dependency-
    /// skipped, aborted, unreported, spawn-failed, and supervisor-failed nodes
    /// are incomplete. This is deliberately separate from node success: compat,
    /// super, and envelope profiles may allow a fully measured failing row.
    complete: bool,
    /// Whether every reported node succeeded or was aborted after a peer failed.
    /// This does not answer whether the planned lane was completely reported.
    ok: bool,
    /// How many retry ROUNDS this lane needed; recorded in the ledger so a green
    /// that only survived because the host was retried is never mistaken for a
    /// green that passed first time.
    env_retries: usize,
    /// The whole-invocation deadline expired during this lane.
    run_timed_out: bool,
}

/// Return the durable log's byte length once it has stopped growing.
///
/// The driver tees its own stdout/stderr through a `tee` child, so a node's
/// `----- detail -----` region reaches the file slightly after the runner emits
/// it. Flushing and waiting for a stable size before taking a watermark or a
/// slice keeps adjacent scheduler invocations from borrowing each other's
/// output.
fn settled_log_len(path: &Path) -> u64 {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    let size = || std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut last = size();
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let now = size();
        if now > 0 && now == last {
            break;
        }
        last = now;
    }
    last
}

/// Read only bytes emitted since this scheduler invocation's start watermark.
///
/// A whole-file `rfind` can reuse attempt 1's banner when attempt 2 emits no
/// detail at all. An empty slice is therefore evidence of NO NEW REGION, not a
/// reason to look backwards. Truncation or unreadability is likewise unknown.
fn read_log_since_settled(path: &Path, start: u64) -> Option<String> {
    let end = settled_log_len(path);
    if end < start {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let start = usize::try_from(start).ok()?;
    if start > bytes.len() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes[start..]).into_owned())
}

/// Forward nested scheduler rows to the directory uploaded by the hosted shard.
///
/// `validate.rs` invokes the scheduler as a library, bypassing the runner CLI's
/// profile writer. Without this explicit forwarding an inner deadline can name
/// the cut probe on stdout yet leave no per-probe artifact. The workflow uploads
/// `$RUN_NODE_PERF_DIR` under `if: always()`, so these rows survive a red job.
fn forward_step_profiles(result: &RunResult, jobs: i64) {
    let Ok(dir) = std::env::var("RUN_NODE_PERF_DIR") else {
        return;
    };
    if dir.is_empty() || result.step_profile_rows.is_empty() {
        return;
    }
    let git_sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    match append_step_profiles(
        Path::new(&dir),
        &result.step_profile_rows,
        &git_sha,
        jobs,
        None,
        "unverified",
        "validate.rs",
        // Each caller passes the rows of exactly one `run_dag_boxed_deadline`
        // execution, so a freshly minted run_id (`None`) groups precisely that
        // execution. An environmental retry is a separate execution and gets its
        // own id, which is what keeps the retry's rows distinguishable from the
        // first attempt's instead of merging both into one apparent run.
        None,
    ) {
        Some(path) => eprintln!(
            "validate: wrote {} inner step profile row(s) to {}",
            result.step_profile_rows.len(),
            path.display()
        ),
        None => eprintln!("validate: could not write inner step profile rows to {dir}"),
    }
}

/// Absolute monotonic deadline for one logical invocation.
///
/// A nested validate must spend from the enclosing scheduler step's clock. Starting a new
/// `Instant` after re-exec and setup made `600 < 720` numerically true but temporally false.
fn env_u64(name: &str) -> Result<Option<u64>, String> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(None);
    };
    let text = raw
        .to_str()
        .ok_or_else(|| format!("{name} is not valid UTF-8"))?;
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{name}={text:?} is not an unsigned integer"))
}

fn deadline_from_sources(
    run_timeout_s: Option<i64>,
    nested: bool,
    in_scope: bool,
    step_started_ns: Option<u64>,
    owned_scope_deadline_ns: Option<u64>,
    now_ns: u64,
) -> Result<Option<u64>, String> {
    let Some(timeout_s) = run_timeout_s else {
        return Ok(None);
    };
    let allowance_ns = (timeout_s as u64)
        .checked_mul(1_000_000_000)
        .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))?;
    let scheduler_deadline = match step_started_ns {
        Some(start) if start > now_ns => {
            return Err(format!(
                "scheduler-owned {STEP_STARTED_MONOTONIC_NS_ENV} is in the future"
            ));
        }
        Some(start) => Some(
            start
                .checked_add(allowance_ns)
                .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))?,
        ),
        None => None,
    };
    // Only the top-level same-logical-run re-exec owns this marker. A nested focused payload
    // inherits its parent's scope marker but owns the scheduler epoch for its own enclosing node.
    if in_scope && !nested {
        if let Some(owned) = owned_scope_deadline_ns {
            let latest = now_ns
                .checked_add(allowance_ns)
                .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))?;
            if owned > latest {
                return Err("invocation-owned scope deadline exceeds a fresh full allowance".into());
            }
            if scheduler_deadline.is_some_and(|scheduler| scheduler != owned) {
                return Err("scheduler epoch and invocation-owned scope deadline disagree".into());
            }
            return Ok(Some(owned));
        }
    }
    if let Some(deadline) = scheduler_deadline {
        return Ok(Some(deadline));
    }
    if nested {
        return Err(format!(
            "nested timed validate lacks the scheduler-owned {STEP_STARTED_MONOTONIC_NS_ENV}; \
             refusing to start a fresh clock that could outlive its enclosing node"
        ));
    }
    now_ns
        .checked_add(allowance_ns)
        .map(Some)
        .ok_or_else(|| format!("run timeout {timeout_s}s overflows the monotonic deadline"))
}

fn invocation_deadline_ns(run_timeout_s: Option<i64>, nested: bool) -> Result<Option<u64>, String> {
    let now_ns = monotonic_now_ns().ok_or_else(|| "CLOCK_MONOTONIC is unavailable".to_string())?;
    deadline_from_sources(
        run_timeout_s,
        nested,
        is_in_scope(),
        env_u64(STEP_STARTED_MONOTONIC_NS_ENV)?,
        env_u64(OWN_SCOPE_DEADLINE_ENV)?,
        now_ns,
    )
}

/// Seconds left on one shared invocation clock, floored so a child cannot outlive it.
fn remaining_budget_s(deadline_ns: Option<u64>) -> Option<i64> {
    let deadline_ns = deadline_ns?;
    // Clock-read failure cannot turn a bounded invocation into `None` (unbounded). Expire it in
    // the safe direction instead.
    let now_ns = monotonic_now_ns().unwrap_or(deadline_ns);
    Some(if now_ns >= deadline_ns {
        0
    } else {
        ((deadline_ns - now_ns) / 1_000_000_000) as i64
    })
}

/// Planned runnable steps absent from both scheduler result collections.
/// Whether the scheduler refused the whole lane before starting any node.
///
/// Every pre-flight refusal path returns empty outcomes, empty dependency skips, an empty
/// `not_launched`, AND empty intentional skips -- deliberately, because nothing was left
/// unlaunched *by a failure*. A planned lane that produced none of the four therefore never
/// started, and its nodes are explained by the refusal, not unaccounted for.
fn scheduler_refused_before_launching(
    planned: usize,
    outcomes: usize,
    skipped: usize,
    not_launched: usize,
    intentional_skips: usize,
) -> bool {
    planned > 0
        && outcomes == 0
        && skipped == 0
        && not_launched == 0
        && intentional_skips == 0
}

/// Replace the recorded reason a planned node did not run with the latest scheduler attempt.
///
/// A retry is a new attempt: a node that was not launched on attempt one may be refused
/// before attempt two, or may run on attempt two. Keeping the old reason would describe history
/// rather than the terminal attempt that makes the lane incomplete.
fn update_not_run_explanations(
    planned: &[String],
    outcomes: usize,
    skipped: usize,
    not_launched: &[String],
    intentional_skips: usize,
    scheduler_not_launched: &mut BTreeSet<String>,
    refused: &mut BTreeSet<String>,
) {
    for tag in planned {
        scheduler_not_launched.remove(tag);
        refused.remove(tag);
    }
    if scheduler_refused_before_launching(
        planned.len(),
        outcomes,
        skipped,
        not_launched.len(),
        intentional_skips,
    ) {
        refused.extend(planned.iter().cloned());
    } else {
        scheduler_not_launched.extend(not_launched.iter().cloned());
    }
}

/// Split nodes that produced no outcome into the two states they actually occupy:
/// those the scheduler returned in `not_launched` after admission stopped
/// (accounted for, without claiming whether fail-fast or the outer budget stopped it),
/// and those nothing explains.
///
/// Both still block a green lane. The distinction is diagnostic, and it is the
/// whole point: a deliberate skip that reads identically to a vanished node makes
/// every deliberate skip look like a defect and hides the real ones among them.
fn partition_unreported(
    unreported: &[String],
    not_launched: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    unreported
        .iter()
        .cloned()
        .partition(|tag| not_launched.contains(tag))
}

fn scheduler_not_launched_message(tags: &[String]) -> String {
    format!(
        "validate: {} planned node(s) DID NOT RUN; the scheduler returned them in \
         not_launched after it stopped admitting work (fail-fast or outer run budget): {}. \
         They are accounted for, but the lane remains incomplete and cannot be green.",
        tags.len(),
        tags.join(", ")
    )
}

fn retry_notice(tag: &str, class: &str, attempt: usize) -> String {
    format!(
        "⚠️  {tag}: RETRY-ELIGIBLE ({class}) — this attempt remains RED unless an actual \
         re-execution passes — retrying (attempt {attempt}/{})",
        validate_runtime::MAX_ATTEMPTS_PER_CELL
    )
}

#[cfg(test)]
mod scheduler_explanation_tests {
    use super::*;

    #[test]
    fn latest_attempt_replaces_the_prior_nonlaunch_reason() {
        let tag = "e2e.manifest_applications".to_string();
        let planned = vec![tag.clone()];
        let mut not_launched = BTreeSet::new();
        let mut refused = BTreeSet::new();

        update_not_run_explanations(
            &planned, 0, 0, std::slice::from_ref(&tag), 0, &mut not_launched, &mut refused,
        );
        assert!(not_launched.contains(&tag));
        assert!(!refused.contains(&tag));

        update_not_run_explanations(
            &planned, 0, 0, &[], 0, &mut not_launched, &mut refused,
        );
        assert!(!not_launched.contains(&tag));
        assert!(refused.contains(&tag));

        update_not_run_explanations(
            &planned, 1, 0, &[], 0, &mut not_launched, &mut refused,
        );
        assert!(!not_launched.contains(&tag));
        assert!(!refused.contains(&tag));
    }

    #[test]
    fn intentional_skip_is_not_a_preflight_refusal() {
        assert!(!scheduler_refused_before_launching(1, 0, 0, 0, 1));
        assert!(scheduler_refused_before_launching(1, 0, 0, 0, 0));
    }

    #[test]
    fn not_launched_diagnostic_does_not_invent_a_specific_cause() {
        let message = scheduler_not_launched_message(&["test.detcore_misc".to_string()]);
        assert_eq!(
            message,
            "validate: 1 planned node(s) DID NOT RUN; the scheduler returned them in not_launched after it stopped admitting work (fail-fast or outer run budget): test.detcore_misc. They are accounted for, but the lane remains incomplete and cannot be green."
        );
    }

    #[test]
    fn retry_notice_has_one_retry_and_never_advertises_a_third_attempt() {
        assert_eq!(validate_runtime::RETRIES_PER_CELL, 1);
        assert_eq!(validate_runtime::MAX_ATTEMPTS_PER_CELL, 2);
        assert!(retry_notice("test.liteinst_strict", "fixture", 2).ends_with("attempt 2/2)"));

        let tag = "test.liteinst_strict";
        let mut attempts = vec![unreported_attempt(tag.into(), 1)];
        assert!(retry_attempt_available(&attempts, tag));
        attempts.push(unreported_attempt(tag.into(), 2));
        assert!(!retry_attempt_available(&attempts, tag));
    }

    #[test]
    fn retry_set_applies_the_attempt_cap_to_carried_cells_too() {
        let capped = "test.capped_unknown";
        let available = "test.available_failure";
        let attempts = vec![
            unreported_attempt(capped.into(), 1),
            unreported_attempt(capped.into(), 2),
            unreported_attempt(available.into(), 1),
        ];
        let mut keep = BTreeSet::from([capped.to_string(), available.to_string()]);

        retain_cells_with_retry_attempt_available(&mut keep, &attempts);

        assert_eq!(keep, BTreeSet::from([available.to_string()]));
    }
}

#[cfg(test)]
mod nextest_timeout_tests {
    /// The per-test cap stays at 15s, and every exemption from it is named,
    /// measured, and justified IN PLACE.
    ///
    /// ⚠️ WIDENED FROM ONE OVERRIDE TO THREE, DELIBERATELY AND WITHOUT WEAKENING.
    /// This gate is a ratchet against exemptions accumulating quietly, so adding
    /// one had to be a visible edit here rather than a silently passing config
    /// change -- which is exactly what it did: each added override failed this
    /// test first. It is now STRICTER than before, not looser: it pins all three
    /// filters and requires each override's justification text to be present, so
    /// a third exemption still cannot appear without an author changing this
    /// list and supplying a reason. The count alone was the weaker check.
    #[test]
    fn nextest_uses_the_manifest_default_and_named_overrides() {
        let config = include_str!("../.config/nextest.toml");
        let manifest = include_str!("../tests/e2e/manifests/defaults.yaml");
        let timeouts: Vec<&str> = config
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("slow-timeout ="))
            .collect();
        assert_eq!(
            timeouts,
            vec![
                "slow-timeout = { period = \"15s\", terminate-after = 1, grace-period = \"2s\" }",
                "slow-timeout = { period = \"30s\", terminate-after = 1, grace-period = \"2s\" }",
                "slow-timeout = { period = \"30s\", terminate-after = 1, grace-period = \"2s\" }",
                "slow-timeout = { period = \"30s\", terminate-after = 1, grace-period = \"2s\" }",
            ],
            "the per-test timeout must stay at 15s with exactly three justified 30s overrides"
        );
        for required in [
            "test(/(^|::)every_record_container_site_classifies_a_child_fault_by_name$/)",
            "test(/(^|::)run_timeout_fallback_fires_when_the_unwind_does_not_finish$/)",
            "binary(=container_init_deadline)",
        ] {
            assert!(config.contains(required), "nextest config lost {required}");
            assert!(manifest.contains(required), "manifest lost {required}");
        }
        for required in [
            "12 separate Hermit processes",
            "25.98-26.30s while passing",
        ] {
            assert!(config.contains(required), "nextest config lost {required}");
            assert!(manifest.contains(required), "manifest lost {required}");
        }
        assert!(config.contains("exercises a 15s timeout"));
        assert!(config.contains("20s teardown budget"));
        assert!(config.contains("15.002s"));
        assert!(config.contains("RUN_TIMEOUT_UNWIND_GRACE"));
        assert!(config.contains("11.2s"));
        assert!(manifest.contains("15-second timeout"));
        assert!(manifest.contains("20-second teardown budget"));
        assert!(manifest.contains("15.002 seconds"));
        assert!(manifest.contains("timeout_seconds: 15"));
    }
}

fn unreported_non_intentional_steps(
    cfg: &DagConfig,
    by_tag: &BTreeMap<String, StepOutcome>,
    skipped: &[String],
) -> Vec<String> {
    let skipped: BTreeSet<&str> = skipped.iter().map(String::as_str).collect();
    cfg.steps
        .iter()
        .filter(|step| {
            let tag = step.tag();
            step.skip_reason.is_none()
                && !by_tag.contains_key(&tag)
                && !skipped.contains(tag.as_str())
        })
        .map(|step| step.tag())
        .collect()
}

/// Keep only retry candidates whose prerequisites are either already complete
/// and successful or will execute in the same retry. Removing one unsafe node
/// can make its dependents unsafe too, so this must reach a fixed point before
/// the retry DAG is built. Dependencies between retained nodes remain intact;
/// only dependencies satisfied by a successful earlier outcome are dropped.
fn retry_steps_with_satisfied_prerequisites(
    cfg: &DagConfig,
    by_tag: &BTreeMap<String, StepOutcome>,
    mut keep: BTreeSet<String>,
) -> Vec<dagrun::model::Step> {
    loop {
        let remove: Vec<String> = cfg
            .steps
            .iter()
            .filter(|step| keep.contains(&step.tag()))
            .filter(|step| {
                step.deps.iter().any(|dependency| {
                    !keep.contains(dependency)
                        && !by_tag
                            .get(dependency)
                            .is_some_and(|outcome| outcome.ok && !outcome.aborted)
                })
            })
            .map(|step| step.tag())
            .collect();
        if remove.is_empty() {
            break;
        }
        for tag in remove {
            keep.remove(&tag);
        }
    }

    cfg.steps
        .iter()
        .filter(|step| keep.contains(&step.tag()))
        .map(|step| {
            let mut step = step.clone();
            step.deps.retain(|dependency| keep.contains(dependency));
            step
        })
        .collect()
}

fn retry_timeout_bound_bracket(root: &Path) -> Result<String, String> {
    const DEFAULT_TEST_CAP_S: i64 = 15;
    const NEXTEST_TERMINATION_GRACE_S: i64 = 2;
    const MANIFEST_TERMINATION_GRACE_S: i64 = 10;

    fn quoted_seconds(line: &str, field: &str) -> Result<i64, String> {
        let prefix = format!("{field} = \"");
        let value = line
            .split_once(&prefix)
            .and_then(|(_, rest)| rest.split_once("s\"").map(|(seconds, _)| seconds))
            .ok_or_else(|| format!("retry bounds: cannot parse {field} from {line:?}"))?;
        value
            .parse::<i64>()
            .map_err(|e| format!("retry bounds: invalid {field} in {line:?}: {e}"))
    }

    fn declared_manifest_caps(root: &Path) -> Result<Vec<i64>, String> {
        let manifests = root.join("tests/e2e/manifests");
        let mut paths = std::fs::read_dir(&manifests)
            .map_err(|e| format!("retry bounds: cannot read {}: {e}", manifests.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("retry bounds: cannot enumerate {}: {e}", manifests.display()))?;
        paths.sort();

        let mut caps = Vec::new();
        for path in paths
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "yaml"))
        {
            let source = std::fs::read_to_string(&path)
                .map_err(|e| format!("retry bounds: cannot read {}: {e}", path.display()))?;
            let mut timeout_map_indent = None;
            for (index, line) in source.lines().enumerate() {
                let trimmed = line.trim();
                let indent = line.len() - line.trim_start().len();
                if let Some(map_indent) = timeout_map_indent {
                    if trimmed.is_empty() || trimmed.starts_with('#') {
                        continue;
                    }
                    if indent <= map_indent {
                        timeout_map_indent = None;
                    } else if let Some((_, value)) = trimmed.split_once(':') {
                        let value = value.trim();
                        let seconds = value.parse::<i64>().map_err(|e| {
                            format!(
                                "retry bounds: invalid timeout override at {}:{}: {e}",
                                path.display(),
                                index + 1
                            )
                        })?;
                        caps.push(seconds);
                        continue;
                    }
                }
                let Some(value) = trimmed.strip_prefix("timeout_seconds:") else {
                    continue;
                };
                let value = value.trim();
                if value.is_empty() {
                    timeout_map_indent = Some(indent);
                } else {
                    caps.push(value.parse::<i64>().map_err(|e| {
                        format!(
                            "retry bounds: invalid timeout at {}:{}: {e}",
                            path.display(),
                            index + 1
                        )
                    })?);
                }
            }
        }
        Ok(caps)
    }

    let nextest = std::fs::read_to_string(root.join(".config/nextest.toml"))
        .map_err(|e| format!("retry bounds: cannot read nextest config: {e}"))?;
    let manifest_defaults =
        std::fs::read_to_string(root.join("tests/e2e/manifests/defaults.yaml"))
            .map_err(|e| format!("retry bounds: cannot read manifest defaults: {e}"))?;
    if !nextest.contains(
        "slow-timeout = { period = \"15s\", terminate-after = 1, grace-period = \"2s\" }",
    ) || !manifest_defaults
        .lines()
        .any(|line| line == "timeout_seconds: 15")
    {
        return Err(
            "retry bounds: the owner-ruled 15-second per-test default is not present in both \
             nextest and the manifest defaults"
                .into(),
        );
    }
    let nextest_caps = nextest
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("slow-timeout ="))
        .map(|line| {
            if !line.contains("terminate-after = 1") {
                return Err(format!(
                    "retry bounds: slow-timeout is not a one-period cap: {line:?}"
                ));
            }
            let period = quoted_seconds(line, "period")?;
            let grace = quoted_seconds(line, "grace-period")?;
            if grace != NEXTEST_TERMINATION_GRACE_S {
                return Err(format!(
                    "retry bounds: expected {NEXTEST_TERMINATION_GRACE_S}s termination grace, \
                     got {grace}s in {line:?}"
                ));
            }
            Ok(period)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let manifest_caps = declared_manifest_caps(root)?;
    let largest_nextest_cap_s = nextest_caps
        .iter()
        .copied()
        .max()
        .ok_or("retry bounds: no declared nextest cap")?;
    let largest_manifest_cap_s = manifest_caps
        .iter()
        .copied()
        .max()
        .ok_or("retry bounds: no declared manifest cap")?;

    let smallest_enclosing_deadline_s = ["portable", "privileged"]
        .into_iter()
        .map(|lane| validate_plan::lane_config(root, lane).map(|cfg| cfg.default_step_timeout))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .min()
        .ok_or("retry bounds: no enclosing lane deadline")?;
    let attempts = validate_runtime::MAX_ATTEMPTS_PER_CELL as i64;
    let default_with_grace_s = DEFAULT_TEST_CAP_S + NEXTEST_TERMINATION_GRACE_S;
    let largest_nextest_with_grace_s = largest_nextest_cap_s + NEXTEST_TERMINATION_GRACE_S;
    let largest_manifest_with_grace_s = largest_manifest_cap_s + MANIFEST_TERMINATION_GRACE_S;
    // A retry is a new DAG execution of the node and receives a fresh node timeout.
    // Multiplying an inner test timeout by the attempt count and comparing that
    // product with one node attempt's deadline compares different scopes. Each
    // inner timeout plus its own termination grace must fit one node attempt.
    if attempts != 2
        || nextest_caps.first().copied() != Some(DEFAULT_TEST_CAP_S)
        || default_with_grace_s >= smallest_enclosing_deadline_s
        || largest_nextest_with_grace_s >= smallest_enclosing_deadline_s
        || largest_manifest_with_grace_s >= smallest_enclosing_deadline_s
    {
        return Err(format!(
            "retry bounds: attempts={attempts}; default nextest cap including grace=\
             {default_with_grace_s}s; largest nextest cap including grace=\
             {largest_nextest_with_grace_s}s; largest manifest cap including grace=\
             {largest_manifest_with_grace_s}s; smallest enclosing lane deadline=\
             {smallest_enclosing_deadline_s}s"
        ));
    }
    Ok(format!(
        "retry bounds: {attempts} attempts receive separate node deadlines; default nextest \
         cap including grace={default_with_grace_s}s; largest nextest cap including grace=\
         {largest_nextest_with_grace_s}s; largest manifest cap including grace=\
         {largest_manifest_with_grace_s}s; each is below the smallest enclosing lane deadline \
         of {smallest_enclosing_deadline_s}s"
    ))
}

/// Fast front-door bracket for the scheduler result shape consumed below.
fn scheduler_accounting_bracket() -> Result<String, String> {
    let tmp = std::env::temp_dir().join(format!(
        "validate-scheduler-accounting-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    std::fs::create_dir(&tmp)
        .map_err(|e| format!("scheduler accounting: cannot create {}: {e}", tmp.display()))?;

    let result = (|| -> Result<(), String> {
    let step = |job: &str, cmd: &str| {
        step_with_caps(
            "fixture",
            job,
            "validate scheduler accounting fixture",
            cmd.to_string(),
            Vec::new(),
            30,
            30,
            64 * 1024 * 1024,
        )
    };
    let mut intentional_skip = step("intentional_skip", "exit 99");
    intentional_skip.skip_reason = Some(
        dagrun::model::IntentionalSkipReason::EmptyManifestBucket,
    );

    // A complete runnable plan plus a typed intentional skip is complete and
    // green. The skipped command is `exit 99`, so executing it cannot accidentally
    // satisfy the positive case.
    let complete_cfg = DagConfig {
        steps: vec![step("pass", "true"), intentional_skip.clone()],
        ..Default::default()
    };
    let complete = run_lane_with_env_retries(
        &complete_cfg,
        1,
        true,
        0,
        None,
        &tmp.join("complete.log"),
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    if !complete.complete
        || !complete.ok
        || complete.env_retries != 0
        || complete.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
            != vec!["fixture.pass"]
    {
        return Err(format!(
            "scheduler accounting: complete plan plus intentional skip was not accepted: complete={} ok={} retries={} outcomes={:?}",
            complete.complete,
            complete.ok,
            complete.env_retries,
            complete.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
        ));
    }

    // A genuine failure must not be reclassified or retried, and the lane's
    // completeness axis must report whether the rest of the plan was measured.
    //
    // These two runs are the same DAG under the two launch policies, and they are
    // bracketed together because the difference between them is the whole point of
    // the completeness axis. Until agent-utils 6a3c2d7 ("make --keep-going keep
    // going") a `keep_going` run suppressed the eager reap of in-flight steps and
    // then launched nothing further, so BOTH policies left the peers unmeasured and
    // this bracket could not tell them apart. It now checks each one separately.
    let failure_log = tmp.join("unclassified.log");
    std::fs::write(
        &failure_log,
        "[fixture.fail] ----- detail -----\n[fixture.fail] ordinary test failure\n[fixture.fail] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write {}: {e}", failure_log.display()))?;
    let failed_cfg = DagConfig {
        steps: vec![
            step("fail", "exit 1"),
            step("pending_a", "true"),
            step("pending_b", "true"),
        ],
        ..Default::default()
    };
    // Default eager-exit: with one worker both independent peers remain runnable but
    // never launched, so the lane is a red that is ALSO incomplete, and the
    // completeness axis must refuse to let it exit 0.
    let failed = run_lane_with_env_retries(
        &failed_cfg,
        1,
        false,
        0,
        None,
        &failure_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    // ⚠️ THIS BRACKET WAS RE-POINTED, NOT WEAKENED (owner directive 2026-08-26).
    // It used to require `env_retries == 0` -- an unclassified failure got no
    // retry, because retry was opt-in. Every cell is now ALWAYS eligible, so this
    // failure is retried exactly once here, bounded by the `max` of 1 passed
    // above. The retry count assertion therefore flipped from 0 to 1.
    //
    // WHAT DID NOT CHANGE, AND IS THE POINT OF THE BRACKET: a retried failure must
    // still be a RED and still be INCOMPLETE. Retry must never launder a failure
    // into a pass or into a complete run. Those three clauses -- `complete`, `ok`,
    // and the nonzero exit code -- are untouched and are what would catch a retry
    // policy that swallowed a real failure.
    if failed.complete
        || failed.ok
        || failed.env_retries != 1
        || failed.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
            != vec!["fixture.fail"]
        || exit_code_with_execution_completeness(0, failed.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: an always-eligible unclassified failure must be retried once at max=1 and REMAIN an incomplete red: complete={} ok={} retries={} outcomes={:?}",
            failed.complete,
            failed.ok,
            failed.env_retries,
            failed.outcomes.iter().map(|o| o.tag.as_str()).collect::<Vec<_>>()
        ));
    }

    // Same failure under keep-going: still an unretried red, and still not green,
    // but now every peer is measured, so the lane is completely accounted for. The
    // failure verdict must not soften just because coverage got wider.
    let kept = run_lane_with_env_retries(
        &failed_cfg,
        1,
        true,
        0,
        None,
        &failure_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let mut kept_tags: Vec<&str> = kept.outcomes.iter().map(|o| o.tag.as_str()).collect();
    kept_tags.sort_unstable();
    // Re-pointed with its twin above: always-eligible retry means this failure is
    // retried once at max=1, so the count moved 0 -> 1. The teeth are unchanged --
    // the lane must stay COMPLETE, stay NOT ok, and still measure every peer. A
    // retry that softened the verdict or lost a peer still fails here.
    if !kept.complete
        || kept.ok
        || kept.env_retries != 1
        || kept_tags != vec!["fixture.fail", "fixture.pending_a", "fixture.pending_b"]
    {
        return Err(format!(
            "scheduler accounting: keep-going did not measure every independent peer while keeping the failure red: complete={} ok={} retries={} outcomes={kept_tags:?}",
            kept.complete, kept.ok, kept.env_retries
        ));
    }

    // A dependency skip is named by the scheduler but still did not execute.
    // It cannot satisfy required-node completeness.
    let dependency_log = tmp.join("dependency-failure.log");
    std::fs::write(
        &dependency_log,
        "[fixture.dependency_failure] ----- detail -----\n[fixture.dependency_failure] ordinary test failure\n[fixture.dependency_failure] ----- end detail -----\n",
    )
    .map_err(|e| {
        format!(
            "scheduler accounting: cannot write {}: {e}",
            dependency_log.display()
        )
    })?;
    let mut dependency_skipped = step("dependency_skipped", "true");
    dependency_skipped.deps = vec!["fixture.dependency_failure".into()];
    let dependency_cfg = DagConfig {
        steps: vec![
            step("dependency_failure", "exit 1"),
            dependency_skipped,
        ],
        ..Default::default()
    };
    let dependency_result = run_lane_with_env_retries(
        &dependency_cfg,
        1,
        true,
        0,
        None,
        &dependency_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    if dependency_result.complete
        || dependency_result.ok
        || dependency_result.skipped != vec!["fixture.dependency_skipped"]
        || exit_code_with_execution_completeness(0, dependency_result.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: dependency-skipped required node did not force incomplete execution: complete={} ok={} skipped={:?}",
            dependency_result.complete, dependency_result.ok, dependency_result.skipped
        ));
    }

    // Eager-exit reports a running peer as aborted. That typed outcome is not a
    // completed required node and must likewise force a nonzero final exit.
    let aborted_log = tmp.join("aborted-peer.log");
    std::fs::write(
        &aborted_log,
        "[fixture.abort_failure] ----- detail -----\n[fixture.abort_failure] ordinary test failure\n[fixture.abort_failure] ----- end detail -----\n",
    )
    .map_err(|e| {
        format!(
            "scheduler accounting: cannot write {}: {e}",
            aborted_log.display()
        )
    })?;
    let aborted_cfg = DagConfig {
        steps: vec![
            step("abort_failure", "sleep 0.1; exit 1"),
            step("aborted_peer", "sleep 5"),
        ],
        ..Default::default()
    };
    let aborted_result = run_lane_with_env_retries(
        &aborted_cfg,
        2,
        false,
        0,
        None,
        &aborted_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let aborted_peer_reported = aborted_result
        .outcomes
        .iter()
        .any(|outcome| outcome.tag == "fixture.aborted_peer" && outcome.aborted);
    if aborted_result.complete
        || aborted_result.ok
        || !aborted_peer_reported
        || exit_code_with_execution_completeness(0, aborted_result.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: aborted required node did not force incomplete execution: complete={} ok={} aborted_peer_reported={aborted_peer_reported} outcomes={:?}",
            aborted_result.complete,
            aborted_result.ok,
            aborted_result
                .outcomes
                .iter()
                .map(|outcome| (outcome.tag.as_str(), outcome.aborted))
                .collect::<Vec<_>>()
        ));
    }

    // Scoped eager-exit keeps BOTH promises in one run: the failing family is still cut short
    // and its true dependent is skipped, while a different family completes. Checking only the
    // independent pass would also accept a blanket keep-going implementation.
    let scoped_log = tmp.join("scoped-eager-exit.log");
    std::fs::write(
        &scoped_log,
        "[fixture.family_failure] ----- detail -----\n[fixture.family_failure] ordinary test failure\n[fixture.family_failure] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write {}: {e}", scoped_log.display()))?;
    let mut family_failure = step("family_failure", "sleep 0.1; exit 1");
    let mut family_peer = step("family_peer", "sleep 5");
    let mut family_dependent = step("family_dependent", "true");
    family_dependent.deps = vec!["fixture.family_failure".into()];
    let marker = tmp.join("independent-family-completed");
    let independent = step(
        "independent_family",
        &format!(
            "sleep 0.3; : > {}",
            validate_plan::shell_quote(&marker.to_string_lossy())
        ),
    );
    for member in [&mut family_failure, &mut family_peer, &mut family_dependent] {
        member.fail_fast_family = Some("fixture.failure-family".into());
    }
    let mut scoped_plan = Plan {
        cfg: DagConfig {
            steps: vec![family_failure, family_peer, family_dependent, independent],
            ..Default::default()
        },
        ..Default::default()
    };
    assign_fail_fast_families(&mut scoped_plan);
    let scoped_families: BTreeMap<String, String> = scoped_plan
        .cfg
        .steps
        .iter()
        .map(|step| (step.tag(), step.fail_fast_family.clone().unwrap_or_default()))
        .collect();
    if scoped_families["fixture.family_peer"] != "fixture.failure-family"
        || scoped_families["fixture.independent_family"] != "fixture.independent_family"
    {
        return Err(format!(
            "scheduler accounting: plan family assignment changed an explicit family or failed to scope an ordinary node by its tag: {scoped_families:?}"
        ));
    }
    let scoped = run_lane_with_env_retries(
        &scoped_plan.cfg,
        3,
        false,
        0,
        None,
        &scoped_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let scoped_by_tag: BTreeMap<&str, &StepOutcome> = scoped
        .outcomes
        .iter()
        .map(|outcome| (outcome.tag.as_str(), outcome))
        .collect();
    let same_family_aborted = scoped_by_tag
        .get("fixture.family_peer")
        .is_some_and(|outcome| outcome.aborted);
    let independent_completed = scoped_by_tag
        .get("fixture.independent_family")
        .is_some_and(|outcome| outcome.ok && !outcome.aborted);
    if scoped.ok
        || scoped.complete
        || !same_family_aborted
        || scoped.skipped != ["fixture.family_dependent".to_string()]
        || !independent_completed
        || !marker.is_file()
    {
        return Err(format!(
            "scheduler accounting: scoped eager-exit did not cancel its own family, skip its dependent, and complete an independent family: complete={} ok={} skipped={:?} outcomes={:?} marker={}",
            scoped.complete,
            scoped.ok,
            scoped.skipped,
            scoped
                .outcomes
                .iter()
                .map(|outcome| (outcome.tag.as_str(), outcome.ok, outcome.aborted))
                .collect::<Vec<_>>(),
            marker.is_file()
        ));
    }

    // A fully reported failing row can be allowed by a profile's existing
    // policy. Completeness must not silently turn every raw node failure into a
    // blocking failure.
    let allowed_log = tmp.join("allowed-failure.log");
    std::fs::write(
        &allowed_log,
        "[fixture.allowed_failure] ----- detail -----\n[fixture.allowed_failure] expected measured failure\n[fixture.allowed_failure] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write {}: {e}", allowed_log.display()))?;
    let allowed_cfg = DagConfig {
        steps: vec![step("allowed_failure", "exit 1"), intentional_skip],
        ..Default::default()
    };
    let allowed = run_lane_with_env_retries(
        &allowed_cfg,
        1,
        true,
        0,
        None,
        &allowed_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    // Re-pointed with its two twins: always-eligible retry makes this failure
    // retry once at max=1, so the count moved 0 -> 1. The teeth are unchanged --
    // a COMPLETE allowed failure must stay complete, stay not-ok, and still exit
    // 0 on the completeness axis, which is the distinction this bracket exists
    // to hold and which retry must not blur.
    if !allowed.complete
        || allowed.ok
        || allowed.env_retries != 1
        || exit_code_with_execution_completeness(0, allowed.complete) != 0
    {
        return Err(format!(
            "scheduler accounting: complete allowed failure was not kept distinct from incomplete execution: complete={} ok={} retries={}",
            allowed.complete, allowed.ok, allowed.env_retries
        ));
    }

    // A classified one-time host failure retries itself and every peer omitted
    // by fail-fast. The dependent is deliberately registered before its
    // prerequisite and fails unless the retry preserves their edge.
    let environmental_log = tmp.join("environmental.log");
    let first_attempt = tmp.join("environmental-first-attempt");
    let edge_ready = tmp.join("edge-ready");
    let environmental_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.environmental] ----- detail -----' \
         '[fixture.environmental] An action was blocked on this server based on a security policy!' \
         '[fixture.environmental] ----- end detail -----' > {log}; exit 1; fi",
        first = validate_plan::shell_quote(&first_attempt.to_string_lossy()),
        log = validate_plan::shell_quote(&environmental_log.to_string_lossy()),
    );
    let mut dependent = step(
        "dependent",
        &format!(
            "test -f {}",
            validate_plan::shell_quote(&edge_ready.to_string_lossy())
        ),
    );
    dependent.deps = vec!["fixture.prerequisite".into()];
    let environmental_cfg = DagConfig {
        steps: vec![
            step("environmental", &environmental_cmd),
            dependent,
            step(
                "prerequisite",
                &format!(
                    ": > {}",
                    validate_plan::shell_quote(&edge_ready.to_string_lossy())
                ),
            ),
        ],
        ..Default::default()
    };
    let retried = run_lane_with_env_retries(
        &environmental_cfg,
        1,
        true,
        0,
        None,
        &environmental_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let retried_tags: BTreeSet<&str> =
        retried.outcomes.iter().map(|outcome| outcome.tag.as_str()).collect();
    let expected_tags: BTreeSet<&str> = [
        "fixture.environmental",
        "fixture.dependent",
        "fixture.prerequisite",
    ]
    .into_iter()
    .collect();
    if !retried.complete
        || !retried.ok
        || retried.env_retries != 1
        || retried_tags != expected_tags
        || !retried.skipped.is_empty()
        || !edge_ready.is_file()
    {
        return Err(format!(
            "scheduler accounting: environmental retry did not run every peer with its edge preserved: complete={} ok={} retries={} outcomes={retried_tags:?} skipped={:?} edge_ready={}",
            retried.complete,
            retried.ok,
            retried.env_retries,
            retried.skipped,
            edge_ready.is_file()
        ));
    }

    // THE RETRY MUST NOT ERASE THE FLAKE. The lane above is GREEN and its
    // terminal outcome for `fixture.environmental` is PASS — which is exactly
    // the shape that used to leave no per-node evidence that anything failed.
    // Assert the superseded attempt survived, with its own ordinal, its own
    // failing verdict, and the ground on which it was retried. Without the
    // attempt history this block cannot be satisfied at all: `attempts` would
    // hold one PASS row per node.
    let environmental_attempts: Vec<&NodeAttempt> = retried
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.environmental")
        .collect();
    let first_failed = environmental_attempts
        .iter()
        .any(|a| a.attempt == 1 && a.ok == Some(false) && a.reported);
    let second_passed = environmental_attempts
        .iter()
        .any(|a| a.attempt == 2 && a.ok == Some(true));
    let names_its_ground = environmental_attempts
        .iter()
        .any(|a| a.attempt == 1 && a.retry_class.as_deref() == Some("bpfjailer-banner"));
    if environmental_attempts.len() != 2
        || !first_failed
        || !second_passed
        || !names_its_ground
    {
        return Err(format!(
            "scheduler accounting: a node that FAILED then PASSED was recorded as if it had \
             passed first time — the retry erased the flake. attempts={:?}",
            environmental_attempts
                .iter()
                .map(|a| (a.attempt, a.ok, a.reported, a.retry_class.clone()))
                .collect::<Vec<_>>()
        ));
    }
    let environmental_outcome = retried
        .outcomes
        .iter()
        .find(|outcome| outcome.tag == "fixture.environmental")
        .ok_or("scheduler accounting: recovered environmental outcome disappeared")?;
    let environmental_gate =
        ledger_gate_with_attempts(environmental_outcome, &retried.attempts);
    if environmental_gate["result"] != "pass"
        || environmental_gate["retries"] != 1
        || environmental_gate["attempts"].as_array().map(Vec::len) != Some(2)
        || environmental_gate["attempts"][0]["result"] != "fail"
        || environmental_gate["attempts"][0]["retry_class"] != "bpfjailer-banner"
        || environmental_gate["attempts"][1]["result"] != "pass"
        || !environmental_gate["attempts"][1]["retry_class"].is_null()
    {
        return Err(format!(
            "scheduler accounting: the ledger erased or misreported a failure followed by a \
             retry pass: {environmental_gate}"
        ));
    }
    // ⚠️ THE FLAKY WARNING MUST APPEAR ON A RUN THAT PASSED, and this is the
    // bracket that holds it there. `retried` above is GREEN — ok=true, no
    // failures — and it contains a node that failed once and recovered. That is
    // precisely the run on which a flake warning looks like noise and gets
    // dropped, and precisely the run where it is the only warning anyone gets
    // before the test fails for real.
    //
    // It also pins the two halves apart: the FLAKY block present, and the
    // FAILURE banner ABSENT. A summary that printed both would be telling the
    // reader a green run had failed.
    {
        let log = "[fixture.environmental] ▶ START fixture\n\
[fixture.environmental] FAIL [ 0.100s] hermit::fixture hard_failure\n\
[fixture.environmental] FAIL [ 0.200s] hermit::fixture recovered_on_retry\n\
[fixture.environmental] FAIL [ 0.200s] hermit::fixture recovered_on_retry\n\
[fixture.environmental] ▶ START fixture\n\
[fixture.environmental] FAIL [ 0.100s] (1/2) hermit::fixture hard_failure\n\
[fixture.environmental] PASS [ 0.200s] (2/2) hermit::fixture recovered_on_retry\n";
        let observations = nextest_test_observations(log);
        // Six terminal-looking lines contain only four executions: nextest's
        // repeated failure recap in attempt 1 is not a retry.
        if observations.len() != 4 {
            return Err(format!(
                "end-of-run summary: nextest duplicate recap was counted as an execution: {:?}",
                observations
            ));
        }
        let e2e_root = tmp.join("summary-e2e");
        std::fs::create_dir_all(&e2e_root)
            .map_err(|e| format!("end-of-run summary: cannot create E2E fixture: {e}"))?;
        std::fs::write(
            e2e_root.join("results.jsonl"),
            "{\"attempt\":1,\"test\":\"applications/example\",\"category\":\"applications\",\"lane\":\"portable\",\"mode\":\"verify\",\"backend\":\"ptrace\",\"outcome\":\"FAIL\"}\n\
{\"attempt\":2,\"test\":\"applications/example\",\"category\":\"applications\",\"lane\":\"portable\",\"mode\":\"verify\",\"backend\":\"ptrace\",\"outcome\":\"PASS\"}\n",
        )
        .map_err(|e| format!("end-of-run summary: cannot write E2E fixture: {e}"))?;
        let e2e = e2e_test_observations(&e2e_root)?;
        if e2e.len() != 2
            || e2e[0].node != "e2e.manifest_applications"
            || e2e[0].id != "applications/example [ptrace/verify]"
            || e2e[0].passed
            || !e2e[1].passed
        {
            return Err(format!(
                "end-of-run summary: E2E rows did not retain test id, backend, mode, attempt, \
                 node, and verdict: {e2e:?}"
            ));
        }
        let failed_nodes = BTreeSet::from(["fixture.environmental".to_string()]);
        let split = test_id_summary(observations, &retried.attempts, &failed_nodes);
        if split.recovered.len() != 1
            || split.recovered[0].id != "hermit::fixture$recovered_on_retry"
            || split.recovered[0].retry_classes != ["bpfjailer-banner"]
            || split.failed.len() != 1
            || split.failed[0].id != "hermit::fixture$hard_failure"
            || split.failed[0].retry_classes != ["bpfjailer-banner"]
            || !split.failed_nodes_without_test_ids.is_empty()
            || split.retry_occurrences != 1
        {
            return Err(format!(
                "end-of-run summary: failed and recovered test ids were conflated, retry counts \
                 did not come from attempts[].retry_class, or a node tag leaked into the test-id \
                 list: {split:?}"
            ));
        }

        let mut red = RunSummary::new(Verdict::Fail, 1, "self-test", Vec::new());
        red.flaky = split.recovered.clone();
        red.failed_ids = split.failed.clone();
        red.retry_occurrences = split.retry_occurrences;
        red.wall_s = Some(0.0);
        red.nodes_executed = 1;
        let red_rendered = run_summary_lines(&red, std::time::Instant::now()).join("\n");
        if !red_rendered.contains(SUMMARY_FLAKY_HEADING)
            || !red_rendered.contains("1 test id(s) recovered")
            || !red_rendered.contains("1 test id(s) failed and did NOT recover")
            || !red_rendered.contains("hermit::fixture$recovered_on_retry  (1 retry")
            || !red_rendered.contains("hermit::fixture$hard_failure  (1 retry")
            || !red_rendered.contains(
                "retries: 1 occurrence(s) recorded from attempts[].retry_class",
            )
        {
            return Err(format!(
                "end-of-run summary: one hard failure and one recovered test id were not shown \
                 as separate counts with their retry counts: {red_rendered}"
            ));
        }

        let mut green = RunSummary::new(Verdict::Pass, 0, "self-test", Vec::new());
        green.flaky = split.recovered.clone();
        green.retry_occurrences = split.retry_occurrences;
        green.wall_s = Some(0.0);
        green.nodes_executed = 1;
        let rendered = run_summary_lines(&green, std::time::Instant::now()).join("\n");
        let names_the_test = rendered.contains("hermit::fixture$recovered_on_retry");
        let names_the_ground = rendered.contains("bpfjailer-banner");
        if !retried.ok
            || !rendered.contains(SUMMARY_FLAKY_HEADING)
            || !names_the_test
            || !names_the_ground
            || rendered.contains("❌ FAILURE")
        {
            return Err(format!(
                "end-of-run summary: a GREEN run carrying a recovered flake did not warn about \
                 it, or warned as a failure: lane_ok={} flaky={:?} heading={} test={} \
                 ground={} failure_banner={}",
                retried.ok,
                split.recovered,
                rendered.contains(SUMMARY_FLAKY_HEADING),
                names_the_test,
                names_the_ground,
                rendered.contains("❌ FAILURE"),
            ));
        }
        // ⚠️ THE THIRD STATE, WHICH THE TWO CHECKS AROUND IT DO NOT COVER: a run
        // with nothing to report must SAY nothing was found, not render nothing.
        // Both blocks above are conditional with no else, so before this a clean
        // run and a run whose retry accounting produced nothing were the same
        // bytes -- absence readable as a result, on the one section the owner
        // reads specifically to spot a flaky pass.
        let mut clean = RunSummary::new(Verdict::Pass, 0, "self-test", Vec::new());
        clean.wall_s = Some(0.0);
        clean.nodes_executed = 3;
        let clean_rendered = run_summary_lines(&clean, std::time::Instant::now()).join("\n");
        if !clean_rendered.contains("no retries, no flaky tests, and no failed test ids")
            || clean_rendered.contains(SUMMARY_FLAKY_HEADING)
            || clean_rendered.contains("\u{274c} FAILURE")
        {
            return Err(format!(
                "end-of-run summary: a CLEAN run must state that it found no retries and no \
                 flaky tests, so silence cannot be confused with the accounting having \
                 produced nothing: stated={} flaky_heading={} failure_banner={}",
                clean_rendered.contains("no retries, no flaky tests, and no failed test ids"),
                clean_rendered.contains(SUMMARY_FLAKY_HEADING),
                clean_rendered.contains("\u{274c} FAILURE"),
            ));
        }
        // And it must NOT claim cleanliness before a DAG ran, which would be the
        // same error inverted: nothing was counted, so nothing can be reported.
        let mut nothing_ran = RunSummary::new(Verdict::Refused, 2, "self-test", Vec::new());
        nothing_ran.wall_s = None;
        let nothing_rendered =
            run_summary_lines(&nothing_ran, std::time::Instant::now()).join("\n");
        if nothing_rendered.contains("no retries, no flaky tests, and no failed test ids") {
            return Err(
                "end-of-run summary: a run that stopped before the DAG claimed it found no \
                 flaky tests; it counted nothing and must claim nothing"
                    .to_string(),
            );
        }

    }

    // The same node must still report its TERMINAL verdict as PASS. Preserving
    // the first attempt must not turn a recovered node red, or the mechanism
    // would trade one wrong answer for another.
    if !retried
        .outcomes
        .iter()
        .any(|o| o.tag == "fixture.environmental" && o.ok)
    {
        return Err(
            "scheduler accounting: preserving the first attempt changed the node's terminal \
             verdict; the retry must record the flake, not manufacture a failure"
                .into(),
        );
    }
    if environmental_assessment(&retried.attempts, environmental_attempts[0])
        != Some((validate_runtime::EnvBlockVerdict::Confirmed, None))
    {
        return Err(
            "scheduler accounting: the actual failed-then-passed execution did not settle its \
             environmental hypothesis as CONFIRMED"
                .into(),
        );
    }

    // EXACT LOG WINDOW: attempt 1 carries both a jail banner and a real test
    // failure; attempt 2 really executes and fails but emits NO new detail. A
    // whole-log rfind would reuse attempt 1's banner. The round watermark must
    // refute the transient hypothesis, but attribution stays unknown: "banner
    // gone" requires an observed new detail region.
    let stale_log = tmp.join("stale-attempt-log.log");
    let stale_first = tmp.join("stale-attempt-first");
    let stale_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.stale_log] ----- detail -----' \
         '[fixture.stale_log] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.stale_log] assertion `left == right` failed' \
         '[fixture.stale_log] ----- end detail -----' > {log}; fi; exit 1",
        first = validate_plan::shell_quote(&stale_first.to_string_lossy()),
        log = validate_plan::shell_quote(&stale_log.to_string_lossy()),
    );
    let stale = run_lane_with_env_retries(
        &DagConfig { steps: vec![step("stale_log", &stale_cmd)], ..Default::default() },
        1,
        true,
        0,
        None,
        &stale_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let stale_attempts: Vec<&NodeAttempt> = stale
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.stale_log")
        .collect();
    if stale.ok
        || !stale.complete
        || stale_attempts.len() != 2
        || stale_attempts[0].environmental_class.as_deref() != Some("bpfjailer-banner")
        || stale_attempts[1].execution != AttemptExecution::Completed
        || stale_attempts[1].detail_observed
        || environmental_assessment(&stale.attempts, stale_attempts[0])
            != Some((validate_runtime::EnvBlockVerdict::Refuted, None))
    {
        return Err(format!(
            "scheduler accounting: a retry with no new detail reused stale attempt-1 evidence, \
             failed to refute, or invented an attribution: ok={} complete={} attempts={:?} \
             assessment={:?}",
            stale.ok,
            stale.complete,
            stale_attempts
                .iter()
                .map(|attempt| (
                    attempt.attempt,
                    attempt.execution,
                    attempt.environmental_class.as_deref(),
                    attempt.detail_observed,
                ))
                .collect::<Vec<_>>(),
            stale_attempts
                .first()
                .and_then(|attempt| environmental_assessment(&stale.attempts, attempt))
        ));
    }

    // A tee can expose the opening delimiter before the matching close. That is
    // an in-progress write, not a detail observation and not a retry ground.
    let partial_log = tmp.join("partial-detail.log");
    let partial_cmd = format!(
        "printf '%s\\n' '[fixture.partial_detail] ----- detail -----' \
         '[fixture.partial_detail] Enforcer: FS, Reason: FILE_OPEN' > {log}; exit 1",
        log = validate_plan::shell_quote(&partial_log.to_string_lossy()),
    );
    let partial = run_lane_with_env_retries(
        &DagConfig { steps: vec![step("partial_detail", &partial_cmd)], ..Default::default() },
        1,
        true,
        0,
        None,
        &partial_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let partial_attempt = partial
        .attempts
        .iter()
        .find(|attempt| attempt.tag == "fixture.partial_detail")
        .ok_or("scheduler accounting: partial-detail attempt was not recorded")?;
    // Re-pointed: always-eligible retry gives this failure one retry at max=1.
    // The teeth are the DETAIL clauses, not the count -- an unterminated detail
    // must still not be accepted as evidence, so `detail_observed` false and
    // `environmental_class` none are unchanged.
    if partial.ok
        || !partial.complete
        || partial.env_retries != 1
        || partial_attempt.detail_observed
        || partial_attempt.environmental_class.is_some()
    {
        return Err(format!(
            "scheduler accounting: unterminated detail was accepted as evidence: ok={} \
             complete={} retries={} detail_observed={} class={:?}",
            partial.ok,
            partial.complete,
            partial.env_retries,
            partial_attempt.detail_observed,
            partial_attempt.environmental_class
        ));
    }

    // EXHAUSTED RETRY OUTPUT IS EVIDENCE, NOT EXONERATION. Both executions
    // observe the same signature and fail. The terminal attempt's own hypothesis
    // is therefore UNCONFIRMED (nothing ran after it), while the node stays RED.
    // The human line must never relabel the product failure as "not a test
    // failure" merely because an environmental signature was also observed.
    let terminal_log = tmp.join("terminal-environmental.log");
    let terminal_first = tmp.join("terminal-environmental-first");
    let terminal_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.terminal_environmental] ----- detail -----' \
         '[fixture.terminal_environmental] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.terminal_environmental] ----- end detail -----' > {log}; else printf '%s\\n' \
         '[fixture.terminal_environmental] ----- detail -----' \
         '[fixture.terminal_environmental] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.terminal_environmental] ----- end detail -----' >> {log}; fi; exit 1",
        first = validate_plan::shell_quote(&terminal_first.to_string_lossy()),
        log = validate_plan::shell_quote(&terminal_log.to_string_lossy()),
    );
    let terminal = run_lane_with_env_retries(
        &DagConfig {
            steps: vec![step("terminal_environmental", &terminal_cmd)],
            ..Default::default()
        },
        1,
        true,
        0,
        None,
        &terminal_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let terminal_attempts: Vec<&NodeAttempt> = terminal
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.terminal_environmental")
        .collect();
    let terminal_line = terminal_environmental_observation(
        &terminal.attempts,
        "fixture.terminal_environmental",
    )
    .ok_or("scheduler accounting: terminal environmental observation was not rendered")?;
    let forbidden_excuse = ["NOT a test", "failure"].join(" ");
    if terminal.ok
        || !terminal.complete
        || terminal_attempts.len() != 2
        || environmental_assessment(&terminal.attempts, terminal_attempts[1])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
        || terminal_line
            != "🧱 fixture.terminal_environmental: observed environmental signature \
                bpfjailer-banner on attempt 2; terminal hypothesis UNCONFIRMED; node remains RED."
        || terminal_line.contains(&forbidden_excuse)
    {
        return Err(format!(
            "scheduler accounting: exhausted environmental retry output excused a RED or misstated \
             its evidence: complete={} ok={} attempts={} line={terminal_line:?}",
            terminal.complete,
            terminal.ok,
            terminal_attempts.len()
        ));
    }

    // COINCIDENT BANNER + REAL FAILURE: unlike the no-region case above, the
    // retry emits its own detail region with the banner gone and the assertion
    // still present. That is the evidence required for REFUTED/BannerGone, and
    // the terminal lane remains RED.
    let coincident_log = tmp.join("coincident-banner.log");
    let coincident_first = tmp.join("coincident-banner-first");
    let coincident_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.coincident] ----- detail -----' \
         '[fixture.coincident] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.coincident] assertion `left == right` failed' \
         '[fixture.coincident] ----- end detail -----' > {log}; else printf '%s\\n' \
         '[fixture.coincident] ----- detail -----' \
         '[fixture.coincident] assertion `left == right` failed' \
         '[fixture.coincident] ----- end detail -----' >> {log}; fi; exit 1",
        first = validate_plan::shell_quote(&coincident_first.to_string_lossy()),
        log = validate_plan::shell_quote(&coincident_log.to_string_lossy()),
    );
    let coincident = run_lane_with_env_retries(
        &DagConfig { steps: vec![step("coincident", &coincident_cmd)], ..Default::default() },
        1,
        true,
        0,
        None,
        &coincident_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let coincident_attempts: Vec<&NodeAttempt> = coincident
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.coincident")
        .collect();
    if coincident.ok
        || !coincident.complete
        || coincident_attempts.len() != 2
        || !coincident_attempts[1].detail_observed
        || environmental_assessment(&coincident.attempts, coincident_attempts[0])
            != Some((
                validate_runtime::EnvBlockVerdict::Refuted,
                Some(validate_runtime::RefutedShape::BannerGone),
            ))
    {
        return Err(format!(
            "scheduler accounting: coincident banner + real failure did not stay RED and settle \
             REFUTED/banner-gone: ok={} complete={} attempts={} assessment={:?}",
            coincident.ok,
            coincident.complete,
            coincident_attempts.len(),
            coincident_attempts
                .first()
                .and_then(|attempt| environmental_assessment(&coincident.attempts, attempt))
        ));
    }

    // TWO-ATTEMPT CAP: every classified attempt gets its own conclusion. The
    // first failure's only retry fails with the same signature, so attempt 1 is
    // REFUTED/Persistent and terminal attempt 2 is UNCONFIRMED. The command
    // would pass on attempt 3; reaching that pass would prove the cap failed.
    let multi_log = tmp.join("multiple-retries.log");
    let multi_first = tmp.join("multiple-retries-first");
    let multi_second = tmp.join("multiple-retries-second");
    let multi_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.multiple] ----- detail -----' \
         '[fixture.multiple] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.multiple] ----- end detail -----' > {log}; exit 1; \
         elif test ! -e {second}; then : > {second}; printf '%s\\n' \
         '[fixture.multiple] ----- detail -----' \
         '[fixture.multiple] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.multiple] ----- end detail -----' >> {log}; exit 1; fi",
        first = validate_plan::shell_quote(&multi_first.to_string_lossy()),
        second = validate_plan::shell_quote(&multi_second.to_string_lossy()),
        log = validate_plan::shell_quote(&multi_log.to_string_lossy()),
    );
    let multiple = run_lane_with_env_retries(
        &DagConfig { steps: vec![step("multiple", &multi_cmd)], ..Default::default() },
        1,
        true,
        0,
        None,
        &multi_log,
        None,
        2,
        &BTreeMap::new(),
        false,
    );
    let multiple_attempts: Vec<&NodeAttempt> = multiple
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.multiple")
        .collect();
    if multiple.ok
        || !multiple.complete
        || multiple_attempts.len() != 2
        || environmental_assessment(&multiple.attempts, multiple_attempts[0])
            != Some((
                validate_runtime::EnvBlockVerdict::Refuted,
                Some(validate_runtime::RefutedShape::Persistent),
            ))
        || environmental_assessment(&multiple.attempts, multiple_attempts[1])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
    {
        return Err(format!(
            "scheduler accounting: a failure/failure/pass-on-third fixture crossed the \
             two-attempt cap or collapsed its per-attempt environmental verdicts: ok={} \
             complete={} attempts={} first={:?} second={:?}",
            multiple.ok,
            multiple.complete,
            multiple_attempts.len(),
            multiple_attempts
                .first()
                .and_then(|attempt| environmental_assessment(&multiple.attempts, attempt)),
            multiple_attempts
                .get(1)
                .and_then(|attempt| environmental_assessment(&multiple.attempts, attempt))
        ));
    }

    // The E2E harness writes every retry to the same bucket results.jsonl. Its
    // row must therefore carry the scheduler's actual attempt ordinal rather
    // than inferring one from whatever rows survived an interrupted attempt.
    let e2e_attempts = tmp.join("e2e-attempts");
    let e2e_log = tmp.join("e2e-attempts.log");
    let e2e_cmd = format!(
        "printf '%s\\n' \"$E2E_ATTEMPT\" >> {}; exit 1",
        validate_plan::shell_quote(&e2e_attempts.to_string_lossy()),
    );
    let mut e2e_step = step("manifest_attempt", &e2e_cmd);
    e2e_step.group = "e2e".into();
    e2e_step.job = "manifest_attempt".into();
    set_manifest_attempt(&mut e2e_step, 1);
    let e2e_retry = run_lane_with_env_retries(
        &DagConfig { steps: vec![e2e_step], ..Default::default() },
        1,
        true,
        0,
        None,
        &e2e_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let recorded_attempts = std::fs::read_to_string(&e2e_attempts)
        .map_err(|e| format!("scheduler accounting: cannot read E2E attempt fixture: {e}"))?;
    if e2e_retry.ok || recorded_attempts.lines().collect::<Vec<_>>() != ["1", "2"] {
        return Err(format!(
            "scheduler accounting: E2E retry did not receive attempts 1 then 2: ok={} rows={recorded_attempts:?}",
            e2e_retry.ok
        ));
    }

    // REPORTED IS NOT EXECUTED. A real first execution classifies, then the
    // wrapper removes itself so the retry's `Command::spawn` fails. Dagrun
    // reports that failure as non-aborted so the DAG cannot wedge, but its
    // returncode is absent and the step's own result is UNKNOWN. That row must
    // neither confirm nor refute the first classification, and it makes the lane
    // incomplete even though a terminal scheduler outcome exists.
    let spawn_log = tmp.join("spawn-unknown.log");
    let fake_bin = tmp.join("spawn-unknown-bin");
    std::fs::create_dir(&fake_bin)
        .map_err(|e| format!("scheduler accounting: cannot create fake bin: {e}"))?;
    let fake_bash = fake_bin.join("bash");
    std::fs::write(
        &fake_bash,
        "#!/bin/sh\n/bin/rm -f -- \"$0\"\nexec /bin/bash \"$@\"\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write fake bash: {e}"))?;
    std::fs::set_permissions(&fake_bash, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("scheduler accounting: cannot chmod fake bash: {e}"))?;
    let spawn_cmd = format!(
        "printf '%s\\n' '[fixture.spawn_unknown] ----- detail -----' \
         '[fixture.spawn_unknown] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.spawn_unknown] ----- end detail -----' > {log}; exit 1",
        log = validate_plan::shell_quote(&spawn_log.to_string_lossy()),
    );
    let mut spawn_step = step("spawn_unknown", &spawn_cmd);
    spawn_step
        .env
        .insert("PATH".into(), fake_bin.to_string_lossy().into_owned());
    let spawn_unknown = run_lane_with_env_retries(
        &DagConfig { steps: vec![spawn_step], ..Default::default() },
        1,
        true,
        0,
        None,
        &spawn_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let spawn_attempts: Vec<&NodeAttempt> = spawn_unknown
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.spawn_unknown")
        .collect();
    if spawn_unknown.ok
        || spawn_unknown.complete
        || spawn_attempts.len() != 2
        || spawn_attempts[0].execution != AttemptExecution::Completed
        || spawn_attempts[1].execution != AttemptExecution::Unknown
        || !spawn_unknown
            .outcomes
            .iter()
            .any(|outcome| outcome.tag == "fixture.spawn_unknown"
                && outcome.summary.contains("spawn failed"))
        || environmental_assessment(&spawn_unknown.attempts, spawn_attempts[0])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
    {
        return Err(format!(
            "scheduler accounting: a reported spawn failure falsely settled an environmental \
             hypothesis or left the lane complete: ok={} complete={} attempts={:?} assessment={:?}",
            spawn_unknown.ok,
            spawn_unknown.complete,
            spawn_attempts
                .iter()
                .map(|attempt| (attempt.attempt, attempt.execution, attempt.reason.as_str()))
                .collect::<Vec<_>>(),
            spawn_attempts
                .first()
                .and_then(|attempt| environmental_assessment(&spawn_unknown.attempts, attempt))
        ));
    }
    let spawn_outcome = spawn_unknown
        .outcomes
        .iter()
        .find(|outcome| outcome.tag == "fixture.spawn_unknown")
        .ok_or("scheduler accounting: reported spawn failure disappeared")?;
    let spawn_gate = ledger_gate_with_attempts(spawn_outcome, &spawn_unknown.attempts);
    if !spawn_gate["result"].is_null()
        || spawn_gate["reported"] != true
        || spawn_gate["execution"] != "unknown"
        || !spawn_gate["failure_origin"].is_null()
        || spawn_gate.get("failed_substeps").is_some()
    {
        return Err(format!(
            "scheduler accounting: reported-but-unexecuted spawn failure acquired false failure \
             evidence: {spawn_gate}"
        ));
    }

    // ROUND-LOCAL MISSING OUTCOME. In round 1 the prerequisite and environmental
    // node run together; the latter fails after the prerequisite passes, leaving
    // the higher-cost slot peer unlaunched. In the retry both are ready, the peer
    // wins the single slot and fails eagerly, so the environmental node receives
    // no outcome at all. Computing absence after merging into cumulative by_tag
    // would see attempt 1's stale failure and lose this unknown attempt.
    let missing_log = tmp.join("missing-retry-outcome.log");
    let missing_cmd = format!(
        "printf '%s\\n' '[fixture.missing_retry] ----- detail -----' \
         '[fixture.missing_retry] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.missing_retry] ----- end detail -----' > {log}; sleep 0.3; exit 1",
        log = validate_plan::shell_quote(&missing_log.to_string_lossy()),
    );
    let mut missing_environmental = step("missing_retry", &missing_cmd);
    missing_environmental.hint.est_duration_s = 10.0;
    missing_environmental.hint.resources.insert("serial".into(), 1);
    let mut spoiler = step("retry_spoiler", "exit 1");
    spoiler.deps = vec!["fixture.retry_prerequisite".into()];
    spoiler.hint.est_duration_s = 20.0;
    spoiler.hint.resources.insert("serial".into(), 1);
    let mut missing_cfg = DagConfig {
        steps: vec![
            missing_environmental,
            step("retry_prerequisite", "true"),
            spoiler,
        ],
        ..Default::default()
    };
    missing_cfg.resource_caps.insert("serial".into(), 1);
    let missing = run_lane_with_env_retries(
        &missing_cfg,
        2,
        false,
        0,
        None,
        &missing_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let missing_attempts: Vec<&NodeAttempt> = missing
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.missing_retry")
        .collect();
    if missing.complete
        || missing_attempts.len() != 2
        || !missing_attempts[0].reported
        || missing_attempts[1].reported
        || missing_attempts[1].attempt != 2
        || missing_attempts[1].execution != AttemptExecution::Unknown
        || environmental_assessment(&missing.attempts, missing_attempts[0])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
        || exit_code_with_execution_completeness(0, missing.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: round-local missing retry completion was hidden by cumulative \
             state: complete={} attempts={:?} assessment={:?}",
            missing.complete,
            missing_attempts
                .iter()
                .map(|attempt| (attempt.attempt, attempt.reported, attempt.execution))
                .collect::<Vec<_>>(),
            missing_attempts
                .first()
                .and_then(|attempt| environmental_assessment(&missing.attempts, attempt))
        ));
    }
    let missing_outcome = missing
        .outcomes
        .iter()
        .find(|outcome| outcome.tag == "fixture.missing_retry")
        .ok_or("scheduler accounting: stale terminal missing-retry outcome disappeared")?;
    let missing_gate = ledger_gate_with_attempts(missing_outcome, &missing.attempts);
    let missing_first_render = retry_attempt_line(&missing.attempts, missing_attempts[0], 2);
    let missing_latest_render = retry_attempt_line(&missing.attempts, missing_attempts[1], 2);
    if !missing_gate["result"].is_null()
        || missing_gate["reported"] != false
        || missing_gate["execution"] != "unknown"
        || !missing_gate["exit_code"].is_null()
        || !missing_gate["real_seconds"].is_null()
        || missing_gate["failure_origin"].is_string()
        || missing_gate.get("failed_substeps").is_some()
        || missing_gate["attempts"][1]["attempt"] != 2
        || !missing_gate["attempts"][1]["result"].is_null()
        || !missing_first_render.contains("ENVIRONMENTAL UNCONFIRMED")
        || !missing_first_render.contains("no actual re-execution completed")
        || !missing_latest_render.contains("attempt 2/2: unknown step result")
    {
        return Err(format!(
            "scheduler accounting: JSON or human rendering reused the stale attempt-1 failure for \
             a latest unreported retry: gate={missing_gate} first={missing_first_render:?} \
             latest={missing_latest_render:?}"
        ));
    }

    // MISSING ATTEMPT PLUS ANOTHER FAILURE. Attempt 2 of `multi_missing`
    // produces no payload because `multi_trigger` wins the only serial slot and
    // fails with its own environmental signature. Both cells have now spent
    // their two attempts, so neither may start a third. The explicit
    // latest-unreported set keeps the missing attempt visible and the lane RED.
    // One dependent completes after the prerequisite, and only the next dependent
    // writes the FIFO. The writer therefore cannot start until the scheduler has
    // recorded both earlier steps as complete. Waiting on that write keeps the
    // first failure from racing a fixed delay against prerequisite completion.
    let multi_missing_log = tmp.join("multi-round-missing.log");
    let multi_missing_first = tmp.join("multi-round-missing-first");
    let multi_trigger_first = tmp.join("multi-round-trigger-first");
    let multi_prerequisite_completed = tmp.join("multi-prerequisite-completed");
    let multi_prerequisite_dependent_first = tmp.join("multi-prerequisite-dependent-first");
    let mkfifo = Command::new("mkfifo")
        .arg(&multi_prerequisite_completed)
        .status()
        .map_err(|e| format!("scheduler accounting: cannot create prerequisite fifo: {e}"))?;
    if !mkfifo.success() {
        return Err(format!("scheduler accounting: mkfifo failed with {mkfifo}"));
    }
    let multi_missing_cmd = format!(
        "if test ! -e {first}; then : > {first}; read prerequisite_complete < {completed}; \
         printf '%s\\n' \
         '[fixture.multi_missing] ----- detail -----' \
         '[fixture.multi_missing] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.multi_missing] ----- end detail -----' > {log}; exit 1; fi",
        first = validate_plan::shell_quote(&multi_missing_first.to_string_lossy()),
        completed = validate_plan::shell_quote(&multi_prerequisite_completed.to_string_lossy()),
        log = validate_plan::shell_quote(&multi_missing_log.to_string_lossy()),
    );
    let multi_trigger_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.multi_trigger] ----- detail -----' \
         '[fixture.multi_trigger] Enforcer: FS, Reason: FILE_OPEN' \
         '[fixture.multi_trigger] ----- end detail -----' >> {log}; exit 1; fi",
        first = validate_plan::shell_quote(&multi_trigger_first.to_string_lossy()),
        log = validate_plan::shell_quote(&multi_missing_log.to_string_lossy()),
    );
    let mut multi_missing_step = step("multi_missing", &multi_missing_cmd);
    multi_missing_step.hint.est_duration_s = 10.0;
    multi_missing_step.hint.resources.insert("serial".into(), 1);
    let mut multi_trigger_step = step("multi_trigger", &multi_trigger_cmd);
    multi_trigger_step.deps = vec!["fixture.multi_prerequisite".into()];
    multi_trigger_step.hint.est_duration_s = 20.0;
    multi_trigger_step.hint.resources.insert("serial".into(), 1);
    let mut multi_prerequisite_dependent = step("multi_prerequisite_dependent", "true");
    multi_prerequisite_dependent.deps = vec!["fixture.multi_prerequisite".into()];
    let mut multi_prerequisite_dependent_writer = step(
        "multi_prerequisite_dependent_writer",
        &format!(
            "if test ! -e {first}; then : > {first}; printf '%s\\n' complete > {completed}; fi",
            first = validate_plan::shell_quote(
                &multi_prerequisite_dependent_first.to_string_lossy()
            ),
            completed =
                validate_plan::shell_quote(&multi_prerequisite_completed.to_string_lossy())
        ),
    );
    multi_prerequisite_dependent_writer.deps =
        vec!["fixture.multi_prerequisite_dependent".into()];
    let mut multi_missing_cfg = DagConfig {
        steps: vec![
            multi_missing_step,
            step("multi_prerequisite", "true"),
            multi_prerequisite_dependent,
            multi_prerequisite_dependent_writer,
            multi_trigger_step,
        ],
        ..Default::default()
    };
    multi_missing_cfg.resource_caps.insert("serial".into(), 1);
    let multi_missing = run_lane_with_env_retries(
        &multi_missing_cfg,
        2,
        false,
        0,
        None,
        &multi_missing_log,
        None,
        2,
        &BTreeMap::new(),
        false,
    );
    let multi_missing_attempts: Vec<&NodeAttempt> = multi_missing
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.multi_missing")
        .collect();
    let multi_trigger_attempts: Vec<&NodeAttempt> = multi_missing
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.multi_trigger")
        .collect();
    let multi_prerequisite_attempts: Vec<&NodeAttempt> = multi_missing
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.multi_prerequisite")
        .collect();
    let multi_prerequisite_dependent_attempts: Vec<&NodeAttempt> = multi_missing
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.multi_prerequisite_dependent")
        .collect();
    if multi_missing.complete
        || multi_missing.ok
        || multi_missing.env_retries != 1
        || multi_missing_attempts.len() != 2
        || multi_missing_attempts[0].execution != AttemptExecution::Completed
        || multi_missing_attempts[1].execution != AttemptExecution::Unknown
        || multi_missing_attempts[1].reported
        || multi_trigger_attempts.len() != 2
        || multi_trigger_attempts[0].execution != AttemptExecution::Unknown
        || multi_trigger_attempts[1].ok != Some(false)
        || multi_prerequisite_attempts.len() != 1
        || multi_prerequisite_attempts[0].execution != AttemptExecution::Completed
        || multi_prerequisite_attempts[0].ok != Some(true)
        || multi_prerequisite_dependent_attempts.last().is_none_or(|attempt| {
            attempt.execution != AttemptExecution::Completed || attempt.ok != Some(true)
        })
        || environmental_assessment(&multi_missing.attempts, multi_missing_attempts[0])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
        || environmental_assessment(&multi_missing.attempts, multi_trigger_attempts[1])
            != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
    {
        return Err(format!(
            "scheduler accounting: a missing cell or later failure crossed the two-attempt cap, \
             disappeared, or became green: complete={} ok={} retries={} missing={:?} trigger={:?} \
             prerequisite={:?} prerequisite_dependent={:?}",
            multi_missing.complete,
            multi_missing.ok,
            multi_missing.env_retries,
            multi_missing_attempts
                .iter()
                .map(|attempt| (attempt.attempt, attempt.reported, attempt.execution, attempt.ok))
                .collect::<Vec<_>>(),
            multi_trigger_attempts
                .iter()
                .map(|attempt| (attempt.attempt, attempt.reported, attempt.execution, attempt.ok))
                .collect::<Vec<_>>(),
            multi_prerequisite_attempts
                .iter()
                .map(|attempt| (attempt.attempt, attempt.reported, attempt.execution, attempt.ok))
                .collect::<Vec<_>>(),
            multi_prerequisite_dependent_attempts
                .iter()
                .map(|attempt| (attempt.attempt, attempt.reported, attempt.execution, attempt.ok))
                .collect::<Vec<_>>()
        ));
    }

    // CARRIED-PEER CAP. With two total attempts, every planned cell has either a
    // reported or an explicit unknown attempt 1 before the first retry starts.
    // Therefore a natural second retry round cannot be constructed without first
    // applying this selection rule. Exercise the exact production helper instead:
    // a dependency-skipped peer already at attempt 2 must be removed after all
    // carried sources are unioned, while the failed cell at attempt 1 remains.
    let capped = "fixture.capped_peer";
    let available = "fixture.available_peer";
    let retry_set_cfg = DagConfig {
        steps: vec![step("capped_peer", "true"), step("available_peer", "true")],
        ..Default::default()
    };
    let retry_set_attempts = vec![
        unreported_attempt(capped.into(), 1),
        unreported_attempt(capped.into(), 2),
        unreported_attempt(available.into(), 1),
    ];
    let retry_set = retry_candidate_tags(
        &retry_set_cfg,
        &[(available.to_string(), "always-eligible: exit 1".to_string())],
        &[capped.to_string()],
        &BTreeMap::new(),
        &BTreeSet::new(),
        &retry_set_attempts,
    );
    if retry_set != BTreeSet::from([available.to_string()]) {
        return Err(format!(
            "retry budget: the production retry-set path admitted a carried peer after its \
             two attempts: {retry_set:?}"
        ));
    }

    // ---- the bound-kill ground, and the two refusals that bound it ----------
    //
    // The measured instance is `scorecard.compatibility` at SHA 485a0ad4: it
    // timed out at 120.45s in 1 of 6 runs of that identical commit and passed in
    // 44.63-45.62s in the other five, while a concurrently started peer held the
    // shared Cargo build directory. This fixture reproduces the SHAPE — a node
    // killed by its own wall budget on attempt 1 that completes on attempt 2 —
    // without reproducing the 120-second cost.
    let bound_log = tmp.join("bound-kill.log");
    let bound_first = tmp.join("bound-kill-first-attempt");
    let bound_cmd = format!(
        "if test ! -e {first}; then : > {first}; sleep 30; fi",
        first = validate_plan::shell_quote(&bound_first.to_string_lossy()),
    );
    let bound_cfg = DagConfig {
        steps: vec![step_with_caps(
            "fixture",
            "bound_kill",
            "validate scheduler accounting fixture",
            bound_cmd,
            Vec::new(),
            2,
            2,
            64 * 1024 * 1024,
        )],
        ..Default::default()
    };
    let bound = run_lane_with_env_retries(
        &bound_cfg,
        1,
        true,
        0,
        None,
        &bound_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let bound_attempts: Vec<&NodeAttempt> = bound
        .attempts
        .iter()
        .filter(|attempt| attempt.tag == "fixture.bound_kill")
        .collect();
    let bound_ground = bound_attempts
        .iter()
        .find(|a| a.attempt == 1)
        .and_then(|a| a.retry_class.clone())
        .unwrap_or_default();
    if !bound.ok
        || bound.env_retries != 1
        || bound_attempts.len() != 2
        || bound_attempts[0].ok != Some(false)
        || bound_attempts[1].ok != Some(true)
        || !bound_ground.starts_with("bound-kill under contention")
    {
        return Err(format!(
            "scheduler accounting: a node killed by its own wall budget was not retried on the \
             bound-kill ground, or the timed-out attempt was not preserved: ok={} retries={} \
             attempts={:?} ground={bound_ground:?}",
            bound.ok,
            bound.env_retries,
            bound_attempts.iter().map(|a| (a.attempt, a.ok)).collect::<Vec<_>>()
        ));
    }

    // ALWAYS-ELIGIBLE ORDINARY FAILURE. Nothing classifies it more specifically,
    // so the blanket ground grants its one retry.
    let ordinary_only_log = tmp.join("ordinary-only.log");
    std::fs::write(
        &ordinary_only_log,
        "[fixture.plain_red] ----- detail -----\n\
[fixture.plain_red] assertion failed: 1 == 2\n\
[fixture.plain_red] ----- end detail -----\n",
    )
    .map_err(|e| format!("scheduler accounting: cannot write the ordinary-only log: {e}"))?;
    let ordinary_only = run_lane_with_env_retries(
        &DagConfig { steps: vec![step("plain_red", "exit 1")], ..Default::default() },
        1,
        true,
        0,
        None,
        &ordinary_only_log,
        None,
        2,
        &BTreeMap::new(),
        false,
    );
    // ⚠️ THIS BRACKET ASSERTED THE OPT-IN POLICY ITSELF AND THE OWNER SUPERSEDED IT
    // (2026-08-26). It required an unclassified product failure to cost ONE attempt
    // -- that was the whole point of opt-in retry. Every cell is now always
    // eligible, so this failure gets one retry and two total attempts.
    //
    // WHAT IS STILL ASSERTED, and is the part worth keeping: the lane is STILL NOT
    // ok. Retrying a genuine red twice must return the same red, never a
    // pass. The cost changed by directive; the verdict must not.
    // ⚠️ PER-CELL BUDGET (owner ruling 2026-08-26): this fixture fails every time,
    // so it uses its whole allowance and stays red. The lane round backstop passed
    // above exceeds the one retry it needs; the per-cell cap is what stops it.
    //
    // The verdict is the invariant: retrying a genuine red twice
    // must return the same red. The cost is policy; the verdict is not.
    if ordinary_only.ok || ordinary_only.env_retries != 1 || ordinary_only.attempts.len() != 2 {
        return Err(format!(
            "scheduler accounting: an always-eligible failure must spend its per-cell \
             allowance and STILL be red: ok={} retries={} attempts={}",
            ordinary_only.ok,
            ordinary_only.env_retries,
            ordinary_only.attempts.len()
        ));
    }

    // ⚠️ THE POINT OF THE RULING: A SECOND CELL GETS ITS OWN TWO, WHATEVER THE
    // FIRST SPENT. Two cells in ONE lane, both failing every time. Under the lane
    // round budget that shipped first, the two shared a pool: whichever failed
    // first consumed the rounds and the other's chance of recovering depended on
    // its neighbour. That is the incoherence the ruling removes, so it is pinned
    // here rather than described.
    //
    // Both must reach exactly MAX_ATTEMPTS_PER_CELL, and the cap must be what
    // stops them: removing the per-cell gate lets the derived lane backstop run
    // them to three attempts each and this bracket fails.
    //
    // ⚠️ WHAT THIS BRACKET DOES NOT PROVE, MEASURED RATHER THAN ASSUMED. It does
    // not discriminate a per-cell cap from a lane-round budget on its own. These
    // two cells fail in the SAME round, so they share every round efficiently and
    // a lane budget of 1 round also yields 2 attempts each. The starvation case the
    // ruling names -- one cell exhausting the pool BEFORE another has failed --
    // cannot be built in this harness, because a node that has passed is never
    // added to a retry set and so can never fail late. The per-cell property is
    // carried by the gate's position in the code, ahead of every ground and keyed
    // on the cell's own tag; this bracket pins that the gate binds at two.
    let twin_log = tmp.join("twin.log");
    let twin_cfg = DagConfig {
        steps: vec![
            step("twin_a", "exit 1"),
            step("twin_b", "exit 1"),
        ],
        ..Default::default()
    };
    let twins = run_lane_with_env_retries(
        &twin_cfg,
        2,
        true,
        0,
        None,
        &twin_log,
        None,
        validate_runtime::lane_round_backstop(twin_cfg.steps.len()),
        &BTreeMap::new(),
        false,
    );
    let attempts_for = |tag: &str| -> usize {
        twins
            .attempts
            .iter()
            .filter(|a| a.tag == tag)
            .map(|a| a.attempt)
            .max()
            .unwrap_or(0)
    };
    let (a, b) = (attempts_for("fixture.twin_a"), attempts_for("fixture.twin_b"));
    if twins.ok
        || a != validate_runtime::MAX_ATTEMPTS_PER_CELL
        || b != validate_runtime::MAX_ATTEMPTS_PER_CELL
    {
        return Err(format!(
            "retry budget: a second failing cell in the same lane did not get its own \
             {} attempts independently of the first -- this is the lane-budget \
             incoherence the per-cell ruling removes: twin_a={a} twin_b={b} ok={}",
            validate_runtime::MAX_ATTEMPTS_PER_CELL,
            twins.ok
        ));
    }
    // And the cap is a CAP: neither may exceed it, or "two attempts" means
    // nothing. The equality above already pins this; stated so a later edit that
    // relaxes it to `>=` has to argue with a comment.

    // REFUSAL 2: a registry entry with a ONE-SIDED sample grants no retry. This
    // is the structural-`no_result` shape the flakiness investigation measured
    // (eight DBT identities `no_result` 5 of 5 because DBT never publishes a
    // terminal verify report), and retrying it returns the same answer twice
    // times. The registry reader must reject it on its own numbers.
    let registry = tmp.join("flaky-cells.json");
    std::fs::write(
        &registry,
        r#"{"schema":1,"cells":[
             {"cell":"real_flake","observed_pass":9,"observed_fail":1,"measured_at":"2026-08-24"},
             {"cell":"structural_no_result","observed_pass":0,"observed_fail":5,"measured_at":"2026-08-24"},
             {"cell":"no_sample_at_all"}
           ]}"#,
    )
    .map_err(|e| format!("scheduler accounting: cannot write the registry fixture: {e}"))?;
    // SAFETY: the self-test is single-threaded here, and the variable is unset
    // again immediately below so no later bracket inherits it.
    unsafe { std::env::set_var("VALIDATE_FLAKY_CELL_REGISTRY", &registry) };
    let admitted = validate_runtime::measured_unstable_nodes(None);
    unsafe { std::env::remove_var("VALIDATE_FLAKY_CELL_REGISTRY") };
    let admitted_names: Vec<&str> = admitted.keys().map(String::as_str).collect();
    if admitted_names != vec!["real_flake"] {
        return Err(format!(
            "scheduler accounting: the measured-instability registry admitted an entry without a \
             two-sided sample; a 0-pass/5-fail identity is structural, not flaky: admitted={admitted_names:?}"
        ));
    }
    // The same registry must match a DAG node whose tag carries its group.
    if validate_runtime::measured_unstable_class(&admitted, "test.real_flake").is_none()
        || validate_runtime::measured_unstable_class(&admitted, "test.structural_no_result")
            .is_some()
    {
        return Err(
            "scheduler accounting: registry lookup did not match a `group.job` tag, or matched a \
             refused entry"
                .into(),
        );
    }

    // An environmental failure may be retried alongside an ordinary failure,
    // but a dependent of the ordinary failure must not become runnable merely
    // because its failed prerequisite is outside the retry DAG. This uses the
    // real scheduler path with both independent failures running concurrently.
    let mixed_log = tmp.join("ordinary-and-environmental.log");
    let ordinary_attempts = tmp.join("ordinary-attempts");
    let mixed_environmental_first = tmp.join("mixed-environmental-first-attempt");
    let mixed_environmental_passed = tmp.join("mixed-environmental-passed");
    let unsafe_dependent_ran = tmp.join("unsafe-dependent-ran");
    let unsafe_transitive_dependent_ran = tmp.join("unsafe-transitive-dependent-ran");
    let ordinary_cmd = format!(
        "printf 'attempt\\n' >> {}; sleep 0.1; exit 1",
        validate_plan::shell_quote(&ordinary_attempts.to_string_lossy())
    );
    let mixed_environmental_cmd = format!(
        "if test ! -e {first}; then : > {first}; printf '%s\\n' \
         '[fixture.environmental_mixed] ----- detail -----' \
         '[fixture.environmental_mixed] An action was blocked on this server based on a security policy!' \
         '[fixture.environmental_mixed] ----- end detail -----' > {log}; exit 1; fi; : > {passed}",
        first = validate_plan::shell_quote(&mixed_environmental_first.to_string_lossy()),
        passed = validate_plan::shell_quote(&mixed_environmental_passed.to_string_lossy()),
        log = validate_plan::shell_quote(&mixed_log.to_string_lossy()),
    );
    let mut unsafe_dependent = step(
        "unsafe_dependent",
        &format!(
            ": > {}",
            validate_plan::shell_quote(&unsafe_dependent_ran.to_string_lossy())
        ),
    );
    unsafe_dependent.deps = vec!["fixture.ordinary_failure".into()];
    let mut unsafe_transitive_dependent = step(
        "unsafe_transitive_dependent",
        &format!(
            ": > {}",
            validate_plan::shell_quote(&unsafe_transitive_dependent_ran.to_string_lossy())
        ),
    );
    unsafe_transitive_dependent.deps = vec!["fixture.unsafe_dependent".into()];
    let mixed_cfg = DagConfig {
        steps: vec![
            step("ordinary_failure", &ordinary_cmd),
            step("environmental_mixed", &mixed_environmental_cmd),
            unsafe_dependent,
            unsafe_transitive_dependent,
        ],
        ..Default::default()
    };
    let mixed = run_lane_with_env_retries(
        &mixed_cfg,
        2,
        true,
        0,
        None,
        &mixed_log,
        None,
        1,
        &BTreeMap::new(),
        false,
    );
    let mixed_by_tag: BTreeMap<&str, &StepOutcome> = mixed
        .outcomes
        .iter()
        .map(|outcome| (outcome.tag.as_str(), outcome))
        .collect();
    let ordinary_attempt_count = std::fs::read_to_string(&ordinary_attempts)
        .unwrap_or_default()
        .lines()
        .count();
    // Re-pointed for always-eligible retry: the ordinary failure is now retried
    // too, so its attempt count moved 1 -> 2. MEASURED that this is the ONLY
    // clause that moved -- dependent_ran=false, transitive_dependent_ran=false,
    // environmental_passed=true and both outcomes were unchanged. The safety
    // property this bracket exists for, that a retry must NOT cross an
    // unsatisfied ordinary dependency into its dependents, still holds and its
    // clauses below are untouched.
    if mixed.complete
        || mixed.env_retries != 1
        || ordinary_attempt_count != 2
        || mixed_by_tag
            .get("fixture.ordinary_failure")
            .is_none_or(|outcome| outcome.ok || outcome.aborted)
        || mixed_by_tag
            .get("fixture.environmental_mixed")
            .is_none_or(|outcome| !outcome.ok || outcome.aborted)
        || !mixed_environmental_passed.is_file()
        || unsafe_dependent_ran.exists()
        || unsafe_transitive_dependent_ran.exists()
        || mixed_by_tag.contains_key("fixture.unsafe_dependent")
        || mixed_by_tag.contains_key("fixture.unsafe_transitive_dependent")
        || exit_code_with_execution_completeness(0, mixed.complete) == 0
    {
        return Err(format!(
            "scheduler accounting: environmental retry crossed an unsatisfied ordinary dependency: complete={} ok={} retries={} ordinary_attempts={ordinary_attempt_count} environmental_passed={} dependent_ran={} transitive_dependent_ran={} outcomes={:?}",
            mixed.complete,
            mixed.ok,
            mixed.env_retries,
            mixed_environmental_passed.is_file(),
            unsafe_dependent_ran.exists(),
            unsafe_transitive_dependent_ran.exists(),
            mixed
                .outcomes
                .iter()
                .map(|outcome| (outcome.tag.as_str(), outcome.ok, outcome.aborted))
                .collect::<Vec<_>>()
        ));
    }
    Ok(())
    })();

    let cleanup = std::fs::remove_dir_all(&tmp)
        .map_err(|e| format!("scheduler accounting: cannot remove {}: {e}", tmp.display()));
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(
            "scheduler accounting: complete and allowed-failure plans accepted; fail-fast, \
             skipped, aborted, exact attempt identity, and three-state environmental retry \
             verdicts bracketed"
                .into(),
        ),
        (Err(problem), Ok(())) => Err(problem),
        (Ok(()), Err(cleanup_problem)) => Err(cleanup_problem),
        (Err(problem), Err(cleanup_problem)) => Err(format!(
            "{problem}; cleanup also failed: {cleanup_problem}"
        )),
    }
}

/// Run one lane, auto-retrying nodes whose failure is an ENVIRONMENTAL block.
///
/// This is `run_check_with_timeout`'s retry loop (validate.sh:2119) moved to DAG
/// granularity. A host FS-permission denial (BPFJailer banner, or a banner-less
/// `EPERM` leaked to `cc1`/`cmake`/`ld`), a `fwdproxy` egress failure, or a
/// vendored third-party (DynamoRIO/elfutils) build flake kills a build or test
/// subprocess for reasons that have nothing to do with the tree under test. Left
/// alone it masquerades as a product failure; the whole point of this loop is
/// that it must not.
///
/// Three properties are preserved from the bash, deliberately:
///
/// * **The classification reads the FAILING NODE's own output**, extracted from
///   the runner's `[tag] ----- detail -----` region, not a whole-log tail. A jail
///   banner printed by a different concurrent node cannot excuse a real red.
/// * **Retries are bounded per cell**: one initial attempt plus exactly one
///   retry attempt. A *persistent* breakage — a bad Reverie pin, a genuinely missing
///   header — fails every attempt and still leaves the run RED. Its per-attempt
///   environmental hypothesis is refuted or left unconfirmed, never silently
///   promoted to a pass.
/// * **Nodes that never ran because the blocked node failed are retried only
///   when their prerequisites remain valid.** Every prerequisite must either
///   have completed successfully already or execute in the same retry. Internal
///   dependency edges are preserved, and removing an unsafe prerequisite also
///   removes its dependents before anything runs.
fn run_lane_with_env_retries(
    cfg: &DagConfig,
    jobs: i64,
    keep_going: bool,
    verbosity: i64,
    cgroups: BoxedCgroups,
    log_path: &Path,
    deadline: Option<u64>,
    max: usize,
    // Nodes whose failure is retry-eligible because instability was MEASURED,
    // mapped to the sample that established it. Empty means "no registry was
    // reachable", and only the environmental classes remain eligible.
    unstable: &BTreeMap<String, String>,
    record_step_profiles: bool,
) -> LaneResult {
    if remaining_budget_s(deadline) == Some(0) {
        eprintln!(
            "validate: whole-run budget expired during setup; no DAG node will be started \
             unbounded, and every planned node is recorded as not attempted"
        );
        return LaneResult {
            outcomes: Vec::new(),
            skipped: cfg.steps.iter().map(|s| s.tag()).collect(),
            // Nothing was dispatched, so there is no attempt to record. An empty
            // list is the honest shape: zero attempts, not one failed attempt.
            attempts: Vec::new(),
            complete: false,
            ok: false,
            env_retries: 0,
            run_timed_out: true,
        };
    }
    let mut round_log_start = settled_log_len(log_path);
    let mut first_cfg = cfg.clone();
    for step in &mut first_cfg.steps {
        set_manifest_attempt(step, 1);
    }
    let first = run_dag_boxed_deadline(
        &first_cfg,
        jobs,
        keep_going,
        verbosity,
        cgroups.clone(),
        None,
        Some(scheduler_cpu_budget()),
        remaining_budget_s(deadline),
    );
    if record_step_profiles {
        forward_step_profiles(&first, jobs);
    }
    let mut run_timed_out = first.run_timed_out;
    // Keep the reason attached to the LATEST scheduler attempt for each node. A
    // retry can replace an earlier fail-fast skip with a pre-flight refusal (or a
    // real outcome), so accumulating every historical reason would misreport the
    // terminal state. All reasons remain non-green; this only names them truthfully.
    let mut latest_not_launched = BTreeSet::new();
    let mut latest_refused = BTreeSet::new();
    let first_planned: Vec<String> = cfg.steps.iter().map(|step| step.tag()).collect();
    update_not_run_explanations(
        &first_planned,
        first.outcomes.len(),
        first.skipped.len(),
        &first.not_launched,
        first.intentional_skips.len(),
        &mut latest_not_launched,
        &mut latest_refused,
    );
    let mut order: Vec<String> = first.outcomes.iter().map(|o| o.tag.clone()).collect();
    let mut by_tag: BTreeMap<String, StepOutcome> =
        first.outcomes.iter().map(|o| (o.tag.clone(), o.clone())).collect();
    let mut skipped = first.skipped.clone();
    let mut env_retries = 0usize;
    // Attempt 1 for everything the first pass reported, recorded BEFORE any
    // retry can replace it in `by_tag`. This is the row that used to vanish.
    let mut attempts: Vec<NodeAttempt> = order
        .iter()
        .filter_map(|tag| by_tag.get(tag))
        .map(|o| reported_attempt(o, 1))
        .collect();
    // A planned node that is neither reported nor dependency-skipped produced no
    // completion payload at all. Today that node is silently swept into the retry
    // set; recorded here it stays separable from a node that ran and failed,
    // which is the distinction a flake-rate measurement cannot make afterwards.
    let first_unreported = unreported_non_intentional_steps(cfg, &by_tag, &skipped);
    let mut latest_unreported: BTreeSet<String> = first_unreported.iter().cloned().collect();
    attempts.extend(first_unreported.into_iter().map(|tag| unreported_attempt(tag, 1)));

    while env_retries < max {
        let failed: Vec<&StepOutcome> = by_tag
            .values()
            .filter(|o| outcome_is_failure(o) && !latest_unreported.contains(o.tag.as_str()))
            .collect();
        if failed.is_empty() {
            break;
        }
        // Only the ENVIRONMENTAL ground reads the durable log: it classifies the
        // failing node's own `----- detail -----` region. The other two grounds
        // read typed fields, so an unreadable log must not suppress them — a
        // wall-budget kill is a wall-budget kill whether or not the tee has
        // flushed. Before this split, an empty log broke out of the loop and
        // disabled every ground at once.
        let round_log = read_log_since_settled(log_path, round_log_start);
        if round_log.as_deref().is_none_or(str::is_empty) {
            eprintln!(
                "validate: WARNING: this scheduler invocation produced no readable new log bytes, \
                 so {} failed node(s) cannot be classified as environmental. Earlier attempts' \
                 detail is never reused. The typed grounds (bound-kill, measured instability) \
                 still apply; an unclassifiable red stays RED.",
                failed.len()
            );
        }
        // THREE grounds for retrying one failed node, each naming itself in the
        // record so a reader never has to guess which fired:
        //
        //   1. the failing node's OWN `----- detail -----` region classifies as
        //      an environmental block — a host condition, unchanged behaviour;
        //   2. the node was killed by its wall or CPU budget. A bound-kill is a
        //      RESOURCE story, which is exactly how the parent's existing
        //      taxonomy already reads it: `ci-hub/validate/retry_class.py` maps
        //      `Completion::KILLED_BY_BOUND` to `RetryClass::TRANSIENT`, "a
        //      retry may well answer". The measured instance is
        //      `scorecard.compatibility` at SHA 485a0ad4, which timed out at
        //      120.45s in 1 of 6 runs of that identical commit while passing in
        //      44.63-45.62s in the other five; its wrapper burned 0.050 CPU
        //      seconds over 93.813 wall seconds because a concurrently started
        //      `test.detcore_parallel` held the shared Cargo build directory.
        //      Nothing about the tree changed between those runs;
        //   3. the node is named in the measured-instability registry, whose
        //      membership rule is a recorded pass/fail sample with provenance.
        //
        // WHAT IS DELIBERATELY *NOT* A GROUND — the flakiness investigation's
        // firmest result, and the one that costs the box the most if ignored. A
        // verdict that CANNOT be published does not become publishable by being
        // re-run. At SHA 0f1f6cd0 eight DBT identities reported `no_result` 5
        // times out of 5 even though both executions completed and printed
        // deterministic success, because DBT never publishes a terminal verify
        // report and the invocation-bound pending one is left standing; and
        // `dbt-unsupported-syscall/ptrace` was pre-comparison `no_result` 5/5.
        // Those are 100% reproducible, so a retry buys two identical answers
        // at twice the cost. Accordingly a node that merely reported
        // nothing is NOT retried on that ground alone: it is recorded (above)
        // and it rides along in the retry set only when one of the three
        // grounds fires for something else.
        let mut environmental = BTreeMap::new();
        if let Some(log) = round_log.as_deref() {
            for outcome in &failed {
                if let Some(detail) = validate_runtime::extract_node_detail(log, &outcome.tag) {
                    let class = validate_runtime::environmental_block_class(&detail);
                    stamp_attempt_detail(&mut attempts, &outcome.tag, class);
                    if let Some(class) = class {
                        environmental.insert(outcome.tag.clone(), class.to_string());
                    }
                }
            }
        }
        let blocked: Vec<(String, String)> = failed
            .iter()
            .filter_map(|o| {
                // ⚠️ THE BUDGET IS PER CELL. Owner ruling 2026-08-26. This cell has
                // had `attempts_so_far` attempts; it gets MAX_ATTEMPTS_PER_CELL and
                // no more, and what any OTHER cell spent does not touch it.
                //
                // The gate is here, ahead of every ground, so it binds uniformly:
                // an environmental block cannot buy a third attempt any more than
                // the blanket arm can.
                if !retry_attempt_available(&attempts, &o.tag) {
                    return None;
                }
                environmental
                    .get(&o.tag)
                    .cloned()
                    .or_else(|| {
                        outcome_hit_its_budget(o)
                            .then(|| format!("bound-kill under contention: {}", o.reason.trim()))
                    })
                    .or_else(|| validate_runtime::measured_unstable_class(unstable, &o.tag))
                    // ALWAYS-ELIGIBLE FALLBACK (owner directive 2026-08-26): every
                    // cell is eligible for retry, not just one that earned it.
                    //
                    // ⚠️ THE OPT-IN GROUND HAD NEVER FIRED. Measured across all 106
                    // recorded runs carrying `retried_nodes`: 12 had a retry and all
                    // 12 were granted environmentally. `measured_unstable_class` --
                    // the registry ground directly above -- has granted zero, and its
                    // registry holds one cell measured 22 days ago. A cell earned a
                    // retry by having already been retried.
                    //
                    // The three grounds above are KEPT AND TRIED FIRST, deliberately:
                    // each names a specific cause, and that name is what reaches the
                    // ledger as `retry_class`. Falling straight to the blanket class
                    // would erase the distinction between "BPF-jailed", "bound-kill
                    // under contention" and "no idea", which is the signal a
                    // multi-week flakiness timeline is built from.
                    //
                    // The two SAFETY refusals still bound this and are not bypassed:
                    // a failure whose prerequisite did not complete is dropped by
                    // `retry_steps_with_satisfied_prerequisites` below, and the whole
                    // loop is bounded by `max` rounds.
                    .or_else(|| Some(format!("always-eligible: {}", o.reason.trim())))
                    .map(|class| (o.tag.clone(), class))
            })
            .collect();
        if blocked.is_empty() {
            break;
        }
        // The retry set: the blocked nodes, plus everything that never ran (or was
        // aborted) because of them. The scheduler's fail-fast result reports only
        // dependency-skipped nodes; an independent runnable node can otherwise be
        // absent from both outcomes and skipped.
        // A retry-round miss is hidden by cumulative `by_tag`, which still holds
        // the prior attempt. The helper carries the explicit latest-unknown set,
        // along with skipped and aborted peers, then applies the per-cell cap to
        // every candidate before any can enter the retry DAG.
        let keep = retry_candidate_tags(
            cfg,
            &blocked,
            &skipped,
            &by_tag,
            &latest_unreported,
            &attempts,
        );
        let steps = retry_steps_with_satisfied_prerequisites(cfg, &by_tag, keep);
        let retry_tags: BTreeSet<String> = steps.iter().map(|step| step.tag()).collect();
        if !blocked.iter().any(|(tag, _)| retry_tags.contains(tag)) {
            eprintln!(
                "validate: retry-eligible failure has no safe retry because a prerequisite did \
                 not complete successfully and is not part of the retry; NOT retrying."
            );
            break;
        }
        // Retries draw down the same clock. Giving every retry a fresh budget
        // would turn a bounded invocation back into an unbounded one.
        if remaining_budget_s(deadline) == Some(0) {
            eprintln!(
                "validate: whole-run budget exhausted; NOT starting retry round {}.",
                env_retries + 1
            );
            run_timed_out = true;
            break;
        }
        env_retries += 1;
        for (tag, class) in blocked.iter().filter(|(tag, _)| retry_tags.contains(tag)) {
            // This says WHY the retry scheduler accepted the prior failure. The
            // later attempt's typed execution state independently decides
            // whether that scheduled retry actually confirmed/refuted anything.
            if let Some(previous) = latest_reported_failure_mut(&mut attempts, tag) {
                previous.retry_class = Some(class.clone());
            }
            println!("{}", retry_notice(tag, class, next_attempt_ordinal(&attempts, tag)));
        }
        let mut retry_cfg = cfg.clone();
        retry_cfg.description = format!("{} — retry round {env_retries}", cfg.description);
        retry_cfg.steps = steps;
        for step in &mut retry_cfg.steps {
            let attempt = next_attempt_ordinal(&attempts, &step.tag());
            set_manifest_attempt(step, attempt);
        }
        // Everything before this byte belongs to an earlier scheduler
        // invocation. The retry may emit no detail at all; that must stay
        // unknown rather than inheriting a stale banner through whole-log rfind.
        let retry_log_start = settled_log_len(log_path);
        let again = run_dag_boxed_deadline(
            &retry_cfg,
            jobs,
            keep_going,
            verbosity,
            cgroups.clone(),
            None,
            Some(scheduler_cpu_budget()),
            remaining_budget_s(deadline),
        );
        if record_step_profiles {
            forward_step_profiles(&again, jobs);
        }
        run_timed_out = run_timed_out || again.run_timed_out;
        let retry_planned: Vec<String> = retry_cfg.steps.iter().map(|step| step.tag()).collect();
        update_not_run_explanations(
            &retry_planned,
            again.outcomes.len(),
            again.skipped.len(),
            &again.not_launched,
            again.intentional_skips.len(),
            &mut latest_not_launched,
            &mut latest_refused,
        );
        // Compute absent work from THIS retry result before cumulative `by_tag`
        // can make an old outcome look current. `RunResult::not_launched` carries
        // the same fact, but recomputing from the two round-local collections
        // keeps this consumer pinned to exactly what it merges below.
        let round_by_tag: BTreeMap<String, StepOutcome> = again
            .outcomes
            .iter()
            .map(|outcome| (outcome.tag.clone(), outcome.clone()))
            .collect();
        let round_unreported =
            unreported_non_intentional_steps(&retry_cfg, &round_by_tag, &again.skipped);
        for step in &retry_cfg.steps {
            latest_unreported.remove(&step.tag());
        }
        latest_unreported.extend(round_unreported.iter().cloned());
        for o in &again.outcomes {
            if !by_tag.contains_key(&o.tag) {
                order.push(o.tag.clone());
            }
            // The map holds the TERMINAL verdict and is still overwritten, which
            // is correct: the last attempt is what the node finally did. The
            // superseded attempt is not destroyed with it — it was appended to
            // `attempts` before this round started, and this round appends its
            // own row below.
            by_tag.insert(o.tag.clone(), o.clone());
            let ordinal = next_attempt_ordinal(&attempts, &o.tag);
            attempts.push(reported_attempt(o, ordinal));
        }
        for tag in round_unreported {
            let ordinal = next_attempt_ordinal(&attempts, &tag);
            attempts.push(unreported_attempt(tag, ordinal));
        }
        skipped = again.skipped.clone();
        round_log_start = retry_log_start;
    }

    // Classify the terminal failed attempt too. It may have no retry because the
    // budget was zero/exhausted or because no safe retry DAG existed; the attempt
    // ledger must call that UNCONFIRMED rather than silently dropping it. Preserve
    // the no-result distinction: exit 75 is not a failure and is never given an
    // environmental verdict merely because its scheduler safety bit is false.
    if by_tag
        .values()
        .any(|o| outcome_is_failure(o) && !latest_unreported.contains(o.tag.as_str()))
    {
        if let Some(log) = read_log_since_settled(log_path, round_log_start) {
            for outcome in by_tag
                .values()
                .filter(|o| outcome_is_failure(o) && !latest_unreported.contains(o.tag.as_str()))
            {
                if let Some(detail) = validate_runtime::extract_node_detail(&log, &outcome.tag) {
                    let class = validate_runtime::environmental_block_class(&detail);
                    stamp_attempt_detail(&mut attempts, &outcome.tag, class);
                    // Report exactly what was observed, without converting an
                    // environmental signature into an excuse for the terminal
                    // product failure. The attempt verdict remains RED.
                    if env_retries == max && class.is_some() {
                        if let Some(line) =
                            terminal_environmental_observation(&attempts, &outcome.tag)
                        {
                            println!("{line}");
                        }
                    }
                }
            }
        }
    }

    let outcomes: Vec<StepOutcome> =
        order.iter().filter_map(|t| by_tag.get(t).cloned()).collect();
    let mut unreported: BTreeSet<String> =
        unreported_non_intentional_steps(cfg, &by_tag, &skipped).into_iter().collect();
    unreported.extend(latest_unreported);
    let unreported: Vec<String> = unreported.into_iter().collect();
    // Three terminal explanations, all still non-green. A retry replaces an earlier
    // explanation for the same tag, so this reports the latest attempt rather than
    // whichever historical set happened to retain it.
    let (refused_before_launching, remaining) =
        partition_unreported(&unreported, &latest_refused);
    let (scheduler_not_launched, unaccounted) =
        partition_unreported(&remaining, &latest_not_launched);
    if !scheduler_not_launched.is_empty() {
        eprintln!("{}", scheduler_not_launched_message(&scheduler_not_launched));
    }
    if !refused_before_launching.is_empty() {
        eprintln!(
            "validate: {} planned node(s) DID NOT RUN because the scheduler REFUSED their latest \
             attempt before launching anything; the refusal above states the reason. The lane is \
             incomplete and cannot be green: {}.",
            refused_before_launching.len(),
            refused_before_launching.join(", ")
        );
    }
    if !unaccounted.is_empty() {
        eprintln!(
            "validate: ERROR: scheduler returned without an outcome, a dependency-skip, or a \
             scheduler not-launched result for {} non-intentional planned node(s): {}. These are UNACCOUNTED \
             FOR -- unlike a scheduler not-launched result or a pre-flight refusal, nothing explains why \
             they did not run. The lane is incomplete and cannot be green.",
            unaccounted.len(),
            unaccounted.join(", ")
        );
    }
    // Raw failure policy may still treat an aborted peer as neutral, but execution
    // completeness may not: dependency-skipped, aborted, timed-out, or unreported
    // required nodes did not finish and therefore cannot support a green run.
    let complete = !run_timed_out
        && unreported.is_empty()
        && skipped.is_empty()
        && outcomes
            .iter()
            .all(|outcome| outcome_execution(outcome) == AttemptExecution::Completed);
    let ok = outcomes.iter().all(|o| o.ok || o.aborted);
    LaneResult { outcomes, skipped, attempts, complete, ok, env_retries, run_timed_out }
}

/// Print every node that took more than one attempt, and every attempt that
/// reported nothing.
///
/// This is the human-facing half of the same fact the ledger now carries. It is
/// printed even when the run is GREEN, which is the whole point: a green that
/// needed a second attempt is the case that used to leave no trace anywhere
/// except a lane-level counter that names no node.
fn retry_attempt_line(
    attempts: &[NodeAttempt],
    row: &NodeAttempt,
    total_attempts: usize,
) -> String {
    let verdict = attempt_result(row).unwrap_or("unknown step result");
    let because = row
        .retry_class
        .as_deref()
        .map(|class| format!(" — retried because: {class}"))
        .unwrap_or_default();
    let detail = if row.reason.is_empty() {
        String::new()
    } else {
        format!(" [{}]", row.reason.trim())
    };
    let environmental = match environmental_assessment(attempts, row) {
        Some((validate_runtime::EnvBlockVerdict::Confirmed, _)) => format!(
            " — ENVIRONMENTAL CONFIRMED ({}): an actual re-execution passed",
            row.environmental_class.as_deref().unwrap_or("unknown")
        ),
        Some((validate_runtime::EnvBlockVerdict::Refuted, Some(shape))) => {
            let attribution = match shape {
                validate_runtime::RefutedShape::BannerGone => {
                    "the environmental banner was gone and the node still failed"
                }
                validate_runtime::RefutedShape::Persistent => {
                    "the same environmental signature persisted"
                }
                validate_runtime::RefutedShape::SignatureChanged => {
                    "the environmental signature changed"
                }
            };
            format!(
                " — ENVIRONMENTAL REFUTED ({}; {}): {attribution}",
                row.environmental_class.as_deref().unwrap_or("unknown"),
                shape.as_str()
            )
        }
        Some((validate_runtime::EnvBlockVerdict::Unconfirmed, _)) => format!(
            " — ENVIRONMENTAL UNCONFIRMED ({}): no actual re-execution completed; this remains an \
             unsettled RED",
            row.environmental_class.as_deref().unwrap_or("unknown")
        ),
        Some((validate_runtime::EnvBlockVerdict::Refuted, None)) => format!(
            " — ENVIRONMENTAL REFUTED ({}; shape unknown): an actual re-execution failed, but \
             emitted no attempt-local detail region for banner attribution",
            row.environmental_class.as_deref().unwrap_or("unknown")
        ),
        None => String::new(),
    };
    format!(
        "  {} attempt {}/{}: {verdict} ({:.1}s){detail}{because}{environmental}",
        row.tag, row.attempt, total_attempts, row.duration_s
    )
}

fn terminal_environmental_observation(attempts: &[NodeAttempt], tag: &str) -> Option<String> {
    let attempt = attempts
        .iter()
        .rev()
        .find(|attempt| attempt.tag == tag && attempt.environmental_class.is_some())?;
    let class = attempt.environmental_class.as_deref()?;
    let verdict = environmental_assessment(attempts, attempt)?.0;
    let verdict = match verdict {
        validate_runtime::EnvBlockVerdict::Confirmed => "CONFIRMED",
        validate_runtime::EnvBlockVerdict::Refuted => "REFUTED",
        validate_runtime::EnvBlockVerdict::Unconfirmed => "UNCONFIRMED",
    };
    Some(format!(
        "🧱 {tag}: observed environmental signature {class} on attempt {}; terminal hypothesis \
         {verdict}; node remains RED.",
        attempt.attempt
    ))
}

fn print_retry_ledger(attempts: &[NodeAttempt]) {
    let mut retried: BTreeMap<&str, Vec<&NodeAttempt>> = BTreeMap::new();
    for attempt in attempts {
        retried.entry(attempt.tag.as_str()).or_default().push(attempt);
    }
    retried.retain(|_, rows| {
        rows.len() > 1
            || rows
                .iter()
                .any(|row| !row.reported || row.environmental_class.is_some())
    });
    if retried.is_empty() {
        return;
    }
    println!(
        "\nRetry and environmental verdict ledger ({} node(s)): every attempt is listed, \
         including the ones a later attempt superseded.",
        retried.len()
    );
    for rows in retried.values() {
        for row in rows {
            println!("{}", retry_attempt_line(attempts, row, rows.len()));
        }
    }
}

/// The exact renderings `step_failure_reason` produces for the two budget
/// breaches, as prefixes. Everything after them is a number, so a prefix is an
/// exact identification of the arm rather than a search for a word.
const WALL_BUDGET_REASON_PREFIX: &str = "TIMEOUT >";
const CPU_BUDGET_REASON_PREFIX: &str = "CPU-TIMEOUT >";

/// Was this node killed by its wall or CPU budget?
///
/// WHY THIS IS NOT A SUBSTRING TEST, which is what it used to be. `reason` is a
/// closed set produced by `dagrun::model::step_failure_reason`, and
/// one member of that set reads:
///
///   `received SIGSEGV with no validate timeout, pids guard, or child-cgroup OOM recorded`
///
/// A `contains("timeout")` test matches that string — INSIDE THE CLAUSE SAYING
/// THERE WAS NO TIMEOUT. Every signal-killed node therefore read as a budget
/// breach, so a segfault, an abort and a SIGKILL were all retry-eligible and
/// would each be re-run to `max` for the same deterministic answer. That is
/// precisely the waste the eligibility rule exists to prevent.
///
/// A SUBSTRING TEST AGAINST A HUMAN-READABLE MESSAGE IS NOT A PREDICATE. The
/// message is written for a reader and can carry the word in a negating
/// context. The typed `timed_out`/`cpu_timed_out` inputs are the real facts, but
/// `StepOutcome` does not carry them — `StepOutcome::failed` takes them and
/// keeps only the rendered string. So this matches the two arms EXACTLY, and
/// `budget_reason_bracket` below pins that match by calling the real producer
/// with the typed inputs rather than by restating its wording here.
fn outcome_hit_its_budget(outcome: &StepOutcome) -> bool {
    reason_is_budget_breach(&outcome.reason)
}

/// The same rule over a bare reason string, so the bracket can exercise it
/// against producer output without building a whole `StepOutcome`.
fn reason_is_budget_breach(reason: &str) -> bool {
    reason.starts_with(WALL_BUDGET_REASON_PREFIX) || reason.starts_with(CPU_BUDGET_REASON_PREFIX)
}

/// Pin the budget-breach predicate to the PRODUCER, not to a copy of its text.
///
/// Every case below is built by calling `step_failure_reason` itself with the
/// typed inputs, then asserting the predicate agrees with `timed_out ||
/// cpu_timed_out`. Nothing here hardcodes a message, so a reworded reason does
/// not silently drift past the predicate — it fails here instead.
fn budget_reason_bracket() -> Result<String, String> {
    use dagrun::model::step_failure_reason;
    // (label, returncode, oomed, timed_out, pids_tripped, detail_failure, cpu_timed_out)
    let cases: &[(&str, Option<i64>, bool, bool, bool, bool, bool)] = &[
        ("wall budget", None, false, true, false, false, false),
        ("cpu budget", None, false, false, false, false, true),
        ("oom kill", Some(-9), true, false, false, false, false),
        ("pids guard", None, false, false, true, false, false),
        ("detail capture", None, false, false, false, true, false),
        ("SIGSEGV", Some(-11), false, false, false, false, false),
        ("SIGABRT", Some(-6), false, false, false, false, false),
        ("SIGKILL", Some(-9), false, false, false, false, false),
        ("ordinary exit", Some(1), false, false, false, false, false),
        ("no exit collected", None, false, false, false, false, false),
    ];
    let mut eligible = Vec::new();
    for (label, rc, oomed, timed_out, pids, detail, cpu_timed_out) in cases {
        let detail_rows: Vec<String> = if *detail {
            vec!["fixture detail write failed".to_string()]
        } else {
            Vec::new()
        };
        let reason = step_failure_reason(
            *rc,
            *oomed,
            if *oomed { 1 } else { 0 },
            *timed_out,
            600,
            *pids,
            pids.then_some("fixture pids guard"),
            &detail_rows,
            *cpu_timed_out,
            300,
            300,
            1.0,
            "",
        );
        let want = *timed_out || *cpu_timed_out;
        let got = reason_is_budget_breach(&reason);
        if got != want {
            return Err(format!(
                "budget reason: {label} rendered {reason:?}; retry-eligible={got} but the typed                  inputs say timed_out={timed_out} cpu_timed_out={cpu_timed_out}. A reason that                  merely MENTIONS a timeout is not a timeout."
            ));
        }
        if got {
            eligible.push(*label);
        }
    }
    // The negative direction, stated as its own assertion rather than left
    // implicit in the loop: the signal arm is the one that used to misclassify,
    // and it must stay ineligible even though its text contains "timeout".
    let segv = step_failure_reason(
        Some(-11),
        false,
        0,
        false,
        600,
        false,
        None,
        &[],
        false,
        300,
        300,
        1.0,
        "",
    );
    if !segv.contains("timeout") {
        return Err(format!(
            "budget reason: the signal arm no longer contains the word 'timeout' ({segv:?}); this              bracket exists because it DOES, so re-check the producer before relaxing it"
        ));
    }
    if reason_is_budget_breach(&segv) {
        return Err(format!("budget reason: signal-killed reason {segv:?} is retry-eligible"));
    }
    Ok(format!(
        "budget reason: 10 producer-rendered reason(s) classified; retry-eligible = {eligible:?};          the SIGSEGV arm contains the word \"timeout\" and is correctly NOT eligible"
    ))
}

/// Nodes the runner reported as killed by their wall or CPU budget.

/// Nodes the runner reported as killed by their wall or CPU budget.
fn timed_out_nodes(outcomes: &[StepOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| outcome_hit_its_budget(o))
        .map(|o| o.tag.clone())
        .collect()
}

// --------------------------------------------------------------------------- ledger

struct LedgerCtx {
    started_at: String,
    host: String,
    toolchain: String,
    slot: String,
    cwd: String,
    profile: String,
    selection_mode: String,
    cache_state: String,
    commit: String,
    tree: String,
    git_depth: u64,
    git_ahead: i64,
    git_behind: i64,
    commit_anchored: bool,
    tree_dirty: bool,
    dag_jobs: i64,
    /// Only the canonical validate-lock owner ancestry establishes admission.
    admission: Option<&'static str>,
    /// Exact base identities from the parent's single receipt finalizer. Each
    /// stays null when that proof cannot be computed.
    base_sha: serde_json::Value,
    base_tree: serde_json::Value,
    reverie_base_sha: serde_json::Value,
    reverie_base_tree: serde_json::Value,
    /// Peak number of OTHER top-level validates that were provably live AND
    /// burning CPU beside this run. `None` means UNKNOWN (never 0-by-default): a
    /// bare run with no registry is not proven exclusive.
    concurrent_validates: Option<i64>,
    /// How that number was established, so a reader never has to guess whether a
    /// `0` is "measured exclusive" or "nobody looked".
    concurrency_proof: Option<&'static str>,
    /// `INT` / `TERM` / `HUP` when an operator stopped the run.
    interruption: Option<String>,
    /// Whole-run CPU seconds (self + reaped children), the same pair printed in
    /// the summary line.
    cpu_user: f64,
    cpu_sys: f64,
    /// Retry ROUNDS spent on environmental blocks; `0` for a clean first pass.
    env_block_retries: i64,
    /// Whether THIS run observed the `pre.reverie_pin` gate pass. Recorded on the
    /// row itself so a reader never has to infer from a bare `pass` that the
    /// archival pin was proved current; the receipt verifier keys on it.
    reverie_pin_current: bool,
    /// Libtest counts aggregated from typed step outcomes; `None` is UNKNOWN.
    executed_tests: Option<i64>,
    filtered_tests: Option<i64>,
}

struct ReceiptEvidence {
    coverage: serde_json::Value,
    base_sha: serde_json::Value,
    base_tree: serde_json::Value,
    reverie_base_sha: serde_json::Value,
    reverie_base_tree: serde_json::Value,
}

impl Default for ReceiptEvidence {
    fn default() -> Self {
        Self {
            coverage: serde_json::Value::Null,
            base_sha: serde_json::Value::Null,
            base_tree: serde_json::Value::Null,
            reverie_base_sha: serde_json::Value::Null,
            reverie_base_tree: serde_json::Value::Null,
        }
    }
}

/// Ask the parent's single receipt finalizer for coverage and base identities.
/// Any missing helper, failed command, or malformed output stays explicit null;
/// the schema-5 consumer then refuses qualification.
fn receipt_evidence(
    parent: Option<&Path>,
    root: &Path,
    log: &Path,
    commit: &str,
) -> ReceiptEvidence {
    let Some(parent) = parent else { return ReceiptEvidence::default() };
    let helper = parent.join("ci-hub/validate/finalize_receipt.py");
    if !helper.is_file() || log.as_os_str().is_empty() || commit.is_empty() {
        return ReceiptEvidence::default();
    }
    let Ok(out) = Command::new("python3")
        .arg(&helper)
        .arg("--log")
        .arg(log)
        .arg("--sha")
        .arg(commit)
        .arg("--hermit-checkout")
        .arg(root)
        .arg("--emit-only")
        .output()
    else {
        return ReceiptEvidence::default();
    };
    if !out.status.success() {
        return ReceiptEvidence::default();
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        return ReceiptEvidence::default();
    };
    let coverage = value.get("coverage").cloned().unwrap_or(serde_json::Value::Null);
    let (_, coverage) = ledger_schema_and_coverage(coverage);
    let field = |name: &str| value.get(name).cloned().unwrap_or(serde_json::Value::Null);
    ReceiptEvidence {
        coverage,
        base_sha: field("base_sha"),
        base_tree: field("base_tree"),
        reverie_base_sha: field("reverie_base_sha"),
        reverie_base_tree: field("reverie_base_tree"),
    }
}

/// Ask the canonical parent lock authority whether this exact run is admitted.
/// Production never trusts caller-supplied owner PIDs or sidecar paths. The
/// stop-test JSON seam is confined to an intrinsically non-qualifying fixture.
fn canonical_validate_lock_admission(
    parent: Option<&Path>,
    commit: &str,
    host: &str,
) -> Result<(), String> {
    let status = if env_flag("HERMIT_VALIDATE_STOP_TEST_MODE", "1") {
        let Ok(fixture) = std::env::var("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON") else {
            return Err("stop-test mode is on but no planted authority status was supplied".into());
        };
        fixture.into_bytes()
    } else {
        let Some(parent) = parent else {
            return Err("no dev-hermit parent was detected".into());
        };
        let ci_hub = parent.join("ci-hub/ci-hub");
        if !ci_hub.is_file() {
            return Err(format!(
                "the canonical launcher is missing at {}",
                ci_hub.display()
            ));
        }
        let Ok(output) = Command::new(&ci_hub)
            .args(["validate-lock", "authority-status", "--json"])
            .output()
        else {
            return Err(format!("could not execute {}", ci_hub.display()));
        };
        if !output.status.success() {
            return Err(format!(
                "`ci-hub validate-lock authority-status --json` exited {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "by signal".into(), |c| c.to_string())
            ));
        }
        output.stdout
    };
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_string());
    canonical_validate_lock_status_reason(
        &status,
        commit,
        host,
        boot_id.as_deref(),
        &mut validate_runtime::identity_in_ancestry,
    )
}

/// Parse and bind one canonical authority response. The injected identity
/// predicate is the real `/proc` ancestry check in production and a planted,
/// inert identity in `--self-test`; no caller-supplied environment marker can
/// bypass these exact commit, host, boot, PID, and start-time checks.
fn canonical_validate_lock_status_admits(
    status: &[u8],
    commit: &str,
    host: &str,
    boot_id: Option<&str>,
    identity_in_ancestry: &mut dyn FnMut(i32, u64) -> bool,
) -> bool {
    canonical_validate_lock_status_reason(status, commit, host, boot_id, identity_in_ancestry)
        .is_ok()
}

/// The single implementation of the admission decision, reporting WHICH
/// conjunct failed.
///
/// ⚠️ SIXTEEN DISTINCT WAYS TO BE REFUSED USED TO COLLAPSE INTO ONE `false`,
/// and the front door then printed one sentence naming all three of exact
/// commit, exact host and live owner ancestry without saying which had failed
/// or what the values were. That is undiagnosable from the outside: the owner
/// hit it from his own checkout and could not tell that his HEAD simply was not
/// the commit his lock was taken for. Measured 2026-08-25 -- checkout HEAD
/// `b120fe5d7653`, lock target `1d558d48b438`.
///
/// The refusal was CORRECT. Validating `b120fe5d` while the receipt says
/// `1d558d48` would record a result against a commit it was not measured on,
/// which is the exact defect the target binding exists to prevent. So nothing
/// here is relaxed and no caller is exempted -- `..._admits` is derived from
/// this function, so the decision cannot drift from the explanation. Only the
/// diagnosis is added.
/// ⚠️ `identity_in_ancestry` IS A TRAIT OBJECT, NOT `impl FnMut`, AND IT HAS TO
/// STAY ONE. This function RECURSES over the `authorities` array and passes
/// `&mut identity_in_ancestry` down, so with a generic parameter each level
/// instantiates one more reference layer -- `F`, `&mut F`, `&mut &mut F`, ...
/// -- and monomorphization never terminates. Measured 2026-08-26: as
/// `impl FnMut` this failed to compile with "reached the recursion limit while
/// instantiating `canonical_validate_lock_status_reason::<&mut &mut &mut &mut
/// &mut ...>`", which took the whole validate gate off the air on `main` --
/// validate could not build, so nothing could be validated at all.
///
/// ⚠️ IT DID NOT SHOW UP IN `--self-test`. `rust-script --test` builds a
/// different crate configuration and passed 16/16 while the release build the
/// gate actually runs could not compile. A green self-test is therefore NOT
/// evidence that this file builds; only a release build is.
fn canonical_validate_lock_status_reason(
    status: &[u8],
    commit: &str,
    host: &str,
    boot_id: Option<&str>,
    identity_in_ancestry: &mut dyn FnMut(i32, u64) -> bool,
) -> Result<(), String> {
    fn object_string<'a>(
        object: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<&'a str> {
        object.get(key).and_then(serde_json::Value::as_str)
    }
    fn shown(value: Option<&str>) -> String {
        value.map_or_else(|| "<absent>".to_string(), |text| format!("{text:?}"))
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(status) else {
        return Err("the authority response is not valid JSON".into());
    };
    if let Some(authorities) = value
        .get("authorities")
        .and_then(serde_json::Value::as_array)
    {
        if authorities.is_empty() {
            return Err("the authority response contains no validation slots".into());
        }
        let mut reasons = Vec::new();
        for authority in authorities {
            let encoded = serde_json::to_vec(authority)
                .map_err(|error| format!("cannot encode validation slot: {error}"))?;
            match canonical_validate_lock_status_reason(
                &encoded,
                commit,
                host,
                boot_id,
                &mut *identity_in_ancestry,
            ) {
                Ok(()) => return Ok(()),
                Err(reason) => reasons.push(reason),
            }
        }
        return Err(format!(
            "none of the canonical validation slots belongs to this run: {}",
            reasons.join("; ")
        ));
    }
    let Some(holder) = value.get("holder").and_then(serde_json::Value::as_object) else {
        return Err(format!(
            "no lock is held: the authority reports state {} (reason {}), so there \
             is no holder to bind to",
            shown(value.get("state").and_then(serde_json::Value::as_str)),
            shown(value.get("reason_code").and_then(serde_json::Value::as_str)),
        ));
    };
    let Some(owner) = value.get("owner").and_then(serde_json::Value::as_object) else {
        return Err("the authority reports a holder but no owner record".into());
    };
    if value.get("schema_version").and_then(serde_json::Value::as_i64) != Some(1)
    {
        return Err("the authority response is not schema_version 1".into());
    }
    if value.get("admissible").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("the authority itself reports admissible=false".into());
    }
    if value.get("state").and_then(serde_json::Value::as_str) != Some("held") {
        return Err(format!(
            "the lock state is {}, not \"held\"",
            shown(value.get("state").and_then(serde_json::Value::as_str))
        ));
    }
    if !value
        .get("reason_code")
        .is_some_and(serde_json::Value::is_null)
    {
        return Err(format!(
            "the authority attached reason_code {}",
            shown(value.get("reason_code").and_then(serde_json::Value::as_str))
        ));
    }
    if value
        .get("canonical_anchor_held")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err("the canonical anchor is not held".into());
    }
    if !matches!(
        value
            .get("cleanup_state")
            .and_then(serde_json::Value::as_str),
        Some("none" | "active-bound")
    ) {
        return Err(format!(
            "cleanup_state is {}, which is neither \"none\" nor \"active-bound\"",
            shown(
                value
                    .get("cleanup_state")
                    .and_then(serde_json::Value::as_str)
            )
        ));
    }
    if object_string(holder, "kind") != Some("validate") {
        return Err(format!(
            "the held lock is kind {}, not \"validate\"",
            shown(object_string(holder, "kind"))
        ));
    }
    if object_string(holder, "target") != Some(commit) {
        return Err(format!(
            "COMMIT MISMATCH: this checkout is at {commit}, but the lock was taken \
             for target {}. Validating here would measure {commit} and record it \
             against the lock's target, so it is refused. Check out the lock's \
             target, or take a lock for this commit.",
            shown(object_string(holder, "target"))
        ));
    }
    if object_string(holder, "host") != Some(host) {
        return Err(format!(
            "HOST MISMATCH: this host is {host:?}, the lock holder's host is {}",
            shown(object_string(holder, "host"))
        ));
    }
    if object_string(owner, "host") != Some(host) {
        return Err(format!(
            "HOST MISMATCH: this host is {host:?}, the lock owner's host is {}",
            shown(object_string(owner, "host"))
        ));
    }
    if object_string(owner, "liveness") != Some("alive") {
        return Err(format!(
            "the lock owner's liveness is {}, not \"alive\"",
            shown(object_string(owner, "liveness"))
        ));
    }
    let Some(pid64) = owner.get("pid").and_then(serde_json::Value::as_i64) else {
        return Err("the lock owner record carries no integer pid".into());
    };
    let Some(start_ticks) = owner.get("start_ticks").and_then(serde_json::Value::as_u64) else {
        return Err("the lock owner record carries no start_ticks".into());
    };
    let Ok(pid) = i32::try_from(pid64) else {
        return Err(format!("the lock owner pid {pid64} is out of range"));
    };
    if pid <= 1 || start_ticks == 0 {
        return Err(format!(
            "the lock owner identity is degenerate (pid {pid}, start_ticks {start_ticks})"
        ));
    }
    if boot_id != object_string(owner, "boot_id") {
        return Err(format!(
            "BOOT MISMATCH: this boot is {}, the lock was taken under boot {}. The lock did not \
             survive a reboot.",
            shown(boot_id),
            shown(object_string(owner, "boot_id"))
        ));
    }
    if !identity_in_ancestry(pid, start_ticks) {
        return Err(format!(
            "ANCESTRY: this process is not a descendant of the lock owner (pid {pid}, start_ticks \
             {start_ticks}). A naked run is refused here even when a lock is held by someone else \
             -- enter through ci-hub so the run is a child of the lock owner."
        ));
    }
    Ok(())
}

/// Inert two-sided bracket for the front door and the canonical authority
/// parser. It proves the new guard neither accepts missing/mismatched authority
/// nor mistakes a generic superproject for dev-hermit. Nested payloads remain
/// subject to the same authority, so their caller-supplied marker cannot become
/// an admission bypass.
fn product_front_door_bracket() -> Result<(), String> {
    let policy_cases = [
        (true, true, false, false, true, "dev-hermit top-level product run"),
        (false, false, false, false, false, "standalone clone"),
        (true, false, false, false, false, "generic Hermit superproject"),
        (true, true, true, false, true, "nested focused payload"),
        (true, true, false, true, false, "show-plan"),
    ];
    for (parent, ci_hub_dir, nested, show_plan, expected, label) in policy_cases {
        let actual = product_front_door_applies(parent, ci_hub_dir, nested, show_plan);
        if actual != expected {
            return Err(format!(
                "product front door classified {label} as applies={actual}, expected {expected}"
            ));
        }
    }

    let commit = "0123456789abcdef0123456789abcdef01234567";
    // Synthetic like the commit and boot_id above it. A real machine name here
    // is inert today but is how a fixture turns into a host dependency, and
    // scripts/check-portable-paths.sh refuses literal hostnames in tracked
    // build/run files for exactly that reason.
    let host = "test-host-0";
    let boot_id = "11111111-2222-3333-4444-555555555555";
    let authority = serde_json::json!({
        "schema_version": 1,
        "admissible": true,
        "state": "held",
        "reason_code": null,
        "canonical_anchor_held": true,
        "cleanup_state": "active-bound",
        "holder": {"kind": "validate", "target": commit, "host": host},
        "owner": {
            "host": host,
            "liveness": "alive",
            "pid": 4242,
            "start_ticks": 987654,
            "boot_id": boot_id
        }
    });
    let encode = |value: &serde_json::Value| serde_json::to_vec(value).unwrap();
    if !canonical_validate_lock_status_admits(
        &encode(&authority),
        commit,
        host,
        Some(boot_id),
        &mut (|pid, ticks| pid == 4242 && ticks == 987654),
    ) {
        return Err("product front door refused exact canonical authority".into());
    }

    // Goalpost safety: every case below weakens or changes an identity claim.
    // Each must remain non-authorizing; improving diagnostics must never turn
    // one into an exemption.
    let mut weakened = Vec::new();
    let mut value = authority.clone();
    value["schema_version"] = serde_json::json!(2);
    weakened.push(("wrong schema", value));
    let mut value = authority.clone();
    value["admissible"] = serde_json::json!(false);
    weakened.push(("not admissible", value));
    let mut value = authority.clone();
    value["state"] = serde_json::json!("free");
    weakened.push(("lock not held", value));
    let mut value = authority.clone();
    value["reason_code"] = serde_json::json!("owner-not-ancestor");
    weakened.push(("non-null refusal reason", value));
    let mut value = authority.clone();
    value["canonical_anchor_held"] = serde_json::json!(false);
    weakened.push(("canonical anchor absent", value));
    let mut value = authority.clone();
    value["cleanup_state"] = serde_json::json!("stale");
    weakened.push(("invalid cleanup state", value));
    let mut value = authority.clone();
    value["holder"]["kind"] = serde_json::json!("other");
    weakened.push(("wrong holder kind", value));
    let mut value = authority.clone();
    value["holder"]["target"] = serde_json::json!("different-commit");
    weakened.push(("wrong commit", value));
    let mut value = authority.clone();
    value["holder"]["host"] = serde_json::json!("other-host");
    weakened.push(("wrong holder host", value));
    let mut value = authority.clone();
    value["owner"]["host"] = serde_json::json!("other-host");
    weakened.push(("wrong owner host", value));
    let mut value = authority.clone();
    value["owner"]["liveness"] = serde_json::json!("dead");
    weakened.push(("owner not alive", value));
    let mut value = authority.clone();
    value["owner"]["pid"] = serde_json::json!(1);
    weakened.push(("unsafe owner pid", value));
    let mut value = authority.clone();
    value["owner"]["start_ticks"] = serde_json::json!(0);
    weakened.push(("zero owner start ticks", value));
    let mut value = authority.clone();
    value["owner"]["boot_id"] = serde_json::json!("other-boot");
    weakened.push(("wrong boot", value));
    for (label, value) in weakened {
        if canonical_validate_lock_status_admits(
            &encode(&value),
            commit,
            host,
            Some(boot_id),
            &mut (|pid, ticks| pid == 4242 && ticks == 987654),
        ) {
            return Err(format!("product front door accepted weakened authority: {label}"));
        }
    }
    if canonical_validate_lock_status_admits(
        &encode(&authority),
        commit,
        host,
        Some(boot_id),
        &mut (|_pid, _ticks| false),
    ) {
        return Err("product front door accepted authority outside owner ancestry".into());
    }

    // These were historical, forgeable authorization inputs. The canonical
    // parser is deliberately pure with respect to the process environment; pin
    // that property against all legacy spellings still present in old tests.
    let legacy_env = [
        ("CI_HUB_VALIDATE_PRODUCER", "forged"),
        ("CI_HUB_VALIDATE_LOCK_OWNER_PID", "4242"),
        ("CI_HUB_VALIDATE_LOCK_OWNER_FILE", "/tmp/forged-owner"),
    ];
    let saved = legacy_env.map(|(name, _)| (name, std::env::var_os(name)));
    // SAFETY: this self-test is single-threaded at this point, and every value
    // is restored before returning from the bracket.
    for (name, value) in legacy_env {
        unsafe { std::env::set_var(name, value) };
    }
    let forged_env_admitted = canonical_validate_lock_status_admits(
        br#"{"schema_version":1,"admissible":false}"#,
        commit,
        host,
        Some(boot_id),
        &mut (|_pid, _ticks| true),
    );
    for (name, value) in saved {
        match value {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }
    if forged_env_admitted {
        return Err("product front door trusted forged legacy owner environment".into());
    }

    let parent = PathBuf::from("/srv/dev-hermit");
    let checkout = parent.join("worktrees/slot07/hermit");
    let refusal = product_front_door_refusal(
        &parent,
        &checkout,
        commit,
        "--strict-compat-only",
        true,
        false,
    )
    .ok_or_else(|| "product front door omitted the naked-run refusal".to_string())?;
    if !refusal.contains("unadmitted ledger row")
        || !refusal.contains(commit)
        || !refusal.contains("ci-hub/ci-hub validate-run")
    {
        return Err(format!("product front-door refusal lost remediation detail: {refusal}"));
    }
    if product_front_door_refusal(
        &parent,
        &checkout,
        commit,
        "--strict-compat-only",
        true,
        true,
    )
    .is_some()
    {
        return Err("product front door refused canonical admission".into());
    }

    let unavailable = product_front_door_refusal(
        &parent,
        &checkout,
        commit,
        "--strict-compat-only",
        false,
        false,
    )
    .ok_or_else(|| "product front door omitted the missing-launcher refusal".to_string())?;
    if !unavailable.contains("launcher is unavailable")
        || unavailable.contains("validate-run --checkout")
    {
        return Err(format!("missing-launcher refusal printed a false remedy: {unavailable}"));
    }

    println!(
        "  product front door: parent+ci-hub and nested payloads require authority; generic/\
         standalone/show-plan paths allowed; exact authority and diagnostics bracketed"
    );
    Ok(())
}

/// Drive this exact executable through the real `run()` entry path. The pure
/// bracket above pins policy details; this process bracket pins the wiring and
/// proves full, focused, and caller-marked nested work all stop before creating
/// validation state when canonical authority is missing or malformed.
fn product_front_door_process_bracket() -> Result<(), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("front-door process bracket: current executable: {error}"))?;
    let git_dir = sh("git", &["rev-parse", "--absolute-git-dir"])
        .ok_or_else(|| "front-door process bracket: cannot resolve git dir".to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("front-door process bracket: system clock: {error}"))?
        .as_nanos();
    let tmp = std::env::temp_dir().join(format!(
        "validate-front-door-process-{}-{nonce}",
        std::process::id()
    ));

    let result = (|| {
        let cases: [(&str, &[&str], bool, bool, &str); 3] = [
            (
                "top-level-missing-launcher",
                &[
                    "full",
                    SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                ],
                false,
                false,
                "launcher is unavailable",
            ),
            (
                "focused-invalid-authority",
                &[
                    "--strict-compat-only",
                    SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                ],
                false,
                true,
                "Run through the canonical launcher",
            ),
            (
                "nested-marker-invalid-authority",
                &[
                    "--strict-compat-only",
                    SKIP_INNER_DIRTY_WORKING_TREE_AND_REBASE_FRESHNESS_CHECKS_OPTION,
                ],
                true,
                true,
                "Run through the canonical launcher",
            ),
        ];

        for (label, args, nested, launcher_present, expected_remediation) in cases {
            let parent = tmp.join(label);
            let checkout = parent.join("hermit");
            let ci_hub_dir = parent.join("ci-hub");
            std::fs::create_dir_all(&checkout).map_err(|error| {
                format!(
                    "front-door process bracket: cannot create {}: {error}",
                    checkout.display()
                )
            })?;
            std::fs::create_dir_all(&ci_hub_dir).map_err(|error| {
                format!(
                    "front-door process bracket: cannot create {}: {error}",
                    ci_hub_dir.display()
                )
            })?;
            std::fs::write(
                parent.join(".gitmodules"),
                "[submodule \"hermit\"]\n\tpath = hermit\n\turl = self-test://unused\n",
            )
            .map_err(|error| {
                format!("front-door process bracket: cannot write .gitmodules: {error}")
            })?;
            if launcher_present {
                let launcher = ci_hub_dir.join("ci-hub");
                std::fs::write(
                    &launcher,
                    "#!/bin/sh\nprintf '%s\\n' \
                     '{\"schema_version\":1,\"admissible\":false}'\n",
                )
                .map_err(|error| {
                    format!(
                        "front-door process bracket: cannot write {}: {error}",
                        launcher.display()
                    )
                })?;
                std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| {
                        format!(
                            "front-door process bracket: cannot chmod {}: {error}",
                            launcher.display()
                        )
                    })?;
            }

            let mut command = Command::new(&executable);
            command
                .args(args)
                .current_dir(&checkout)
                .env("GIT_DIR", &git_dir)
                .env("GIT_WORK_TREE", &checkout)
                .env_remove("HERMIT_VALIDATE_STOP_TEST_MODE")
                .env_remove("VALIDATE_STOP_TEST_AUTHORITY_STATUS_JSON")
                .env_remove("HERMIT_VALIDATE_STOP_TEST_EXIT_EARLY")
                .env_remove(PARENT_ENV)
                .env_remove(validate_runtime::ACTIVE_ENV)
                .env_remove("CI_HUB_VALIDATE_LOCK_OWNER_PID")
                .env_remove("CI_HUB_VALIDATE_LOCK_OWNER_FILE");
            if nested {
                command
                    .env(validate_runtime::ACTIVE_ENV, std::process::id().to_string())
                    .env("CI_HUB_VALIDATE_LOCK_OWNER_PID", std::process::id().to_string())
                    .env(
                        "CI_HUB_VALIDATE_LOCK_OWNER_FILE",
                        parent.join("caller-forged-owner"),
                    );
            }
            let output = command.output().map_err(|error| {
                format!("front-door process bracket: cannot launch {label}: {error}")
            })?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let rendered = format!(
                "{}{}",
                stdout,
                String::from_utf8_lossy(&output.stderr)
            );
            if output.status.code() != Some(i32::from(COULD_NOT_RUN_EXIT_CODE))
                || !rendered.contains("product validation inside dev-hermit")
                || !rendered.contains(expected_remediation)
                || stdout.lines().last()
                    != Some("FINAL_VALIDATE_STATUS: COULD_NOT_RUN")
            {
                return Err(format!(
                    "front-door process bracket: {label} escaped/refused incorrectly: status={:?} \
                     output={rendered}",
                    output.status.code()
                ));
            }
            for unexpected in [
                checkout.join("target/validation"),
                checkout.join("ignored/validate"),
                parent.join("ledger"),
            ] {
                if unexpected.exists() {
                    return Err(format!(
                        "front-door process bracket: {label} created side effect before refusal: {}",
                        unexpected.display()
                    ));
                }
            }
        }
        Ok(())
    })();

    let cleanup = std::fs::remove_dir_all(&tmp)
        .map_err(|error| format!("front-door process bracket: cannot remove {}: {error}", tmp.display()));
    match (result, cleanup) {
        (Ok(()), Ok(())) => {
            println!(
                "  product front door process: full/focused/nested missing-authority runs refused \
                 before validation state"
            );
            Ok(())
        }
        (Err(problem), Ok(())) => Err(problem),
        (Ok(()), Err(cleanup_problem)) => Err(cleanup_problem),
        (Err(problem), Err(cleanup_problem)) => {
            Err(format!("{problem}; cleanup also failed: {cleanup_problem}"))
        }
    }
}

/// Aggregate libtest `executed` / `filtered` counts from typed step outcomes.
///
/// **This is the field the whole receipt rests on.** A row whose
/// `executed_tests` is null is a NON-VERDICT: every downstream completeness
/// predicate keys `is_clean_full_pass` on a nonzero executed count, so a driver
/// that ran no tests at all would otherwise emit a row indistinguishable from one
/// that ran the whole suite. `main` at `61edbef4` recorded 862 executed / 693
/// filtered, and a port that cannot reproduce that number has not preserved the
/// thing validate exists to do.
///
/// The runner derives these values from each step's COMPLETE captured bytes
/// before verbosity filters presentation. Thus level 1 can stay O(steps)
/// without erasing the receipt's evidence. `None` remains UNKNOWN and `Some(0)`
/// remains a demonstrated vacuous run; neither is coerced.
fn sum_typed_count(
    outcomes: &[StepOutcome],
    select: fn(&StepOutcome) -> Option<u64>,
) -> Option<i64> {
    let mut seen = false;
    let mut total = 0u64;
    for outcome in outcomes {
        if let Some(value) = select(outcome) {
            seen = true;
            total = total.checked_add(value)?;
        }
    }
    seen.then(|| i64::try_from(total).ok()).flatten()
}

fn libtest_counts(outcomes: &[StepOutcome]) -> (Option<i64>, Option<i64>) {
    (
        sum_typed_count(outcomes, |o| o.executed_tests),
        sum_typed_count(outcomes, |o| o.filtered_tests),
    )
}

/// Correct only an incorrect zero classification from the parent finalizer when this
/// driver's typed final outcome proves that the same planned node ran tests.
/// The parent remains the plan/coverage authority. Every other classification,
/// and every malformed or incomplete coverage object, is preserved fail-closed.
fn correct_test_node_coverage(
    mut coverage: serde_json::Value,
    planned_test_nodes: &BTreeSet<String>,
    outcomes: &[StepOutcome],
) -> serde_json::Value {
    let Some(planned_count) = coverage
        .get("planned_test_nodes")
        .and_then(serde_json::Value::as_u64)
    else {
        return coverage;
    };
    let Some(executed_count) = coverage
        .get("executed_test_nodes")
        .and_then(serde_json::Value::as_u64)
    else {
        return coverage;
    };
    let strings = |field: &str| -> Option<Vec<String>> {
        coverage
            .get(field)?
            .as_array()?
            .iter()
            .map(|value| value.as_str().map(str::to_string))
            .collect()
    };
    let Some(zero_executed_nodes) = strings("zero_executed_nodes") else {
        return coverage;
    };
    let Some(absent_nodes) = strings("absent_nodes") else {
        return coverage;
    };

    let zero_set: BTreeSet<String> = zero_executed_nodes.iter().cloned().collect();
    let absent_set: BTreeSet<String> = absent_nodes.iter().cloned().collect();
    let shape_is_valid = planned_count == planned_test_nodes.len() as u64
        && zero_set.len() == zero_executed_nodes.len()
        && absent_set.len() == absent_nodes.len()
        && zero_set.is_disjoint(&absent_set)
        && zero_set.iter().chain(absent_set.iter()).all(|tag| planned_test_nodes.contains(tag))
        && executed_count
            .checked_add(zero_set.len() as u64)
            .and_then(|count| count.checked_add(absent_set.len() as u64))
            == Some(planned_count);
    if !shape_is_valid {
        return coverage;
    }

    let final_outcomes: BTreeMap<&str, &StepOutcome> =
        outcomes.iter().map(|outcome| (outcome.tag.as_str(), outcome)).collect();
    let corrected: BTreeSet<String> = zero_set
        .iter()
        .filter(|tag| {
            final_outcomes
                .get(tag.as_str())
                .is_some_and(|outcome| {
                    !outcome.ok
                        && !outcome.aborted
                        && outcome.executed_tests.is_some_and(|n| n > 0)
                })
        })
        .cloned()
        .collect();
    if corrected.is_empty() {
        return coverage;
    }
    let Some(new_executed_count) = executed_count.checked_add(corrected.len() as u64) else {
        return coverage;
    };
    let remaining_zero: Vec<String> = zero_executed_nodes
        .into_iter()
        .filter(|tag| !corrected.contains(tag.as_str()))
        .collect();
    let Some(object) = coverage.as_object_mut() else {
        return coverage;
    };
    object.insert("executed_test_nodes".into(), serde_json::json!(new_executed_count));
    object.insert("zero_executed_nodes".into(), serde_json::json!(remaining_zero));
    coverage
}

/// Two-sided bracket for the narrow zero-classification correction. It proves that a
/// failed node with a positive typed count is corrected without changing its
/// failure, while every unproved or malformed case remains byte-for-value.
fn test_node_coverage_bracket() -> Result<(), String> {
    let outcome = |tag: &str, ok: bool, aborted: bool, executed_tests| StepOutcome {
        tag: tag.into(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests,
        filtered_tests: Some(0),
        returncode: Some(if ok { 0 } else { 100 }),
        reason: if ok { String::new() } else { "test failure".into() },
        aborted,
    };

    let planned = BTreeSet::from(["test.ran_failed".to_string()]);
    let parent_zero = serde_json::json!({
        "planned_test_nodes": 1,
        "executed_test_nodes": 0,
        "zero_executed_nodes": ["test.ran_failed"],
        "absent_nodes": [],
    });
    let ran_failed = outcome("test.ran_failed", false, false, Some(23));
    let coverage = correct_test_node_coverage(
        parent_zero.clone(),
        &planned,
        std::slice::from_ref(&ran_failed),
    );
    if coverage
        != serde_json::json!({
            "planned_test_nodes": 1,
            "executed_test_nodes": 1,
            "zero_executed_nodes": [],
            "absent_nodes": [],
        })
        || ran_failed.ok
    {
        return Err(
            "test-node coverage: a 23-test failed node must be executed and remain failed".into(),
        );
    }

    for (name, outcomes) in [
        ("missing", Vec::new()),
        ("zero", vec![outcome("test.ran_failed", true, false, Some(0))]),
        ("passing", vec![outcome("test.ran_failed", true, false, Some(23))]),
        ("count-unknown", vec![outcome("test.ran_failed", true, false, None)]),
        ("aborted", vec![outcome("test.ran_failed", false, true, Some(23))]),
        ("unplanned", vec![outcome("test.other", false, false, Some(23))]),
    ] {
        let unchanged = correct_test_node_coverage(parent_zero.clone(), &planned, &outcomes);
        if unchanged != parent_zero {
            return Err(format!(
                "test-node coverage: {name} evidence must not change the parent's classification"
            ));
        }
    }

    let parent_absent = serde_json::json!({
        "planned_test_nodes": 1,
        "executed_test_nodes": 0,
        "zero_executed_nodes": [],
        "absent_nodes": ["test.ran_failed"],
    });
    if correct_test_node_coverage(parent_absent.clone(), &planned, std::slice::from_ref(&ran_failed))
        != parent_absent
    {
        return Err("test-node coverage: an absent classification must not be rewritten".into());
    }
    let parent_executed = serde_json::json!({
        "planned_test_nodes": 1,
        "executed_test_nodes": 1,
        "zero_executed_nodes": [],
        "absent_nodes": [],
    });
    if correct_test_node_coverage(parent_executed.clone(), &planned, std::slice::from_ref(&ran_failed))
        != parent_executed
    {
        return Err("test-node coverage: an already-executed classification must not change".into());
    }
    for malformed in [
        serde_json::Value::Null,
        serde_json::json!({"planned_test_nodes": 1}),
        serde_json::json!({
            "planned_test_nodes": 1,
            "executed_test_nodes": 1,
            "zero_executed_nodes": ["test.ran_failed"],
            "absent_nodes": [],
        }),
    ] {
        if correct_test_node_coverage(malformed.clone(), &planned, std::slice::from_ref(&ran_failed))
            != malformed
        {
            return Err("test-node coverage: malformed evidence must remain unchanged".into());
        }
    }

    println!(
        "  test-node coverage: one incorrect zero corrected; unproved and malformed cases unchanged"
    );
    Ok(())
}

fn typed_libtest_count_bracket() -> Result<(), String> {
    let outcome = |tag: &str, ok: bool, executed_tests, filtered_tests| StepOutcome {
        tag: tag.into(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests,
        filtered_tests,
        returncode: Some(if ok { 0 } else { 100 }),
        reason: if ok { String::new() } else { "test failure".into() },
        aborted: false,
    };
    let full = vec![
        outcome("test.a", true, Some(398), Some(0)),
        outcome("test.b", true, Some(475), Some(350)),
    ];
    if libtest_counts(&full) != (Some(873), Some(350)) {
        return Err("typed libtest counts: complete outcomes did not sum to 873/350".into());
    }
    let failed = outcome("test.failed", false, Some(23), Some(5));
    if libtest_counts(std::slice::from_ref(&failed)) != (Some(23), Some(5)) || failed.ok {
        return Err(
            "typed libtest counts: a 23-test failed outcome must contribute counts and remain failed"
                .into(),
        );
    }
    if libtest_counts(&[outcome("test.zero", true, Some(0), Some(0))]) != (Some(0), Some(0)) {
        return Err("typed libtest counts: demonstrated zero was not preserved".into());
    }
    if libtest_counts(&[outcome("build.only", true, None, None)]) != (None, None) {
        return Err("typed libtest counts: unknown bannerless output was coerced".into());
    }
    println!(
        "  typed libtest counts: 873/350 pass and 23/5 failure counted; 0/0 preserved; unknown stayed null"
    );
    Ok(())
}

fn set_gate_failure_evidence(gate: &mut serde_json::Value, failed: bool) {
    gate["failure_origin"] = serde_json::json!(failed.then_some("outer_gate"));
    if failed {
        // This producer schedules the named outer DAG node directly, so an
        // atomic failure positively has zero failed lane substeps.
        gate["failed_substeps"] = serde_json::json!([]);
    } else {
        // Absence means no failure evidence applies. Never serialize an unknown
        // collection as null: the typed reader correctly rejects that shape.
        gate.as_object_mut()
            .expect("ledger gate must remain a JSON object")
            .remove("failed_substeps");
    }
}

fn ledger_gate(outcome: &StepOutcome) -> serde_json::Value {
    let mut gate = serde_json::json!({
        "name": outcome.tag,
        "result": ledger_gate_result(outcome),
        "exit_code": outcome.returncode,
        "reason": outcome.reason,
        "aborted": outcome.aborted,
        "real_seconds": outcome.duration_s,
    });
    set_gate_failure_evidence(&mut gate, outcome_is_failure(outcome));
    gate
}

/// Serialize one gate from the attempt ledger, not merely cumulative `by_tag`.
///
/// `by_tag` deliberately retains the last reported outcome so later retries can
/// still reason about it. If the latest scheduler invocation returned no payload,
/// however, that retained outcome is not the terminal execution fact. Promote the
/// latest attempt's explicit UNKNOWN state into the gate row so JSON consumers do
/// not read an earlier failure (or pass) as the result of an execution that never
/// completed.
fn ledger_gate_with_attempts(outcome: &StepOutcome, attempts: &[NodeAttempt]) -> serde_json::Value {
    let node_attempts_raw: Vec<&NodeAttempt> =
        attempts.iter().filter(|attempt| attempt.tag == outcome.tag).collect();
    let first = node_attempts_raw.first().copied();
    let latest = node_attempts_raw.last().copied();
    let node_attempts: Vec<serde_json::Value> = node_attempts_raw
        .iter()
        .map(|a| {
            let assessment = environmental_assessment(attempts, a);
            let environmental_verdict = assessment.map(|(verdict, _)| verdict.as_str());
            let environmental_refuted_shape = assessment
                .and_then(|(_, shape)| shape)
                .map(validate_runtime::RefutedShape::as_str);
            serde_json::json!({
                "attempt": a.attempt,
                // `null` is UNKNOWN and stays UNKNOWN: no completion payload
                // arrived, which is not the same as a failure and must never be
                // readable as a pass.
                "result": attempt_result(a),
                "reported": a.reported,
                "execution": a.execution.as_str(),
                "exit_code": a.returncode,
                "reason": a.reason,
                "aborted": a.aborted,
                "real_seconds": a.reported.then_some(a.duration_s),
                // Why this attempt was given another go. `null` on the last
                // attempt of every node, since nothing followed it.
                "retry_class": a.retry_class,
                // Classification is only a hypothesis. These fields say whether
                // a later actual execution confirmed/refuted it, or whether no
                // such execution occurred.
                "environmental_class": a.environmental_class,
                "environmental_detail_observed": a.detail_observed,
                "environmental_verdict": environmental_verdict,
                "environmental_refuted_shape": environmental_refuted_shape,
            })
        })
        .collect();

    // Synthetic stop-path fixtures predate attempt capture and intentionally pass
    // an empty ledger. Preserve their typed StepOutcome fallback; every scheduler
    // lane supplies attempts and therefore takes the exact-attempt branch.
    let mut gate = ledger_gate(outcome);
    gate["reported"] = serde_json::json!(latest.map(|attempt| attempt.reported).unwrap_or(true));
    gate["execution"] = serde_json::json!(latest
        .map(|attempt| attempt.execution)
        .unwrap_or_else(|| outcome_execution(outcome))
        .as_str());
    if let Some(attempt) = latest {
        gate["result"] = serde_json::json!(attempt_result(attempt));
        gate["exit_code"] = serde_json::json!(attempt.returncode);
        gate["reason"] = serde_json::json!(attempt.reason);
        gate["aborted"] = serde_json::json!(attempt.aborted);
        gate["real_seconds"] = serde_json::json!(attempt.reported.then_some(attempt.duration_s));
        set_gate_failure_evidence(&mut gate, attempt_is_failure(attempt));
    }
    gate["attempts"] = serde_json::json!(node_attempts);
    gate["retries"] = serde_json::json!(node_attempts_raw.len().saturating_sub(1));
    gate["first_attempt_result"] = serde_json::json!(first.and_then(attempt_result));
    gate["first_attempt_reason"] =
        serde_json::json!(first.map(|attempt| attempt.reason.as_str()));
    gate
}

fn ledger_gate_origin_bracket() -> Result<(), String> {
    let failed = StepOutcome {
        tag: "test.fixture".into(),
        ok: false,
        duration_s: 5.0,
        summary: String::new(),
        executed_tests: Some(1),
        filtered_tests: Some(0),
        returncode: Some(1),
        reason: "fixture failure".into(),
        aborted: false,
    };
    let row = ledger_gate(&failed);
    if row["failure_origin"] != "outer_gate"
        || row["failed_substeps"] != serde_json::json!([])
    {
        return Err(
            "ledger gate origin: failed outer gate did not carry a known-empty substep list"
                .into(),
        );
    }
    let mut passed = failed.clone();
    passed.ok = true;
    passed.returncode = Some(0);
    passed.reason.clear();
    let row = ledger_gate(&passed);
    if !row["failure_origin"].is_null() || row.get("failed_substeps").is_some() {
        return Err("ledger gate origin: passing gate claimed failure evidence".into());
    }
    let mut no_result = failed.clone();
    no_result.returncode = Some(NO_RESULT_EXIT_CODE);
    let row = ledger_gate(&no_result);
    if row["result"] != "no_result"
        || !row["failure_origin"].is_null()
        || row.get("failed_substeps").is_some()
    {
        return Err("ledger gate origin: no-result gate claimed failure evidence".into());
    }
    let mut aborted = failed.clone();
    aborted.aborted = true;
    aborted.returncode = Some(-15);
    let row = ledger_gate(&aborted);
    if !row["failure_origin"].is_null() || row.get("failed_substeps").is_some() {
        return Err("ledger gate origin: aborted gate claimed failure evidence".into());
    }

    // The stop-path integration test reaches the real write_ledger function but
    // deliberately has no scheduler attempts. Bracket the production serializer's
    // latest-attempt override separately, including stale cumulative outcomes.
    let assert_attempt_gate =
        |label: &str,
         outcome: &StepOutcome,
         attempts: &[NodeAttempt],
         expected_result: Option<&str>,
         expected_reported: bool,
         expected_execution: &str,
         expected_failure: bool|
         -> Result<(), String> {
            let row = ledger_gate_with_attempts(outcome, attempts);
            let latest = row["attempts"]
                .as_array()
                .and_then(|attempts| attempts.last())
                .ok_or_else(|| format!("ledger gate origin: {label} omitted attempt history"))?;
            let result = row.get("result").and_then(serde_json::Value::as_str);
            let origin = row
                .get("failure_origin")
                .and_then(serde_json::Value::as_str);
            let failure_evidence_matches = if expected_failure {
                origin == Some("outer_gate")
                    && row.get("failed_substeps") == Some(&serde_json::json!([]))
            } else {
                origin.is_none() && row.get("failed_substeps").is_none()
            };
            if result != expected_result
                || row["reported"].as_bool() != Some(expected_reported)
                || row["execution"].as_str() != Some(expected_execution)
                || latest.get("result").and_then(serde_json::Value::as_str) != expected_result
                || latest["reported"].as_bool() != Some(expected_reported)
                || latest["execution"].as_str() != Some(expected_execution)
                || !failure_evidence_matches
            {
                return Err(format!(
                    "ledger gate origin: {label} did not follow the latest attempt: {row}"
                ));
            }
            Ok(())
        };

    let failed_attempt = reported_attempt(&failed, 1);
    let passed_attempt = reported_attempt(&passed, 1);
    let aborted_attempt = reported_attempt(&aborted, 1);
    assert_attempt_gate(
        "latest genuine failure",
        &failed,
        std::slice::from_ref(&failed_attempt),
        Some("fail"),
        true,
        "completed",
        true,
    )?;
    assert_attempt_gate(
        "latest pass",
        &passed,
        std::slice::from_ref(&passed_attempt),
        Some("pass"),
        true,
        "completed",
        false,
    )?;
    assert_attempt_gate(
        "latest aborted attempt",
        &aborted,
        std::slice::from_ref(&aborted_attempt),
        None,
        true,
        "unknown",
        false,
    )?;

    let mut passed_retry = passed_attempt.clone();
    passed_retry.attempt = 2;
    let fail_then_pass = [failed_attempt.clone(), passed_retry];
    assert_attempt_gate(
        "stale failure followed by pass",
        &failed,
        &fail_then_pass,
        Some("pass"),
        true,
        "completed",
        false,
    )?;
    let mut aborted_retry = aborted_attempt.clone();
    aborted_retry.attempt = 2;
    let fail_then_aborted = [failed_attempt.clone(), aborted_retry];
    assert_attempt_gate(
        "stale failure followed by abort",
        &failed,
        &fail_then_aborted,
        None,
        true,
        "unknown",
        false,
    )?;
    let mut failed_retry = failed_attempt;
    failed_retry.attempt = 2;
    let pass_then_failure = [passed_attempt, failed_retry];
    assert_attempt_gate(
        "stale pass followed by genuine failure",
        &passed,
        &pass_then_failure,
        Some("fail"),
        true,
        "completed",
        true,
    )?;
    println!(
        "  ledger gate origin: fallback and latest-attempt failure evidence stayed typed"
    );
    Ok(())
}

fn requalification_plan_bracket(root: &Path) -> Result<(), String> {
    let args = parse_argv(&[
        "--requalify-cell".into(),
        "applications/timed-progress-bar".into(),
        "verify".into(),
        "ptrace".into(),
        "--no-label-pr".into(),
    ])
    .map_err(|code| format!("requalification plan: CLI refused with exit {code}"))?;
    let plan = build_plan(root, &args, &std::env::temp_dir().join("validate-requalification-plan"))?;
    if plan.suite_complete
        || plan.selection_mode != "targeted"
        || plan.cell_evidence_expected.as_ref().map(Vec::len) != Some(1)
    {
        return Err("requalification plan: targeted evidence was mistaken for a full suite".into());
    }
    let step = plan
        .cfg
        .steps
        .iter()
        .find(|step| step.tag() == "requalify.cell")
        .ok_or("requalification plan: exact cell step is absent")?;
    if step.hint.preferred_inner_jobs != Some(8) {
        return Err(
            "requalification plan: wrapper cannot admit the nested release build's eight workers"
                .into(),
        );
    }
    if step.jobs_flag.as_deref() != Some("") {
        return Err(
            "requalification plan: outer scheduler can append an unsupported -j flag".into(),
        );
    }
    for token in [
        "env -u DEV_HERMIT_PARENT ./ci/compat-envelope/pressure-test.rs run",
        "--test applications/timed-progress-bar",
        "--mode verify",
        "--backend ptrace",
        "--repetitions 1",
        "--run-id-prefix \"$E2E_RUN_ID-pid$$\"",
    ] {
        if !step.cmd.contains(token) {
            return Err(format!("requalification plan: command omitted {token}"));
        }
    }
    println!("  requalification plan: one exact selected cell, schema-7 eligible, never full authority");
    Ok(())
}

fn validate_series_writer_bracket() -> Result<(), String> {
    let root = std::env::temp_dir().join(format!(
        "validate-series-writer-{}-{}",
        std::process::id(),
        epoch_now()
    ));
    let parent = root.join("parent");
    let checkout = root.join("checkout");
    let results = root.join("results/bucket");
    std::fs::create_dir_all(parent.join("ci-hub/series"))
        .and_then(|_| std::fs::create_dir_all(&checkout))
        .and_then(|_| std::fs::create_dir_all(&results))
        .map_err(|error| format!("validate series writer: cannot create fixture: {error}"))?;
    std::fs::write(
        parent.join("ci-hub/series/series.py"),
        r#"import json
import pathlib
import sys
parent = pathlib.Path(sys.argv[sys.argv.index("--parent") + 1])
parent.joinpath("captured.json").write_text(json.dumps({"argv": sys.argv[1:], "stdin": sys.stdin.read()}))
print("fixture append accepted")
"#,
    )
    .map_err(|error| format!("validate series writer: cannot write fixture script: {error}"))?;
    let row = |attempt| {
        serde_json::json!({
            "schema": 4,
            "attempt": attempt,
            "hermit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_tree_dirty": false,
            "test": "applications/fixture",
            "category": "applications",
            "lane": "portable",
            "mode": "verify",
            "backend": "ptrace",
            "outcome": if attempt == 1 { "FAIL" } else { "PASS" },
        })
    };
    std::fs::write(
        results.join("results.jsonl"),
        format!("{}\n{}\n", row(1), row(2)),
    )
    .map_err(|error| format!("validate series writer: cannot write result fixture: {error}"))?;
    let saved = std::env::var_os("E2E_RUN_ID");
    // SAFETY: the validate self-test is single-threaded here and restores the
    // process environment before returning.
    unsafe { std::env::set_var("E2E_RUN_ID", "validate-series-fixture") };
    let appended = append_validate_series(
        Some(&parent),
        &checkout,
        &root.join("results"),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    match saved {
        Some(value) => unsafe { std::env::set_var("E2E_RUN_ID", value) },
        None => unsafe { std::env::remove_var("E2E_RUN_ID") },
    }
    appended?;
    let captured: serde_json::Value = serde_json::from_slice(
        &std::fs::read(parent.join("captured.json"))
            .map_err(|error| format!("validate series writer: cannot read captured call: {error}"))?,
    )
    .map_err(|error| format!("validate series writer: malformed captured call: {error}"))?;
    let argv = captured["argv"]
        .as_array()
        .ok_or("validate series writer: captured argv is not an array")?;
    let arguments = argv.iter().filter_map(serde_json::Value::as_str).collect::<Vec<_>>();
    for pair in [
        ["--checkout", checkout.to_str().ok_or("fixture checkout is not UTF-8")?],
        ["--producer", "validate"],
        ["--run-id", "validate-series-fixture"],
        ["--tree", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"],
    ] {
        if !arguments.windows(2).any(|window| window == pair) {
            return Err(format!("validate series writer: omitted argument pair {pair:?}"));
        }
    }
    let rows = captured["stdin"]
        .as_str()
        .ok_or("validate series writer: captured stdin is not a string")?
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("validate series writer: malformed captured row: {error}"))?;
    if rows.len() != 2 || rows[0]["attempt"] != 1 || rows[1]["attempt"] != 2 {
        return Err(format!(
            "validate series writer: ordinary appended attempts were not both sent: {rows:?}"
        ));
    }
    let _ = std::fs::remove_dir_all(&root);
    println!("  validate series writer: checkout identity and both appended attempts carried");
    Ok(())
}

fn possible_missing_artifact_bracket() -> Result<(), String> {
    let row = |tag: &str, ok: bool, returncode: Option<i64>, duration_s: f64| StepOutcome {
        tag: tag.into(),
        ok,
        duration_s,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        returncode,
        reason: String::new(),
        aborted: false,
    };
    let outcomes = vec![
        row("fixture.missing", false, Some(127), 1.0),
        row("fixture.slow_127", false, Some(127), 5.0),
        row("fixture.other_exit", false, Some(126), 1.0),
        row("fixture.passed", true, Some(0), 1.0),
    ];
    if possible_missing_artifact_nodes("only", &outcomes) != vec!["fixture.missing"] {
        return Err("missing-artifact hint did not select exactly the fast --only exit-127 row".into());
    }
    for mode in ["full", "targeted", "selective"] {
        if !possible_missing_artifact_nodes(mode, &outcomes).is_empty() {
            return Err(format!(
                "missing-artifact hint escaped --only into selection mode {mode}"
            ));
        }
    }
    println!(
        "  missing-artifact hint: 1 --only candidate / 3 non-qualifying shapes; other profiles silent"
    );
    Ok(())
}

fn no_result_propagation_bracket() -> Result<(), String> {
    let outcome = |tag: &str, returncode: i64, aborted: bool| StepOutcome {
        tag: tag.into(),
        ok: returncode == 0 && !aborted,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        returncode: Some(returncode),
        reason: String::new(),
        aborted,
    };

    let pass = outcome("pass", 0, false);
    if outcome_is_no_result(&pass)
        || outcome_is_failure(&pass)
        || ledger_gate_result(&pass) != "pass"
        || completed_exit_code(0, 0, false, false) != 0
        || ledger_run_results(0, 0, 0, false) != ("pass", "pass")
    {
        return Err("no-result propagation: exit 0 no longer stays PASS".into());
    }

    let no_result = outcome("no-result", NO_RESULT_EXIT_CODE, false);
    if !outcome_is_no_result(&no_result)
        || outcome_is_failure(&no_result)
        || ledger_gate_result(&no_result) != "no_result"
        || completed_exit_code(0, 1, false, false) != NO_RESULT_EXIT_CODE as u8
        || ledger_run_results(NO_RESULT_EXIT_CODE as u8, 0, 1, false)
            != ("fail", "no_result")
    {
        return Err("no-result propagation: exit 75 did not remain a distinct NO_RESULT".into());
    }
    let no_result_attempt = reported_attempt(&no_result, 1);
    let no_result_gate = ledger_gate_with_attempts(&no_result, std::slice::from_ref(&no_result_attempt));
    if no_result_gate["result"] != "no_result"
        || no_result_gate["attempts"][0]["result"] != "no_result"
        || !no_result_gate["failure_origin"].is_null()
        || no_result_gate.get("failed_substeps").is_some()
    {
        return Err(format!(
            "no-result propagation: exact-attempt ledger weakened exit 75: {no_result_gate}"
        ));
    }

    // A completed no-result retry ran, but it did not produce a pass/fail
    // verdict. It therefore cannot confirm or refute an earlier environmental
    // hypothesis; the hypothesis remains explicitly UNCONFIRMED.
    let initial_environmental = outcome("environmental-no-result", 1, false);
    let retry_no_result = outcome("environmental-no-result", NO_RESULT_EXIT_CODE, false);
    let mut initial_attempt = reported_attempt(&initial_environmental, 1);
    initial_attempt.environmental_class = Some("bpfjailer-banner".into());
    initial_attempt.detail_observed = true;
    let no_result_retry_attempt = reported_attempt(&retry_no_result, 2);
    let no_result_attempts = [initial_attempt, no_result_retry_attempt];
    if environmental_assessment(&no_result_attempts, &no_result_attempts[0])
        != Some((validate_runtime::EnvBlockVerdict::Unconfirmed, None))
    {
        return Err(
            "no-result propagation: exit 75 falsely settled an environmental hypothesis".into(),
        );
    }

    for returncode in [-9, 1, 2, 3, 74, 76, 124, 127] {
        let failure = outcome("failure", returncode, false);
        if outcome_is_no_result(&failure)
            || !outcome_is_failure(&failure)
            || ledger_gate_result(&failure) != "fail"
            || completed_exit_code(1, 0, false, false) != 1
            || ledger_run_results(1, 1, 0, false) != ("fail", "fail")
        {
            return Err(format!(
                "no-result propagation: genuine failure exit {returncode} was weakened"
            ));
        }
    }

    if completed_exit_code(1, 1, false, false) != 1
        || ledger_run_results(1, 1, 1, false) != ("fail", "fail")
        || ledger_run_results(1, 0, 1, false) != ("fail", "fail")
        || ledger_run_results(NO_RESULT_EXIT_CODE as u8, 1, 1, false) != ("fail", "fail")
    {
        return Err("no-result propagation: a sibling exit 75 hid a genuine failure".into());
    }

    if completed_exit_code(0, 1, true, false) != 1 {
        return Err("no-result propagation: a run timeout was weakened to NO_RESULT".into());
    }
    if completed_exit_code(0, 1, false, true) != 1 {
        return Err(
            "no-result propagation: an unexplained runner failure was weakened to NO_RESULT".into(),
        );
    }

    let aborted = outcome("aborted", NO_RESULT_EXIT_CODE, true);
    if outcome_is_no_result(&aborted) || outcome_is_failure(&aborted) {
        return Err("no-result propagation: an aborted row acquired a completed verdict".into());
    }

    println!(
        "  no-result propagation: 75 stayed distinct; 0 passed; 8 other exits and mixed 75+failure stayed RED"
    );
    Ok(())
}

/// Write one validation record through the single configured authority.
///
/// Every qualification is written HERE, at the single write point, so no
/// downstream reader can pair a bare `pass` with inferred coverage. Field names
/// and schema match what `validate.sh` wrote, so the parent aggregator and the
/// merge gate keep reading one shape across the port.
#[allow(clippy::too_many_arguments)]
fn write_ledger(
    ledger: &Path,
    ctx: &LedgerCtx,
    outcomes: &[StepOutcome],
    // Every attempt of every node, so a retried node's superseded verdict
    // reaches the row instead of being replaced by the one that followed it.
    attempts: &[NodeAttempt],
    skipped: &[String],
    host_inapplicable: &[validate_plan::HostInapplicableNode],
    planned_tags: &BTreeSet<String>,
    wall_s: f64,
    exit_code: u8,
    log_file: &str,
    suite_complete: bool,
    coverage: serde_json::Value,
    cell_results: Option<&validate_cell_results::RetainedCellResults>,
) {
    let (coverage_schema, coverage) = ledger_schema_and_coverage(coverage);
    let ledger_schema = ledger_schema_version(coverage_schema, cell_results);
    let gates_run = outcomes.len();
    let failures = outcomes.iter().filter(|o| outcome_is_failure(o)).count();
    let no_results = outcomes.iter().filter(|o| outcome_is_no_result(o)).count();
    // An operator stop learned nothing new about the product. Preserve the raw
    // shell outcome for forensics, but do not mint a FAILED verdict unless a
    // completed gate had already established one before the stop
    // (validate.sh:1473 `interruption_is_no_result`).
    let (raw_result, result) =
        ledger_run_results(exit_code, failures, no_results, ctx.interruption.is_some());
    let timed_out = timed_out_nodes(outcomes);
    // Stable per-row identity. Corrections never edit a row; they append a new
    // one carrying `corrects: <this id>`, which is what keeps the shard
    // append-only and safe to union across machines.
    let record_id = format!("{}-{}-{}", ctx.host, epoch_now(), std::process::id());
    // The PLANNED denominator, not the executed one. A node withheld as
    // host-inapplicable is added back here, so withholding can never shrink the
    // contract a green is measured against; the consumer's accounting
    // (`executed + intentionally skipped == expected`) then has to balance.
    // With nothing withheld this is byte-identical to what it has always been.
    let gates_expected = if ctx.profile == "full" && suite_complete {
        serde_json::json!(gates_run + host_inapplicable.len())
    } else {
        serde_json::Value::Null
    };
    // A host-inapplicable node is NEVER in `gates`: that array is the executed
    // PASS/FAIL list, and a node that did not run belongs in neither state. It
    // is carried in its own typed field instead, with the observation behind the
    // judgement, so no reader can mistake absence for coverage.
    let intentional_skipped_nodes: Vec<serde_json::Value> = host_inapplicable
        .iter()
        .map(|n| {
            serde_json::json!({
                "name": n.tag,
                "reason": validate_plan::HOST_INAPPLICABLE_REASON,
                "capability": n.capability.value(),
                "evidence": n.evidence,
            })
        })
        .collect();
    let gates: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|outcome| ledger_gate_with_attempts(outcome, attempts))
        .collect();
    // Nodes that were re-run, and nodes for which no completion payload ever
    // arrived. The second list is the verdict-capture population: a node that
    // reported nothing established nothing, and it is deliberately NOT retried
    // on that ground alone, so it must at least be counted here.
    let retried_nodes: Vec<&str> = {
        let mut names: Vec<&str> = attempts
            .iter()
            .filter(|a| a.attempt > 1)
            .map(|a| a.tag.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let unreported_attempt_nodes: Vec<&str> = {
        let mut names: Vec<&str> = attempts
            .iter()
            .filter(|a| !a.reported)
            .map(|a| a.tag.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    };
    let accounted: BTreeSet<&str> = outcomes
        .iter()
        .map(|o| o.tag.as_str())
        .chain(skipped.iter().map(String::as_str))
        .chain(host_inapplicable.iter().map(|n| n.tag.as_str()))
        .collect();
    let unaccounted_nodes: Vec<&str> = planned_tags
        .iter()
        .map(String::as_str)
        .filter(|tag| !accounted.contains(tag))
        .collect();
    let record = serde_json::json!({
        "schema_version": ledger_schema,
        "repo": "hermit",
        "producer": LEDGER_PRODUCER,
        "admission": ctx.admission,
        // Immutable-row identity. `corrects` is null here; a correcting row
        // repeats this shape with `corrects` set to the id it supersedes.
        "record_id": record_id,
        "corrects": serde_json::Value::Null,
        "run_id": cell_results.map(|results| results.run_id.as_str()),
        "started_at": ctx.started_at,
        "finished_at": utc_now(),
        "host": ctx.host,
        "toolchain": ctx.toolchain,
        "slot": ctx.slot,
        "cwd": ctx.cwd,
        "profile": ctx.profile,
        "selection_mode": ctx.selection_mode,
        "cache_state": ctx.cache_state,
        "commit": ctx.commit,
        "tree": ctx.tree,
        "git_depth": ctx.git_depth,
        "git_ahead": ctx.git_ahead,
        "git_behind": ctx.git_behind,
        "commit_anchored": ctx.commit_anchored,
        "tree_dirty": ctx.tree_dirty,
        "base_sha": ctx.base_sha,
        "base_tree": ctx.base_tree,
        "reverie_base_sha": ctx.reverie_base_sha,
        "reverie_base_tree": ctx.reverie_base_tree,
        "reverie_pin_current": ctx.reverie_pin_current,
        "result": result,
        "raw_result": raw_result,
        "exit_code": exit_code,
        "checks": gates_run,
        "failures": failures,
        "dag_jobs": ctx.dag_jobs,
        // Peak CPU-ACTIVE peer validates, and HOW that was established. `null`
        // means UNKNOWN — a bare run with no observed peer is not proven
        // exclusive, and writing 0 there would be a fabricated exclusivity claim.
        "concurrent_validates": ctx.concurrent_validates,
        "concurrency_proof": ctx.concurrency_proof,
        // Present (non-null) only for an operator stop; `result` above is then
        // `no_result` unless a completed gate had already failed.
        "interruption_signal": ctx.interruption,
        // Whole-run CPU (self + reaped children), the same numbers the printed
        // summary carries. Wall alone cannot separate a busy run from a wedged
        // one; the pair can.
        "user_seconds": ctx.cpu_user,
        "sys_seconds": ctx.cpu_sys,
        // Retry ROUNDS spent on environmental blocks. A green that only survived
        // because the host was retried must be distinguishable from a first-pass
        // green.
        "env_block_retries": ctx.env_block_retries,
        // WHICH nodes were retried, not merely how many rounds the lane spent.
        // `env_block_retries` alone cannot answer "what flaked?", so a per-node
        // rate was not computable from any row written before this field.
        "retried_nodes": retried_nodes,
        // Nodes that produced no completion payload on some attempt. Separate
        // from a failure on purpose: the run learned nothing about these, and
        // the two need opposite fixes.
        "unreported_attempt_nodes": unreported_attempt_nodes,
        // LIBTEST counts aggregated from the runner's typed step outcomes before
        // verbosity filters their human-facing presentation.
        // `null` is UNKNOWN and stays UNKNOWN: the receipt publisher fails closed
        // rather than turning missing evidence into a zero or a pass. These are
        // the counts every downstream `is_clean_full_pass` predicate keys on, so
        // a row without them is a NON-VERDICT, not a green.
        "executed_tests": ctx.executed_tests,
        "filtered_tests": ctx.filtered_tests,
        "gates_run": gates_run,
        "gates_expected": gates_expected,
        "skipped_nodes": skipped.len() + intentional_skipped_nodes.len(),
        // Typed pre-spawn omissions: nodes this MACHINE provably cannot run.
        // The reason vocabulary is closed on BOTH sides. The parent consumer
        // (ci-hub/validate/gate_completeness.py, ci-hub/lib/qualifying_receipt.rs)
        // admits only `empty-manifest-bucket`, so a row carrying
        // `host-inapplicable` is NOT a qualifying receipt until the owner opts
        // that reason in. Recording the omission honestly is what costs the
        // receipt; it is not a way to buy one.
        "intentional_skipped_nodes": intentional_skipped_nodes,
        // Nodes that never ran because something they depend on failed. Named,
        // not just counted, so a reader can tell the two kinds of absence apart.
        "dependency_skipped_nodes": skipped,
        // Planned nodes with NO terminal result and no recorded reason —
        // computed against the planned tag set rather than asserted empty, so a
        // deadline cut or a lane that never started is visible instead of
        // vanishing.
        "unaccounted_nodes": unaccounted_nodes,
        // A timeout is a RESULT, so it is recorded rather than dropped, and it is
        // named so a reader can separate "the tree is broken" from "a gate blew
        // its budget". Operator interrupts never reach this function at all.
        "timed_out_nodes": timed_out,
        // NODE counts, deliberately NOT named executed_tests/filtered_tests: a
        // schema<5 consumer keys is_clean_full_pass on those libtest-count names,
        // and a ~47-NODE DAG run must never be readable as a 47-TEST pass. The
        // counted receipt consumes the explicit test fields above rather than
        // treating this node count as test evidence.
        "executed_nodes": gates_run,
        "real_seconds": wall_s,
        "log_file": log_file,
        "coverage": coverage,
        "cell_results": cell_results.map(|results| &results.evidence),
        "gates": gates,
    });
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    let explicit = std::env::var(LEDGER_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .is_some_and(|value| Path::new(&value) == ledger);
    if !explicit && ledger.file_name().is_some_and(|name| name == "ledger") {
        let Some(parent) = ledger.parent() else {
            eprintln!("validate: warning: canonical ledger root has no parent: {}", ledger.display());
            return;
        };
        let adapter = parent.join("ci-hub/ledger/validate_rows.py");
        let mut child = match Command::new("python3")
            .arg(&adapter)
            .arg("record")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!(
                    "validate: warning: cannot launch canonical ledger writer {}: {e}",
                    adapter.display()
                );
                return;
            }
        };
        use std::io::Write;
        let write_error = child
            .stdin
            .take()
            .and_then(|mut stdin| stdin.write_all(line.as_bytes()).err());
        let output = child.wait_with_output();
        if let Some(error) = write_error {
            eprintln!("validate: warning: cannot send row to canonical ledger writer: {error}");
            return;
        }
        match output {
            Ok(output) if output.status.success() => eprintln!(
                "validate: canonical ledger record appended via {}: {}",
                adapter.display(),
                String::from_utf8_lossy(&output.stdout).trim()
            ),
            Ok(output) => eprintln!(
                "validate: warning: canonical ledger writer {} refused: {}",
                adapter.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(e) => eprintln!(
                "validate: warning: cannot wait for canonical ledger writer {}: {e}",
                adapter.display()
            ),
        }
        return;
    }

    if let Some(dir) = ledger.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("validate: warning: cannot create ledger dir {}: {e}", dir.display());
                return;
            }
        }
    }
    use std::io::Write;
    match std::fs::OpenOptions::new().create(true).append(true).open(ledger) {
        Ok(mut f) => match f.write_all(line.as_bytes()) {
            Ok(()) => {
                eprintln!(
                    "validate: fixture/standalone ledger record appended to {}",
                    ledger.display()
                );
                warn_if_unreadable_ledger(ledger);
            }
            Err(e) => eprintln!("validate: warning: cannot append ledger {}: {e}", ledger.display()),
        },
        Err(e) => eprintln!("validate: warning: cannot open ledger {}: {e}", ledger.display()),
    }
}

/// SHORT hostname, never an FQDN.
///
/// The shard name is part of a committed path, and an FQDN would leak internal
/// domain structure into the repository as well as making the same machine
/// produce different shard names depending on how DNS resolved that day. `hostname
/// -s` is the short form; anything with a dot is truncated at the first label as a
/// belt-and-braces guard in case `-s` is unavailable.
fn short_hostname() -> String {
    let raw = sh("hostname", &["-s"])
        .or_else(|| sh("hostname", &[]))
        .unwrap_or_else(|| "unknown".into());
    raw.split('.').next().unwrap_or("unknown").to_string()
}

/// Resolve the logical ledger authority. Precedence:
///   1. `$HERMIT_VALIDATE_LEDGER` — explicit fixture/standalone file.
///   2. `$DEV_HERMIT_PARENT/ledger` — the canonical adapter-backed union.
///   3. A discovered dev-hermit parent's canonical union.
///   4. The standalone in-repo diagnostic shard.
fn ledger_path(root: &Path) -> PathBuf {
    if let Ok(explicit) = std::env::var(LEDGER_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    if let Ok(parent) = std::env::var(PARENT_ENV) {
        if !parent.is_empty() {
            return PathBuf::from(parent).join("ledger");
        }
    }
    let team = std::env::var(LEDGER_TEAM_ENV)
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| LEDGER_TEAM_DEFAULT.to_string());
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
            .collect::<String>()
    };
    // CONFLICT RESOLUTION (rebase onto cd428f96): main added this parent-discovery step and this
    // PR replaced the fallback beneath it. Both are kept -- the discovery runs FIRST, then this
    // PR's team/host fallback. Dropping it would have silently reverted a landed fix.
    // main's rationale, preserved verbatim: the env var being unset does NOT mean there is no
    // parent -- far more often it means a run inside a dev-hermit slot that simply did not export
    // it. Measured 2026-08-08: 111 real rows sat in two slots' local ledgers for exactly that
    // reason, and `ci-hub validate-status` could not see one of them.
    if let Some(found) = discover_parent_ledger(root) {
        eprintln!(
            "validate.rs: {PARENT_ENV} is unset; recording to the DISCOVERED parent ledger {}",
            found.display()
        );
        return found;
    }
    root.join(LEDGER_DIR)
        .join(format!("{}.{}.jsonl", sanitize(&team), sanitize(&short_hostname())))
}

/// Walk up from `root` for the dev-hermit parent that owns the canonical adapter.
///
/// Deliberately keyed on the executable contract, not a directory name or a
/// retired raw file. Returns `None` only for a genuinely standalone checkout.
fn discover_parent_ledger(root: &Path) -> Option<PathBuf> {
    let mut dir = root.parent();
    while let Some(candidate) = dir {
        let adapter = candidate.join("ci-hub/ledger/validate_rows.py");
        if adapter.is_file() {
            return Some(candidate.join("ledger"));
        }
        dir = candidate.parent();
    }
    None
}

/// Say plainly that a row is not going anywhere a reader will look.
///
/// A writer that SUCCEEDS into a location no consumer reads reports success and attests nothing --
/// the same shape as a `locally-validated` label with no backing run. This does not fail the run,
/// because a standalone checkout must still be able to validate; it makes the invisibility
/// impossible to miss, so "silent success" stops being the failure mode.
///
/// CONFLICT RESOLUTION: main keyed this on `LOCAL_LEDGER_BASENAME`, which this PR removes. Re-keyed
/// to this PR's `LEDGER_DIR` fallback, which is the same thing under the new design -- the location
/// no reader queries. Behaviour preserved, constant adapted.
fn warn_if_unreadable_ledger(ledger: &Path) {
    if !ledger.parent().is_some_and(|p| p.ends_with(LEDGER_DIR)) {
        return;
    }
    eprintln!(
        "validate.rs: WARNING: this row is going to the CHECKOUT-LOCAL ledger {}, which NO reader \
         queries -- `ci-hub validate-status` will report NOT-VALIDATED for this commit even though \
         the run passed. Set {PARENT_ENV} to the dev-hermit workspace (or {LEDGER_ENV} to an \
         explicit file) if this row is meant to count.",
        ledger.display()
    );
}

// --------------------------------------------------------------------------- main

// --------------------------------------------------------------------- summary

/// What the invocation concluded. One variant per way validate can stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verdict {
    Pass,
    Fail,
    /// A completed gate could not determine its condition.
    NoResult,
    /// Admission control declined to run: dirty tree, stale base, unplannable
    /// profile, uncapped node, no boxing, no durable log, bad arguments.
    Refused,
    /// An operator stop. A NO-RESULT, not a failure.
    Interrupted,
    /// `--show-plan`: nothing was executed by design.
    PlanOnly,
    /// A prior passing record for this exact tree was reused.
    CacheHit,
    SelfTest,
    /// `--help`; the usage text IS the output.
    Help,
}

const FINAL_VALIDATE_STATUS_PREFIX: &str = "FINAL_VALIDATE_STATUS: ";
const FAILED_EXIT_CODE: u8 = 1;
const COULD_NOT_RUN_EXIT_CODE: u8 = NO_RESULT_EXIT_CODE as u8;

/// The machine-readable result of an actual validation attempt.
///
/// Help, host-capability queries and `--show-plan` do not attempt validation, so
/// they do not emit this contract. Every path that does attempt validation maps
/// to exactly one of these three values and to exactly one exit code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FinalValidateStatus {
    Passed,
    Failed,
    CouldNotRun,
}

impl FinalValidateStatus {
    fn for_verdict(verdict: Verdict) -> Option<Self> {
        match verdict {
            Verdict::Pass | Verdict::SelfTest | Verdict::CacheHit => Some(Self::Passed),
            Verdict::Fail => Some(Self::Failed),
            Verdict::NoResult | Verdict::Refused | Verdict::Interrupted => {
                Some(Self::CouldNotRun)
            }
            Verdict::PlanOnly | Verdict::Help => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Self::Passed => "PASSED",
            Self::Failed => "FAILED",
            Self::CouldNotRun => "COULD_NOT_RUN",
        }
    }

    fn exit_code(self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Failed => FAILED_EXIT_CODE,
            Self::CouldNotRun => COULD_NOT_RUN_EXIT_CODE,
        }
    }
}

fn final_validate_status_from_output(output: &str) -> Result<Option<FinalValidateStatus>, String> {
    // `next_back()`, not `last()`: both yield the final matching line, but `last()`
    // walks the whole iterator to get there and clippy refuses it on a
    // double-ended iterator. The LAST occurrence is deliberate -- a nested run can
    // emit the prefix more than once and the outermost status is the one that counts.
    let Some(value) = output
        .lines()
        .filter_map(|line| line.strip_prefix(FINAL_VALIDATE_STATUS_PREFIX))
        .next_back()
    else {
        return Ok(None);
    };
    match value {
        "PASSED" => Ok(Some(FinalValidateStatus::Passed)),
        "FAILED" => Ok(Some(FinalValidateStatus::Failed)),
        "COULD_NOT_RUN" => Ok(Some(FinalValidateStatus::CouldNotRun)),
        other => Err(format!("unknown final validate status {other:?}")),
    }
}

impl Verdict {
    fn marker(self) -> &'static str {
        match self {
            Verdict::Pass | Verdict::SelfTest | Verdict::CacheHit => "✅",
            Verdict::Fail => "❌",
            Verdict::NoResult => "⏹",
            Verdict::Refused => "🚫",
            Verdict::Interrupted => "⏹",
            Verdict::PlanOnly => "📋",
            Verdict::Help => "",
        }
    }
    fn word(self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Fail => "FAIL",
            Verdict::NoResult => "NO-RESULT",
            Verdict::Refused => "REFUSED",
            Verdict::Interrupted => "INTERRUPTED (no result)",
            Verdict::PlanOnly => "PLAN ONLY (nothing executed)",
            Verdict::CacheHit => "PASS (cache hit; nothing executed)",
            Verdict::SelfTest => "SELF-TEST",
            Verdict::Help => "HELP",
        }
    }
}

/// The end-of-run summary. **Every** exit path constructs one.
///
/// Owner directive (2026-08-07): "Validate itself should ALWAYS print a SUMMARY
/// at the end." That is enforced STRUCTURALLY rather than by discipline: `run`
/// returns a `RunSummary` instead of an exit code, so a new early return cannot
/// compile without saying what it concluded, and `main` is the single place that
/// renders it. A scope guard would have been weaker — it can only print a
/// default, whereas this makes each path state WHAT was refused and WHY.
///
/// The renderer runs BEFORE `DurableLog::finish`, so the summary is written into
/// the durable log as well as the terminal. The motivating gap was a real run
/// (main tip d2cdd2317, slot sol-validate, 2026-08-07T16:37:51Z) whose log ended
/// with a bare `Exit: 1 / Duration: 0s` and no conclusion at all.
struct RunSummary {
    verdict: Verdict,
    exit_code: u8,
    /// One or more lines naming what happened; for a refusal, what and why.
    detail: Vec<String>,
    /// Operator action rendered after the common footer and immediately before
    /// the final machine-readable status. A refusal's pasteable recovery command
    /// belongs here so it stays adjacent to the conclusion without violating the
    /// status-line ordering contract.
    epilogue: Vec<String>,
    profile: String,
    commit: String,
    nodes_executed: usize,
    nodes_failed: usize,
    nodes_skipped: usize,
    /// Planned nodes withheld because the MACHINE provably cannot run them.
    /// Counted separately from `nodes_executed` and `nodes_skipped` so the
    /// one-line accounting can never read as though everything planned ran.
    nodes_host_inapplicable: usize,
    /// Individual test ids that failed and then passed, with the retry grants
    /// that followed their failed attempts. Rendered even on a green run.
    flaky: Vec<TestIdRetry>,
    /// Individual test ids that failed FINALLY, after any retries were exhausted.
    failed_ids: Vec<TestIdRetry>,
    /// Failed DAG nodes for which no individual failing test id was emitted.
    /// Kept separate so a node tag is never presented as a test id.
    failed_nodes_without_test_ids: Vec<String>,
    /// Exact count of retry grants, read from non-null
    /// `attempts[].retry_class`. This is deliberately not `retried_nodes`, which
    /// includes successful peers re-run as part of a lane.
    retry_occurrences: usize,
    wall_s: Option<f64>,
    jobs: Option<i64>,
    log: Option<PathBuf>,
    ledger: Option<PathBuf>,
    /// `(wall, user, sys)` seconds for the WHOLE invocation, measured once at the
    /// single cleanup point so the ledger row and the printed summary carry
    /// byte-identical numbers (validate.sh:1855 made the same guarantee, and for
    /// the same reason: two independently-sampled "totals" that disagree make the
    /// receipt unciteable). `None` on a path that stopped before cleanup; `main`
    /// then measures live rather than printing nothing.
    cpu_wall: Option<(f64, f64, f64)>,
}

impl RunSummary {
    fn new(verdict: Verdict, exit_code: u8, profile: &str, detail: Vec<String>) -> Self {
        let exit_code = FinalValidateStatus::for_verdict(verdict)
            .map(FinalValidateStatus::exit_code)
            .unwrap_or(exit_code);
        RunSummary {
            verdict,
            exit_code,
            detail,
            epilogue: Vec::new(),
            profile: profile.to_string(),
            commit: git_sha(),
            nodes_executed: 0,
            nodes_failed: 0,
            nodes_skipped: 0,
            nodes_host_inapplicable: 0,
            flaky: Vec::new(),
            failed_ids: Vec::new(),
            failed_nodes_without_test_ids: Vec::new(),
            retry_occurrences: 0,
            wall_s: None,
            jobs: None,
            log: None,
            ledger: None,
            cpu_wall: None,
        }
    }
    /// Admission control declined. `what` names the gate, `why` the reason.
    fn refused(exit_code: u8, profile: &str, what: &str, why: Vec<String>) -> Self {
        let mut detail = vec![format!("refused by: {what}")];
        detail.extend(why);
        RunSummary::new(Verdict::Refused, exit_code, profile, detail)
    }

    fn with_epilogue(mut self, epilogue: Vec<String>) -> Self {
        self.epilogue = epilogue;
        self
    }
}

fn unavailable_invocation_lock_summary(profile: &str, error: String) -> RunSummary {
    RunSummary::refused(
        3,
        profile,
        "the per-checkout invocation lock",
        vec![format!(
            "cannot establish per-checkout exclusion: {error}; refusing rather than running two validates against shared target output"
        )],
    )
}

/// How many attempts this cell has had so far, by highest recorded ordinal.
///
/// Counting rows would undercount a cell whose attempt was never reported; the
/// ordinal is assigned when the attempt is made, so it is the honest measure of
/// "how many chances has this cell already had".
fn attempts_so_far(attempts: &[NodeAttempt], tag: &str) -> usize {
    attempts
        .iter()
        .filter(|a| a.tag == tag)
        .map(|a| a.attempt)
        .max()
        .unwrap_or(0)
}

fn retry_attempt_available(attempts: &[NodeAttempt], tag: &str) -> bool {
    attempts_so_far(attempts, tag) < validate_runtime::MAX_ATTEMPTS_PER_CELL
}

fn retain_cells_with_retry_attempt_available(
    keep: &mut BTreeSet<String>,
    attempts: &[NodeAttempt],
) {
    keep.retain(|tag| retry_attempt_available(attempts, tag));
}

fn retry_candidate_tags(
    cfg: &DagConfig,
    blocked: &[(String, String)],
    skipped: &[String],
    by_tag: &BTreeMap<String, StepOutcome>,
    latest_unreported: &BTreeSet<String>,
    attempts: &[NodeAttempt],
) -> BTreeSet<String> {
    let mut keep: BTreeSet<String> = blocked.iter().map(|(tag, _)| tag.clone()).collect();
    keep.extend(skipped.iter().cloned());
    keep.extend(by_tag.values().filter(|outcome| outcome.aborted).map(|outcome| outcome.tag.clone()));
    keep.extend(unreported_non_intentional_steps(cfg, by_tag, skipped));
    keep.extend(latest_unreported.iter().cloned());
    retain_cells_with_retry_attempt_available(&mut keep, attempts);
    keep
}

const SUMMARY_FLAKY_HEADING: &str =
    "⚠️  FLAKY — these test ids passed only after a retry:";

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestIdRetry {
    id: String,
    retry_classes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TestAttemptObservation {
    node: String,
    attempt: usize,
    id: String,
    passed: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TestIdSummary {
    recovered: Vec<TestIdRetry>,
    failed: Vec<TestIdRetry>,
    failed_nodes_without_test_ids: Vec<String>,
    retry_occurrences: usize,
}

/// Render the complete id list. Failure output is complete by contract, and a
/// truncated summary would not answer which tests failed.
fn summary_id_list(ids: &[String]) -> Vec<String> {
    ids.iter().map(|id| format!("     - {id}")).collect()
}

/// Retry grants attributable to one individual test id's failed observations.
///
/// ⚠️ READ `attempts[].retry_class`, NEVER `retried_nodes`. THIS IS NOT A STYLE
/// PREFERENCE AND THE WRONG FIELD PRODUCES A PLAUSIBLE NUMBER. `retried_nodes` is
/// every node with `attempt > 1`, and an environmental retry round RE-RUNS THE
/// WHOLE LANE -- so it fills with nodes that never failed. Measured on real rows:
///
/// ```text
///     2026-08-25T05:08:07Z   retried_nodes = 37   env_block_retries = 2
///     2026-08-25T02:27:14Z   retried_nodes = 22   env_block_retries = 1
/// ```
///
/// At most a handful of those 37 failed; the rest were re-run because something
/// else did. A retry count built from that field is roughly 5x too high, and it
/// looks entirely reasonable, which is why nothing would catch it. `retry_class`
/// is set only on an attempt for which a retry was actually GRANTED, per node,
/// and carries the reason in the words the retry line printed.
fn retry_classes_for_test(
    observations: &[TestAttemptObservation],
    attempts: &[NodeAttempt],
) -> Vec<String> {
    observations
        .iter()
        .filter(|observation| !observation.passed)
        .filter_map(|observation| {
            attempts
                .iter()
                .find(|attempt| {
                    attempt.tag == observation.node && attempt.attempt == observation.attempt
                })
                .and_then(|attempt| attempt.retry_class.clone())
        })
        .collect()
}

/// Parse a nextest terminal line after the `[node]` prefix.
///
/// nextest prints a failing test once when it executes and again in its failure
/// recap. The caller deduplicates within one node attempt; this function only
/// identifies the terminal row and returns its stable `binary$test` id.
fn nextest_test_observation(rest: &str) -> Option<(bool, String)> {
    let rest = rest.trim_start();
    let (passed, after_verdict) = if let Some(rest) = rest.strip_prefix("PASS") {
        (true, rest)
    } else {
        let verdicts = ["FAIL", "TIMEOUT", "LEAK", "ABORT", "SIGSEGV", "SIGABRT"];
        if let Some(rest) = verdicts.iter().find_map(|verdict| rest.strip_prefix(verdict)) {
            (false, rest)
        } else {
            let rest = rest.strip_prefix("TRY ")?;
            let (_ordinal, rest) = rest.split_once(" FAIL")?;
            (false, rest)
        }
    };
    let after_verdict = after_verdict.trim_start();
    let close = after_verdict.find(']')?;
    if !after_verdict.starts_with('[') {
        return None;
    }
    let mut identity = after_verdict[close + 1..].trim_start();
    // Newer nextest versions may print `(n/N)` between the elapsed time and the
    // binary id. It is progress, not part of the id.
    if identity.starts_with('(') {
        identity = identity[identity.find(')')? + 1..].trim_start();
    }
    let (binary, test) = identity.split_once(char::is_whitespace)?;
    let test = test.trim();
    if binary.is_empty() || test.is_empty() {
        return None;
    }
    Some((passed, format!("{binary}${test}")))
}

fn nextest_test_observations(log: &str) -> Vec<TestAttemptObservation> {
    let mut node_attempts: BTreeMap<String, usize> = BTreeMap::new();
    let mut seen: BTreeSet<(String, usize, String)> = BTreeSet::new();
    let mut observations = Vec::new();
    for line in log.lines() {
        let Some(after_open) = line.strip_prefix('[') else { continue };
        let Some((node, rest)) = after_open.split_once(']') else { continue };
        if rest.trim_start().starts_with("▶ START") {
            *node_attempts.entry(node.to_string()).or_default() += 1;
            continue;
        }
        let Some((passed, id)) = nextest_test_observation(rest) else { continue };
        let attempt = node_attempts.get(node).copied().unwrap_or(1);
        // The first row is the actual execution. A later identical row in the
        // same node attempt is nextest's recap, not another retry.
        if seen.insert((node.to_string(), attempt, id.clone())) {
            observations.push(TestAttemptObservation {
                node: node.to_string(), attempt, id, passed,
            });
        }
    }
    observations
}

fn collect_e2e_result_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in std::fs::read_dir(path)
        .map_err(|error| format!("cannot read per-cell result root {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read per-cell result entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot classify {}: {error}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_e2e_result_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == "results.jsonl" {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn e2e_test_observations(root: &Path) -> Result<Vec<TestAttemptObservation>, String> {
    let mut files = Vec::new();
    collect_e2e_result_files(root, &mut files)?;
    files.sort();
    let mut seen = BTreeSet::new();
    let mut observations = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: serde_json::Value = serde_json::from_str(line).map_err(|error| {
                format!("{}:{} malformed result row: {error}", file.display(), line_number + 1)
            })?;
            let field = |name: &str| {
                row.get(name)
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("{}:{} has no {name}", file.display(), line_number + 1))
            };
            let lane = field("lane")?;
            let category = field("category")?;
            let test = field("test")?;
            let mode = field("mode")?;
            let backend = field("backend")?;
            let outcome = field("outcome")?;
            let attempt = row.get("attempt").and_then(serde_json::Value::as_u64).unwrap_or(1);
            let attempt = usize::try_from(attempt)
                .map_err(|_| format!("{}:{} attempt does not fit usize", file.display(), line_number + 1))?;
            if attempt == 0 {
                return Err(format!("{}:{} attempt must be positive", file.display(), line_number + 1));
            }
            let id = format!("{test} [{backend}/{mode}]");
            if !seen.insert((lane.to_string(), id.clone(), attempt)) {
                return Err(format!(
                    "{}:{} duplicates test id {id} attempt {attempt}",
                    file.display(), line_number + 1
                ));
            }
            let group = match lane {
                "portable" => "e2e",
                "privileged" => "privileged-e2e",
                _ => {
                    return Err(format!(
                        "{}:{} has unrecognized lane {lane}",
                        file.display(), line_number + 1
                    ));
                }
            };
            let category = category.replace('-', "_");
            observations.push(TestAttemptObservation {
                node: format!("{group}.manifest_{category}"),
                attempt,
                id,
                passed: outcome == "PASS",
            });
        }
    }
    observations.sort_by(|left, right| {
        (&left.id, left.attempt, &left.node).cmp(&(&right.id, right.attempt, &right.node))
    });
    Ok(observations)
}

fn test_id_summary(
    mut observations: Vec<TestAttemptObservation>,
    attempts: &[NodeAttempt],
    failed_nodes: &BTreeSet<String>,
) -> TestIdSummary {
    observations.sort_by(|left, right| {
        (&left.id, left.attempt, &left.node).cmp(&(&right.id, right.attempt, &right.node))
    });
    let mut by_id: BTreeMap<String, Vec<TestAttemptObservation>> = BTreeMap::new();
    for observation in observations {
        by_id.entry(observation.id.clone()).or_default().push(observation);
    }
    let mut recovered = Vec::new();
    let mut failed = Vec::new();
    let mut failed_nodes_with_test_ids = BTreeSet::new();
    for (id, observations) in by_id {
        let Some(last) = observations.last() else { continue };
        let retry_classes = retry_classes_for_test(&observations, attempts);
        let was_retried = !retry_classes.is_empty();
        let item = TestIdRetry { id, retry_classes };
        if last.passed {
            if was_retried {
                recovered.push(item);
            }
        } else if failed_nodes.contains(&last.node) {
            failed_nodes_with_test_ids.insert(last.node.clone());
            failed.push(item);
        }
    }
    TestIdSummary {
        recovered,
        failed,
        failed_nodes_without_test_ids: failed_nodes
            .difference(&failed_nodes_with_test_ids)
            .cloned()
            .collect(),
        retry_occurrences: attempts.iter().filter(|attempt| attempt.retry_class.is_some()).count(),
    }
}

fn render_test_id_retry(item: &TestIdRetry) -> String {
    let retries = item.retry_classes.len();
    let classes = if item.retry_classes.is_empty() {
        String::new()
    } else {
        format!(": {}", item.retry_classes.join("; "))
    };
    format!(
        "{}  ({retries} retr{}{})",
        item.id,
        if retries == 1 { "y" } else { "ies" },
        classes
    )
}

/// The ONE summary renderer. Called from exactly one place.
///
/// `started` is the process's own start instant, used only when a path stopped
/// before cleanup could take the authoritative measurement.
fn run_summary_lines(s: &RunSummary, started: std::time::Instant) -> Vec<String> {
    if s.verdict == Verdict::Help {
        return Vec::new();
    }
    let mut lines = vec![
        String::new(),
        format!(
            "{} validate {} (exit {}) — profile {} @ {}",
            s.verdict.marker(),
            s.verdict.word(),
            s.exit_code,
            s.profile,
            s.commit
        ),
    ];
    for line in &s.detail {
        lines.push(format!("   {line}"));
    }
    // ---- the four-part end-of-run summary (owner directive 2026-08-26) ----
    //
    // ⚠️ THE FLAKY BLOCK IS RENDERED ON A PASSING RUN. That is the whole point and
    // it is the part that gets dropped, because on a green run there is nothing
    // demanding attention and the block looks like noise. A test that failed and
    // then passed is the only warning anyone gets before it fails for real.
    if !s.flaky.is_empty() {
        lines.push(String::new());
        lines.push(format!("   {SUMMARY_FLAKY_HEADING}"));
        let ids: Vec<String> = s.flaky.iter().map(render_test_id_retry).collect();
        lines.extend(summary_id_list(&ids));
        lines.push(format!("     {} test id(s) recovered", s.flaky.len()));
    }
    if !s.failed_ids.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "   ❌ FAILURE — {} test id(s) failed and did NOT recover on retry:",
            s.failed_ids.len()
        ));
        let ids: Vec<String> = s.failed_ids.iter().map(render_test_id_retry).collect();
        lines.extend(summary_id_list(&ids));
    }
    if !s.failed_nodes_without_test_ids.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "   ❌ FAILURE — {} node(s) failed without emitting an individual test id:",
            s.failed_nodes_without_test_ids.len()
        ));
        lines.extend(summary_id_list(&s.failed_nodes_without_test_ids));
    }
    if s.wall_s.is_some() && s.nodes_executed > 0 {
        lines.push(format!(
            "   retries: {} occurrence(s) recorded from attempts[].retry_class",
            s.retry_occurrences
        ));
    }
    // ⚠️ A CLEAN RUN SAYS SO, RATHER THAN SAYING NOTHING. With both blocks above
    // conditional and no else, a run with nothing to report rendered IDENTICALLY
    // to a run whose retry accounting produced nothing — the two are the same
    // bytes, so absence was readable as a result. That is the defect shape this
    // project keeps paying for, and it lands specifically on the reader the
    // trailing section exists for: someone scanning for a flaky warning cannot
    // tell "there were none" from "the question was never asked".
    //
    // Only when a DAG actually ran. Before that there is genuinely nothing to
    // have counted, and claiming otherwise would be the same error inverted.
    if s.flaky.is_empty()
        && s.failed_ids.is_empty()
        && s.failed_nodes_without_test_ids.is_empty()
        && s.retry_occurrences == 0
        && s.wall_s.is_some()
        && s.nodes_executed > 0
    {
        lines.push(String::new());
        lines.push(
            "   no retries, no flaky tests, and no failed test ids: every executed node passed first time"
                .to_string(),
        );
    }
    // Node accounting is printed whenever a DAG ran, and deliberately printed as
    // an explicit zero when one did not, so "no nodes ran" is a stated fact
    // rather than an absent line a reader has to interpret.
    match s.wall_s {
        Some(wall) => lines.push(format!(
            "   nodes: {} executed, {} failed, {} skipped{} in {}{}",
            s.nodes_executed,
            s.nodes_failed,
            s.nodes_skipped,
            if s.nodes_host_inapplicable == 0 {
                String::new()
            } else {
                format!(
                    ", {} host-inapplicable (NOT RUN, NOT passed)",
                    s.nodes_host_inapplicable
                )
            },
            human_duration(wall),
            s.jobs.map(|j| format!(" at -j {j}")).unwrap_or_default()
        )),
        None => lines.push("   nodes: none executed (stopped before the DAG ran)".into()),
    }
    match &s.log {
        Some(p) => lines.push(format!("   durable log: {}", p.display())),
        None => lines.push("   durable log: (none — stopped before one was opened)".into()),
    }
    if let Some(p) = &s.ledger {
        lines.push(format!("   ledger: {}", p.display()));
    }
    // ALWAYS printed, on success, failure, refusal, timeout and interruption
    // alike (validate.sh:1751). Wall alone cannot tell a busy run from a wedged
    // one; CPU (user+sys, this process plus every child it reaped) against wall
    // can, and that ratio is how the 53-minute pre-gate wedge was identified on
    // 2026-08-07 — the wall clock said "still going", the ratio said "waiting".
    let (wall, user, sys) = s
        .cpu_wall
        .unwrap_or_else(|| {
            let (u, sy) = validate_runtime::process_cpu_seconds();
            (started.elapsed().as_secs_f64(), u, sy)
        });
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    lines.push(format!(
        "   {}",
        validate_runtime::cpu_wall_line(human_duration, wall, user, sys, host_cpus)
    ));
    lines.extend(s.epilogue.iter().cloned());
    if let Some(status) = FinalValidateStatus::for_verdict(s.verdict) {
        // LAST by contract. A wrapper, guest, fixture or quoted diagnostic may
        // have written an earlier lookalike to the same channel; readers use the
        // last occurrence and require its value to agree with the exit code.
        lines.push(format!("{FINAL_VALIDATE_STATUS_PREFIX}{}", status.word()));
    }
    lines
}

fn print_run_summary(s: &RunSummary, started: std::time::Instant) {
    for line in run_summary_lines(s, started) {
        println!("{line}");
    }
}

/// `--probe-host-capability <name>`: report THIS machine's verdict for one
/// capability and exit, printing `PRESENT\t<evidence>` or `ABSENT\t<evidence>`.
///
/// A read-only query seam, in the same class as `--show-plan`: it runs no gate,
/// writes no ledger, and applies no label. It exists so a consumer that is not
/// this driver can reuse the SAME probe. Today that consumer is
/// `target/debug/test-harness`, which withholds a manifest CELL the machine cannot run
/// the way the driver withholds a NODE. Exposing the existing probe was the
/// alternative to writing a second one, and two probes for one question would
/// eventually disagree.
///
/// An unrecognized name exits 2 rather than answering: the vocabulary is closed
/// in [`validate_plan::HostCapability`], and inventing an answer for a name
/// nobody defined is exactly how a bogus reason to skip work would appear.
///
/// Returns `None` when the flag is absent, so ordinary parsing proceeds.
fn probe_host_capability_query() -> Option<u8> {
    let mut argv = std::env::args().skip(1);
    let name = loop {
        let arg = argv.next()?;
        if let Some(value) = arg.strip_prefix("--probe-host-capability=") {
            break value.to_string();
        }
        if arg == "--probe-host-capability" {
            match argv.next() {
                Some(value) => break value,
                None => {
                    eprintln!("validate: --probe-host-capability needs a capability name");
                    return Some(2);
                }
            }
        }
    };
    let Some(capability) = validate_plan::HostCapability::from_value(&name) else {
        eprintln!(
            "validate: unknown host capability '{name}'; the vocabulary is closed \
             (scripts/lib/validate_plan.rs::HostCapability) and an unrecognized name is refused \
             rather than answered"
        );
        return Some(2);
    };
    let verdict = validate_plan::probe_host_capability(capability);
    println!(
        "{}\t{}",
        if verdict.present { "PRESENT" } else { "ABSENT" },
        verdict.evidence
    );
    Some(0)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    // Answered before anything else because it is a question ABOUT THE MACHINE,
    // not a validation run: no handlers, no log, no plan, no gate.
    if let Some(code) = probe_host_capability_query() {
        return ExitCode::from(code);
    }
    install_stop_handlers();
    let started = std::time::Instant::now();

    // The durable log outlives `run` so the summary lands INSIDE it.
    let mut durable: Option<DurableLog> = None;
    let summary = run(&mut durable);
    print_run_summary(&summary, started);
    if let Some(d) = durable.take() {
        d.finish();
    }
    ExitCode::from(summary.exit_code)
}

/// The whole invocation, returning what it concluded rather than an exit code.
fn run(durable_slot: &mut Option<DurableLog>) -> RunSummary {
    let args = match parse_args() {
        Ok(a) => a,
        // `parse_args` returns 0 only for `--help`, whose usage text is the
        // output; anything else is a genuine CLI refusal and gets a summary.
        Err(0) => return RunSummary::new(Verdict::Help, 0, "help", vec![]),
        Err(code) => {
            return RunSummary::refused(
                code,
                "(arguments not parsed)",
                "argument parsing",
                vec!["see the message above; run --help for the accepted flags".into()],
            )
        }
    };

    if args.self_test && std::env::var_os(SUMMARY_EPILOGUE_SELF_TEST_ENV).is_some() {
        return RunSummary::refused(
            3,
            "self-test",
            "the per-checkout invocation lock",
            vec!["another validate is already running".into()],
        )
        .with_epilogue(vec![
            "watch the holder's live log with:".into(),
            "  tail -F -- $'/tmp/holder run.log'".into(),
        ]);
    }

    if nested_scope_probe_selected(args.self_test, nested_scope_probe_requested()) {
        return match run_nested_scope_probe() {
            Ok(detail) => RunSummary::new(
                Verdict::SelfTest, 0, "nested safe-ci scope self-test", vec![detail],
            ),
            Err(error) => {
                eprintln!("validate: NESTED SCOPE SELF-TEST FAILED: {error}");
                RunSummary::new(
                    Verdict::Fail, 2, "nested safe-ci scope self-test",
                    vec![format!("nested scope self-test failed: {error}")],
                )
            }
        };
    }

    if args.self_test {
        return match self_test() {
            Ok(()) => RunSummary::new(
                Verdict::SelfTest,
                0,
                "self-test",
                vec![
                    "force-full policy brackets, shell quoting, corpus counts, super gate table, \
                     envelope scoring/comparison, ledger cache, receipt eligibility, and the \
                     selective/only subset builders all passed"
                        .into(),
                    "policy/data brackets are inert; the cgroup bracket runs only inside a bounded \
                     disposable scope and neither publishes nor writes the real ledger"
                        .into(),
                ],
            ),
            Err(e) => {
                eprintln!("validate: SELF-TEST FAILED: {e}");
                RunSummary::new(Verdict::Fail, 2, "self-test", vec![format!("self-test failed: {e}")])
            }
        };
    }

    let level_name = args.level.name().to_string();
    let root = repo_root();
    if std::env::set_current_dir(&root).is_err() {
        return RunSummary::refused(
            2,
            &level_name,
            "repository root",
            vec![format!("cannot cd to repo root {}", root.display())],
        );
    }
    let parent = find_parent(&root);
    if std::env::var_os(PARENT_ENV).is_none() {
        if let Some(parent) = &parent {
            // Child test-harness processes use the same parent checkout for the
            // per-cell series writer. The validate driver already discovered
            // this path for its ledger, so do not make every child rediscover it.
            std::env::set_var(PARENT_ENV, parent);
        }
    }
    // The profile name is needed by the admission gates below, which run BEFORE
    // the plan exists. It is derived exactly as `build_plan` derives it, so the
    // lock record and the ledger row can never disagree about what was running.
    let profile_name =
        args.focused.as_ref().map(|f| f.profile()).unwrap_or_else(|| level_name.clone());

    // ---- re-entrancy (validate.sh:460) ---------------------------------------
    //
    // `ci/dag/portable.json`'s `test.strict_compat` node runs
    // `./scripts/validate.rs --portable-strict-compat-only`, so re-entry is a DESIGNED
    // path. What must never happen is a full driver inside a full driver: it pays
    // the whole preamble twice, appends a SECOND ledger row, and can publish a
    // SECOND receipt for one logical run. A nested FOCUSED invocation is a
    // PAYLOAD — the outer run owns the ledger, receipt, cache, lock and
    // concurrency accounting; a nested non-focused level is refused outright.
    let nesting = validate_runtime::detect_nesting();
    if let Some(stale) = nesting.stale_marker {
        eprintln!(
            "validate: ignoring a STALE {} marker naming pid {stale}: that pid is not an ancestor \
             of this process, so this is a TOP-LEVEL run. (Treating the bare env var as proof of \
             nesting would refuse every legitimate full run in a shell that once exported it.)",
            validate_runtime::ACTIVE_ENV
        );
    }
    // The marker is claimed LATER, after the cgroup re-exec -- see the call site
    // below resolve_cgroups. Claiming it here made the driver REFUSE ITSELF:
    // resolve_cgroups re-execs into a transient systemd scope for boxing, the
    // re-exec inherits the environment, and the new process is a genuine
    // DESCENDANT of the claimer -- so is_ancestor() was true and it read its own
    // boxing re-exec as a nested run. Measured: a full profile could not start at
    // all under boxing, refusing with "outer pid <the scope's own parent>" in 0s.
    // --self-test and --show-plan both missed it because neither re-execs.
    //
    // The boxing re-exec is the SAME logical run, not a nested one. Only the
    // process that survives the re-exec should claim the marker.
    if nesting.nested && args.focused.is_none() {
        let outer = nesting.outer_pid.unwrap_or(-1);
        eprintln!(
            "validate: refusing to re-enter a full validation level from inside validate (outer \
             pid {outer}); nested invocations may only run a focused mode."
        );
        return RunSummary::refused(
            2,
            &profile_name,
            "the re-entrancy guard",
            vec![
                format!("this process is a descendant of validate pid {outer}, which is already driving a run"),
                "a full suite inside a full suite would pay the whole preamble twice, append a \
                 SECOND ledger row, and could publish a SECOND receipt for one logical run"
                    .into(),
                "nested invocations may run ONE focused mode as a payload; the outer run owns the \
                 ledger, receipt, cache and concurrency accounting"
                    .into(),
            ],
        );
    }

    // ---- stop-path test seam (validate.sh:1899) ------------------------------
    //
    // Placed before every admission gate on purpose: this fixture exists to
    // exercise the REAL signal traps and the REAL ledger writer without starting
    // a product build, so making it depend on the checkout's cleanliness or
    // freshness would turn `scripts/test_validate_stop_paths.py` into a test of
    // this tree's state instead of the stop paths. It deliberately does NOT take
    // the invocation lock: it never runs a gate, and a leaked fixture must never
    // wedge a real run.
    if validate_runtime::stop_test_requested() {
        return stop_test_seam(&root, &profile_name, parent.as_deref());
    }

    // ---- dev-hermit product front door -------------------------------------
    //
    // Refuse real product work before the invocation lock, cache, cgroup boxing,
    // durable log, ledger, or DAG can create side effects. A parent `ci-hub/`
    // directory identifies dev-hermit; within that boundary, a missing or
    // unreadable launcher/authority is a refusal rather than a standalone
    // escape. Nested focused payloads are legitimate descendants of the same
    // lock owner and pass that canonical check; they are not exempted based on
    // their caller-supplied nesting marker.
    // Help, self-test and the stop-test seam returned above; `--show-plan` is
    // explicitly inert here.
    let ci_hub_dir_present =
        parent.as_ref().is_some_and(|candidate| candidate.join("ci-hub").is_dir());
    if product_front_door_applies(
        parent.is_some(),
        ci_hub_dir_present,
        nesting.nested,
        args.show_plan,
    ) {
        let parent = parent.as_deref().expect("front-door predicate requires a parent");
        let commit = git_sha();
        let host = short_hostname();
        let ci_hub_launcher_available = parent.join("ci-hub/ci-hub").is_file();
        let admission = canonical_validate_lock_admission(Some(parent), &commit, &host);
        // NAME THE CONJUNCT THAT FAILED. The decision is unchanged -- it is still
        // exactly `admission.is_ok()` -- but a refusal that lists three
        // possibilities and identifies none is undiagnosable from outside, and
        // that is what left the owner unable to see that his checkout simply was
        // not at the commit his lock was taken for.
        let admitted = admission.is_ok();
        let why = admission.err();
        if let Some(refusal) = product_front_door_refusal(
            parent,
            &root,
            &commit,
            &requested_validate_args(),
            ci_hub_launcher_available,
            admitted,
        ) {
            eprintln!("{refusal}");
            if let Some(reason) = &why {
                eprintln!("\nWhy this run was not admitted:\n  {reason}");
            }
            let mut detail = vec![match &why {
                Some(reason) => format!(
                    "the dev-hermit boundary was detected and admission was not established: \
                     {reason}"
                ),
                None => "the dev-hermit boundary was detected, but exact-commit, exact-host, \
                         live validate-lock owner ancestry was not established"
                    .to_string(),
            }];
            detail.push(
                "repair ci-hub if needed, then use its validate-run entry point; environment \
                 markers cannot authorize product work"
                    .into(),
            );
            return RunSummary::refused(
                4,
                &profile_name,
                "the dev-hermit product front door",
                detail,
            );
        }
    }

    // Anchor the logical run before locks, freshness checks, plan construction, cgroup re-exec,
    // durable-log setup, and registration.  A nested focused payload inherits the enclosing
    // safe-ci step's scheduler-owned epoch; a top-level run owns its epoch here.
    let run_timeout = args
        .run_timeout
        .or_else(|| env_positive("HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS"));
    let deadline_ns = if args.show_plan {
        None
    } else {
        match invocation_deadline_ns(run_timeout, nesting.nested) {
            Ok(deadline) => deadline,
            Err(msg) => {
                eprintln!("validate: REFUSED — {msg}");
                return RunSummary::refused(
                    3,
                    &profile_name,
                    "the shared timeout epoch",
                    vec![msg],
                );
            }
        }
    };

    // ---- concurrent invocation (validate.sh:492) -----------------------------
    //
    // A second validate in the SAME checkout is unambiguously wrong: both drive
    // one `target/` tree and one ledger. Refuse LOUDLY and IMMEDIATELY, naming
    // the holder — never wait, and never let two interleave. Scope is
    // PER-CHECKOUT; box-wide exclusivity belongs to `ci-hub validate-lock`, and
    // duplicating it here would give the fleet two admission controllers that can
    // disagree. `--show-plan` executes nothing, so it is not a second driver and
    // does not contend.
    let mut invocation_lock;
    if !nesting.nested && !args.show_plan {
        match validate_runtime::acquire_invocation_lock(&root, &profile_name, &git_sha()) {
            validate_runtime::LockOutcome::Acquired(l) => invocation_lock = Some(l),
            validate_runtime::LockOutcome::Busy { detail, epilogue } => {
                return RunSummary::refused(
                    3,
                    &profile_name,
                    "the per-checkout invocation lock",
                    detail,
                )
                .with_epilogue(epilogue);
            }
            validate_runtime::LockOutcome::SafetyRefusal(error) => {
                return RunSummary::refused(
                    3,
                    &profile_name,
                    "the per-checkout invocation safety guard",
                    vec![error],
                );
            }
            validate_runtime::LockOutcome::Unavailable(e) => {
                return unavailable_invocation_lock_summary(&profile_name, e);
            }
        }
    } else {
        invocation_lock = None;
    }

    // Dirty-tree gate, BEFORE any state is created, so a refusal leaves nothing
    // behind. A result validated against uncommitted changes describes a tree
    // that exists nowhere in history and cannot be reproduced or compared.
    // Skipped for a nested payload: the outer run already made this judgement
    // about the same checkout, and a second answer could only disagree.
    let wt_dirty = worktree_dirty();
    if !nesting.nested
        && wt_dirty
        && !args.skip_inner_dirty_working_tree_and_rebase_freshness_checks
    {
        eprintln!("validate: refusing to run on a dirty working tree.");
        eprintln!("  HEAD {} has uncommitted working-tree changes, so a record anchored to it", git_sha());
        eprintln!("  would describe a tree that exists nowhere in history. Commit (preferred), or");
        eprintln!("  stage the WIP with 'git add', then re-run. To force an explicitly unanchored");
        eprintln!(
            "  run pass --skip-inner-dirty-working-tree-and-rebase-freshness-checks \
             (agents must not). This skips only scripts/validate.rs's dirty-working-tree and"
        );
        eprintln!("  rebase-freshness checks; it does not bypass ci-hub validate-lock admission.");
        let _ = Command::new("git").args(["status", "--short"]).status();
        return RunSummary::refused(
            2,
            &profile_name,
            "the dirty-working-tree gate",
            vec![
                "HEAD has uncommitted working-tree changes, so a record anchored to it would \
                 describe a tree that exists nowhere in history"
                    .into(),
                "commit (preferred) or `git add` the WIP, then re-run; \
                 --skip-inner-dirty-working-tree-and-rebase-freshness-checks forces an explicitly \
                 unanchored run but does not bypass ci-hub validate-lock admission"
                    .into(),
            ],
        );
    }

    // Rebase-freshness gate. Mechanically enforced, not advisory. A nested
    // payload inherits the outer run's verdict on the very same checkout; it also
    // must not spend a network round trip inside a budgeted DAG node.
    match rebase_freshness(
        args.skip_inner_dirty_working_tree_and_rebase_freshness_checks || nesting.nested,
    ) {
        Ok(msg) => eprintln!("validate: {msg}"),
        Err(msg) => {
            eprintln!("validate: refusing to validate a stale base.\n  {msg}");
            return RunSummary::refused(
                2,
                &profile_name,
                "the rebase-freshness gate",
                msg.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
            );
        }
    }

    // Run state lives under target/, never under HERMIT_DIR (a user setting).
    let tmp = root.join("target/validation").join(format!("run-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        return RunSummary::refused(
            2,
            &profile_name,
            "run-state setup",
            vec![format!("cannot create {}: {e}", tmp.display())],
        );
    }
    if !nesting.nested {
        for (variable, name) in [
            ("TMPDIR", "tmp"),
            ("XDG_CACHE_HOME", "cache"),
            ("PYTHONPYCACHEPREFIX", "python-cache"),
            ("HERMIT_DATA_DIR", "hermit-data"),
        ] {
            if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
                continue;
            }
            let path = tmp.join(name);
            if let Err(error) = std::fs::create_dir_all(&path) {
                return RunSummary::refused(
                    2,
                    &profile_name,
                    "run-owned temporary path setup",
                    vec![format!("cannot create {} for {variable}: {error}", path.display())],
                );
            }
            std::env::set_var(variable, path);
        }
    }

    let mut plan = match build_plan(&root, &args, &tmp) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("validate: cannot build the execution plan: {e}");
            return RunSummary::refused(
                2,
                &profile_name,
                "plan construction",
                vec![
                    e,
                    "no substitute profile was run: reporting a DIFFERENT gate set under the \
                     requested name would be worse than refusing"
                        .into(),
                ],
            );
        }
    };

    assign_fail_fast_families(&mut plan);

    // Nested validate payloads are ordinary DAG children. Carry the selected
    // level through the plan so `--verbosity 5` does not become level 1 at the
    // nested strict-compat boundary (and default level 1 stays bounded there).
    propagate_verbosity(&mut plan, args.verbosity);

    if let Err(error) = reuse_current_validate_executable(&mut plan) {
        eprintln!("validate: cannot bind the nested strict-compatibility payload: {error}");
        return RunSummary::refused(
            2,
            &profile_name,
            "nested strict-compatibility validate executable",
            vec![
                error,
                "test.strict_compat was not run with an unverified or changed invocation".into(),
            ],
        );
    }

    // A node this machine provably cannot run is withheld here, BEFORE anything
    // spawns, and recorded as host-inapplicable. Nothing a node DOES can reach
    // this decision, so a node that is merely broken still runs and still fails.
    if let Err(e) = withhold_host_inapplicable(&root, &mut plan) {
        eprintln!("validate: cannot resolve host-capability requirements: {e}");
        return RunSummary::refused(
            2,
            &level_name,
            "host-capability resolution",
            vec![
                e,
                "no node was omitted and no substitute profile was run: an unevaluable capability \
                 declaration is refused, never treated as a reason to skip work"
                    .into(),
            ],
        );
    }

    // Per-gate budget overrides, preserved from validate.sh
    // (VALIDATE_GATE_TIMEOUT_SECONDS / VALIDATE_GATE_CPU_TIMEOUT_SECONDS). These
    // LOWER a node's ceiling, never raise it: a caller tightening budgets to
    // reproduce a timeout must not accidentally loosen a node that already
    // declared something stricter. They are also how the timeout path is
    // exercised on demand without waiting for a real runaway.
    // DERIVE every node's ceiling from the budget that will actually be ENFORCED.
    // The scheduler is handed `remaining_budget_s(deadline)`, not the nominal run
    // budget, so a ceiling chosen BESIDE the nominal one silently inverts as soon
    // as preparation has spent part of the epoch: the node budget stops being
    // smaller than the bound that will cut it, and the scheduler refuses the whole
    // lane rather than running work it could not attribute.
    //
    // Measured 2026-08-25 on the strict-compat lane: the ladder is written
    // `420 prep < 480 gate < 600 run`, but prep and gate are spent SEQUENTIALLY
    // from one clock, so the run budget would have to be at least 900s for that to
    // hold. 159s of the 600s epoch was already gone when the scheduler started,
    // leaving 441s against a 480s gate ceiling, and all 193 compat nodes reported
    // nothing. Deriving the ceiling here makes the inversion unreachable instead of
    // making it fit for one particular preparation time -- the same fixed-versus-
    // derived defect as a node pinned at 120s losing to a 120.03s measurement.
    if let Some(remaining) = remaining_budget_s(deadline_ns) {
        clamp_wall(&mut plan, derived_wall_ceiling(remaining));
    }
    if let Some(cap) = env_positive("VALIDATE_GATE_TIMEOUT_SECONDS") {
        clamp_wall(&mut plan, cap);
        eprintln!("validate: VALIDATE_GATE_TIMEOUT_SECONDS={cap}: every gate's wall ceiling lowered to at most {cap}s");
    }
    if let Some(cap) = env_positive("VALIDATE_GATE_CPU_TIMEOUT_SECONDS") {
        clamp_cpu(&mut plan, cap);
        eprintln!("validate: VALIDATE_GATE_CPU_TIMEOUT_SECONDS={cap}: every gate's CPU budget lowered to at most {cap}s");
    }

    // Fail-closed caps audit. A node without declared caps would run UNBOXED
    // while the driver still printed "boxing ACTIVE" — a green verifying less
    // than it claims. Refuse rather than run.
    // FAIL CLOSED on capacity that can never be granted. A step demanding a
    // resource the config does not cap is unschedulable forever, and the
    // scheduler expresses that as an infinite 50 ms sleep, not an error --
    // measured: 21 of ~58 nodes done, then 14 minutes at 0% CPU with no exit.
    // Refuse here so it is a named refusal before anything runs.
    let mut ungrantable = validate_plan::ungrantable_resources(&plan.cfg);
    if let Some(second) = &plan.second {
        ungrantable.extend(validate_plan::ungrantable_resources(second));
    }
    if !ungrantable.is_empty() {
        return RunSummary::refused(
            3,
            &plan.profile,
            "ungrantable scarce-resource demand",
            vec![
                format!("{} step(s) demand capacity the DAG config never grants:", ungrantable.len()),
            ]
            .into_iter()
            .chain(capped_refusal_items(
                ungrantable.iter().map(|b| format!("  {b}")).collect(),
            ))
            .chain(std::iter::once(
                "the scheduler would sleep forever rather than fail: its only exit is                  running.is_empty() && done+skipped >= steps.len()".to_string(),
            ))
            .collect(),
        );
    }
    let mut undeclared = validate_plan::undeclared_nodes(&plan.cfg);
    if let Some(second) = &plan.second {
        undeclared.extend(validate_plan::undeclared_nodes(second));
    }
    if !undeclared.is_empty() {
        eprintln!(
            "validate: ERROR: {} node(s) lack declared resource caps and would run UNBOXED: {}",
            undeclared.len(),
            undeclared.join(", ")
        );
        eprintln!("  Declare timeout + cpu_timeout + a memory hint for each; see scripts/lib/validate_plan.rs.");
        return RunSummary::refused(
            3,
            &plan.profile,
            "the declared-caps audit",
            vec![
                format!(
                    "{} node(s) would run UNBOXED while the driver claimed boxing was active: {}",
                    undeclared.len(),
                    undeclared.join(", ")
                ),
                "declare timeout + cpu_timeout + a memory hint for each; see \
                 scripts/lib/validate_plan.rs"
                    .into(),
            ],
        );
    }

    // The whole-run budget is the first boundary able to stop cumulative cost
    // while preserving evidence. Per-node caps cannot bound a sequence of legal
    // nodes, and the hosted job kill discards the diagnostic tail.
    // Refuse an inverted ladder before even `--show-plan` succeeds. A node with
    // an allowance at least as large as the run budget can only be cut by the
    // less-specific outer clock, losing attribution to the node.
    if let Some(secs) = run_timeout {
        let mut bad = steps_violating_run_timeout(&plan.cfg, secs);
        if let Some(second) = &plan.second {
            bad.extend(steps_violating_run_timeout(second, secs));
        }
        if !bad.is_empty() {
            bad.sort();
            bad.dedup();
            return RunSummary::refused(
                3,
                &plan.profile,
                "whole-run budget is not larger than every node budget",
                std::iter::once(format!(
                    "{} node(s) declare a wall budget >= the {secs}s whole-run budget:",
                    bad.len()
                ))
                .chain(capped_refusal_items(
                    bad.iter()
                        .map(|(tag, t)| format!("  {tag} ({t}s)"))
                        .collect(),
                ))
                .chain(std::iter::once(
                    "lower the named node budgets so each can diagnose itself before the whole-run boundary"
                        .to_string(),
                ))
                .collect(),
            );
        }
    }

    // Print the plan and exit. This makes "what will actually run, and under what
    // caps" reviewable without spending a validate slot — and it is how the
    // declared-caps claim above can be checked by eye rather than trusted.
    if args.show_plan {
        let mut all: Vec<&DagConfig> = vec![&plan.cfg];
        if let Some(s) = &plan.second {
            all.push(s);
        }
        println!("profile: {}  selection: {}", plan.profile, plan.selection_mode);
        for (i, cfg) in all.iter().enumerate() {
            println!("\n--- DAG {} of {} ({}) : {} node(s)", i + 1, all.len(), cfg.description, cfg.steps.len());
            println!("{:<40} {:>7} {:>7} {:>8}  deps", "node", "wall_s", "cpu_s", "mem");
            for s in &cfg.steps {
                let cpu = if s.cpu_timeout > 0 { s.cpu_timeout } else { cfg.default_step_cpu_timeout };
                let mem = s.hint.hard_mem_max_bytes.or(s.hint.rss_baseline_bytes).unwrap_or(0);
                println!(
                    "{:<40} {:>7} {:>7} {:>7}M  {}",
                    s.tag(), s.timeout, cpu, mem / (1024 * 1024), s.deps.join(",")
                );
            }
        }
        let total: usize = all.iter().map(|c| c.steps.len()).sum();
        println!("\ntotal boxed nodes: {total}; all have declared wall+cpu+memory caps (audited above).");
        return RunSummary::new(
            Verdict::PlanOnly,
            0,
            &plan.profile,
            vec![
                format!("--show-plan: {total} boxed node(s) printed, all with declared wall+cpu+memory caps"),
                "nothing was executed and no ledger row was written".into(),
            ],
        );
    }

    // ---- tree-keyed result cache (validate.sh:620/655) -------------------
    //
    // Runs BEFORE boxing and before the durable log, so a hit leaves no partial
    // state behind and appends no derived record — the same placement the bash
    // used. The key is the TREE hash, not the commit: a rebase or amend that
    // leaves content byte-identical is the same thing to validate, and keying on
    // the commit would re-run it. `--ignore-cache` forces a real run; a focused
    // or selective profile is never cached because `selection_mode == "full"` is
    // part of the key.
    let ledger = ledger_path(&root);
    let ledger_rows = validate_history::read_rows(&ledger);
    let tree = git_tree();
    let host = short_hostname();
    let toolchain = sh("rustc", &["--version"]).unwrap_or_else(|| "unknown".into());
    let cache = cache_state(&root);
    let cache_key = validate_history::CacheKey {
        tree: &tree,
        profile: &plan.profile,
        host: &host,
        toolchain: &toolchain,
    };
    // A nested payload never consults the cache: the outer run already did, and a
    // payload that "hit" would report a green for a lane it never ran.
    if !nesting.nested
        && !args.ignore_cache
        && plan.cacheable
        && !wt_dirty
        && !tree_dirty()
        && plan.selection_mode == "full"
    {
        if let Some(hit) = validate_history::cache_lookup(&ledger_rows, "pass", &cache_key) {
            println!("# ============================================================");
            println!("# validate CACHE HIT for tree {tree}");
            println!("#   (commit {})", git_sha());
            println!(
                "#   passed {} (wall {}, CPU {}, {} {} executed)",
                hit.finished_at,
                human_duration(hit.real_seconds),
                human_duration(hit.cpu_seconds),
                hit.executed,
                hit.executed_unit
            );
            println!(
                "#   from a run of commit {} by {} -- use --ignore-cache to force a real run",
                hit.commit, hit.producer
            );
            println!("#   profile={} host={host} toolchain={toolchain}", plan.profile);
            println!("#   NO gates ran this invocation; reused a clean, commit-anchored passing");
            println!("#   record (nonzero executed count, satisfied gate coverage) from the");
            println!("#   run-ledger ({}).", ledger.display());
            println!("# ============================================================");
            let _ = std::fs::remove_dir_all(&tmp);
            let mut s = RunSummary::new(
                Verdict::CacheHit,
                0,
                &plan.profile,
                vec![
                    format!(
                        "reused the passing record from {} (commit {}, producer {}), keyed on tree {tree}",
                        hit.finished_at, hit.commit, hit.producer
                    ),
                    format!(
                        "that run recorded {} {} executed with satisfied gate coverage; \
                         --ignore-cache forces a real run",
                        hit.executed, hit.executed_unit
                    ),
                ],
            );
            s.ledger = Some(ledger.clone());
            return s;
        }
        // A prior genuine FAIL prevents the PASS cache lookup above from
        // succeeding. Note it and run so targeted requalification evidence can
        // be produced; a lucky sibling PASS cannot turn this invocation into a
        // zero-gate cache hit.
        if let Some(prev) = validate_history::cache_lookup(&ledger_rows, "fail", &cache_key) {
            eprintln!(
                "# validate: tree {tree} has a prior FAIL record ({}) on this host+toolchain; \
                 running anyway (a fail may be flaky/environmental). Only a PASS satisfies the \
                 landing predicate.",
                prev.finished_at
            );
        }
    }

    match run_timeout {
        Some(secs) => eprintln!(
            "validate: whole-run budget {secs}s across lanes and retries; in-flight nodes are cut and rows flushed on breach"
        ),
        None => eprintln!(
            "validate: WARNING: no whole-run budget (--run-timeout / HERMIT_VALIDATE_RUN_TIMEOUT_SECONDS); per-node caps do not bound cumulative wall time"
        ),
    }

    let cgroups: BoxedCgroups =
        match resolve_cgroups(args.allow_cgroup_failure, run_timeout, deadline_ns) {
            Ok(c) => {
                // Claim the re-entrancy marker HERE, not before resolve_cgroups.
                // On the default path resolve_cgroups re-execs into a transient
                // systemd scope and does not return, so the process that reaches
                // this line is the one that will actually drive the run -- and it is
                // the only one whose pid a nested payload should see. Claiming
                // earlier made the driver read its own boxing re-exec as a nested
                // invocation and refuse itself.
                validate_runtime::claim_active_marker();
                c
            }
            Err(code) => {
                return RunSummary::refused(
                    code,
                    &plan.profile,
                    "cgroup boxing (fail-closed)",
                    vec![
                        "two-level cgroup-v2 boxing could not be established; see the message above"
                            .into(),
                        "resource boxing is this tool's primary purpose — re-run with \
                         --allow-cgroup-failure to accept an UNBOXED run"
                            .into(),
                    ],
                )
            }
        };

    let commit = git_sha();
    let git_depth = match measure_git_depth(&commit) {
        Ok(depth) => depth,
        Err(error) => {
            return RunSummary::refused(
                2,
                &plan.profile,
                "git depth measurement",
                vec![
                    error,
                    "the schema requires a real git_depth; refusing instead of omitting it or inventing a value"
                        .into(),
                ],
            )
        }
    };
    match setup_durable_log(&root, &plan.profile, &commit) {
        Ok(d) => *durable_slot = Some(d),
        Err(code) => {
            return RunSummary::refused(
                code,
                &plan.profile,
                "durable-log setup",
                vec![
                    "a run with no durable receipt is a silent no-result; see the message above"
                        .into(),
                ],
            )
        }
    }
    // Safe: just assigned. Cloned so the summary and the ledger can both name it
    // without borrowing the live tee handle.
    let log_path = durable_slot.as_ref().map(|d| d.path.clone()).unwrap_or_default();
    // Now that the log exists, tell the holder record where it is, so a validate
    // REFUSED against this one can print a command to tail it. Gated on actually
    // holding the lock: a nested payload must never rewrite the outer run's
    // record, and an UNGUARDED run (lock unavailable) has no record to append to.
    if let Some(lock) = invocation_lock.as_mut() {
        validate_runtime::record_invocation_log_path(lock, &log_path);
    }
    let e2e_result_root =
        match configure_e2e_result_root(&root, &log_path, &tmp.join("e2e-build")) {
            Ok(path) => path,
            Err(message) => {
                eprintln!("validate: ERROR: {message}");
                return RunSummary::refused(
                    4,
                    &plan.profile,
                    "durable per-cell result setup",
                    vec![
                        message,
                        "a validate without retained per-cell rows cannot produce the compatibility table"
                            .into(),
                    ],
                );
            }
        };
    eprintln!("validate: per-cell results: {}", e2e_result_root.display());

    // ---- box-wide concurrency observation (validate.sh:1499) -----------------
    //
    // PORTED CORRECTED, NOT VERBATIM. The bash counted process-group EXISTENCE
    // (`ps -eo pgid=,args=` matching `validate\.sh`), so a parked stop-test
    // fixture counted identically to a 22-core validate. That is not a modelling
    // nicety: measured on this box 2026-08-07 the six live `validate.sh` process
    // groups were ALL orphaned fixtures at CPU/wall ~0.00, and the shipped ledger
    // carries `concurrent_validates` up to 20 as a result.
    //
    // Here a peer must clear two observable bars: it REGISTERED itself as a
    // top-level driver (so nested payloads and fixtures are excluded by
    // construction, not by filtering), its registration flock is still held (so
    // liveness is the kernel's answer, not a pid guess), and its process tree
    // BURNED CPU between two samples. A running peak is kept for the whole run
    // because a point-in-time probe misses a peer that starts and ends in the
    // middle.
    let registry = validate_runtime::registry_dir(parent.as_deref());
    let run_record = if nesting.nested {
        None
    } else {
        validate_runtime::register_run(&registry, &plan.profile, &root)
    };
    let monitor = if nesting.nested {
        None
    } else {
        Some(validate_runtime::ConcurrencyMonitor::start(
            registry.clone(),
            std::time::Duration::from_secs(2),
        ))
    };
    if nesting.nested {
        println!(
            "Nested validate (payload of outer pid {}): focused mode {} only; the outer run owns \
             the ledger, receipt, cache, invocation lock and concurrency accounting.",
            nesting.outer_pid.unwrap_or(-1),
            plan.profile
        );
    }

    let jobs = args.jobs.unwrap_or_else(default_jobs);
    let started_at = utc_now();
    let started_epoch = epoch_now();
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let node_count = plan.cfg.steps.len() + plan.second.as_ref().map(|c| c.steps.len()).unwrap_or(0);
    // Every node the profile PLANNED, including the ones withheld as
    // host-inapplicable. The ledger's `unaccounted_nodes` is computed against
    // this set, so a node that neither ran nor carries a recorded reason is
    // named rather than lost.
    let planned_tags: BTreeSet<String> = std::iter::once(&plan.cfg)
        .chain(plan.second.iter())
        .flat_map(|cfg| cfg.steps.iter().map(|s| s.tag()))
        .chain(plan.host_inapplicable.iter().map(|n| n.tag.clone()))
        .collect();

    println!("Validation profile: {} (selection: {})", plan.profile, plan.selection_mode);
    println!("Commit: {commit} ({})", if tree_dirty() { "⚠️  NOT commit-anchored: dirty tree" } else { "clean tree, commit-anchored" });
    println!("Build cache: {cache}; host cores: {host_cpus}; scheduler width: -j {jobs}");
    println!(
        "Plan: {node_count} boxed DAG node(s){}{}",
        if plan.second.is_some() { " across 2 sequential lanes" } else { "" },
        if plan.host_inapplicable.is_empty() {
            String::new()
        } else {
            format!(
                "; {} planned node(s) withheld as host-inapplicable and NOT counted as passing: {}",
                plan.host_inapplicable.len(),
                plan.host_inapplicable
                    .iter()
                    .map(|n| n.tag.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    );
    // A measured estimate from THIS machine's own history, or an honest "not
    // enough history" (validate.sh:936). Printed after the durable log is
    // established so the receipt carries the prediction next to the outcome.
    println!(
        "Estimated time: {}",
        validate_history::history_estimate(&ledger_rows, &plan.profile, cache, &host, ledger.exists())
    );
    if plan.super_mode {
        println!(
            "Super stress: {} repetitions/probe scheduled as individual boxed nodes at -j {jobs} \
             ({host_cpus} online CPUs)",
            validate_super::repetitions()
        );
    }

    // Level 1 is deliberately O(1) per step. The runner still captures every
    // byte and prints COMPLETE detail on failure; only passing chatter is
    // suppressed. Levels 2-4 stream tagged step output, while level 5 adds the
    // deepest observed test identity to every streamed line.
    let verbosity = args.verbosity;
    // The envelope profile is a MEASUREMENT: an eager exit on the first probe
    // failure would truncate the very vector it exists to produce.
    let keep_going = args.keep_going || plan.force_keep_going;

    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut attempts: Vec<NodeAttempt> = Vec::new();
    let mut ok = true;
    let mut execution_complete = true;
    let mut env_retries = 0usize;

    // Read once per invocation, so both lanes apply the same policy and the run
    // can name the registry it used. An unreachable registry is an empty map,
    // which widens nothing.
    let unstable = validate_runtime::measured_unstable_nodes(parent.as_deref());
    if !unstable.is_empty() {
        println!(
            "Retry-eligible on measured instability: {} node(s) — {}",
            unstable.len(),
            unstable
                .iter()
                .map(|(node, sample)| format!("{node} ({sample})"))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    // One clock for the whole invocation. Sequential lanes and retries spend
    // from the same allowance rather than each receiving a fresh 600 seconds.
    let deadline = deadline_ns;
    let lane = |cfg: &DagConfig| -> LaneResult {
        run_lane_with_env_retries(
            cfg,
            jobs,
            keep_going,
            verbosity,
            cgroups.clone(),
            &log_path,
            deadline,
            validate_runtime::lane_round_backstop(cfg.steps.len()),
            &unstable,
            true,
        )
    };
    let mut run_timed_out = false;

    let r = lane(&plan.cfg);
    outcomes.extend(r.outcomes.iter().cloned());
    skipped.extend(r.skipped.iter().cloned());
    attempts.extend(r.attempts.iter().cloned());
    ok = ok && r.ok;
    execution_complete = execution_complete && r.complete;
    env_retries += r.env_retries;
    run_timed_out = run_timed_out || r.run_timed_out;

    if let Some(second) = &plan.second {
        // Sequential lanes are separate fail-fast families. A failure in the first lane must not
        // suppress the second: the lane runner still cancels failed-family peers and skips true
        // dependents, while the second lane records its own real outcomes.
        let r2 = lane(second);
        outcomes.extend(r2.outcomes.iter().cloned());
        skipped.extend(r2.skipped.iter().cloned());
        attempts.extend(r2.attempts.iter().cloned());
        ok = ok && r2.ok;
        execution_complete = execution_complete && r2.complete;
        env_retries += r2.env_retries;
        run_timed_out = run_timed_out || r2.run_timed_out;
    }

    let wall = (epoch_now() - started_epoch) as f64;
    if run_timed_out {
        println!(
            "⏱ VALIDATE RUN BUDGET EXCEEDED after {wall:.0}s (budget {}s): remaining work was \
             cut so its node identities and rows could still be reported. This is an incomplete \
             judgement, not a product verdict.",
            run_timeout.unwrap_or(0)
        );
    }
    print_cost_table(&outcomes, &skipped, &plan.host_inapplicable);
    print_retry_ledger(&attempts);

    // ---- the single cleanup / evidence-commit point (validate.sh:1812) -------
    //
    // From here to the ledger append is ONE critical section. A second stop
    // signal must not abort it between teardown and the append, or a run that did
    // real work would leave no record of having run at all — which reads exactly
    // like never having started. `SIG_IGN` for the window is what `trap ''
    // INT TERM HUP` bought the bash.
    validate_runtime::enter_cleanup_critical_section();
    let interruption = interrupted_by().map(|s| s.to_string());
    let series_error = if nesting.nested {
        None
    } else {
        match append_validate_series(parent.as_deref(), &root, &e2e_result_root, &commit) {
            Ok(_) => None,
            Err(error) => {
                eprintln!("validate: ERROR: completed cell results were not added to the series: {error}");
                Some(error)
            }
        }
    };
    // Stop the monitor and take the peak ONCE, here, so the ledger and the
    // summary cannot disagree about how crowded the box was.
    let (peak_active, peak_live) = match &monitor {
        Some(m) => {
            let (a, l) = m.finish();
            (Some(a as i64), Some(l as i64))
        }
        None => (None, None),
    };
    // Whole-run CPU, taken once in THIS process (a worker thread would see only
    // its own accounting, exactly as a bash subshell's `times` would).
    let (cpu_user, cpu_sys) = validate_runtime::process_cpu_seconds();
    let (executed_tests, filtered_tests) = libtest_counts(&outcomes);
    if executed_tests.is_none() {
        eprintln!(
            "validate: WARNING: libtest counts are UNKNOWN for this run. A ledger row with \
             executed_tests=null is a NON-VERDICT, not a green: no downstream completeness \
             predicate can qualify it."
        );
    }

    // Observed, not inferred: did the pin gate actually run and pass in THIS run?
    let pin_gate_passed = outcomes.iter().any(|o| o.tag == PIN_GATE_TAG && o.ok);
    // Only a top-level run writes a ledger row or publishes a receipt. Building
    // those records for a nested payload had no consumer, yet repeated the
    // receipt finalizer, git inspection, and `ci-hub validate-lock
    // authority-status` after all 193 compatibility nodes had finished. The
    // required authority admission BEFORE product work remains unchanged above;
    // its self-test explicitly requires the same exact authority for nested
    // payloads and rejects every weakened identity claim.
    let (ctx, coverage, dirty_now, commit_anchored) = if nesting.nested {
        (None, serde_json::Value::Null, false, false)
    } else {
        // The parent remains the plan/coverage authority. Correct only the one
        // case it cannot see in a failed nextest log: a planned node it
        // classified as zero whose typed final outcome carries a positive
        // executed-test count.
        let receipt = receipt_evidence(parent.as_deref(), &root, &log_path, &commit);
        let coverage = correct_test_node_coverage(
            receipt.coverage.clone(),
            &plan.planned_test_nodes,
            &outcomes,
        );
        let behind_ahead = sh(
            "git",
            &["rev-list", "--left-right", "--count", "origin/main...HEAD"],
        )
        .unwrap_or_else(|| "0 0".into());
        let mut ba = behind_ahead.split_whitespace();
        let git_behind: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let git_ahead: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let dirty_now = tree_dirty();
        let commit_anchored = commit != "unknown" && !dirty_now;
        let lock_admitted =
            canonical_validate_lock_admission(parent.as_deref(), &commit, &host).is_ok();
        let ctx = LedgerCtx {
            started_at,
            host: host.clone(),
            toolchain: toolchain.clone(),
            slot: slot_name(&root, parent.as_deref()),
            cwd: root.to_string_lossy().into(),
            profile: plan.profile.clone(),
            selection_mode: plan.selection_mode.into(),
            cache_state: cache.into(),
            commit: commit.clone(),
            tree: git_tree(),
            git_depth,
            git_ahead,
            git_behind,
            commit_anchored,
            tree_dirty: dirty_now,
            dag_jobs: jobs,
            admission: lock_admitted.then_some("ci-hub-validate-lock"),
            base_sha: receipt.base_sha,
            base_tree: receipt.base_tree,
            reverie_base_sha: receipt.reverie_base_sha,
            reverie_base_tree: receipt.reverie_base_tree,
            reverie_pin_current: pin_gate_passed,
            concurrent_validates: peak_active,
            concurrency_proof: if lock_admitted {
                Some(if peak_active.unwrap_or(0) == 0 {
                    "validate_lock_owner_ancestry"
                } else {
                    "validate_lock_owner_ancestry+live_flock_registry_cpu_delta"
                })
            } else {
                peak_active.map(|_| "live_flock_registry_cpu_delta")
            },
            interruption: interruption.clone(),
            cpu_user,
            cpu_sys,
            env_block_retries: env_retries as i64,
            executed_tests,
            filtered_tests,
        };
        (Some(ctx), coverage, dirty_now, commit_anchored)
    };
    if let (Some(a), Some(l)) = (peak_active, peak_live) {
        println!(
            "Peer validates: {a} peak CPU-active of {l} peak live top-level run(s) registered in \
             {} (existence alone is not concurrency; each peer had to hold its own flock AND burn \
             CPU between two samples).",
            registry.display()
        );
    }

    // An operator stop is a NO-RESULT, and it is RECORDED as one. It is not
    // silently dropped: `scripts/test_validate_stop_paths.py` is the durable
    // consumer contract for exactly this row (result `no_result`, raw_result
    // `fail`, interruption_signal named), and every reader already knows the
    // no_result verdict. A TIMEOUT, by contrast, is a completed run and falls
    // through to the normal verdict below.
    if let Some(sig) = &interruption {
        if !nesting.nested {
            write_ledger(
                &ledger,
                ctx.as_ref().expect("top-level run constructed ledger context"),
                &outcomes,
                &attempts,
                &skipped,
                &plan.host_inapplicable,
                &planned_tags,
                wall,
                130,
                &log_path.to_string_lossy(),
                false,
                coverage.clone(),
                None,
            );
        }
        drop(run_record);
        let _ = std::fs::remove_dir_all(&tmp);
        let mut detail = vec![
            format!("stopped by SIG{sig}; recorded as a NO-RESULT, not a failure"),
            "an interrupt learned nothing about the tree, so it does not establish a product \
             verdict — a TIMEOUT, by contrast, does"
                .into(),
        ];
        if let Some(error) = &series_error {
            detail.push(format!(
                "completed cell results could not be added to the series: {error}"
            ));
        }
        let mut s = RunSummary::new(
            Verdict::Interrupted,
            130,
            &plan.profile,
            detail,
        );
        s.nodes_executed = outcomes.len();
        s.nodes_failed = outcomes.iter().filter(|o| outcome_is_failure(o)).count();
        s.nodes_skipped = skipped.len();
        s.nodes_host_inapplicable = plan.host_inapplicable.len();
        s.wall_s = Some(wall);
        s.jobs = Some(jobs);
        s.log = Some(log_path);
        s.cpu_wall = Some((wall, cpu_user, cpu_sys));
        if !nesting.nested {
            s.ledger = Some(ledger);
        }
        return s;
    }

    // Compatibility ratchet, evaluated from typed outcomes.
    let mut compat_blocking = 0usize;
    // Carried to the verdict: a compat profile that measured nothing must not be
    // able to reach PASS through an empty set of failing rows.
    let mut compat_measured: Option<usize> = None;
    if let Some(mode) = plan.compat {
        let (passed, measured, blocking) = print_compat_summary(mode, &outcomes);
        compat_blocking = blocking.len();
        compat_measured = Some(measured);
        let floor = match mode {
            CompatMode::Sabre => Some(validate_corpus::SABRE_COMPAT_EXPECTED),
            CompatMode::Rr => Some(validate_corpus::RR_COMPAT_EXPECTED),
            _ => None,
        };
        if let Some(f) = floor {
            if passed < f {
                println!("❌ {} ratchet: {passed}/{measured} passing, floor {f} — BELOW FLOOR", mode.display_name());
                ok = false;
            } else {
                println!("✅ {} ratchet: {passed}/{measured} passing, floor {f} — met", mode.display_name());
            }
        }
        if !blocking.is_empty() {
            println!("❌ {} blocking failures ({}): {}", mode.display_name(), blocking.len(), blocking.join(", "));
        }
    }

    // Super stress pass rates, from typed outcomes rather than a scraped report.
    let mut super_blocking = 0usize;
    if plan.super_mode {
        let reps = validate_super::repetitions();
        let rates = validate_super::stress_rates(&outcomes, reps);
        super_blocking = validate_super::stress_verdict(&rates, reps, jobs, host_cpus);
    }

    // Working-envelope vector: score, emit JSON, print the human summary, and
    // enforce monotonicity when a baseline was supplied.
    let mut envelope_regressed = false;
    let mut envelope_error: Option<(u8, String)> = None;
    if let Some(env) = &plan.envelope {
        let short = sh("git", &["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
        let vector = validate_envelope::score(&outcomes, env.reps, &short);
        let json_file = validate_envelope::json_path(&root);
        let text = validate_envelope::to_ordered_json(&vector);
        if let Err(e) = std::fs::write(&json_file, format!("{text}\n")) {
            eprintln!("validate: warning: cannot write {}: {e}", json_file.display());
        }
        validate_envelope::print_summary(&vector, env.reps, &json_file);
        if let Some(baseline) = &env.baseline {
            match validate_envelope::compare(&vector, baseline) {
                Ok(reg) => envelope_regressed = reg,
                Err((code, msg)) => {
                    eprintln!("{msg}");
                    envelope_error = Some((code, msg));
                }
            }
        }
    }

    let failures = outcomes.iter().filter(|o| outcome_is_failure(o)).count();
    let no_result_nodes: Vec<&str> = outcomes
        .iter()
        .filter(|o| outcome_is_no_result(o))
        .map(|o| o.tag.as_str())
        .collect();
    let no_results = no_result_nodes.len();
    // The verdict is the RATCHET, not the raw node count.
    //
    // Three profiles deliberately have a verdict narrower than "every node
    // passed", and each states which rows it excluded and why:
    //   * compat — known fail-closed rows and bounded portable diagnostics are
    //     nonblocking by policy;
    //   * super — the KVM/DBI stress rows were unreachable in validate.sh, so
    //     their first measurement is reported rather than ratcheted;
    //   * envelope — it is a measurement, so probe failures lower a count and
    //     only the build/preflight spine can fail it.
    let blocking_failures = outcomes
        .iter()
        .filter(|o| outcome_is_failure(o) && !plan.nonblocking.contains(&o.tag))
        .count();
    // Failures OUTSIDE the measured matrix: the build/prep/gate spine. `compat.*`
    // rows are excluded because the compat ratchet already judges them (and
    // excuses the known-fail-closed ones), so counting them here would both
    // double-count and re-block rows policy has excused. Everything else — a
    // failed `compatprep.*`, `pre.*`, `gate.*`, `build.*` — is a node whose
    // failure can EMPTY the matrix, and no matrix ratchet can speak to that.
    let structural_failures = outcomes
        .iter()
        .filter(|o| {
            outcome_is_failure(o)
                && !o.tag.starts_with("compat.")
                && !plan.nonblocking.contains(&o.tag)
        })
        .count();
    let effective_failures = if plan.compat.is_some() {
        compat_blocking + structural_failures
    } else if plan.super_mode {
        blocking_failures + super_blocking
    } else {
        blocking_failures
    };
    // `ok` from the runner reflects every node, including the nonblocking ones,
    // so it is only authoritative when nothing is excused. A known exit 75
    // fully explains why the runner returned non-ok; any other unexplained
    // non-ok state remains a failure.
    let unexplained_runner_failure =
        plan.nonblocking.is_empty() && plan.compat.is_none() && !ok && no_results == 0;
    let mut exit_code = completed_exit_code(
        effective_failures,
        no_results,
        run_timed_out,
        unexplained_runner_failure,
    );
    if envelope_regressed {
        exit_code = 1;
    }
    if let Some((code, _)) = &envelope_error {
        exit_code = *code;
    }
    if series_error.is_some() {
        exit_code = 1;
    }
    if !execution_complete {
        eprintln!(
            "validate: ERROR: not every required node completed with a non-aborted outcome; \
             dependency-skipped, aborted, timed-out, and unreported work makes validation \
             incomplete and cannot report PASS."
        );
    }
    exit_code = exit_code_with_execution_completeness(exit_code, execution_complete);

    // Completeness is not the ratchet's to decide. A ratchet narrows WHICH
    // measured rows may fail; it cannot answer whether anything was measured, so
    // these conditions are checked separately and named individually.
    let refusals = verdict_refusals(compat_measured, structural_failures, executed_tests);
    if exit_code == 0 && !refusals.is_empty() {
        for why in &refusals {
            eprintln!("validate: ERROR: {why}");
        }
        eprintln!(
            "validate: refusing to report PASS: the run did not measure enough to certify \
             anything."
        );
        exit_code = 1;
    }

    // Receipt production is itself an enforcement path (validate.sh:1846).
    //
    // Every profile plans `pre.reverie_pin` and every lane node depends on it, so
    // in principle a green cannot happen without it. This asserts that anyway: if
    // a future fast path, cache branch, or early return ever bypasses the pin
    // gate, it must not emit PASS merely because the tests it did select happened
    // to pass. The archival pin is not a testing exemption, and "the DAG makes it
    // impossible" is a structural argument, not an observation of this run.
    let mut pin_gate_bypassed = false;
    if exit_code == 0 && !pin_gate_passed {
        eprintln!(
            "validate: ERROR: this path produced a PASS without a passing {PIN_GATE_TAG} gate; \
             refusing a passing receipt."
        );
        exit_code = 1;
        pin_gate_bypassed = true;
    }

    // A full top-level run must carry the exact per-cell population it just
    // judged. Older schema-5 rows could say only that buckets passed; they could
    // not open or satisfy a cell-specific failure obligation. Retain the typed
    // rows before appending the ledger entry so schema 7 is emitted only when
    // the artifact has actually been published and bound by checksum.
    let should_retain_cells = plan.suite_complete || plan.cell_evidence_expected.is_some();
    let retained_cell_results = if !nesting.nested && should_retain_cells && execution_complete {
        let expected = match &plan.cell_evidence_expected {
            Some(expected) => Ok(expected.clone()),
            None => validate_cell_results::expected_plan(&root),
        };
        let result = expected.and_then(|expected| {
            validate_cell_results::retain(
                parent.as_deref().unwrap_or(&root),
                &e2e_result_root,
                &commit,
                &expected,
            )
        });
        match result {
            Ok(results) => Some(results),
            Err(error) => {
                eprintln!(
                    "validate: ERROR: cannot retain complete per-cell evidence: {error}; \
                     refusing a schema-7 receipt"
                );
                exit_code = 1;
                None
            }
        }
    } else {
        None
    };
    // `--only` deliberately drops build dependencies, so a fast 127 there is
    // useful evidence of an absent prerequisite. It is still a red result and
    // only a possibility: exit 127 can also mean a missing host tool or typo.
    let missing_artifact = possible_missing_artifact_nodes(plan.selection_mode, &outcomes);
    if !missing_artifact.is_empty() {
        eprintln!(
            "validate: NOTE: {} --only node(s) exited 127 (command not found) in under 5s: {}. \
             Because --only drops outside dependencies, this MAY mean a required build artifact \
             is absent; it remains a RED test/configuration failure. Build the named dependencies \
             and re-run, or inspect the node command for a missing tool or typo.",
            missing_artifact.len(),
            missing_artifact.join(", ")
        );
    }

    // A NESTED payload writes nothing: the outer run owns the ledger and the
    // receipt, and a second row for one logical run is exactly the duplication
    // the re-entrancy guard exists to prevent.
    if !nesting.nested {
        write_ledger(
            &ledger,
            ctx.as_ref().expect("top-level run constructed ledger context"),
            &outcomes,
            &attempts,
            &skipped,
            &plan.host_inapplicable,
            &planned_tags,
            wall,
            exit_code,
            &log_path.to_string_lossy(),
            plan.suite_complete && execution_complete,
            coverage,
            retained_cell_results.as_ref(),
        );
    }

    // Receipt publication, strictly AFTER the ledger append: `ci-hub
    // apply-local-label` re-derives the receipt FROM the ledger, so publishing
    // first would label the PR from the previous run's newest row. Non-fatal by
    // contract — the exit code is already decided above and nothing here can
    // change it (validate.sh:1735).
    match validate_receipt::eligible(
        exit_code,
        effective_failures,
        args.label_pr && !nesting.nested,
        commit_anchored,
        dirty_now,
        &plan.profile,
    ) {
        Ok(()) => {
            let _ = validate_receipt::publish();
        }
        Err(why) => {
            if args.verbosity >= 2 {
                eprintln!("validate: not publishing a receipt-backed label: {why}");
            }
        }
    }

    // Read the individual results before removing the disposable build root: a
    // caller may deliberately place E2E_RESULT_ROOT there. The scheduler is
    // finished, and `read_log_since_settled` flushes the live tee before reading.
    let failed_finally: BTreeSet<String> = outcomes
        .iter()
        .filter(|o| outcome_is_failure(o))
        .map(|o| o.tag.clone())
        .collect();
    let mut test_summary_errors = Vec::new();
    let mut test_observations = match read_log_since_settled(&log_path, 0) {
        Some(log) => nextest_test_observations(&log),
        None => {
            test_summary_errors.push(
                "individual nextest test ids could not be read from the durable log; failed DAG \
                 nodes are listed separately below rather than mislabeled as test ids"
                    .to_string(),
            );
            Vec::new()
        }
    };
    match e2e_test_observations(&e2e_result_root) {
        Ok(mut observations) => test_observations.append(&mut observations),
        Err(error) => test_summary_errors.push(format!(
            "individual E2E test ids could not be read from the per-cell results: {error}; \
             failed DAG nodes are listed separately below rather than mislabeled as test ids"
        )),
    }
    let test_summary = test_id_summary(test_observations, &attempts, &failed_finally);

    drop(run_record);
    let _ = std::fs::remove_dir_all(&tmp);

    // The completed-run summary. Names the excused rows explicitly, so a green
    // verdict that ignored some failures can never read as "everything passed".
    let mut detail = Vec::new();
    let excused = failures - blocking_failures;
    if exit_code == 0 {
        detail.push(format!("every blocking gate passed ({} node(s) ran)", outcomes.len()));
    } else if exit_code == NO_RESULT_EXIT_CODE as u8 {
        detail.push(format!(
            "{} gate(s) could not determine their condition: {}",
            no_results,
            no_result_nodes.join(", ")
        ));
    } else {
        let (_named, listing) = blocking_listing(&outcomes, &plan.nonblocking, effective_failures);
        detail.push(format!("{effective_failures} blocking failure(s){listing}"));
    }
    if exit_code != NO_RESULT_EXIT_CODE as u8 && no_results > 0 {
        detail.push(format!(
            "{} gate(s) reported NO_RESULT but did not hide the genuine failure(s): {}",
            no_results,
            no_result_nodes.join(", ")
        ));
    }
    if excused > 0 {
        detail.push(format!(
            "{excused} failing node(s) were NONBLOCKING by policy and excluded from the verdict \
             (see the ratchet lines above for which and why)"
        ));
    }
    // A nonzero exit that came from the envelope comparison rather than from a
    // gate must SAY so: "0 blocking failure(s)" beside exit 2 is unreadable.
    if envelope_regressed {
        detail.push(
            "the working-envelope vector REGRESSED below its baseline (see the monotonicity \
             table above); no gate failed"
                .into(),
        );
    }
    if let Some((_, msg)) = &envelope_error {
        detail.push(format!("envelope comparison could not run: {msg}"));
    }
    if !timed_out_nodes(&outcomes).is_empty() {
        detail.push(format!(
            "{} node(s) hit a wall or CPU budget; a timeout IS a recorded result: {}",
            timed_out_nodes(&outcomes).len(),
            timed_out_nodes(&outcomes).join(", ")
        ));
    }
    if !skipped.is_empty() {
        detail.push(format!("{} node(s) never ran because a dependency failed", skipped.len()));
    }
    if !execution_complete {
        detail.push(
            "not every required node completed with a non-aborted outcome; dependency-skipped, \
             aborted, timed-out, or unreported work made the run incomplete"
                .into(),
        );
    }
    // Named in the verdict itself, not only in the plan header. A green summary
    // that omitted this would let a reader take the run for full coverage.
    if !plan.host_inapplicable.is_empty() {
        detail.push(format!(
            "{} planned node(s) were NOT RUN because this machine provably cannot run them, and \
             are recorded as '{}': {}. This is NOT a pass and NOT coverage — whatever those nodes \
             verify is UNVERIFIED by this run, and the ledger row carries the omission so the \
             parent's receipt gate can refuse it.",
            plan.host_inapplicable.len(),
            validate_plan::HOST_INAPPLICABLE_REASON,
            plan.host_inapplicable
                .iter()
                .map(|n| format!("{} (needs {})", n.tag, n.capability.value()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for why in &refusals {
        detail.push(format!("REFUSED ON COMPLETENESS: {why}"));
    }
    if pin_gate_bypassed {
        detail.push(
            "this path reached a PASS without a passing pre.reverie_pin gate; the receipt was \
             REFUSED and the verdict forced to fail (the archival pin is not a testing exemption)"
                .into(),
        );
    }
    if env_retries > 0 {
        detail.push(format!(
            "{env_retries} retry round(s) were spent on retry-eligible failures; the per-attempt \
             ledger says whether each environmental classification was confirmed, refuted, or \
             left unconfirmed. This verdict did NOT pass on the first attempt"
        ));
    }
    match executed_tests {
        Some(n) => detail.push(format!(
            "{n} test(s) executed, {} filtered (aggregated from typed step outcomes)",
            filtered_tests.map(|f| f.to_string()).unwrap_or_else(|| "unknown".into())
        )),
        None => detail.push(
            "executed_tests is UNKNOWN — this row is a NON-VERDICT and cannot qualify a receipt, \
             whatever the exit code says"
                .into(),
        ),
    }
    detail.extend(test_summary_errors);

    let mut s = RunSummary::new(
        match exit_code {
            0 => Verdict::Pass,
            code if code == NO_RESULT_EXIT_CODE as u8 => Verdict::NoResult,
            _ => Verdict::Fail,
        },
        exit_code,
        &plan.profile,
        detail,
    );
    s.nodes_executed = outcomes.len();
    s.nodes_failed = failures;
    s.flaky = test_summary.recovered;
    s.failed_ids = test_summary.failed;
    s.failed_nodes_without_test_ids = test_summary.failed_nodes_without_test_ids;
    s.retry_occurrences = test_summary.retry_occurrences;
    s.nodes_skipped = skipped.len();
    s.nodes_host_inapplicable = plan.host_inapplicable.len();
    s.wall_s = Some(wall);
    s.jobs = Some(jobs);
    s.log = Some(log_path);
    s.cpu_wall = Some((wall, cpu_user, cpu_sys));
    if !nesting.nested {
        s.ledger = Some(ledger);
    }
    s
}

// ------------------------------------------------------------- stop-path seam

/// The `HERMIT_VALIDATE_STOP_TEST_MODE` fixture (validate.sh:1899).
///
/// It exercises this driver's REAL stop handlers and REAL ledger writer without
/// starting a product build, which is the only way to test the signal paths in
/// bounded time. It cannot produce a pass: it records two synthetic gates and
/// then waits to be stopped. `scripts/test_validate_stop_paths.py` is its
/// consumer and asserts the exact row shape produced here.
///
/// # The leak this closes
///
/// The fixture parks until its parent test signals it, and the test spawns it
/// with `start_new_session=True` — so if the test dies first (an assertion before
/// the signal, a `wait` timeout, or the agent being recycled) nothing ever
/// signals it, and nothing in its new session can. Measured on this box
/// 2026-08-07: six orphaned `validate.sh full` process groups, all `ppid=1`, ages
/// 2h20m to 4h30m, each parked in `sleep 1` at CPU/wall ~0.00. Two exits now make
/// that unrepresentable — orphan detection (`getppid() == 1`) and a lifetime
/// deadline — and the Python harness additionally tears its own child's process
/// group down in a `finally`.
fn stop_test_seam(root: &Path, profile: &str, parent: Option<&Path>) -> RunSummary {
    let started_at = utc_now();
    let started = std::time::Instant::now();
    let prior_failure = env_flag("VALIDATE_STOP_TEST_PRIOR_FAILURE", "1");
    let synth = |name: &str, ok: bool| StepOutcome {
        tag: name.to_string(),
        ok,
        duration_s: 0.0,
        summary: String::new(),
        executed_tests: None,
        filtered_tests: None,
        returncode: Some(if ok { 0 } else { 1 }),
        reason: if ok { String::new() } else { "stop-test synthetic failure".into() },
        aborted: false,
    };
    let outcomes =
        vec![synth("stop-test completed gate 1", !prior_failure), synth("stop-test completed gate 2", true)];

    let commit = git_sha();
    let git_depth = match measure_git_depth(&commit) {
        Ok(depth) => depth,
        Err(error) => {
            return RunSummary::refused(
                2,
                profile,
                "git depth measurement",
                vec![
                    error,
                    "the schema requires a real git_depth; refusing instead of omitting it or inventing a value"
                        .into(),
                ],
            )
        }
    };

    validate_runtime::stop_test_announce();
    let exit = validate_runtime::stop_test_park(interrupted_by);

    // Cleanup is the evidence-commit point: make it signal-atomic BEFORE the
    // readiness hook fires, because the cleanup-race case then hammers this
    // process with SIGTERM and must not be able to abort the single append.
    validate_runtime::enter_cleanup_critical_section();
    validate_runtime::stop_test_cleanup_hook();

    let interruption = match exit {
        validate_runtime::StopTestExit::Signalled => interrupted_by().map(|s| s.to_string()),
        _ => None,
    };
    let exit_code: u8 = if interruption.is_some() { 130 } else { 1 };
    let (cpu_user, cpu_sys) = validate_runtime::process_cpu_seconds();
    let wall = started.elapsed().as_secs_f64();
    let ledger = ledger_path(root);
    let host = short_hostname();
    let lock_admitted = canonical_validate_lock_admission(parent, &commit, &host).is_ok();
    let ctx = LedgerCtx {
        started_at,
        host,
        toolchain: sh("rustc", &["--version"]).unwrap_or_else(|| "unknown".into()),
        slot: slot_name(root, parent),
        cwd: root.to_string_lossy().into(),
        profile: profile.to_string(),
        selection_mode: "full".into(),
        cache_state: cache_state(root).into(),
        commit,
        tree: git_tree(),
        git_depth,
        git_ahead: 0,
        git_behind: 0,
        commit_anchored: false,
        tree_dirty: tree_dirty(),
        dag_jobs: 0,
        admission: lock_admitted.then_some("ci-hub-validate-lock"),
        base_sha: serde_json::Value::Null,
        base_tree: serde_json::Value::Null,
        reverie_base_sha: serde_json::Value::Null,
        reverie_base_tree: serde_json::Value::Null,
        // The fixture runs no gates at all, so it never observed the pin gate.
        reverie_pin_current: false,
        // The fixture never registers as a top-level driver, so it can neither
        // observe peers nor be counted as one.
        concurrent_validates: lock_admitted.then_some(0),
        concurrency_proof: lock_admitted.then_some("validate_lock_owner_ancestry"),
        interruption: interruption.clone(),
        cpu_user,
        cpu_sys,
        env_block_retries: 0,
        executed_tests: None,
        filtered_tests: None,
    };
    // `suite_complete: false` — a fixture that ran two synthetic gates must never
    // publish a gates_expected obligation, which is what would make it look like
    // a completed full profile.
    // The fixture plans exactly the synthetic gates it ran, withholds nothing,
    // and leaves nothing unaccounted.
    let planned_tags: BTreeSet<String> = outcomes.iter().map(|o| o.tag.clone()).collect();
    write_ledger(
        &ledger,
        &ctx,
        &outcomes,
        &[],
        &[],
        &[],
        &planned_tags,
        wall,
        exit_code,
        "",
        false,
        serde_json::json!({}),
        None,
    );

    let detail = match exit {
        validate_runtime::StopTestExit::Signalled => vec![format!(
            "stop-path fixture: stopped by SIG{}; recorded as {}",
            interruption.clone().unwrap_or_default(),
            if prior_failure { "fail (a completed gate had already failed)" } else { "no_result" }
        )],
        validate_runtime::StopTestExit::EarlyExit => vec![
            "stop-path fixture: VALIDATE_STOP_TEST_EXIT_EARLY — an ordinary incomplete exit, NOT \
             an operator stop, so the row stays a raw fail with no interruption signal"
                .into(),
        ],
        validate_runtime::StopTestExit::Orphaned => vec![
            "stop-path fixture: ORPHANED (getppid()==1) — the test that spawned it died without \
             signalling, so it self-terminated instead of parking forever"
                .into(),
        ],
        validate_runtime::StopTestExit::Deadline => vec![
            "stop-path fixture: lifetime deadline expired (VALIDATE_STOP_TEST_MAX_SECONDS); \
             self-terminated rather than leaking a parked process group"
                .into(),
        ],
    };
    let mut s = RunSummary::new(
        if interruption.is_some() { Verdict::Interrupted } else { Verdict::Fail },
        exit_code,
        profile,
        detail,
    );
    s.nodes_executed = outcomes.len();
    s.nodes_failed = outcomes.iter().filter(|o| !o.ok).count();
    s.wall_s = Some(wall);
    s.cpu_wall = Some((wall, cpu_user, cpu_sys));
    s.ledger = Some(ledger);
    s
}

#[cfg(test)]
mod e2e_attempt_tests {
    use super::*;

    #[test]
    fn only_manifest_harness_steps_receive_the_retry_attempt() {
        let mut manifest = step_with_caps(
            "quick",
            "e2e_verify",
            "fixture",
            "target/debug/test-harness run --lane portable --ci-only".into(),
            Vec::new(),
            30,
            30,
            64 * 1024 * 1024,
        );
        set_manifest_attempt(&mut manifest, 2);
        assert_eq!(
            manifest
                .env
                .get(validate_plan::E2E_ATTEMPT_ENV)
                .map(String::as_str),
            Some("2")
        );

        let mut unrelated = step_with_caps(
            "test",
            "unit",
            "fixture",
            "cargo test".into(),
            Vec::new(),
            30,
            30,
            64 * 1024 * 1024,
        );
        set_manifest_attempt(&mut unrelated, 2);
        assert!(!unrelated.env.contains_key(validate_plan::E2E_ATTEMPT_ENV));
    }
}
