#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! validate.rs — PHASE 1 thin, typed wrapper that drives the CI validation
//! lanes by calling `safe-ci-dag-runner` **as a library** (an in-process typed
//! call, NOT a subprocess).
//!
//! # What this is (and is not) yet
//!
//! The long-term goal is to DELETE the 4000-line `validate.sh` bash orchestrator
//! and replace it with a thin Rust entrypoint over the DAG runner. That full
//! migration — porting the ~60 non-DAG gates and repointing `make validate` — is
//! PHASE 2. This file is PHASE 1: an ADDITIVE new entrypoint that runs the
//! DAG-lane profiles (`ci/dag/<profile>.json`) through the typed library and
//! establishes the ledger / boxing / typed-classification foundation. It does
//! not delete `validate.sh` and does not yet repoint `make validate`.
//!
//! # Why a library call, not a subprocess
//!
//! Calling `run_dag_boxed_ordered` directly gives us TYPED results
//! (`RunResult`/`StepOutcome`) instead of scraped text. Every decision this
//! wrapper makes — process exit code, per-node cost table, ledger `result`, and
//! failure classification — is derived STRUCTURALLY from typed fields
//! (`RunResult.ok`, `StepOutcome.ok`/`returncode`/`reason`/`aborted`). We never
//! grep stdout to decide anything. `StepOutcome.reason` is produced by the
//! library's own `step_failure_reason`, which already classifies
//! oom/timeout/cpu_timeout/signal, so classification is not re-implemented here.
//!
//! # Boxing is the primary purpose — fail closed by default
//!
//! cgroup-v2 two-level boxing is the reason the DAG runner exists. This wrapper
//! reproduces the library's own `resolve_cgroups` policy exactly: by DEFAULT it
//! re-execs into a transient `systemd --user` scope (the "systemd --user scope
//! producer path") and, if boxing still cannot be established, exits 3. Passing
//! `--allow-cgroup-failure` downgrades to an UNBOXED run with a loud warning.
//! This is not a bypass; it is the same fail-closed contract the CLI enforces.
//!
//! # Ledger schema-transition design constraint — VERSION-AWARE ACCEPTANCE
//!
//! The ledger PRODUCER travels with the branch: an in-flight PR carries its own
//! (possibly older) copy of this file, so a PR emits records in ITS producer's
//! schema, not whatever `main` currently writes. As of this writing 57 of 74
//! open PRs predate `bfb0a9ef` and therefore emit an OLDER schema. A consumer
//! that hard-rejects an older-but-valid version breaks every one of them at once
//! — which is exactly the live incident this design must prevent: a consumer
//! tightened AHEAD of its producers and began rejecting 254 of 255 ledger rows
//! fleet-wide, forcing a hermit-validate pause. Tightening a reader before the
//! producers emit the newly-required shape is the same failure mode as deleting
//! a producer before its replacement covers every gate.
//!
//! The durable cure is VERSION-AWARE ACCEPTANCE (chosen over a time-boxed grace
//! period or a forced fleet-wide rebase, because only version-awareness survives
//! a THIRD tightening). Its contract, which any future bump MUST preserve:
//!
//!   1. THE WRITER STAMPS A SCHEMA VERSION and ALWAYS emits its
//!      selection-accounting fields (`schema_version` + `executed_nodes` +
//!      `skipped_nodes` + `profile`) with REAL values on every run. A record is
//!      never emitted with these fields omitted or zero-filled. Crucially the
//!      NODE-count fields are NOT named `executed_tests`/`filtered_tests`: those
//!      libtest-count names are reserved for a real per-test count, so a
//!      schema<5 consumer never reads a DAG-node count as a test count.
//!   2. THE READER ACCEPTS OLDER VALID VERSIONS instead of hard-rejecting them:
//!      it dispatches on `schema_version`, reads every field via a
//!      get-with-default, and treats an older-but-valid record as valid.
//!   3. DEFINED DEFAULT/DERIVATION FOR EACH NEW REQUIRED FIELD. Any field a new
//!      schema treats as required must have a well-defined value for records an
//!      OLDER producer wrote without it (a static default or a derivation from
//!      fields that already exist). A bump that cannot supply such a default
//!      would retroactively invalidate green receipts from open PRs and is
//!      therefore disallowed.
//!
//! Concretely: this producer writes `schema_version: 3` and ALWAYS emits the
//! selection-accounting fields `profile`, `executed_nodes`, and `skipped_nodes`
//! (plus `commit`/`commit_anchored`/`tree_dirty` for commit anchoring). Because
//! the qualification travels WITH the value (all written at the single
//! ledger-write point below), a downstream reader can never pair a bare `pass`
//! with inferred coverage.
//!
//! ## What `executed_nodes` / `skipped_nodes` MEAN for a DAG-lane run
//!
//! The unit of execution in a DAG lane is the NODE (gate) — each node runs one
//! command (a build, a `cargo test` target, a harness). The typed `RunResult`
//! exposes NODE outcomes and resource metrics, not individual cargo-test-case
//! counts (the runner surfaces only the last output line as `summary`, not a
//! parsed per-test count). So this producer binds:
//!   * `executed_nodes` = number of gates that actually RAN (`outcomes.len()`),
//!   * `skipped_nodes`  = number of gates SKIPPED because a dependency failed
//!                        (`skipped.len()`; a full green run has zero).
//! These are genuine NODE counts from typed fields, never fabricated or
//! zero-filled. They are DELIBERATELY NOT named `executed_tests`/`filtered_tests`
//! (the libtest-count field names a schema<5 consumer keys `is_clean_full_pass`
//! on): a validate.rs receipt must NEVER be mistakable for a qualifying full-TEST
//! pass just because it ran ~47 DAG nodes. Real libtest-count parsing is Phase 2;
//! the counted+coverage receipt is minted by `finalize_receipt.py --scan`.
//!
//! # Usage
//!
//! ```text
//! ./scripts/validate.rs <profile> [-j N] [-v] [--allow-cgroup-failure]
//!                       [--perf-dir DIR] [-k|--keep-going] [--dag-file PATH]
//! ```
//!
//! `<profile>` selects `ci/dag/<profile>.json` (portable | privileged, or any
//! other `ci/dag/*.json` present). `--dag-file PATH` (or the `RUN_DAG_FILE_OVERRIDE`
//! env, mirroring `ci/run-dag.sh`) runs an exact DAG file instead, keeping the
//! profile label for the ledger.
//!
//! ```cargo
//! [dependencies]
//! safe-ci-dag-runner = { path = "../agent-utils/rs/safe-ci-dag-runner" }
//! serde_json = "1"
//! libc = "0.2"
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

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
use safe_ci_dag_runner::io::dag_from_json;
use safe_ci_dag_runner::model::StepOutcome;
use safe_ci_dag_runner::perflog::append_step_profiles;
use safe_ci_dag_runner::scheduler::run_dag_boxed_ordered;
use safe_ci_dag_runner::scheduler::BoxedCgroups;

/// Ledger schema this producer emits. See the schema-transition constraint in
/// the module doc comment before changing this.
///
/// Kept at 3 DELIBERATELY. validate.rs emits NODE-granularity fields
/// (`executed_nodes`/`skipped_nodes`) and NOT the libtest-count fields
/// (`executed_tests`/`filtered_tests`), precisely so a schema<5 consumer's
/// `counts_present` branch (`is_clean_full_pass`) can never mistake a
/// validate.rs receipt for a qualifying full-TEST pass — the DAG-node count
/// (~47) would otherwise be read as "47 tests executed". The authoritative
/// counted+coverage receipt is minted by `finalize_receipt.py --scan` off the
/// durable log; Phase 2 will add real libtest-count parsing here.
///
/// Bumping to schema 5 would be WRONG: schema>=5 triggers a per-node coverage
/// contract this Phase-1 wrapper cannot satisfy. The `producer:"validate.rs"`
/// field already disambiguates this row for a version-aware reader.
const LEDGER_SCHEMA_VERSION: i64 = 3;

/// Producer identity recorded in each ledger row, so a backward-tolerant reader
/// can tell a validate.rs receipt from a validate.sh one without inference.
const LEDGER_PRODUCER: &str = "validate.rs";

/// Env var that names an explicit ledger file (highest precedence). Matches the
/// override `validate.sh` honors so both producers can share one ledger.
const LEDGER_ENV: &str = "HERMIT_VALIDATE_LEDGER";

/// Env var naming the dev-hermit parent workspace (second precedence).
const PARENT_ENV: &str = "DEV_HERMIT_PARENT";

/// Checkout-local default ledger file (third precedence). This is the landmine
/// fix: a STANDALONE checkout with neither env set previously produced no
/// receipt at all; now it always writes here so a green claim has evidence.
const LOCAL_LEDGER_BASENAME: &str = ".hermit-validate-ledger.jsonl";

/// Env override for an exact DAG file, mirroring `ci/run-dag.sh`.
const DAG_FILE_OVERRIDE_ENV: &str = "RUN_DAG_FILE_OVERRIDE";

/// Profile-store dir env, mirroring the runner's own default resolution.
const PROFILE_DIR_ENV: &str = "SAFE_CI_DAG_RUNNER_PROFILE_DIR";

/// The meta-profile that subsumes the GitHub-authoritative validation surface:
/// bootstrap preflight + the manifest gate + BOTH the portable and privileged
/// DAG lanes in one boxed, self-teed run. See `run_full_profile`.
const FULL_PROFILE: &str = "full";

// --------------------------------------------------------------------------- unified gate outcome

/// A single gate outcome, unified across the two kinds of work a `full` run does:
///   * an out-of-process preflight/bootstrap gate (submodule init, reverie pin,
///     manifest) run as a subprocess, and
///   * an in-process DAG NODE executed by `safe-ci-dag-runner` (a `StepOutcome`).
///
/// Collapsing both into one type lets the ledger, the cost table, and the verdict
/// treat every gate identically and STRUCTURALLY (never by scraping text), so a
/// silently-dropped gate cannot hide behind a different code path.
#[derive(Clone)]
struct GateOutcome {
    tag: String,
    ok: bool,
    returncode: Option<i64>,
    reason: String,
    aborted: bool,
    duration_s: f64,
}

impl From<&StepOutcome> for GateOutcome {
    fn from(o: &StepOutcome) -> Self {
        GateOutcome {
            tag: o.tag.clone(),
            ok: o.ok,
            returncode: o.returncode,
            reason: o.reason.clone(),
            aborted: o.aborted,
            duration_s: o.duration_s,
        }
    }
}

// --------------------------------------------------------------------------- args

struct Args {
    profile: String,
    dag_file: Option<String>,
    jobs: Option<i64>,
    verbosity: i64,
    keep_going: bool,
    allow_cgroup_failure: bool,
    perf_dir: Option<String>,
}

fn usage() -> &'static str {
    "usage: validate.rs <profile> [options]\n\
     \n\
     PHASE 1 typed wrapper: run a CI validation lane as a safe-ci-dag-runner DAG,\n\
     in-process (library call, not a subprocess), boxed by default.\n\
     \n\
     <profile>                selects ci/dag/<profile>.json (e.g. portable, privileged)\n\
     -j N                     scheduler width (default: host_cpus/8, floor 2, cap 16)\n\
     -v                       increase verbosity (repeatable)\n\
     -k, --keep-going         do not eager-exit on the first failure\n\
     --allow-cgroup-failure   downgrade to an UNBOXED run instead of failing closed\n\
     --perf-dir DIR           forward per-step profile rows to DIR\n\
     --dag-file PATH          run this exact DAG file (keeps <profile> as the label);\n\
     \x20                        also settable via RUN_DAG_FILE_OVERRIDE\n\
     -h, --help               print this help and exit"
}

/// Parse argv. Returns `Err(code)` for a usage error (2) or a handled `--help` (0).
fn parse_args() -> Result<Args, u8> {
    let mut profile: Option<String> = None;
    let mut dag_file: Option<String> = std::env::var(DAG_FILE_OVERRIDE_ENV).ok().filter(|s| !s.is_empty());
    let mut jobs: Option<i64> = None;
    let mut verbosity: i64 = 0;
    let mut keep_going = false;
    let mut allow_cgroup_failure = false;
    let mut perf_dir: Option<String> = None;

    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            "-h" | "--help" => {
                println!("{}", usage());
                return Err(0);
            }
            "-v" => verbosity += 1,
            "-k" | "--keep-going" => keep_going = true,
            "--allow-cgroup-failure" => allow_cgroup_failure = true,
            "-j" => {
                i += 1;
                let v = argv.get(i).ok_or_else(|| {
                    eprintln!("validate.rs: -j requires an argument");
                    2u8
                })?;
                jobs = Some(v.parse::<i64>().map_err(|_| {
                    eprintln!("validate.rs: -j argument must be an integer, got {v:?}");
                    2u8
                })?);
            }
            "--perf-dir" => {
                i += 1;
                perf_dir = Some(
                    argv.get(i)
                        .ok_or_else(|| {
                            eprintln!("validate.rs: --perf-dir requires an argument");
                            2u8
                        })?
                        .clone(),
                );
            }
            "--dag-file" => {
                i += 1;
                dag_file = Some(
                    argv.get(i)
                        .ok_or_else(|| {
                            eprintln!("validate.rs: --dag-file requires an argument");
                            2u8
                        })?
                        .clone(),
                );
            }
            other if other.starts_with('-') => {
                eprintln!("validate.rs: unknown option {other:?}");
                eprintln!("{}", usage());
                return Err(2);
            }
            other => {
                if profile.is_some() {
                    eprintln!("validate.rs: unexpected extra positional argument {other:?}");
                    return Err(2);
                }
                profile = Some(other.to_string());
            }
        }
        i += 1;
    }

    let profile = profile.ok_or_else(|| {
        eprintln!("validate.rs: missing required <profile> argument");
        eprintln!("{}", usage());
        2u8
    })?;

    Ok(Args {
        profile,
        dag_file,
        jobs,
        verbosity,
        keep_going,
        allow_cgroup_failure,
        perf_dir,
    })
}

// --------------------------------------------------------------------------- jobs default

/// Default scheduler width, honoring the SAME shared runtime authority
/// validate.sh uses so both producers pick identical widths on the same host.
///
/// Precedence mirrors validate.sh:606-635 (the `VALIDATION_DAG_JOBS`
/// derivation), which is the shared spec:
///   * `${CI_DAG_JOBS:-$CI_DAG_JOBS_DEFAULT}` — an explicitly-set `CI_DAG_JOBS`
///     env var is the override and is used EXACTLY, with NO clamp (validate.sh
///     clamps only the *default*, never the override; it only requires the
///     override be a positive integer, else it exits 2).
///   * otherwise the host-adaptive default `CI_DAG_JOBS_DEFAULT = host_cpus/8`,
///     floored at 2 and capped at 16 (validate.sh:628-630).
///
/// Called from exactly one site (main), only when `-j` was not supplied — so an
/// explicit `-j` (also unclamped, like the env override) still wins over both.
///
/// FOLLOW-UP: fully extracting this width rule into safe-ci-dag-runner so the
/// three consumers (validate.sh, run-dag.sh, validate.rs) call one function is
/// Phase 2; for now this reads the same `CI_DAG_JOBS` runtime authority.
fn default_jobs() -> i64 {
    // CI_DAG_JOBS override: used EXACTLY (no clamp), matching validate.sh's
    // `${CI_DAG_JOBS:-...}`. An empty value is treated as unset (the `:-` form).
    // validate.sh rejects a set-but-invalid value with exit 2; here we can only
    // return an i64, so an unparseable/non-positive value falls back to the
    // default (deviation noted in the commit message).
    if let Ok(v) = std::env::var("CI_DAG_JOBS") {
        if !v.is_empty() {
            if let Ok(n) = v.parse::<i64>() {
                if n > 0 {
                    return n;
                }
            }
            eprintln!(
                "validate.rs: warning: CI_DAG_JOBS={v:?} is not a positive integer; \
                 falling back to the host-adaptive default (validate.sh would exit 2)."
            );
        }
    }
    let host_cpus = std::thread::available_parallelism()
        .map(|n| n.get() as i64)
        .unwrap_or(1);
    (host_cpus / 8).clamp(2, 16)
}

// --------------------------------------------------------------------------- boxing

/// Establish the two-level cgroup-v2 boxing that is the runner's PRIMARY purpose,
/// mirroring the library's private `cli::resolve_cgroups`. Returns the manager to
/// use (`None` = intentional UNBOXED run) or `Err(exit_code)` the caller returns.
/// On the default path this re-execs into a transient `systemd --user` scope and
/// never returns on success.
fn resolve_cgroups(allow_failure: bool) -> Result<BoxedCgroups, u8> {
    if is_in_scope() {
        let mgr = Cgroups::new();
        if mgr.enabled() {
            install_scope_teardown();
            eprintln!(
                "validate.rs: cgroup boxing ACTIVE (two-level cgroup-v2 scope; per-step \
                 memory/CPU caps + setsid-proof teardown)."
            );
            return Ok(Some(Arc::new(mgr) as Arc<dyn CgroupManager>));
        }
        if allow_failure {
            eprintln!(
                "validate.rs: warning: inside a scope but per-step cgroup setup failed; \
                 running best-effort UNBOXED (--allow-cgroup-failure)."
            );
            return Ok(None);
        }
        eprintln!(
            "validate.rs: ERROR: inside a managed scope but per-step cgroups could not be \
             set up; re-run with --allow-cgroup-failure to run UNBOXED."
        );
        return Err(3);
    }
    if allow_failure {
        eprintln!(
            "validate.rs: warning: cgroup boxing not established (--allow-cgroup-failure); \
             running UNBOXED (process-group teardown only, no per-step memory/CPU caps)."
        );
        return Ok(None);
    }
    // Default: boxing is required -> re-exec into a transient systemd --user scope.
    // On success this never returns (exec replaces the process); a return means
    // boxing is unavailable.
    let reexeced_or_skipped = reexec_in_scope(None, None);
    let detail = if reexeced_or_skipped {
        "boxing was skipped (e.g. CI without a systemd --user scope)"
    } else {
        "cgroup-v2 + a working systemd --user scope are unavailable"
    };
    eprintln!(
        "validate.rs: ERROR: cgroup boxing could not be established: {detail}. Cgroup \
         resource boxing is this tool's primary purpose; re-run with --allow-cgroup-failure \
         to run UNBOXED."
    );
    Err(3)
}

// --------------------------------------------------------------------------- durable log (self-tee)

/// A live self-tee: everything this process writes to fd 1 / fd 2 is duplicated
/// into a durable, absolute log file AND still shown on the original terminal.
///
/// WHY validate.rs tees ITS OWN log (owner directive, "option C"): the
/// authoritative counted+coverage receipt is minted by `finalize_receipt.py
/// --scan`, which reads the `log_file` recorded in the ledger row and parses the
/// libtest banners out of it. A STANDALONE `./scripts/validate.rs` launched with
/// no `ci-hub validate-run` systemd unit around it has NO durable sink — it would
/// run, pass, and leave no receipt, which is indistinguishable from never having
/// run (the same defect class as `--cgroups` silently running nothing). Teeing
/// here makes the RECEIPT PATH INDEPENDENT OF THE LAUNCH PATH: the log exists
/// whether the run was launched by `validate-run`, by `make validate`, or by a
/// bare `./scripts/validate.rs`. It is deliberately NOT deferred to the runner
/// library or to `start_unit.py`, either of which would leave the standalone case
/// broken.
struct DurableLog {
    path: PathBuf,
    tee: std::process::Child,
    orig_stdout: i32,
    orig_stderr: i32,
}

impl DurableLog {
    /// Flush our buffered output, restore the original fds so any later message
    /// goes to the terminal, close the tee's write ends, and reap it.
    fn finish(mut self) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        // Restoring fds 1 and 2 replaces the pipe write-ends, dropping the last
        // references to the pipe so `tee` observes EOF and exits.
        unsafe {
            libc::dup2(self.orig_stdout, 1);
            libc::dup2(self.orig_stderr, 2);
            libc::close(self.orig_stdout);
            libc::close(self.orig_stderr);
        }
        let _ = self.tee.wait();
    }
}

/// Resolve the durable log path: `<parent|repo-root>/ignored/validate/` holds
/// machine-local run logs (an ignored dir, never committed). The name carries the
/// profile, a short SHA, and a timestamp so concurrent runs never collide. The
/// path is ALWAYS ABSOLUTE — `verify_receipt.sh` (the merge gate) requires the
/// recorded `durable_log_file` to start with `/`.
fn durable_log_path(root: &Path, profile: &str, sha: &str) -> PathBuf {
    let dir = if let Ok(parent) = std::env::var(PARENT_ENV) {
        if !parent.is_empty() {
            PathBuf::from(parent).join("ignored").join("validate")
        } else {
            root.join("ignored").join("validate")
        }
    } else {
        root.join("ignored").join("validate")
    };
    let sha12: String = sha.chars().take(12).collect();
    let ts = utc_now().replace([':', '-'], "");
    dir.join(format!("validate-rs-{profile}-{sha12}-{ts}.log"))
}

/// Establish the self-tee. FAIL-CLOSED: on any failure to create the directory,
/// spawn `tee`, or redirect the fds, returns `Err(exit_code)` so the caller exits
/// LOUDLY rather than running without a durable receipt. Must be called AFTER
/// `resolve_cgroups` (which re-execs on the default path), so the tee is set up
/// exactly once, in the final boxed process — never inherited across the re-exec.
fn setup_durable_log(root: &Path, profile: &str, sha: &str) -> Result<DurableLog, u8> {
    use std::os::unix::io::AsRawFd;
    let path = durable_log_path(root, profile, sha);
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!(
                "validate.rs: ERROR: cannot create durable-log dir {}: {e}. A run with no \
                 durable receipt is a silent no-result; refusing to proceed.",
                dir.display()
            );
            return Err(4);
        }
    }
    // Spawn `tee -a <log>` BEFORE redirecting our fds so tee inherits the real
    // terminal as its stdout (output still shows live), and appends to the file.
    let mut tee = match Command::new("tee")
        .arg("-a")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "validate.rs: ERROR: cannot spawn `tee` for the durable log {}: {e}. \
                 Refusing to run without a durable receipt.",
                path.display()
            );
            return Err(4);
        }
    };
    // Save the originals, then point fd 1 and fd 2 at the tee's stdin pipe.
    let (orig_stdout, orig_stderr, ok) = unsafe {
        let so = libc::dup(1);
        let se = libc::dup(2);
        let pipe_fd = tee.stdin.as_ref().map(|s| s.as_raw_fd()).unwrap_or(-1);
        let ok = so >= 0 && se >= 0 && pipe_fd >= 0 && libc::dup2(pipe_fd, 1) >= 0 && libc::dup2(pipe_fd, 2) >= 0;
        (so, se, ok)
    };
    if !ok {
        eprintln!("validate.rs: ERROR: could not redirect stdout/stderr into the durable log.");
        let _ = tee.kill();
        return Err(4);
    }
    // dup2 gave fds 1 and 2 independent duplicates of the pipe write-end; drop the
    // ChildStdin so the pipe has exactly two write-ends (fd 1 and fd 2), which
    // `finish` closes to signal EOF to tee.
    drop(tee.stdin.take());
    eprintln!("validate.rs: durable log: {}", path.display());
    Ok(DurableLog {
        path,
        tee,
        orig_stdout,
        orig_stderr,
    })
}

// --------------------------------------------------------------------------- subprocess gates

/// Run one out-of-process preflight/bootstrap gate (submodule init, reverie pin,
/// manifest) as a subprocess, inheriting our (teed, boxed) fds so its output
/// lands in the durable log. The verdict is STRUCTURAL: `ok` is the exit status,
/// `returncode` the raw code, mirroring how the library classifies a `StepOutcome`
/// so both gate kinds flow through one ledger/cost path.
fn run_subprocess_gate(tag: &str, cwd: &Path, program: &str, args: &[&str]) -> GateOutcome {
    eprintln!("\n[{tag}] $ {program} {}", args.join(" "));
    let start = std::time::Instant::now();
    let status = Command::new(program).args(args).current_dir(cwd).status();
    let duration_s = start.elapsed().as_secs_f64();
    match status {
        Ok(st) => {
            let rc = st.code().map(|c| c as i64).or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                st.signal().map(|s| -(s as i64))
            });
            let ok = st.success();
            let reason = if ok {
                String::new()
            } else if let Some(c) = st.code() {
                format!("exit {c}")
            } else {
                use std::os::unix::process::ExitStatusExt;
                format!("signal {}", st.signal().unwrap_or(0))
            };
            eprintln!("[{tag}] {} ({:.1}s)", if ok { "PASS" } else { "FAIL" }, duration_s);
            GateOutcome {
                tag: tag.to_string(),
                ok,
                returncode: rc,
                reason,
                aborted: false,
                duration_s,
            }
        }
        Err(e) => {
            eprintln!("[{tag}] FAIL could not spawn {program:?}: {e}");
            GateOutcome {
                tag: tag.to_string(),
                ok: false,
                returncode: None,
                reason: format!("spawn error: {e}"),
                aborted: false,
                duration_s,
            }
        }
    }
}

// --------------------------------------------------------------------------- full meta-profile

/// Run ONE DAG lane in-process via the library and return its gates as the
/// unified `GateOutcome`, the skipped node names, wall seconds, and the library's
/// own structural `RunResult.ok` verdict. Node tags are lane-prefixed so the
/// ledger and cost table stay unambiguous across the two lanes.
fn run_dag_lane(
    root: &Path,
    lane: &str,
    jobs: i64,
    keep_going: bool,
    verbosity: i64,
    cgroups: BoxedCgroups,
) -> Result<(Vec<GateOutcome>, Vec<String>, f64, bool), u8> {
    let dag_path = root.join("ci").join("dag").join(format!("{lane}.json"));
    let dag_text = std::fs::read_to_string(&dag_path).map_err(|e| {
        eprintln!("validate.rs: cannot read {}: {e}", dag_path.display());
        2u8
    })?;
    let cfg = dag_from_json(&dag_text).map_err(|e| {
        eprintln!("validate.rs: invalid DAG {}: {e}", dag_path.display());
        2u8
    })?;
    eprintln!("\n[{lane} CI DAG lane] $ safe-ci-dag-runner {} -j {jobs}", dag_path.display());
    let result = run_dag_boxed_ordered(&cfg, jobs, keep_going, verbosity, cgroups, None, None);
    let gates: Vec<GateOutcome> = result
        .outcomes
        .iter()
        .map(|o| {
            let mut g = GateOutcome::from(o);
            g.tag = format!("{lane}:{}", o.tag);
            g
        })
        .collect();
    let skipped: Vec<String> = result.skipped.iter().map(|s| format!("{lane}:{s}")).collect();
    Ok((gates, skipped, result.wall_s, result.ok))
}

/// The `full` meta-profile: the honest Rust subsumption of validate.sh's
/// `run_full_suite` (its 6 `run_check` gates). Runs the two always-on preflight
/// gates (fail-fast, mirroring validate.sh:4537), the centralized manifest gate,
/// then BOTH the portable and privileged DAG lanes in-process. Verbosity is
/// forced to >=2 so the runner streams each node's `[tag]`-prefixed output and
/// terminal PASS/FAIL into the (teed) log, which `finalize_receipt.py --scan`
/// parses to mint the counted+coverage schema-5 receipt.
///
/// Returns (all gates, all skipped, total wall seconds, overall_ok).
fn run_full_profile(
    root: &Path,
    jobs: i64,
    keep_going: bool,
    verbosity: i64,
    cgroups: BoxedCgroups,
) -> (Vec<GateOutcome>, Vec<String>, f64, bool) {
    let verbosity = verbosity.max(2);
    let with_proxy = has_cmd("with-proxy");
    let mut gates: Vec<GateOutcome> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut wall = 0.0_f64;

    // Preflight gate 1: submodule init (validate.sh:4533 / initialize_repository_submodules).
    let g = if with_proxy {
        run_subprocess_gate(
            "Initialize repository submodules",
            root,
            "with-proxy",
            &["git", "submodule", "update", "--init", "--recursive"],
        )
    } else {
        run_subprocess_gate(
            "Initialize repository submodules",
            root,
            "git",
            &["submodule", "update", "--init", "--recursive"],
        )
    };
    wall += g.duration_s;
    let pre1_ok = g.ok;
    gates.push(g);

    // Preflight gate 2: reverie pin consistency (validate.sh:4536).
    let pin = "./scripts/check-reverie-pin.rs";
    let g = if with_proxy {
        run_subprocess_gate("Reverie pin consistency", root, "with-proxy", &[pin])
    } else {
        run_subprocess_gate("Reverie pin consistency", root, pin, &[])
    };
    wall += g.duration_s;
    let pre2_ok = g.ok;
    gates.push(g);

    // Fail-fast: do NOT run heavy lanes if preflight failed (validate.sh:4537).
    if !pre1_ok || !pre2_ok {
        eprintln!("validate.rs: preflight gate failed; skipping DAG lanes (matches validate.sh fail-fast).");
        return (gates, skipped, wall, false);
    }

    // Manifest gate (validate.sh:4178). Runs once (validate.sh runs the identical
    // command per lane; deduped here — the gate runs, it is not dropped).
    let g = run_subprocess_gate(
        "Centralized test manifest and inventory",
        root,
        "./ci/test_harness.sh",
        &["validate"],
    );
    wall += g.duration_s;
    let manifest_ok = g.ok;
    gates.push(g);

    let mut overall_ok = manifest_ok;
    for lane in ["portable", "privileged"] {
        match run_dag_lane(root, lane, jobs, keep_going, verbosity, cgroups.clone()) {
            Ok((lg, ls, lw, lok)) => {
                gates.extend(lg);
                skipped.extend(ls);
                wall += lw;
                overall_ok = overall_ok && lok;
            }
            Err(_) => {
                overall_ok = false;
            }
        }
    }
    (gates, skipped, wall, overall_ok)
}

/// Mirror validate.sh's `command -v <name>` availability probe.
fn has_cmd(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// --------------------------------------------------------------------------- git / ledger

fn git_sha() -> String {
    match Command::new("git").args(["rev-parse", "HEAD"]).output() {
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

/// True when the working tree differs from HEAD in ANY way (porcelain non-empty).
/// Drives commit anchoring: a record is only faithfully attributable to a SHA
/// when the tree exactly matches that HEAD.
fn tree_dirty() -> bool {
    match Command::new("git").args(["status", "--porcelain"]).output() {
        Ok(o) if o.status.success() => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        // Outside a git repo or on error: not dirty, just "not anchored".
        _ => false,
    }
}

/// Repo root via `git rev-parse --show-toplevel`, so profile/DAG paths resolve
/// no matter the caller's cwd. Falls back to the current dir.
fn repo_root() -> PathBuf {
    match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
        _ => {}
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Resolve the ledger file with a defined precedence that is NEVER empty (the
/// standalone-checkout landmine fix):
///   1. `$HERMIT_VALIDATE_LEDGER` — explicit file.
///   2. `$DEV_HERMIT_PARENT/ignored/validate-run-ledger.jsonl` — parent workspace.
///   3. `<repo_root>/.hermit-validate-ledger.jsonl` — checkout-local default.
fn ledger_path(root: &Path) -> PathBuf {
    if let Ok(explicit) = std::env::var(LEDGER_ENV) {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    if let Ok(parent) = std::env::var(PARENT_ENV) {
        if !parent.is_empty() {
            return PathBuf::from(parent)
                .join("ignored")
                .join("validate-run-ledger.jsonl");
        }
    }
    root.join(LOCAL_LEDGER_BASENAME)
}

fn utc_now() -> String {
    match Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    }
}

/// Write one JSONL ledger record. Every qualification (profile/executed_nodes/
/// skipped_nodes, commit anchoring, per-gate reason) is written HERE, at the
/// single ledger-write point, so no downstream reader can pair a bare `pass`
/// with inferred coverage.
#[allow(clippy::too_many_arguments)]
fn write_ledger_record(
    ledger: &Path,
    started_at: &str,
    finished_at: &str,
    profile: &str,
    gates: &[GateOutcome],
    skipped: &[String],
    wall_s: f64,
    overall_ok: bool,
    log_file: Option<&str>,
    exit_code: u8,
    commit: &str,
    tree_is_dirty: bool,
    selection_mode: &str,
) {
    // DAG-lane semantics: the gate (NODE) is the unit of execution, NOT a libtest
    // test case. See the module doc comment. These are DAG-node counts:
    //   executed_nodes = DAG nodes (gates) that actually RAN;
    //   skipped_nodes  = DAG nodes skipped because a dependency failed.
    // They are deliberately NOT named executed_tests/filtered_tests, so a
    // schema<5 consumer never mistakes a node count for a libtest test count.
    let executed_nodes = gates.len();
    let skipped_nodes = skipped.len();
    // Genuine, non-aborted failures — the honest failure count.
    let failures = gates.iter().filter(|o| !o.ok && !o.aborted).count();
    let commit_anchored = commit != "unknown" && !tree_is_dirty;
    let overall = if overall_ok && failures == 0 {
        "pass"
    } else {
        "fail"
    };

    let host = Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    let gates_json: Vec<serde_json::Value> = gates
        .iter()
        .map(|o| {
            serde_json::json!({
                "name": o.tag,
                "result": if o.ok { "pass" } else { "fail" },
                "returncode": o.returncode,
                "reason": o.reason,
                "aborted": o.aborted,
                "real_seconds": o.duration_s,
            })
        })
        .collect();

    let record = serde_json::json!({
        "schema_version": LEDGER_SCHEMA_VERSION,
        "producer": LEDGER_PRODUCER,
        "started_at": started_at,
        "finished_at": finished_at,
        "host": host,
        // Selection accounting. These are NODE-granularity counts, deliberately
        // NOT named executed_tests/filtered_tests: emitting node values under the
        // libtest-count field names would let a schema<5 consumer's counts_present
        // branch (is_clean_full_pass) read a ~47-NODE DAG run as a 47-TEST full
        // pass. Fail-closed: no libtest-count-named field is written here. The
        // authoritative counted+coverage receipt is minted by
        // finalize_receipt.py --scan off the durable log; producer="validate.rs"
        // already disambiguates this row.
        "profile": profile,
        "executed_nodes": executed_nodes,
        "skipped_nodes": skipped_nodes,
        "selection_mode": selection_mode,
        // Self-describing partialness (Blocker 4): a single-profile Phase-1
        // DAG-lane run is never the full multi-lane validate; a full-coverage
        // landing receipt requires both portable and privileged lanes plus the
        // non-DAG gates that validate.sh still owns. So this is always false here.
        "full_coverage": false,
        // Commit anchoring.
        "commit": commit,
        "commit_anchored": commit_anchored,
        "tree_dirty": tree_is_dirty,
        // Verdict.
        "result": overall,
        "exit_code": exit_code,
        "failures": failures,
        "real_seconds": wall_s,
        // Absolute path to this run's durable self-teed log. finalize_receipt.py
        // --scan reads this to mint the counted schema-5 receipt; verify_receipt.sh
        // (merge gate) requires it to exist and start with '/'.
        "log_file": log_file,
        "gates": gates_json,
    });

    if let Some(dir) = ledger.parent() {
        if !dir.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!(
                    "validate.rs: warning: could not create ledger dir {}: {e}",
                    dir.display()
                );
                return;
            }
        }
    }

    use std::io::Write;
    let line = format!("{}\n", serde_json::to_string(&record).unwrap());
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(ledger)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                eprintln!(
                    "validate.rs: warning: could not append ledger {}: {e}",
                    ledger.display()
                );
            } else {
                eprintln!("validate.rs: ledger record appended to {}", ledger.display());
            }
        }
        Err(e) => eprintln!(
            "validate.rs: warning: could not open ledger {}: {e}",
            ledger.display()
        ),
    }
}

// --------------------------------------------------------------------------- reporting

/// The headline feature: a readable per-node cost table built entirely from typed
/// `StepOutcome` fields (never scraped text).
fn print_cost_table(outcomes: &[GateOutcome], skipped: &[String]) {
    println!("\n=== per-node cost (safe-ci-dag-runner) ===");
    println!("{:<40} {:>10}  {:<8} {}", "node", "seconds", "status", "reason/returncode");
    println!("{}", "-".repeat(80));
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
        // Prefer the library-derived reason; fall back to the typed returncode.
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
        println!("{:<40} {:>10.2}  {:<8} {}", o.tag, o.duration_s, status, detail);
    }
    println!("{}", "-".repeat(80));
    println!("{:<40} {:>10.2}  (sum of node wall)", "TOTAL", total);
    if !skipped.is_empty() {
        println!(
            "\nskipped (dependency failed, never ran): {}",
            skipped.join(", ")
        );
    }
}

// --------------------------------------------------------------------------- main

fn main() -> ExitCode {
    // FIRST thing, before any output: tolerate a downstream reader closing the
    // pipe early (the typed cure for the SIGPIPE-text-grep landmine).
    rust_script_prelude::init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(code) => return ExitCode::from(code),
    };

    let root = repo_root();

    // The `full` meta-profile is NOT a single DAG file: it subsumes validate.sh's
    // run_full_suite (preflight gates + manifest + BOTH the portable and privileged
    // DAG lanes). Any other profile — or an explicit --dag-file override — resolves
    // to a single ci/dag/<name>.json and runs the runner once.
    let is_full = args.profile == FULL_PROFILE && args.dag_file.is_none();

    // Resolve/validate the single DAG file up front (only for the single-lane path);
    // for `full` there is no ci/dag/full.json and this would spuriously error.
    let cfg = if is_full {
        None
    } else {
        // explicit --dag-file / RUN_DAG_FILE_OVERRIDE wins, else ci/dag/<profile>.json.
        let dag_path: PathBuf = match &args.dag_file {
            Some(p) => PathBuf::from(p),
            None => root.join("ci").join("dag").join(format!("{}.json", args.profile)),
        };
        if !dag_path.is_file() {
            eprintln!(
                "validate.rs: no such DAG file: {} (profile {:?})",
                dag_path.display(),
                args.profile
            );
            return ExitCode::from(2);
        }
        let dag_text = match std::fs::read_to_string(&dag_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("validate.rs: cannot read {}: {e}", dag_path.display());
                return ExitCode::from(2);
            }
        };
        match dag_from_json(&dag_text) {
            Ok(c) => Some((c, dag_path)),
            Err(e) => {
                eprintln!("validate.rs: invalid DAG {}: {e}", dag_path.display());
                return ExitCode::from(2);
            }
        }
    };

    // Fail-closed boxing. On the default path this re-execs into a transient
    // systemd --user scope and never returns on success; the code below runs in
    // the boxed re-exec.
    let cgroups: BoxedCgroups = match resolve_cgroups(args.allow_cgroup_failure) {
        Ok(c) => c,
        Err(code) => return ExitCode::from(code),
    };

    // Self-tee a durable log AFTER the boxing re-exec, so a standalone
    // `./scripts/validate.rs` (no systemd unit, no ci-hub launcher) still leaves a
    // receipt-quality log on disk. The RECEIPT PATH is thus independent of the
    // LAUNCH PATH: without this, a green standalone run leaves no trace, which is
    // indistinguishable from never having run. Fail-closed: exits 4 on any failure.
    let durable = match setup_durable_log(&root, &args.profile, &git_sha()) {
        Ok(d) => d,
        Err(code) => return ExitCode::from(code),
    };

    let jobs = args.jobs.unwrap_or_else(default_jobs);
    let started_at = utc_now();

    // Run the gates. `full` fans out to the subsumption; everything else runs the
    // single resolved DAG once (and forwards per-step perf rows, which only the
    // single-lane path produces via the typed RunResult).
    let (gates, skipped, wall, ok, selection_mode): (Vec<GateOutcome>, Vec<String>, f64, bool, &str) =
        if is_full {
            eprintln!("validate.rs: running profile \"full\" (subsumes run_full_suite) at -j {jobs}");
            let (g, s, w, o) =
                run_full_profile(&root, jobs, args.keep_going, args.verbosity, cgroups);
            (g, s, w, o, "full")
        } else {
            let (cfg, dag_path) = cfg.expect("single-lane path always resolves a cfg");
            eprintln!(
                "validate.rs: running profile {:?} (DAG {}) at -j {jobs}",
                args.profile,
                dag_path.display()
            );
            let result = run_dag_boxed_ordered(
                &cfg,
                jobs,
                args.keep_going,
                args.verbosity,
                cgroups,
                None,
                None,
            );

            // Forward per-step profile rows only when a profile dir is configured
            // (--perf-dir or the env), mirroring the runner's own opt-in.
            let profile_dir = args
                .perf_dir
                .clone()
                .or_else(|| std::env::var(PROFILE_DIR_ENV).ok().filter(|s| !s.is_empty()));
            if let Some(dir) = profile_dir {
                let sha = git_sha();
                append_step_profiles(
                    Path::new(&dir),
                    &result.step_profile_rows,
                    &sha,
                    jobs,
                    None,
                    "unverified",
                    LEDGER_PRODUCER,
                );
                eprintln!(
                    "validate.rs: forwarded {} step profile row(s) to {dir}",
                    result.step_profile_rows.len()
                );
            }

            let gates: Vec<GateOutcome> =
                result.outcomes.iter().map(GateOutcome::from).collect();
            let selection_mode = if args.dag_file.is_some() { "override" } else { "full" };
            (gates, result.skipped.clone(), result.wall_s, result.ok, selection_mode)
        };

    let finished_at = utc_now();

    // Structural verdict — never text-grep. A genuine, non-aborted failing gate is
    // the honest failure; `ok` is the library/subsumption's own no-failure verdict.
    let failures = gates.iter().filter(|o| !o.ok && !o.aborted).count();
    let exit_code: u8 = if ok && failures == 0 { 0 } else { 1 };

    print_cost_table(&gates, &skipped);

    // Ledger — always writes (checkout-local default when no env override), at the
    // single write point that carries every qualification with the value, including
    // the absolute durable log path for finalize_receipt.py --scan / verify_receipt.sh.
    let commit = git_sha();
    let dirty = tree_dirty();
    let log_path = durable.path.clone();
    write_ledger_record(
        &ledger_path(&root),
        &started_at,
        &finished_at,
        &args.profile,
        &gates,
        &skipped,
        wall,
        ok,
        log_path.to_str(),
        exit_code,
        &commit,
        dirty,
        selection_mode,
    );

    let verdict = if exit_code == 0 { "PASS" } else { "FAIL" };
    eprintln!(
        "validate.rs: {verdict} - {} executed, {failures} failed, {} skipped in {:.1}s",
        gates.len(),
        skipped.len(),
        wall
    );

    // Flush + restore fds + reap the tee BEFORE returning, so the durable log is
    // complete on disk when the process exits.
    durable.finish();

    ExitCode::from(exit_code)
}
