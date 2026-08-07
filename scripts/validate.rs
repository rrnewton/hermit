#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! validate.rs — Hermit's validation driver.
//!
//! This is THE driver. `validate.sh` is a five-line shim that `exec`s this file;
//! there is no second implementation. The shim exists only so that `validate.sh`
//! remains a valid entrypoint name at every commit, which is what lets `git
//! bisect`, `ci-hub`, and historical replay invoke one command across the
//! refactor boundary. A shim is a STABLE NAME, not a second version.
//!
//! # Contract
//!
//! * **Everything runs as a `safe-ci-dag-runner` node.** Preflight, the manifest
//!   gate, every CI-lane node, and every compatibility probe. The driver makes
//!   exactly one kind of call — `run_dag_boxed_ordered` — and never spawns a gate
//!   itself. See `lib/validate_plan.rs` for why that rule is load-bearing and for
//!   the measured evidence that an undeclared node is unboxed.
//! * **Boxing is fail-closed.** Default path re-execs into a transient
//!   `systemd --user` scope; if two-level cgroup-v2 boxing cannot be established
//!   the driver exits 3 rather than running unboxed.
//! * **Per-node output is live.** Verbosity is floored at 2 so the runner streams
//!   each node's `[tag]`-prefixed stdout/stderr as it happens. You should never be
//!   looking at a silent terminal wondering which node is running.
//! * **Every claim carries its conditions.** One ledger write point emits the
//!   profile, the executed/skipped/failed counts, commit anchoring, the tree hash,
//!   the toolchain, and the absolute durable log path together, so a downstream
//!   reader can never pair a bare `pass` with inferred coverage.
//! * **`HERMIT_DIR` is a USER-facing setting.** Validation never writes there.
//!   Run state goes to `target/validation/`, durable logs to `ignored/validate/`.
//!
//! # CLI
//!
//! The flag surface is `validate.sh`'s, verbatim, because the shim forwards `"$@"`
//! untouched and because in-tree callers already depend on it — notably
//! `ci/dag/portable.json`'s `test.strict_compat` node, which invokes
//! `./validate.sh --portable-strict-compat-only`, plus
//! `.github/workflows/validation-levels.yml`, three `Makefile` targets, and
//! `hermit-cli/tests/{analyze,rr_suite}.rs`. Changing the surface would have
//! required touching all of them in the same change.
//!
//! ```cargo
//! [dependencies]
//! safe-ci-dag-runner = { path = "../agent-utils/rs/safe-ci-dag-runner" }
//! serde_json = "1"
//! libc = "0.2"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

#[path = "lib/validate_corpus.rs"]
mod validate_corpus;

#[path = "lib/validate_plan.rs"]
mod validate_plan;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::sync::Arc;

use safe_ci_dag_runner::cgroup::install_scope_teardown;
use safe_ci_dag_runner::cgroup::is_in_scope;
use safe_ci_dag_runner::cgroup::reexec_in_scope;
use safe_ci_dag_runner::cgroup::CgroupManager;
use safe_ci_dag_runner::cgroup::Cgroups;
use safe_ci_dag_runner::model::DagConfig;
use safe_ci_dag_runner::model::StepOutcome;
use safe_ci_dag_runner::scheduler::run_dag_boxed_ordered;
use safe_ci_dag_runner::scheduler::BoxedCgroups;

use validate_plan::CompatMode;

/// Ledger schema this producer emits.
///
/// 4 matches what `validate.sh` wrote, field for field, so the parent aggregator,
/// `ci-hub/validate/*`, and the merge gate keep reading one schema across the
/// refactor. Deliberately NOT bumped as part of the port: changing the driver and
/// the record shape in one step would make a consumer regression indistinguishable
/// from a port regression.
const LEDGER_SCHEMA_VERSION: i64 = 4;

/// Recorded in each row so a version-aware reader can tell which driver produced
/// it without inference.
const LEDGER_PRODUCER: &str = "validate.rs";

const LEDGER_ENV: &str = "HERMIT_VALIDATE_LEDGER";
const PARENT_ENV: &str = "DEV_HERMIT_PARENT";

/// In-repo ledger directory. One append-only JSONL SHARD per (team, machine).
///
/// Sharding is what makes the ledger committable. A single shared file would
/// conflict on every concurrent append across machines; one file per
/// (team, short-machine) means each writer owns its own file, appends never
/// collide, and a reader UNIONS the shards locally. Rows are IMMUTABLE — a
/// correction appends a new row carrying `corrects: <record_id>` rather than
/// editing history, so the ledger stays append-only and auditable.
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
    Only { lane: String, nodes: String },
    Selective { shallow: bool },
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
            Focused::Only { lane, .. } => format!("only-{lane}"),
            Focused::Selective { .. } => "selective".into(),
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
            Focused::Only { .. } => "only",
            Focused::Selective { shallow } => {
                if *shallow {
                    "shallow-select"
                } else {
                    "selective"
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
    run_on_dirty_tree: bool,
    ignore_cache: bool,
    label_pr: bool,
    verbose: bool,
    jobs: Option<i64>,
    keep_going: bool,
    allow_cgroup_failure: bool,
    merge_lanes: bool,
    self_test: bool,
    show_plan: bool,
}

fn usage() -> &'static str {
    "Usage: ./validate.sh [LEVEL] [OPTIONS]        (validate.sh execs scripts/validate.rs)\n\
     \n\
     Run Hermit's local validation suite. Every gate executes as a boxed\n\
     safe-ci-dag-runner DAG node; nothing runs outside the runner.\n\
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
     \x20 --strict-compat-only          Run the blocking L2 app matrix.\n\
     \x20 --portable-strict-compat-only Portable L2 matrix with bounded diagnostics.\n\
     \x20 --rr-compat-only              Gate the known-passing record/replay matrix.\n\
     \x20 --sabre-compat-only           Gate the measured SaBRe matrix.\n\
     \x20 --e9patch-compat-only         Gate core + installed e9patch L2 apps.\n\
     \x20 --liteinst-compat-only        Run the portable CI liteinst_strict test.\n\
     \x20 --qemu-l2-only                Run the heavyweight QEMU L2 boot.\n\
     \x20 --portable-only               No PMU/CPUID hardware required.\n\
     \x20 --privileged-only             PMU/CPUID-dependent tests only.\n\
     \x20 --only <lane> <group.job>[,...]  Run ONE DAG shard (no deps).\n\
     \x20 --selective, --since-green    Only nodes affected since the last green baseline.\n\
     \x20 --shallow-select              Like --selective but pin the baseline to HEAD~1.\n\
     \x20 --baseline <sha>              Known-green baseline commit for --selective.\n\
     \x20 --all, --full-run             Assert the COMPLETE suite explicitly.\n\
     \n\
     Other options:\n\
     \x20 --verbose        Extra per-gate detail (per-node output is always streamed).\n\
     \x20 --run-on-dirty-tree  Escape hatch; AGENTS SHOULD NOT USE THIS.\n\
     \x20 --label-pr       Publish a receipt and label the PR after a full green (default).\n\
     \x20 --no-label-pr    Disable the non-fatal receipt publication and label update.\n\
     \x20 --ignore-cache   Force a real run even on a tree-keyed cache hit.\n\
     \x20 -j N             Scheduler width (default: host_cpus/8, floor 2, cap 16).\n\
     \x20 -k, --keep-going Do not eager-exit on the first failure.\n\
     \x20 --allow-cgroup-failure  Downgrade to an UNBOXED run instead of failing closed.\n\
     \x20 --merge-lanes    EXPERIMENT: fuse the portable and privileged lanes into one\n\
     \x20                  DAG so they overlap instead of running back to back.\n\
     \x20 --show-plan      Print the boxed DAG plan (nodes, caps, deps) and exit.\n\
     \x20 --self-test      Run the driver's inert policy/quoting brackets and exit.\n\
     \x20 -h, --help       Show this help and exit.\n\
     \n\
     Environment: VALIDATE_LEVEL, VALIDATE_LABEL_PR, VALIDATE_RUN_ON_DIRTY_TREE,\n\
     VALIDATE_IGNORE_CACHE, VALIDATE_VERBOSE, VALIDATE_FORCE_FULL, CI_DAG_JOBS,\n\
     HERMIT_VALIDATE_LEDGER, PR_NUMBER."
}

fn env_flag(name: &str, want: &str) -> bool {
    std::env::var(name).map(|v| v == want).unwrap_or(false)
}

fn parse_args() -> Result<Args, u8> {
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
    let mut args = Args {
        level,
        level_explicit,
        focused: None,
        force_full: env_flag("VALIDATE_FORCE_FULL", "1"),
        baseline: None,
        run_on_dirty_tree: env_flag("VALIDATE_RUN_ON_DIRTY_TREE", "1"),
        ignore_cache: env_flag("VALIDATE_IGNORE_CACHE", "1"),
        label_pr: !env_flag("VALIDATE_LABEL_PR", "0"),
        verbose: env_flag("VALIDATE_VERBOSE", "1"),
        jobs: None,
        keep_going: false,
        allow_cgroup_failure: false,
        merge_lanes: false,
        self_test: false,
        show_plan: false,
    };
    let mut shallow = false;
    let mut selective = false;
    let mut show_plan = false;

    let argv: Vec<String> = std::env::args().skip(1).collect();
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
            // Recognized but NOT ported. These are accepted so the failure is a
            // clear refusal rather than "unknown argument" — in-tree callers
            // (scripts/progress-report.sh, the progress-rubric skill) pass them,
            // and an unknown-argument exit would read as a CLI regression rather
            // than as the unported feature it is.
            "--envelope-only" => {
                eprintln!("validate: --envelope-only is not ported to the Rust driver yet.");
                eprintln!("  The working-envelope measurement still lives in validate.sh; this");
                eprintln!("  driver will not report a different measurement under its name.");
                return Err(2);
            }
            "--envelope-compare" => {
                eprintln!("validate: --envelope-compare is not ported to the Rust driver yet.");
                return Err(2);
            }
            "--show-plan" => show_plan = true,
            "--selective" | "--since-green" => selective = true,
            "--shallow-select" => {
                selective = true;
                shallow = true;
            }
            "--all" | "--full-run" => args.force_full = true,
            "--run-on-dirty-tree" => args.run_on_dirty_tree = true,
            "--ignore-cache" => args.ignore_cache = true,
            "--label-pr" => args.label_pr = true,
            "--no-label-pr" => args.label_pr = false,
            "--verbose" => args.verbose = true,
            "--merge-lanes" => args.merge_lanes = true,
            "--self-test" => args.self_test = true,
            "-k" | "--keep-going" => args.keep_going = true,
            "--allow-cgroup-failure" => args.allow_cgroup_failure = true,
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
                    eprintln!("          e.g. ./validate.sh --only portable test.sabre_examples");
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

/// Inert brackets for the policy predicate and the shell quoter.
///
/// These cannot launch a run or authorize a receipt — they only prove the
/// predicate refuses every non-qualifying case AND accepts the one qualifying
/// case, so it is not vacuously true. `validate.sh` ran the equivalent brackets
/// on every invocation (validate.sh:308); here they are a `--self-test` subcommand
/// so the cost is not paid on the hot path.
fn self_test() -> Result<(), String> {
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

/// Establish two-level cgroup-v2 boxing, mirroring the runner's own
/// `resolve_cgroups` policy. Returns the manager (`None` = intentional unboxed
/// run) or `Err(exit_code)`. On the default path this re-execs into a transient
/// `systemd --user` scope and does not return on success.
fn resolve_cgroups(allow_failure: bool) -> Result<BoxedCgroups, u8> {
    if is_in_scope() {
        let mgr = Cgroups::new();
        if mgr.enabled() {
            install_scope_teardown();
            eprintln!(
                "validate: cgroup boxing ACTIVE (two-level cgroup-v2 scope; per-step memory/CPU \
                 caps + setsid-proof teardown)."
            );
            return Ok(Some(Arc::new(mgr) as Arc<dyn CgroupManager>));
        }
        if allow_failure {
            eprintln!("validate: WARNING: per-step cgroup setup failed; running UNBOXED (--allow-cgroup-failure).");
            return Ok(None);
        }
        eprintln!(
            "validate: ERROR: inside a managed scope but per-step cgroups could not be set up; \
             re-run with --allow-cgroup-failure to run UNBOXED."
        );
        return Err(3);
    }
    if allow_failure {
        eprintln!(
            "validate: WARNING: cgroup boxing not established (--allow-cgroup-failure); running \
             UNBOXED (process-group teardown only, no per-step memory/CPU caps)."
        );
        return Ok(None);
    }
    let reexeced_or_skipped = reexec_in_scope(None, None);
    let detail = if reexeced_or_skipped {
        "boxing was skipped (e.g. CI without a systemd --user scope)"
    } else {
        "cgroup-v2 + a working systemd --user scope are unavailable"
    };
    eprintln!(
        "validate: ERROR: cgroup boxing could not be established: {detail}. Resource boxing is \
         this tool's primary purpose; re-run with --allow-cgroup-failure to run UNBOXED."
    );
    Err(3)
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
    let ts = utc_now().replace([':', '-'], "");
    dir.join(format!("validate-{profile}-{sha12}-{ts}.log"))
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

fn is_self_output(path: &str) -> bool {
    SELF_OUTPUT_PREFIXES.iter().any(|p| path.starts_with(p))
}

/// Entries from a git listing that are not validate's own output.
///
/// The callers emit two different shapes — `git status --porcelain` prefixes each
/// path with a two-character status plus a space, while `git diff --name-only`
/// and `git ls-files` emit a bare path. Testing BOTH forms is deliberate: an
/// earlier version stripped three characters unconditionally, which turned
/// `ci/validate-ledger/...` into `validate-ledger/...` for the bare-path callers,
/// failed the prefix match, and made validate refuse on its own ledger write.
fn foreign_porcelain(args: &[&str]) -> Vec<String> {
    let Some(out) = sh("git", args) else { return Vec::new() };
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            let bare = l.trim().trim_matches('"');
            let stripped = l.get(3..).unwrap_or(l).trim().trim_matches('"');
            !is_self_output(bare) && !is_self_output(stripped)
        })
        .map(|l| l.to_string())
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
         To validate a deliberately stale base anyway, pass --run-on-dirty-tree."
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
    planned_test_nodes: BTreeSet<String>,
    /// Set when this profile is a compatibility matrix, so the ratchet and the
    /// per-program summary are evaluated afterwards.
    compat: Option<CompatMode>,
    /// True only for a complete `full` plan, authorizing `gates_expected` to be
    /// derived from what ran (validate.sh:718).
    suite_complete: bool,
}

fn test_nodes_of(cfg: &DagConfig) -> BTreeSet<String> {
    cfg.steps
        .iter()
        .map(|s| s.tag())
        .filter(|t| t.starts_with("test.") || t.contains(":test."))
        .collect()
}

/// Build the execution plan for the selected level/mode.
fn build_plan(root: &Path, args: &Args, tmp: &Path) -> Result<Plan, String> {
    let with_proxy = has_cmd("with-proxy");
    let pre = validate_plan::preflight_nodes(with_proxy);
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
        // The corpus needs a release Hermit and the functional fixtures; both are
        // DAG nodes so they are boxed and timed like everything else.
        steps.push(build_release_hermit_node(gate, &hermit_bin));
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
            suite_complete: false,
        });
    }

    // Focused single-shard mode: run one already-built DAG shard, no deps.
    if let Some(Focused::Only { lane, nodes }) = &args.focused {
        let mut steps = pre;
        steps.push(shard_node(gate, lane, nodes));
        let cfg = validate_plan::config_from(steps, "single DAG shard");
        return Ok(Plan {
            planned_test_nodes: test_nodes_of(&cfg),
            cfg,
            second: None,
            profile: args.focused.as_ref().unwrap().profile(),
            selection_mode: "only",
            compat: None,
            suite_complete: false,
        });
    }

    // Focused liteinst matrix (validate.sh:4815): three ordered gates.
    if matches!(args.focused, Some(Focused::LiteinstCompat)) {
        let mut steps = pre;
        steps.push(step_with_caps("liteinst", "hermit_release", "Release Hermit for LiteInst compatibility",
            "cargo build --release --locked -p hermit --features third-party-backends".into(),
            vec![gate.to_string()], 1200, 3600, 16 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("liteinst", "runtime", "Release LiteInst runtime",
            "./scripts/stage-liteinst-runtime.sh release $PWD/target/release/libreverie_liteinst.so $PWD/target/liteinst-runtime-build".into(),
            vec!["liteinst.hermit_release".into()], 900, 1800, 8 * 1024 * 1024 * 1024));
        steps.push(step_with_caps("liteinst", "strict", "Portable CI liteinst_strict",
            "HERMIT_LITEINST_TEST_BINARY=$PWD/target/release/hermit cargo test -p hermit --features third-party-backends --test liteinst_advanced -- --test-threads=1".into(),
            vec!["liteinst.runtime".into()], 900, 1800, 8 * 1024 * 1024 * 1024));
        let cfg = validate_plan::config_from(steps, "liteinst compatibility");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: args.focused.as_ref().unwrap().profile(), selection_mode: "full",
            compat: None, suite_complete: false });
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
            compat: None, suite_complete: false });
    }

    // `quick` is NOT "the portable lane" — it is seven specific smoke gates
    // (validate.sh:4583). Mapping it onto a lane would run a different, much
    // larger thing under the same name.
    if args.level == Level::Quick && args.focused.is_none() {
        let hermit = "target/debug/hermit";
        let marker = "hermit-validation-smoke";
        let run_args = "run --base-env=minimal --no-virtualize-cpuid --max-timeslice=disabled";
        let mut steps = pre;
        let mut add = |job: &str, desc: &str, cmd: String, dep: &str, t: i64, mem: i64| {
            steps.push(step_with_caps("quick", job, desc, cmd, vec![dep.to_string()], t, t * 2, mem));
        };
        add("build", "Build workspace", "cargo build --workspace --features third-party-backends".into(), gate, 3600, 16 * 1024 * 1024 * 1024);
        add("e2e_metadata", "Portable E2E metadata", "./ci/test_harness.sh validate".into(), "quick.build", 600, 4 * 1024 * 1024 * 1024);
        add("e2e_verify", "Portable ptrace E2E verification", "./ci/test_harness.sh run --lane portable --mode verify --backend ptrace --ci-only".into(), "quick.build", 1800, 8 * 1024 * 1024 * 1024);
        add("detcore_unit", "Detcore core unit tests", "cargo test -p hermit-detcore --lib".into(), "quick.build", 1800, 8 * 1024 * 1024 * 1024);
        add("run_smoke", "Hermit run smoke test",
            format!("out=$(timeout 30s {hermit} {run_args} -- /bin/echo {marker}) && test \"$out\" = {marker}"),
            "quick.build", 120, 4 * 1024 * 1024 * 1024);
        add("verify_smoke", "Hermit verify-mode smoke test",
            format!("timeout 30s {hermit} {run_args} --verify -- /bin/echo {marker}"),
            "quick.build", 120, 4 * 1024 * 1024 * 1024);
        add("record_replay_smoke", "Hermit record/replay smoke test",
            format!("timeout 30s {hermit} record start --verify -- /bin/echo {marker}"),
            "quick.build", 180, 4 * 1024 * 1024 * 1024);
        let cfg = validate_plan::config_from(steps, "quick smoke suite");
        return Ok(Plan { planned_test_nodes: test_nodes_of(&cfg), cfg, second: None,
            profile: "quick".into(), selection_mode: "full", compat: None, suite_complete: false });
    }

    // REFUSE rather than silently substitute. `super` (the 20x stress repetition
    // suite) and the `--envelope-*` measurement modes are NOT ported yet. Falling
    // through to a lane would run something ELSE under the requested name and
    // report it as success — a wrong answer is worse than a refusal.
    if args.level == Level::Super && args.focused.is_none() {
        return Err(
            "the `super` stress suite is not ported to the Rust driver yet, and this driver will \
             not silently run a different profile in its place. Use validate.sh's super suite \
             until it lands (tracked in the PR as the remaining port work)."
                .into(),
        );
    }

    // Lane-based profiles.
    let lanes: Vec<&str> = match (&args.focused, args.level) {
        (Some(Focused::PrivilegedOnly), _) => vec!["privileged"],
        // --selective's documented FAIL-SAFE is to run the complete portable
        // lane on any doubt; node-level selection is not ported, so it always
        // takes the safe branch and says so, rather than quietly running fewer
        // tests than the selector proved safe to omit.
        (Some(Focused::Selective { .. }), _) => {
            eprintln!(
                "validate: node-level selection is not ported; running the FULL portable lane \
                 (--selective's documented fail-safe branch)."
            );
            vec!["portable"]
        }
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
        a.extend(validate_plan::lane_nodes(root, lanes[0], "", gate)?);
        let mut b = validate_plan::lane_nodes(root, lanes[1], "", gate)?;
        // The second run repeats preflight-free; its nodes hang off nothing.
        for s in b.iter_mut() {
            s.deps.retain(|d| d != gate);
        }
        let cfg_a = validate_plan::config_from(a, "portable lane");
        let cfg_b = validate_plan::config_from(b, "privileged lane");
        let mut planned = test_nodes_of(&cfg_a);
        planned.extend(test_nodes_of(&cfg_b));
        return Ok(Plan {
            cfg: cfg_a,
            second: Some(cfg_b),
            profile,
            selection_mode,
            planned_test_nodes: planned,
            compat: None,
            suite_complete: args.level == Level::Full && args.focused.is_none(),
        });
    }

    let mut steps = pre;
    for lane in &lanes {
        let prefix = if lanes.len() > 1 { format!("{lane}-") } else { String::new() };
        steps.extend(validate_plan::lane_nodes(root, lane, &prefix, gate)?);
    }
    // Fusing lanes can duplicate identical work (both lanes ship check.reverie_pin
    // and e2e.metadata with byte-identical commands). Drop the later duplicate and
    // repoint its dependents, so the fused DAG does not pay for the same node
    // twice — the dedup is recorded rather than silent.
    let removed = dedupe_identical(&mut steps);
    if !removed.is_empty() {
        eprintln!("validate: fused lanes; deduped {} identical node(s): {}", removed.len(), removed.join(", "));
    }
    let cfg = validate_plan::config_from(steps, "fused lanes");
    Ok(Plan {
        planned_test_nodes: test_nodes_of(&cfg),
        cfg,
        second: None,
        profile,
        selection_mode,
        compat: None,
        suite_complete: args.level == Level::Full && args.focused.is_none(),
    })
}

/// Remove later steps whose (job, cmd) exactly matches an earlier step's, and
/// repoint every dependency onto the survivor. Returns the removed tags.
fn dedupe_identical(steps: &mut Vec<safe_ci_dag_runner::model::Step>) -> Vec<String> {
    let mut seen: BTreeMap<(String, String), String> = BTreeMap::new();
    let mut remap: BTreeMap<String, String> = BTreeMap::new();
    let mut keep = Vec::with_capacity(steps.len());
    let mut removed = Vec::new();
    for s in steps.drain(..) {
        let key = (s.job.clone(), s.cmd.clone());
        match seen.get(&key) {
            Some(surv) => {
                remap.insert(s.tag(), surv.clone());
                removed.push(s.tag());
            }
            None => {
                seen.insert(key, s.tag());
                keep.push(s);
            }
        }
    }
    for s in keep.iter_mut() {
        for d in s.deps.iter_mut() {
            if let Some(t) = remap.get(d) {
                *d = t.clone();
            }
        }
        s.deps.sort();
        s.deps.dedup();
    }
    *steps = keep;
    removed
}

fn build_release_hermit_node(gate: &str, bin: &str) -> safe_ci_dag_runner::model::Step {
    let default = bin.ends_with("target/release/hermit");
    let cmd = if default {
        "cargo build --release -p hermit --features third-party-backends".to_string()
    } else {
        // A caller-supplied binary is reused rather than rebuilt, but it must
        // exist: silently proceeding with a missing binary would fail every row
        // for a reason that has nothing to do with compatibility.
        format!("test -x {}", validate_plan::shell_quote(bin))
    };
    step_with_caps("compatprep", "hermit_release", "Release Hermit for compatibility", cmd, vec![gate.to_string()], 3600, 7200, 16 * 1024 * 1024 * 1024)
}

fn prepare_fixtures_node(_tag: &str, fixtures: &Path) -> safe_ci_dag_runner::model::Step {
    step_with_caps(
        "compatprep",
        "fixtures",
        "Functional compatibility fixtures",
        format!(
            "./tests/compat/prepare_real_compat_fixtures.sh {}",
            validate_plan::shell_quote(&fixtures.to_string_lossy())
        ),
        vec!["compatprep.hermit_release".to_string()],
        900,
        900,
        4 * 1024 * 1024 * 1024,
    )
}

/// `require_e9patch_artifacts`' files-only NSS fixture (validate.sh:4095): keeps
/// host identity-daemon races out of the e9patch L2 measurement.
fn nsswitch_fixture_node(path: &Path) -> safe_ci_dag_runner::model::Step {
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

fn shard_node(gate: &str, lane: &str, nodes: &str) -> safe_ci_dag_runner::model::Step {
    step_with_caps(
        "shard",
        &validate_plan::sanitize_job(&format!("{lane}_{}", nodes.replace([',', '.'], "_"))),
        &format!("DAG shard {lane}:{nodes}"),
        format!(
            "./ci/run-node.sh {} {}",
            validate_plan::shell_quote(lane),
            validate_plan::shell_quote(nodes)
        ),
        vec![gate.to_string()],
        7200,
        7200,
        16 * 1024 * 1024 * 1024,
    )
}

fn step_with_caps(
    group: &str,
    job: &str,
    desc: &str,
    cmd: String,
    deps: Vec<String>,
    timeout: i64,
    cpu_timeout: i64,
    mem: i64,
) -> safe_ci_dag_runner::model::Step {
    safe_ci_dag_runner::model::Step {
        group: group.into(),
        job: job.into(),
        desc: desc.into(),
        description: String::new(),
        cmd,
        deps,
        env: BTreeMap::new(),
        hint: safe_ci_dag_runner::model::ResourceHint {
            rss_baseline_bytes: Some(mem),
            hard_mem_max_bytes: Some(mem),
            ..Default::default()
        },
        networkonly: false,
        engine_only: false,
        timeout,
        cpu_timeout,
        jobs_flag: None,
    }
}

// --------------------------------------------------------------------------- reporting

/// Per-node cost table, built entirely from typed `StepOutcome` fields.
fn print_cost_table(outcomes: &[StepOutcome], skipped: &[String]) {
    println!("\n=== per-node cost (safe-ci-dag-runner) ===");
    println!("{:<44} {:>9}  {:<8} {}", "node", "seconds", "status", "reason/returncode");
    println!("{}", "-".repeat(84));
    let mut total = 0.0_f64;
    for o in outcomes {
        total += o.duration_s;
        let status = if o.ok {
            "ok"
        } else if o.aborted {
            "ABORTED"
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
}

/// Per-program compatibility summary, built from typed node outcomes rather than
/// a scraped TSV. Reproduces `print_compatibility_summary`'s category table.
fn print_compat_summary(mode: CompatMode, outcomes: &[StepOutcome]) -> (usize, usize, Vec<String>) {
    let known = validate_corpus::known_failclosed();
    let diag = validate_corpus::portable_diagnostic();
    let mut per_cat: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut passed = 0usize;
    let mut measured = 0usize;
    let mut blocking_failures: Vec<String> = Vec::new();
    for o in outcomes {
        let Some(label) = o.tag.strip_prefix("compat.") else { continue };
        let cat = validate_corpus::category_of(label);
        let e = per_cat.entry(cat).or_insert((0, 0));
        e.1 += 1;
        measured += 1;
        if o.ok {
            e.0 += 1;
            passed += 1;
            if mode == CompatMode::Strict && known.contains_key(label) {
                println!("  WARN {label} unexpectedly passed fail-closed --strict; drop it from the known-failure table");
            }
        } else if mode == CompatMode::Strict && known.contains_key(label) {
            println!("  WARN {label} known fail-closed under --strict ({}; nonblocking)", known[label]);
        } else if mode == CompatMode::PortableStrict && diag.contains_key(label) {
            println!("  WARN {label} is a bounded portable diagnostic: {}", diag[label]);
        } else {
            blocking_failures.push(label.to_string());
        }
    }
    println!("\nCOMPATIBILITY SUMMARY ({measured} measured programs, mode {})", mode.assurance());
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

fn interrupted_by() -> Option<&'static str> {
    match INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst) {
        0 => None,
        libc::SIGINT => Some("SIGINT"),
        libc::SIGTERM => Some("SIGTERM"),
        libc::SIGHUP => Some("SIGHUP"),
        _ => Some("signal"),
    }
}

/// Nodes the runner reported as killed by their wall or CPU budget. The runner's
/// own `step_failure_reason` produces these strings, so this reads its typed
/// classification rather than re-deriving one.
fn timed_out_nodes(outcomes: &[StepOutcome]) -> Vec<String> {
    outcomes
        .iter()
        .filter(|o| {
            let r = o.reason.to_ascii_lowercase();
            r.contains("timeout") || r.contains("timed out")
        })
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
    git_ahead: i64,
    git_behind: i64,
    commit_anchored: bool,
    tree_dirty: bool,
    dag_jobs: i64,
}

/// Write one JSONL ledger record.
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
    skipped: &[String],
    wall_s: f64,
    exit_code: u8,
    log_file: &str,
    suite_complete: bool,
    coverage: serde_json::Value,
) {
    let gates_run = outcomes.len();
    let failures = outcomes.iter().filter(|o| !o.ok && !o.aborted).count();
    let result = if exit_code == 0 && failures == 0 { "pass" } else { "fail" };
    let timed_out = timed_out_nodes(outcomes);
    // Stable per-row identity. Corrections never edit a row; they append a new
    // one carrying `corrects: <this id>`, which is what keeps the shard
    // append-only and safe to union across machines.
    let record_id = format!("{}-{}-{}", ctx.host, epoch_now(), std::process::id());
    let gates_expected = if ctx.profile == "full" && suite_complete {
        serde_json::json!(gates_run)
    } else {
        serde_json::Value::Null
    };
    let gates: Vec<serde_json::Value> = outcomes
        .iter()
        .map(|o| {
            serde_json::json!({
                "name": o.tag,
                "result": if o.ok { "pass" } else { "fail" },
                "exit_code": o.returncode,
                "reason": o.reason,
                "aborted": o.aborted,
                "real_seconds": o.duration_s,
            })
        })
        .collect();
    let record = serde_json::json!({
        "schema_version": LEDGER_SCHEMA_VERSION,
        "producer": LEDGER_PRODUCER,
        // Immutable-row identity. `corrects` is null here; a correcting row
        // repeats this shape with `corrects` set to the id it supersedes.
        "record_id": record_id,
        "corrects": serde_json::Value::Null,
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
        "git_ahead": ctx.git_ahead,
        "git_behind": ctx.git_behind,
        "commit_anchored": ctx.commit_anchored,
        "tree_dirty": ctx.tree_dirty,
        "result": result,
        "raw_result": result,
        "exit_code": exit_code,
        "checks": gates_run,
        "failures": failures,
        "dag_jobs": ctx.dag_jobs,
        "gates_run": gates_run,
        "gates_expected": gates_expected,
        "skipped_nodes": skipped.len(),
        // A timeout is a RESULT, so it is recorded rather than dropped, and it is
        // named so a reader can separate "the tree is broken" from "a gate blew
        // its budget". Operator interrupts never reach this function at all.
        "timed_out_nodes": timed_out,
        // NODE counts, deliberately NOT named executed_tests/filtered_tests: a
        // schema<5 consumer keys is_clean_full_pass on those libtest-count names,
        // and a ~47-NODE DAG run must never be readable as a 47-TEST pass. The
        // counted receipt is minted by finalize_receipt.py --scan off the log.
        "executed_nodes": gates_run,
        "real_seconds": wall_s,
        "log_file": log_file,
        "coverage": coverage,
        "gates": gates,
    });
    if let Some(dir) = ledger.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("validate: warning: cannot create ledger dir {}: {e}", dir.display());
                return;
            }
        }
    }
    use std::io::Write;
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    match std::fs::OpenOptions::new().create(true).append(true).open(ledger) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!("validate: warning: cannot append ledger {}: {e}", ledger.display());
            } else {
                eprintln!("validate: ledger record appended to {}", ledger.display());
            }
        }
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

/// Resolve the ledger shard. Precedence:
///   1. `$HERMIT_VALIDATE_LEDGER` — explicit file (existing consumers rely on it).
///   2. `$DEV_HERMIT_PARENT/ignored/validate-run-ledger.jsonl` — the parent
///      workspace ledger `ci-hub` already aggregates.
///   3. The in-repo per-(team, machine) shard.
fn ledger_path(root: &Path) -> PathBuf {
    if let Ok(explicit) = std::env::var(LEDGER_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    if let Ok(parent) = std::env::var(PARENT_ENV) {
        if !parent.is_empty() {
            return PathBuf::from(parent).join("ignored").join("validate-run-ledger.jsonl");
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
    root.join(LEDGER_DIR)
        .join(format!("{}.{}.jsonl", sanitize(&team), sanitize(&short_hostname())))
}

// --------------------------------------------------------------------------- main

fn main() -> ExitCode {
    rust_script_prelude::init();
    install_stop_handlers();

    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return ExitCode::from(code),
    };

    if args.self_test {
        return match self_test() {
            Ok(()) => {
                println!("validate: self-test OK (force-full policy brackets, shell quoting, corpus counts)");
                ExitCode::from(0)
            }
            Err(e) => {
                eprintln!("validate: SELF-TEST FAILED: {e}");
                ExitCode::from(2)
            }
        };
    }

    let root = repo_root();
    if std::env::set_current_dir(&root).is_err() {
        eprintln!("validate: cannot cd to repo root {}", root.display());
        return ExitCode::from(2);
    }
    let parent = find_parent(&root);

    // Dirty-tree gate, BEFORE any state is created, so a refusal leaves nothing
    // behind. A result validated against uncommitted changes describes a tree
    // that exists nowhere in history and cannot be reproduced or compared.
    let wt_dirty = worktree_dirty();
    if wt_dirty && !args.run_on_dirty_tree {
        eprintln!("validate: refusing to run on a dirty working tree.");
        eprintln!("  HEAD {} has uncommitted working-tree changes, so a record anchored to it", git_sha());
        eprintln!("  would describe a tree that exists nowhere in history. Commit (preferred), or");
        eprintln!("  stage the WIP with 'git add', then re-run. To force an explicitly unanchored");
        eprintln!("  run pass --run-on-dirty-tree (agents must not).");
        let _ = Command::new("git").args(["status", "--short"]).status();
        return ExitCode::from(2);
    }

    // Rebase-freshness gate. Mechanically enforced, not advisory.
    match rebase_freshness(args.run_on_dirty_tree) {
        Ok(msg) => eprintln!("validate: {msg}"),
        Err(msg) => {
            eprintln!("validate: refusing to validate a stale base.\n  {msg}");
            return ExitCode::from(2);
        }
    }

    // Run state lives under target/, never under HERMIT_DIR (a user setting).
    let tmp = root.join("target/validation").join(format!("run-{}", std::process::id()));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!("validate: cannot create {}: {e}", tmp.display());
        return ExitCode::from(2);
    }

    let mut plan = match build_plan(&root, &args, &tmp) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("validate: cannot build the execution plan: {e}");
            return ExitCode::from(2);
        }
    };

    // Per-gate budget overrides, preserved from validate.sh
    // (VALIDATE_GATE_TIMEOUT_SECONDS / VALIDATE_GATE_CPU_TIMEOUT_SECONDS). These
    // LOWER a node's ceiling, never raise it: a caller tightening budgets to
    // reproduce a timeout must not accidentally loosen a node that already
    // declared something stricter. They are also how the timeout path is
    // exercised on demand without waiting for a real runaway.
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
        return ExitCode::from(3);
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
            println!("{:<40} {:>7} {:>7} {:>8}  {}", "node", "wall_s", "cpu_s", "mem", "deps");
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
        return ExitCode::from(0);
    }

    let cgroups: BoxedCgroups = match resolve_cgroups(args.allow_cgroup_failure) {
        Ok(c) => c,
        Err(code) => return ExitCode::from(code),
    };

    let commit = git_sha();
    let durable = match setup_durable_log(&root, &plan.profile, &commit) {
        Ok(d) => d,
        Err(code) => return ExitCode::from(code),
    };

    let jobs = args.jobs.unwrap_or_else(default_jobs);
    let started_at = utc_now();
    let started_epoch = epoch_now();
    let cache = cache_state(&root);
    let host_cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let node_count = plan.cfg.steps.len() + plan.second.as_ref().map(|c| c.steps.len()).unwrap_or(0);

    println!("Validation profile: {} (selection: {})", plan.profile, plan.selection_mode);
    println!("Commit: {commit} ({})", if tree_dirty() { "⚠️  NOT commit-anchored: dirty tree" } else { "clean tree, commit-anchored" });
    println!("Build cache: {cache}; host cores: {host_cpus}; scheduler width: -j {jobs}");
    println!("Plan: {node_count} boxed DAG node(s){}", if plan.second.is_some() { " across 2 sequential lanes" } else { "" });

    // Verbosity floored at 2: the runner streams each node's tagged output, so
    // the operator always sees which node is running. Never blind.
    let verbosity = if args.verbose { 3 } else { 2 };

    let mut outcomes: Vec<StepOutcome> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut ok = true;

    let r = run_dag_boxed_ordered(&plan.cfg, jobs, args.keep_going, verbosity, cgroups.clone(), None, None);
    outcomes.extend(r.outcomes.iter().cloned());
    skipped.extend(r.skipped.iter().cloned());
    ok = ok && r.ok;

    if let Some(second) = &plan.second {
        if ok || args.keep_going {
            let r2 = run_dag_boxed_ordered(second, jobs, args.keep_going, verbosity, cgroups.clone(), None, None);
            outcomes.extend(r2.outcomes.iter().cloned());
            skipped.extend(r2.skipped.iter().cloned());
            ok = ok && r2.ok;
        } else {
            eprintln!("validate: first lane failed; skipping the second lane (eager exit).");
        }
    }

    let wall = (epoch_now() - started_epoch) as f64;
    print_cost_table(&outcomes, &skipped);

    // Operator stop => NO-RESULT => no ledger row. A timeout, by contrast, is a
    // completed run and falls through to the normal write below. See
    // `install_stop_handlers` for why an interrupt row is worse than no row.
    if let Some(sig) = interrupted_by() {
        println!(
            "\n⏹ Validation interrupted by {sig} after {} — recording NO ledger row.",
            human_duration(wall)
        );
        println!(
            "   An interrupt learned nothing about the tree, so it is not a result. \
             {} node(s) had completed; partial output is in the durable log.",
            outcomes.len()
        );
        println!("   durable log: {}", durable.path.display());
        let _ = std::fs::remove_dir_all(&tmp);
        durable.finish();
        return ExitCode::from(130);
    }

    // Compatibility ratchet, evaluated from typed outcomes.
    let mut compat_blocking = 0usize;
    if let Some(mode) = plan.compat {
        let (passed, measured, blocking) = print_compat_summary(mode, &outcomes);
        compat_blocking = blocking.len();
        let floor = match mode {
            CompatMode::Sabre => Some(validate_corpus::SABRE_COMPAT_EXPECTED),
            CompatMode::Rr => Some(validate_corpus::RR_COMPAT_EXPECTED),
            _ => None,
        };
        if let Some(f) = floor {
            if passed < f {
                println!("❌ {} ratchet: {passed}/{measured} passing, floor {f} — BELOW FLOOR", mode.assurance());
                ok = false;
            } else {
                println!("✅ {} ratchet: {passed}/{measured} passing, floor {f} — met", mode.assurance());
            }
        }
        if !blocking.is_empty() {
            println!("❌ {} blocking failures ({}): {}", mode.assurance(), blocking.len(), blocking.join(", "));
        }
    }

    let failures = outcomes.iter().filter(|o| !o.ok && !o.aborted).count();
    // A compat run's verdict is the RATCHET, not the raw node count: known
    // fail-closed rows and bounded portable diagnostics are nonblocking by
    // policy, so they must not turn the run red on their own.
    let effective_failures = if plan.compat.is_some() { compat_blocking } else { failures };
    let exit_code: u8 = if ok && effective_failures == 0 { 0 } else { 1 };

    // Coverage in the consumer's exact CoverageRow shape. NODE-RAN granularity:
    // zero_executed_nodes is always empty and means "not determinable here", not
    // "verified none" — a node that ran while executing zero test cases is only
    // visible in the log banners, which is finalize_receipt.py --scan's job.
    let executed: BTreeSet<String> = outcomes
        .iter()
        .filter(|o| !o.aborted)
        .map(|o| o.tag.clone())
        .filter(|t| plan.planned_test_nodes.contains(t))
        .collect();
    let absent: Vec<&String> = plan.planned_test_nodes.iter().filter(|t| !executed.contains(*t)).collect();
    let coverage = serde_json::json!({
        "planned_test_nodes": plan.planned_test_nodes.len(),
        "executed_test_nodes": executed.len(),
        "zero_executed_nodes": Vec::<String>::new(),
        "absent_nodes": absent,
    });

    let behind_ahead = sh("git", &["rev-list", "--left-right", "--count", "origin/main...HEAD"])
        .unwrap_or_else(|| "0 0".into());
    let mut ba = behind_ahead.split_whitespace();
    let git_behind: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let git_ahead: i64 = ba.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let dirty_now = tree_dirty();

    let ctx = LedgerCtx {
        started_at,
        host: sh("hostname", &["-s"]).unwrap_or_else(|| "unknown".into()),
        toolchain: sh("rustc", &["--version"]).unwrap_or_else(|| "unknown".into()),
        slot: slot_name(&root, parent.as_deref()),
        cwd: root.to_string_lossy().into(),
        profile: plan.profile.clone(),
        selection_mode: plan.selection_mode.into(),
        cache_state: cache.into(),
        commit: commit.clone(),
        tree: git_tree(),
        git_ahead,
        git_behind,
        commit_anchored: commit != "unknown" && !dirty_now,
        tree_dirty: dirty_now,
        dag_jobs: jobs,
    };
    write_ledger(
        &ledger_path(&root),
        &ctx,
        &outcomes,
        &skipped,
        wall,
        exit_code,
        &durable.path.to_string_lossy(),
        plan.suite_complete,
        coverage,
    );

    let marker = if exit_code == 0 { "✅" } else { "❌" };
    println!(
        "{marker} {} — {} node(s) executed, {failures} failed, {} skipped in {} at -j {jobs}",
        if exit_code == 0 { "PASS" } else { "FAIL" },
        outcomes.len(),
        skipped.len(),
        human_duration(wall)
    );
    println!("   durable log: {}", durable.path.display());

    let _ = std::fs::remove_dir_all(&tmp);
    durable.finish();
    ExitCode::from(exit_code)
}
