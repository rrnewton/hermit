#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Manifest-driven e2e test harness (CI Overhaul v2).
//!
//! Where [`manifest-plan.rs`](manifest-plan.rs) is the *loader/validator/planner*
//! (it parses `tests/e2e/manifests/*.toml`, enforces the schema-v2 rules from
//! `README.md`, and prints the expanded `(test × mode × backend)` plan), this
//! script is the *builder/runner/DAG-generator* layered on top of it. It adds the
//! three capabilities the loader deliberately leaves out — the pieces this task
//! (`ci-ov2-harness-builder`) is responsible for:
//!
//!   1. **Implicit build dispatch.** A `[[test]]`'s `program` extension selects
//!      the runner and the build: `*.sh` runs directly (its `--prepare`/`--run`
//!      protocol), `*.c` is compiled with `cc`, `*.rs` is compiled with `rustc`.
//!      This is what unlocks the ~170 wrapper-free `tests/c/*.c` guests that the
//!      v1 `ci/test_harness.sh` (which discovers only annotated `*.sh`) cannot
//!      reach. Per-test `[test.build]` `cflags` / `extra_sources` are honored.
//!   2. **An executing runner.** `run` builds a manifest entry and executes every
//!      enabled `(mode, backend)` cell — `verify`, `chaos`, `replay`, `naked`,
//!      and `custom` — with the same Hermit invocations and portable-lane profile
//!      (`--no-virtualize-cpuid --max-timeslice=disabled`) as the v1 bash harness,
//!      then reports PASS/FAIL per cell. `--dry-run` prints the commands instead.
//!   3. **DAG generation.** `dag` emits a `safe-ci-dag-runner` plan (the exact
//!      `ci/dag/*.json` shape: `resource_caps` / `steps` with `group`/`job`/
//!      `cmd`/`deps`/`timeout`/`hint`) so manifest buckets run as boxed,
//!      dependency-ordered nodes via `ci/run-dag.sh`.
//!
//! Validation is delegated to `manifest-plan.rs` so the schema rules live in one
//! place; `validate`/`plan` here are thin proxies to it.
//!
//! Usage:
//!   ./manifest-harness.rs validate
//!   ./manifest-harness.rs plan [--format text|json] [--lane L]
//!   ./manifest-harness.rs build <test-id> [--out DIR]
//!   ./manifest-harness.rs build --all [--lane L] [--out DIR]
//!   ./manifest-harness.rs run   <test-id> [--mode M] [--backend B] [--dry-run]
//!   ./manifest-harness.rs run   --bucket B --lane L [--prebuilt] [--dry-run]
//!       [--results PATH] [--junit PATH]
//!   ./manifest-harness.rs run   --all --lane L [--prebuilt] [--dry-run]
//!   ./manifest-harness.rs dag   [--lane portable|privileged] [--format json]
//!
//! Environment:
//!   HERMIT_BIN   Hermit binary for run cells (default: target/debug/hermit).
//!
//! ```cargo
//! [dependencies]
//! toml = "0.8"
//! ```

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::exit;
use std::process::Command;
use std::time::Instant;

use toml::Value;

const KNOWN_BACKENDS: [&str; 5] = ["ptrace", "dbi", "kvm", "sabre", "liteinst"];
const ACCOUNTED_MODES: [&str; 4] = ["verify", "chaos", "replay", "naked"];

fn die(msg: String) -> ! {
    eprintln!("manifest-harness: {msg}");
    exit(2);
}

// ---------------------------------------------------------------------------
// Parsed model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ModeSpec {
    name: String,
    backends: Vec<String>, // empty for `naked`
    seeds: Vec<i64>,       // chaos only
    runs: i64,             // naked/custom
    min_distinct: i64,
    min_passes: i64,
    min_failures: i64,
    repeat_identical: bool, // custom
    args: Vec<String>,      // custom extra hermit args
}

#[derive(Debug, Clone)]
struct TestEntry {
    bucket: String,
    id: String,
    lane: String,
    timeout: i64,
    program: Option<String>,
    direct: Option<String>,
    cflags: Vec<String>,
    extra_sources: Vec<String>,
    modes: Vec<ModeSpec>,
}

fn as_str_vec(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

fn as_i64_vec(v: Option<&Value>) -> Vec<i64> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_integer).collect())
        .unwrap_or_default()
}

fn repo_root() -> PathBuf {
    let script_dir = Path::new(file!())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // tests/e2e/manifests/../../.. == repo root
    let root = script_dir.join("../../..");
    root.canonicalize().unwrap_or(root)
}

fn manifests_dir() -> PathBuf {
    let script_dir = Path::new(file!())
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    script_dir.canonicalize().unwrap_or(script_dir)
}

fn load_entries() -> Vec<TestEntry> {
    let dir = manifests_dir();
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| die(format!("cannot read {}: {e}", dir.display())))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    manifests.sort();

    let mut entries = Vec::new();
    for path in &manifests {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| die(format!("cannot read {}: {e}", path.display())));
        let doc: Value = text
            .parse()
            .unwrap_or_else(|e| die(format!("{}: invalid TOML: {e}", path.display())));
        let bucket = doc
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or_else(|| die(format!("{}: missing `bucket`", path.display())))
            .to_string();
        let tests = doc
            .get("test")
            .and_then(Value::as_array)
            .unwrap_or_else(|| die(format!("{}: missing [[test]]", path.display())));
        for t in tests {
            entries.push(parse_entry(t, &bucket));
        }
    }
    entries
}

fn parse_entry(t: &Value, bucket: &str) -> TestEntry {
    let id = t
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_else(|| die(format!("{bucket}: a [[test]] is missing `id`")))
        .to_string();
    let lane = t.get("lane").and_then(Value::as_str).unwrap_or("portable").to_string();
    let timeout = t.get("timeout_seconds").and_then(Value::as_integer).unwrap_or(60);
    let program = t.get("program").and_then(Value::as_str).map(String::from);
    let direct = t.get("direct").and_then(Value::as_str).map(String::from);

    let build = t.get("build").and_then(Value::as_table);
    let cflags = build.map(|b| as_str_vec(b.get("cflags"))).unwrap_or_default();
    let extra_sources = build.map(|b| as_str_vec(b.get("extra_sources"))).unwrap_or_default();

    let mut modes = Vec::new();
    if let Some(mt) = t.get("modes").and_then(Value::as_table) {
        for (name, spec) in mt {
            let assert = spec.get("assert").and_then(Value::as_table);
            let ai = |k: &str, d: i64| {
                assert
                    .and_then(|a| a.get(k))
                    .and_then(Value::as_integer)
                    .unwrap_or(d)
            };
            modes.push(ModeSpec {
                name: name.clone(),
                backends: as_str_vec(spec.get("backends_enabled")),
                seeds: as_i64_vec(spec.get("seeds")),
                runs: spec.get("runs").and_then(Value::as_integer).unwrap_or(3),
                min_distinct: ai("min_distinct", 2),
                min_passes: ai("min_passes", 0),
                min_failures: ai("min_failures", 0),
                repeat_identical: assert
                    .and_then(|a| a.get("repeat_identical"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                args: as_str_vec(spec.get("args")),
            });
        }
    }
    modes.sort_by(|a, b| a.name.cmp(&b.name));

    TestEntry {
        bucket: bucket.to_string(),
        id,
        lane,
        timeout,
        program,
        direct,
        cflags,
        extra_sources,
        modes,
    }
}

fn find_entry(entries: &[TestEntry], id: &str) -> TestEntry {
    entries
        .iter()
        .find(|e| e.id == id)
        .cloned()
        .unwrap_or_else(|| die(format!("no test with id `{id}` in any manifest")))
}

// ---------------------------------------------------------------------------
// Build dispatch (the core new capability)
// ---------------------------------------------------------------------------

/// How a built entry is executed.
enum Program {
    /// Inline `direct = "…"` shell one-liner.
    Direct(String),
    /// A `*.sh` wrapper honoring the `--prepare`/`--run` protocol (absolute path).
    Script(PathBuf),
    /// A compiled `*.c`/`*.rs` guest (absolute binary path).
    Binary(PathBuf),
}

fn sanitized(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn run_tool(desc: &str, mut cmd: Command) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| die(format!("{desc}: failed to spawn: {e}")));
    if !status.success() {
        die(format!("{desc}: exited with {status}"));
    }
}

/// Build (or resolve) the entry's program, returning how to run it.
fn build_program(entry: &TestEntry, out_dir: &Path, dry_run: bool, prebuilt: bool) -> Program {
    if let Some(cmd) = &entry.direct {
        return Program::Direct(cmd.clone());
    }
    let root = repo_root();
    let program = entry
        .program
        .as_ref()
        .unwrap_or_else(|| die(format!("{}: has neither `program` nor `direct`", entry.id)));
    let abs = root.join(program);
    if !abs.exists() {
        die(format!("{}: program path does not exist: {program}", entry.id));
    }
    let ext = Path::new(program)
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("");
    match ext {
        "sh" => Program::Script(abs),
        "c" => {
            std::fs::create_dir_all(out_dir).ok();
            let out = out_dir.join(sanitized(&entry.id));
            if prebuilt {
                if !dry_run && !out.is_file() {
                    die(format!(
                        "{}: prebuilt guest is missing: {}",
                        entry.id,
                        out.display()
                    ));
                }
                return Program::Binary(out);
            }
            // README default: cc -std=c11 -O2 -g -Wall -Wextra -Werror [+ per-test cflags]
            let mut args: Vec<String> = vec![
                "-std=c11".into(),
                "-O2".into(),
                "-g".into(),
                "-Wall".into(),
                "-Wextra".into(),
                "-Werror".into(),
            ];
            args.extend(entry.cflags.iter().cloned());
            args.push(abs.to_string_lossy().to_string());
            for src in &entry.extra_sources {
                args.push(root.join(src).to_string_lossy().to_string());
            }
            args.push("-o".into());
            args.push(out.to_string_lossy().to_string());
            let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
            if dry_run {
                println!("{cc} {}", args.join(" "));
            } else {
                let mut c = Command::new(&cc);
                c.args(&args);
                run_tool(&format!("cc {}", entry.id), c);
            }
            Program::Binary(out)
        }
        "rs" => {
            std::fs::create_dir_all(out_dir).ok();
            let out = out_dir.join(sanitized(&entry.id));
            if prebuilt {
                if !dry_run && !out.is_file() {
                    die(format!(
                        "{}: prebuilt guest is missing: {}",
                        entry.id,
                        out.display()
                    ));
                }
                return Program::Binary(out);
            }
            let mut args: Vec<String> = vec!["-O".into()];
            args.extend(entry.cflags.iter().cloned()); // extra rustc flags reuse cflags
            args.push(abs.to_string_lossy().to_string());
            args.push("-o".into());
            args.push(out.to_string_lossy().to_string());
            if dry_run {
                println!("rustc {}", args.join(" "));
            } else {
                let mut c = Command::new("rustc");
                c.args(&args);
                run_tool(&format!("rustc {}", entry.id), c);
            }
            Program::Binary(out)
        }
        other => die(format!("{}: unsupported program extension `.{other}`", entry.id)),
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

fn hermit_bin() -> String {
    std::env::var("HERMIT_BIN")
        .unwrap_or_else(|_| repo_root().join("target/debug/hermit").to_string_lossy().to_string())
}

/// The argv fragment that names the guest to execute for each run mode.
fn guest_argv(prog: &Program) -> Vec<String> {
    match prog {
        Program::Direct(cmd) => vec!["sh".into(), "-c".into(), cmd.clone()],
        Program::Script(p) => vec![p.to_string_lossy().to_string(), "--run".into()],
        Program::Binary(p) => vec![p.to_string_lossy().to_string()],
    }
}

struct Attempt {
    status: i32,
    stdout: Vec<u8>,
}

struct CellResult {
    test: String,
    bucket: String,
    lane: String,
    mode: String,
    backend: String,
    outcome: String,
    duration_ms: u128,
    reason: String,
}

fn cell_env(cell: &Path, isolated_guest_tmp: bool) -> Vec<(String, String)> {
    let tmpdir = if isolated_guest_tmp {
        PathBuf::from("/tmp/hermit-e2e")
    } else {
        cell.join("tmp")
    };
    vec![
        ("LC_ALL".into(), "C".into()),
        ("TZ".into(), "UTC".into()),
        ("HOME".into(), cell.join("home").to_string_lossy().to_string()),
        (
            "XDG_CONFIG_HOME".into(),
            cell.join("xdg-config").to_string_lossy().to_string(),
        ),
        ("E2E_TMPDIR".into(), tmpdir.to_string_lossy().to_string()),
        (
            "E2E_FIXTURE_DIR".into(),
            cell.join("fixtures").to_string_lossy().to_string(),
        ),
    ]
}

fn run_argv(argv: &[String], env: &[(String, String)], dry_run: bool) -> Attempt {
    if dry_run {
        println!("{}", argv.join(" "));
        return Attempt { status: 0, stdout: Vec::new() };
    }
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c
        .output()
        .unwrap_or_else(|e| die(format!("failed to run {}: {e}", argv[0])));
    Attempt {
        status: out.status.code().unwrap_or(-1),
        stdout: out.stdout,
    }
}

/// Build the hermit argv for a `(mode, backend)` cell given the guest argv.
fn hermit_argv(
    mode: &str,
    backend: &str,
    lane: &str,
    seed: Option<i64>,
    extra: &[String],
    guest: &[String],
) -> Vec<String> {
    let hb = hermit_bin();
    let portable_profile = lane == "portable" && mode != "naked";
    let profile = |v: &mut Vec<String>| {
        if portable_profile {
            v.push("--no-virtualize-cpuid".into());
            v.push("--max-timeslice=disabled".into());
        }
    };
    let mut a: Vec<String> = Vec::new();
    match mode {
        "naked" => return guest.to_vec(),
        "verify" => {
            a.extend([hb, "--log=info".into(), "run".into(), "--backend".into(), backend.into(), "--strict".into(), "--verify".into()]);
            profile(&mut a);
        }
        "replay" => {
            a.extend([hb, "--log=info".into(), "--backend".into(), backend.into(), "record".into(), "start".into(), "--strict".into(), "--verify".into()]);
        }
        "chaos" => {
            a.extend([hb, "--log=off".into(), "run".into(), "--backend".into(), backend.into(), "--strict".into(), "--chaos".into(), "--sched-heuristic=random".into(), format!("--seed={}", seed.unwrap_or(0))]);
            profile(&mut a);
        }
        "custom" => {
            // `custom` is fully explicit: the manifest's `args` control every
            // determinism flag, so the portable profile is NOT auto-appended
            // (doing so would duplicate flags the author already specified).
            a.extend([hb, "--log=info".into(), "run".into(), "--backend".into(), backend.into(), "--strict".into()]);
            a.extend(extra.iter().cloned());
        }
        other => die(format!("unsupported mode `{other}`")),
    }
    a.push("--".into());
    a.extend(guest.iter().cloned());
    a
}

/// Prepare a cell working directory mirroring ci/test_harness.sh.
fn prepare_cell(cell: &Path) {
    for sub in ["home", "xdg-config", "tmp", "fixtures", "recording", "captures"] {
        std::fs::create_dir_all(cell.join(sub)).ok();
    }
    let xdg = repo_root().join("tests/e2e/xdg-config");
    if xdg.is_dir() {
        // best-effort copy of the shared xdg-config skeleton
        let mut c = Command::new("cp");
        c.arg("-a").arg(format!("{}/.", xdg.display())).arg(cell.join("xdg-config"));
        let _ = c.status();
    }
}

fn run_entry(
    entry: &TestEntry,
    mode_filter: &str,
    backend_filter: &str,
    dry_run: bool,
    prebuilt: bool,
    results: &mut Vec<CellResult>,
) -> bool {
    let out_dir = repo_root().join("target/e2e-harness/build");
    let prog = build_program(entry, &out_dir, dry_run, prebuilt);
    let cell = repo_root().join(format!("target/e2e-harness/runs/{}", sanitized(&entry.id)));
    if !dry_run {
        prepare_cell(&cell);
        // .sh wrappers build their sibling .c via --prepare.
        if let Program::Script(p) = &prog {
            let env = cell_env(&cell, false);
            let mut c = Command::new(p);
            c.arg("--prepare");
            for (k, v) in &env {
                c.env(k, v);
            }
            let st = c.status().unwrap_or_else(|e| die(format!("prepare {}: {e}", entry.id)));
            if !st.success() {
                println!("ERROR {} - fixture preparation failed", entry.id);
                results.push(CellResult {
                    test: entry.id.clone(),
                    bucket: entry.bucket.clone(),
                    lane: entry.lane.clone(),
                    mode: "prepare".into(),
                    backend: String::new(),
                    outcome: "ERROR".into(),
                    duration_ms: 0,
                    reason: "fixture preparation failed".into(),
                });
                return false;
            }
        }
    }
    let guest = guest_argv(&prog);
    let mut all_pass = true;

    for m in &entry.modes {
        if !mode_filter.is_empty() && m.name != mode_filter {
            continue;
        }
        // Hermit's built-in verification executes the guest twice. Keep its
        // scratch path inside the fresh guest /tmp so run one cannot mutate a
        // host-backed directory observed by run two.
        let env = cell_env(&cell, m.name != "naked");
        if m.name == "naked" {
            let started = Instant::now();
            let mut seen: BTreeSet<(i32, Vec<u8>)> = BTreeSet::new();
            for _ in 0..m.runs.max(2) {
                let at = run_argv(&guest, &env, dry_run);
                seen.insert((at.status, at.stdout));
            }
            let distinct = if dry_run { m.min_distinct } else { seen.len() as i64 };
            let pass = distinct >= m.min_distinct;
            all_pass &= pass;
            report(
                pass,
                entry,
                "naked",
                "native",
                &format!("distinct={distinct} need>={}", m.min_distinct),
                started.elapsed().as_millis(),
                results,
            );
            continue;
        }
        let backends = &m.backends;
        for b in backends {
            if !backend_filter.is_empty() && b != backend_filter {
                continue;
            }
            let started = Instant::now();
            match m.name.as_str() {
                "verify" | "custom" => {
                    let repeats = if m.name == "custom" { m.runs.max(1) } else { 1 };
                    let mut first: Option<(i32, Vec<u8>)> = None;
                    let mut pass = true;
                    for _ in 0..repeats {
                        let argv = hermit_argv(&m.name, b, &entry.lane, None, &m.args, &guest);
                        let at = run_argv(&argv, &env, dry_run);
                        if at.status != 0 {
                            pass = false;
                        }
                        if m.repeat_identical {
                            match &first {
                                None => first = Some((at.status, at.stdout)),
                                Some(f) => {
                                    if *f != (at.status, at.stdout) {
                                        pass = false;
                                    }
                                }
                            }
                        }
                    }
                    all_pass &= pass;
                    report(
                        pass,
                        entry,
                        &m.name,
                        b,
                        "",
                        started.elapsed().as_millis(),
                        results,
                    );
                }
                "replay" => {
                    let argv = hermit_argv("replay", b, &entry.lane, None, &[], &guest);
                    let at = run_argv(&argv, &env, dry_run);
                    let pass = at.status == 0;
                    all_pass &= pass;
                    report(
                        pass,
                        entry,
                        "replay",
                        b,
                        "",
                        started.elapsed().as_millis(),
                        results,
                    );
                }
                "chaos" => {
                    let seeds = if m.seeds.is_empty() { vec![0, 1] } else { m.seeds.clone() };
                    let mut distinct: BTreeSet<(i32, Vec<u8>)> = BTreeSet::new();
                    let (mut passes, mut failures, mut mism) = (0i64, 0i64, 0i64);
                    for s in &seeds {
                        let argv = hermit_argv("chaos", b, &entry.lane, Some(*s), &[], &guest);
                        let a1 = run_argv(&argv, &env, dry_run);
                        let a2 = run_argv(&argv, &env, dry_run);
                        if a1.status == 0 { passes += 1 } else { failures += 1 }
                        if (a1.status, &a1.stdout) != (a2.status, &a2.stdout) {
                            mism += 1;
                        }
                        distinct.insert((a1.status, a1.stdout));
                    }
                    let d = if dry_run { m.min_distinct } else { distinct.len() as i64 };
                    let pass = mism == 0 && d >= m.min_distinct && passes >= m.min_passes && failures >= m.min_failures;
                    all_pass &= pass;
                    report(
                        pass,
                        entry,
                        "chaos",
                        b,
                        &format!("distinct={d} passes={passes} failures={failures} repeat_mismatch={mism}"),
                        started.elapsed().as_millis(),
                        results,
                    );
                }
                other => die(format!("{}: unsupported mode `{other}`", entry.id)),
            }
        }
    }
    all_pass
}

fn report(
    pass: bool,
    entry: &TestEntry,
    mode: &str,
    backend: &str,
    note: &str,
    duration_ms: u128,
    results: &mut Vec<CellResult>,
) {
    let tag = if pass { "PASS" } else { "FAIL" };
    let suffix = if note.is_empty() { String::new() } else { format!(" - {note}") };
    println!("{tag:<5} {:<10} {mode:<7} {backend:<8} {}{suffix}", entry.lane, entry.id);
    results.push(CellResult {
        test: entry.id.clone(),
        bucket: entry.bucket.clone(),
        lane: entry.lane.clone(),
        mode: mode.into(),
        backend: backend.into(),
        outcome: tag.into(),
        duration_ms,
        reason: note.into(),
    });
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn write_results(results: &[CellResult], results_path: Option<&Path>, junit_path: Option<&Path>) {
    if let Some(path) = results_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| die(format!("cannot create {}: {e}", parent.display())));
        }
        let mut file = File::create(path)
            .unwrap_or_else(|e| die(format!("cannot create {}: {e}", path.display())));
        for result in results {
            writeln!(
                file,
                "{{\"schema\":1,\"test\":{},\"category\":{},\"lane\":{},\"mode\":{},\"backend\":{},\"outcome\":{},\"duration_ms\":{},\"reason\":{}}}",
                json_str(&result.test),
                json_str(&result.bucket),
                json_str(&result.lane),
                json_str(&result.mode),
                json_str(&result.backend),
                json_str(&result.outcome),
                result.duration_ms,
                json_str(&result.reason),
            )
            .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
        }
        let passed = results.iter().filter(|r| r.outcome == "PASS").count();
        let failed = results.iter().filter(|r| r.outcome == "FAIL").count();
        let errors = results.iter().filter(|r| r.outcome == "ERROR").count();
        let summary = path.parent().unwrap_or(Path::new(".")).join("summary.json");
        std::fs::write(
            &summary,
            format!(
                "{{\"schema\":1,\"tests\":{},\"cells\":{},\"passed\":{passed},\"failed\":{failed},\"errors\":{errors}}}\n",
                results.iter().map(|r| &r.test).collect::<BTreeSet<_>>().len(),
                results.len(),
            ),
        )
        .unwrap_or_else(|e| die(format!("cannot write {}: {e}", summary.display())));
    }

    if let Some(path) = junit_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| die(format!("cannot create {}: {e}", parent.display())));
        }
        let failures = results.iter().filter(|r| r.outcome == "FAIL").count();
        let errors = results.iter().filter(|r| r.outcome == "ERROR").count();
        let mut file = File::create(path)
            .unwrap_or_else(|e| die(format!("cannot create {}: {e}", path.display())));
        writeln!(
            file,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"hermit-e2e\" tests=\"{}\" failures=\"{failures}\" errors=\"{errors}\">",
            results.len()
        )
        .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
        for result in results {
            let backend = if result.backend.is_empty() { "none" } else { &result.backend };
            write!(
                file,
                "  <testcase classname=\"{}\" name=\"{}/{}/{}\" time=\"{:.3}\">",
                xml_escape(&result.bucket),
                xml_escape(&result.test),
                xml_escape(&result.mode),
                xml_escape(backend),
                result.duration_ms as f64 / 1000.0,
            )
            .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
            if result.outcome == "FAIL" {
                write!(file, "<failure>{}</failure>", xml_escape(&result.reason))
                    .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
            } else if result.outcome == "ERROR" {
                write!(file, "<error>{}</error>", xml_escape(&result.reason))
                    .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
            }
            writeln!(file, "</testcase>")
                .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
        }
        writeln!(file, "</testsuite>")
            .unwrap_or_else(|e| die(format!("cannot write {}: {e}", path.display())));
    }
}

// ---------------------------------------------------------------------------
// DAG generation
// ---------------------------------------------------------------------------

fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            _ => o.push(c),
        }
    }
    o.push('"');
    o
}

fn emit_dag(entries: &[TestEntry], lane: &str) {
    // Buckets that have at least one test in the requested lane.
    let mut buckets: Vec<String> = entries
        .iter()
        .filter(|e| e.lane == lane)
        .map(|e| e.bucket.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    buckets.sort();

    let mut steps: Vec<String> = Vec::new();
    // Build the workspace (provides the hermit binary the run cells need).
    steps.push(String::from(
        r#"    {"group":"build","job":"workspace","desc":"Build workspace (cargo build --workspace)","cmd":"cargo build --workspace","timeout":1200,"hint":{"est_duration_s":360,"rss_baseline_bytes":5368709120,"hard_mem_max_bytes":8589934592,"classification":"cpu-bound"}}"#,
    ));
    // Validate manifests (schema rules enforced by manifest-plan.rs).
    steps.push(String::from(
        r#"    {"group":"e2e","job":"manifest_validate","desc":"Validate centralized e2e manifests (schema v2)","cmd":"./tests/e2e/manifests/manifest-plan.rs","timeout":60,"hint":{"est_duration_s":5,"rss_baseline_bytes":268435456,"hard_mem_max_bytes":1073741824,"classification":"light"}}"#,
    ));
    steps.push(format!(
        "    {{\"group\":\"build\",\"job\":\"manifest_guests\",\"desc\":\"Build all direct manifest guests ({lane} lane)\",\"cmd\":\"./tests/e2e/manifests/manifest-harness.rs build --all --lane {lane}\",\"deps\":[\"e2e.manifest_validate\"],\"timeout\":1800,\"hint\":{{\"est_duration_s\":180,\"rss_baseline_bytes\":1073741824,\"hard_mem_max_bytes\":3221225472,\"classification\":\"cpu-bound\"}}}}"
    ));
    // One independently schedulable run node per bucket. The node timeout is
    // the sum of the bucket's per-test timeouts (tests remain serial within a
    // bucket), floored at the DAG default.
    for b in &buckets {
        let job = format!("manifest_{}", b.replace('-', "_"));
        let cmd = format!(
            "./tests/e2e/manifests/manifest-harness.rs run --bucket {b} --lane {lane} --prebuilt --results target/e2e/{lane}/{b}/results.jsonl --junit target/e2e/{lane}/{b}/junit.xml"
        );
        let desc = format!("Manifest bucket `{b}` ({lane} lane): run all enabled cells with prebuilt guests");
        let bucket_timeout: i64 = entries
            .iter()
            .filter(|e| e.bucket == *b && e.lane == lane)
            .map(|e| e.timeout)
            .sum::<i64>()
            .max(600);
        steps.push(format!(
            "    {{\"group\":\"e2e\",\"job\":{job},\"desc\":{desc},\"cmd\":{cmd},\"deps\":[\"build.workspace\",\"build.manifest_guests\",\"e2e.manifest_validate\"],\"timeout\":{bucket_timeout},\"hint\":{{\"est_duration_s\":150,\"rss_baseline_bytes\":1073741824,\"hard_mem_max_bytes\":3221225472,\"classification\":\"latency-bound\"}}}}",
            job = json_str(&job),
            desc = json_str(&desc),
            cmd = json_str(&cmd),
        ));
    }

    println!("{{");
    // Bucket cells use disjoint run directories and only read prebuilt guests.
    // Let the runner's -j / memory limits schedule independent buckets in parallel.
    println!("  \"resource_caps\": {{}},");
    println!("  \"mem_cap_factor\": 1.25,");
    println!("  \"mem_cap_floor_bytes\": 8589934592,");
    println!("  \"outer_mem_safety_factor\": 1.0,");
    println!("  \"default_step_timeout\": 600,");
    println!("  \"steps\": [");
    println!("{}", steps.join(",\n"));
    println!("  ]");
    println!("}}");
}

// ---------------------------------------------------------------------------
// Sub-commands
// ---------------------------------------------------------------------------

fn delegate_plan(args: &[String]) {
    let plan = manifests_dir().join("manifest-plan.rs");
    let mut c = Command::new(&plan);
    c.args(args);
    let status = c.status().unwrap_or_else(|e| {
        // Fall back to invoking rust-script explicitly if the file is not +x.
        let mut r = Command::new("rust-script");
        r.arg(&plan).args(args);
        r.status().unwrap_or_else(|_| die(format!("cannot run manifest-plan.rs: {e}")))
    });
    exit(status.code().unwrap_or(2));
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let sub = argv.get(1).map(String::as_str).unwrap_or("");
    let rest = &argv[2.min(argv.len())..];

    match sub {
        "validate" => {
            // manifest-plan.rs prints "PASS: …" to stderr on success.
            delegate_plan(&[]);
        }
        "plan" => {
            delegate_plan(rest);
        }
        "build" => {
            let mut id = String::new();
            let mut bucket = String::new();
            let mut lane = String::new();
            let mut out = repo_root().join("target/e2e-harness/build");
            let mut all = false;
            let mut dry = false;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--out" => {
                        out = PathBuf::from(rest.get(i + 1).cloned().unwrap_or_else(|| die("--out needs a value".into())));
                        i += 2;
                    }
                    "--all" => {
                        all = true;
                        i += 1;
                    }
                    "--bucket" => {
                        bucket = rest.get(i + 1).cloned().unwrap_or_else(|| die("--bucket needs a value".into()));
                        i += 2;
                    }
                    "--lane" => {
                        lane = rest.get(i + 1).cloned().unwrap_or_else(|| die("--lane needs a value".into()));
                        i += 2;
                    }
                    "--dry-run" => {
                        dry = true;
                        i += 1;
                    }
                    s if !s.starts_with("--") => {
                        id = s.to_string();
                        i += 1;
                    }
                    s => die(format!("build: unknown option {s}")),
                }
            }
            let entries = load_entries();
            let selected: Vec<TestEntry> = if !id.is_empty() {
                if all || !bucket.is_empty() || !lane.is_empty() {
                    die("build: <test-id> cannot be combined with --all, --bucket, or --lane".into());
                }
                vec![find_entry(&entries, &id)]
            } else if all || !bucket.is_empty() {
                entries
                    .into_iter()
                    .filter(|e| bucket.is_empty() || e.bucket == bucket)
                    .filter(|e| lane.is_empty() || e.lane == lane)
                    .collect()
            } else {
                die("build: needs a <test-id>, --all, or --bucket".into());
            };
            if selected.is_empty() {
                die("build: selection matched no tests".into());
            }
            let mut compiled = 0;
            for entry in &selected {
                match build_program(entry, &out, dry, false) {
                    Program::Direct(_) | Program::Script(_) => {}
                    Program::Binary(p) => {
                        compiled += 1;
                        println!("binary: {}", p.display());
                    }
                }
            }
            println!(
                "PASS: {} manifest entries selected; {compiled} compiled guest(s)",
                selected.len()
            );
        }
        "run" => {
            let mut id = String::new();
            let mut bucket = String::new();
            let mut lane = String::new();
            let mut mode = String::new();
            let mut backend = String::new();
            let mut dry = false;
            let mut prebuilt = false;
            let mut all = false;
            let mut results_path: Option<PathBuf> = None;
            let mut junit_path: Option<PathBuf> = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--bucket" => { bucket = rest[i + 1].clone(); i += 2; }
                    "--lane" => { lane = rest[i + 1].clone(); i += 2; }
                    "--mode" => { mode = rest[i + 1].clone(); i += 2; }
                    "--backend" => { backend = rest[i + 1].clone(); i += 2; }
                    "--dry-run" => { dry = true; i += 1; }
                    "--prebuilt" => { prebuilt = true; i += 1; }
                    "--all" => { all = true; i += 1; }
                    "--results" => {
                        results_path = Some(PathBuf::from(
                            rest.get(i + 1).cloned().unwrap_or_else(|| die("--results needs a value".into())),
                        ));
                        i += 2;
                    }
                    "--junit" => {
                        junit_path = Some(PathBuf::from(
                            rest.get(i + 1).cloned().unwrap_or_else(|| die("--junit needs a value".into())),
                        ));
                        i += 2;
                    }
                    s if !s.starts_with("--") => { id = s.to_string(); i += 1; }
                    s => die(format!("run: unknown option {s}")),
                }
            }
            let entries = load_entries();
            let selected: Vec<TestEntry> = if !id.is_empty() {
                if all || !bucket.is_empty() {
                    die("run: <test-id> cannot be combined with --all or --bucket".into());
                }
                vec![find_entry(&entries, &id)]
            } else if !bucket.is_empty() {
                if all {
                    die("run: --all cannot be combined with --bucket".into());
                }
                entries
                    .into_iter()
                    .filter(|e| e.bucket == bucket && (lane.is_empty() || e.lane == lane))
                    .collect()
            } else if all {
                entries
                    .into_iter()
                    .filter(|e| lane.is_empty() || e.lane == lane)
                    .collect()
            } else {
                die("run: needs a <test-id>, --bucket, or --all".into());
            };
            if selected.is_empty() {
                die("run: selection matched no tests".into());
            }
            let mut failures = 0;
            let mut cell_results = Vec::new();
            for e in &selected {
                if !run_entry(e, &mode, &backend, dry, prebuilt, &mut cell_results) {
                    failures += 1;
                }
            }
            write_results(
                &cell_results,
                results_path.as_deref(),
                junit_path.as_deref(),
            );
            exit(if failures == 0 { 0 } else { 1 });
        }
        "dag" => {
            let mut lane = String::from("portable");
            let mut i = 0;
            while i < rest.len() {
                match rest[i].as_str() {
                    "--lane" => { lane = rest[i + 1].clone(); i += 2; }
                    "--format" => { i += 2; } // json is the only format
                    s => die(format!("dag: unknown option {s}")),
                }
            }
            if lane != "portable" && lane != "privileged" {
                die(format!("dag: lane must be portable|privileged, got `{lane}`"));
            }
            let entries = load_entries();
            emit_dag(&entries, &lane);
        }
        "" | "-h" | "--help" => {
            eprintln!(
                "usage: manifest-harness.rs <validate|plan|build|run|dag> [options]\n\
                 see the header of this script for full option docs."
            );
            exit(if sub.is_empty() { 2 } else { 0 });
        }
        other => die(format!("unknown subcommand `{other}` (try validate|plan|build|run|dag)")),
    }

    // Silence unused-const warnings when a build only touches some paths.
    let _ = (KNOWN_BACKENDS, ACCOUNTED_MODES);
}
