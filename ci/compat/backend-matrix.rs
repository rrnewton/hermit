#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Drive ONE backend-agnostic corpus across EVERY backend, through the front
//! door, and emit FOUR-STATE cells.
//!
//! # The front door, and why the structure is backend-agnostic
//!
//! Every cell is the same user-facing invocation with one token changed:
//!
//! ```text
//! hermit run --backend <b> --strict --verify [--verify-strict] --verify-json <f> -- <argv>
//! ```
//!
//! No backend gets a bespoke code path, a bespoke corpus file, or a bespoke test
//! function. Outcomes differ per backend — that is expected and is the thing
//! being measured — but the STRUCTURE does not. That is the whole point: adding a
//! seventh backend must mean adding a name to `BACKENDS`, not authoring a new
//! harness.
//!
//! # Four states, and why two is a lie
//!
//! A two-state table cannot distinguish a backend that HANDLES a program from one
//! that NEVER RAN IT. Both render as "not failing". This tool refuses to collapse
//! them:
//!
//! | state | meaning | how it is decided |
//! | --- | --- | --- |
//! | `NOT_ATTEMPTED` | no result exists, with a reason | backend prerequisites unmet, or the corpus records an exclusion, or the program is absent from every legacy mode with no reason recorded (`unexplained`) |
//! | `ATTEMPTED_UNQUALIFIABLE` | it ran and agreed, but at a tier that cannot certify | `verified` yet `bitwise_parity != true`, or zero log messages compared |
//! | `PASS` | ran and agreed at a qualifying tier | `verified && bitwise_parity && compared > 0` |
//! | `FAIL` / `TIMEOUT` | ran and disagreed, or exceeded its bound | non-zero exit, `verified=false`, or the wall bound |
//!
//! **`ATTEMPTED_UNQUALIFIABLE` is not pedantry — it is measured today.** On this
//! host `hermit --backend kvm run --strict --verify --verify-strict` on
//! `/bin/echo hi` returns **rc=0** with `verified: true` and
//! `compared_log_messages: null` — it compared NOTHING and still reported
//! success, because KVM's `--verify` is an output-only fallback
//! (`bitwise_parity: false`). ptrace on the identical guest returns
//! `bitwise_parity: true` over 302 compared messages. Under any two-state
//! reading those two cells are the same green. They are not the same green.
//!
//! # Binding, not string-matching
//!
//! State is derived from `--verify-json` — the product's own structured verdict
//! (`verified`, `bitwise_parity`, `verdict`, `compared_log_messages`,
//! `comparison.strictness`) — never from grepping stdout. A log line is a
//! correlated label; the JSON is the thing itself. Availability likewise comes
//! from Hermit's own refusal (`backend '<b>' is unavailable: <reason>`), which is
//! `Backend::unavailable_reason()` surfacing at the front door.
//!
//! # The prerequisite/function gap, handled explicitly
//!
//! `Backend::is_available()` checks PREREQUISITES, never FUNCTION: `/dev/kvm`
//! being openable does not mean KVM runs. So availability is decided by an actual
//! `/bin/true` smoke, and a backend whose prerequisites are met but which cannot
//! run `/bin/true` is recorded `FAIL` (`smoke-failed`) for every cell — never
//! `NOT_ATTEMPTED`. Failing to run is a result; being unable to try is not.
//!
//! # Ratios carry both terms
//!
//! The summary never prints a bare percentage. Two denominators are reported
//! separately because they answer different questions:
//!
//! * `PASS / ATTEMPTED` — how good is this backend at what it actually ran?
//! * `ATTEMPTED / CORPUS` — how much of the corpus does it even try?
//!
//! Collapsing these is how "16% guest-executing" made the backend with the
//! HIGHEST absolute count look like the laggard.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1.0"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

/// Every backend the product exposes, in `Backend::ALL` order
/// (`hermit-cli/src/lib.rs`). Adding a backend is adding a string here.
const BACKENDS: [&str; 6] = ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"];

/// The legacy per-mode corpora, used only to explain absences during migration.
const LEGACY_MODES: [&str; 4] = ["strict", "sabre", "e9patch", "rr"];

/// Guest used to decide whether a backend FUNCTIONS, not merely whether its
/// prerequisites are met.
const SMOKE_GUEST: &str = "/bin/true";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Pass,
    Fail,
    Timeout,
    NotAttempted,
    AttemptedUnqualifiable,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Pass => "PASS",
            State::Fail => "FAIL",
            State::Timeout => "TIMEOUT",
            State::NotAttempted => "NOT_ATTEMPTED",
            State::AttemptedUnqualifiable => "ATTEMPTED_UNQUALIFIABLE",
        }
    }
    /// Only these two mean "a result exists for this cell".
    fn is_attempted(self) -> bool {
        !matches!(self, State::NotAttempted)
    }
}

struct Cell {
    program: String,
    backend: String,
    state: State,
    reason: String,
    bitwise_parity: String,
    verified: String,
    verdict: String,
    compared_left: String,
    compared_right: String,
    strictness: String,
    rc: String,
    duration_ms: u128,
    legacy_modes: String,
}

fn die(msg: &str) -> ! {
    eprintln!("backend-matrix: {msg}");
    std::process::exit(1);
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

struct Program {
    label: String,
    argv: Vec<String>,
    modes: Vec<String>,
}

/// Expand the corpus placeholders the extractor left behind.
fn expand(arg: &str, root: &Path, fixtures: &Path, tmp: &Path, shell_build: &Path) -> String {
    arg.replace("{{ROOT_DIR}}", &root.to_string_lossy())
        .replace("{{REAL_COMPAT_FIXTURES}}", &fixtures.to_string_lossy())
        .replace("{{VALIDATION_TMP_DIR}}", &tmp.to_string_lossy())
        .replace("{{SHELL_BUILD_DIR}}", &shell_build.to_string_lossy())
}

fn load_corpus(
    path: &Path,
    root: &Path,
    fixtures: &Path,
    tmp: &Path,
    shell_build: &Path,
) -> (Vec<Program>, serde_json::Value) {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", path.display())));
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| die(&format!("invalid JSON in {}: {e}", path.display())));
    let programs = doc
        .get("programs")
        .and_then(|p| p.as_array())
        .unwrap_or_else(|| die("corpus has no `programs` array"));
    let out = programs
        .iter()
        .map(|p| {
            let label = p.get("label").and_then(|v| v.as_str()).unwrap_or_else(|| die("no label"));
            let argv = p
                .get("argv")
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| die(&format!("{label}: no argv")))
                .iter()
                .filter_map(|a| a.as_str())
                .map(|a| expand(a, root, fixtures, tmp, shell_build))
                .collect();
            let modes = p
                .get("modes")
                .and_then(|m| m.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(String::from).collect())
                .unwrap_or_default();
            Program { label: label.to_string(), argv, modes }
        })
        .collect();
    let exclusions = doc.get("exclusions").cloned().unwrap_or(serde_json::json!({}));
    (out, exclusions)
}

/// Run one command with a wall bound. Returns `(rc, combined_output, timed_out)`.
///
/// The child is put in its OWN process group and the group is signalled on
/// timeout, so a wedged guest's descendants die with it rather than leaking. Only
/// this tool's own children are ever signalled.
fn run_bounded(argv: &[String], bound: Duration) -> (Option<i32>, String, bool) {
    let out_path =
        std::env::temp_dir().join(format!("bmx-{}-{:?}.out", std::process::id(), std::thread::current().id()));
    let script = format!(
        "exec >{o} 2>&1; setsid \"$@\" & pid=$!; ( sleep {s}; kill -TERM -$pid 2>/dev/null; \
         sleep 2; kill -KILL -$pid 2>/dev/null ) & w=$!; wait $pid; rc=$?; kill $w 2>/dev/null; exit $rc",
        o = shell_quote(&out_path.to_string_lossy()),
        s = bound.as_secs()
    );
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(script).arg("bash");
    for a in argv {
        cmd.arg(a);
    }
    let started = Instant::now();
    let status = cmd.status();
    let text = std::fs::read_to_string(&out_path).unwrap_or_default();
    let _ = std::fs::remove_file(&out_path);
    match status {
        Ok(s) => {
            let rc = s.code();
            // A TERM/KILL from our own watchdog at (or past) the bound is a timeout,
            // not a product failure. Distinguish it so a slow cell never reads as a red.
            let timed_out = rc.is_none() || (started.elapsed() >= bound);
            (rc, text, timed_out && rc != Some(0))
        }
        Err(e) => (None, format!("spawn failed: {e}"), false),
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Decide whether a backend FUNCTIONS. Returns `Err(reason)` when it does not.
///
/// Two distinct negatives, deliberately kept apart:
/// * Hermit refuses with `is unavailable: <reason>` — prerequisites unmet →
///   `NOT_ATTEMPTED`, reason supplied by the product.
/// * Hermit tries and fails/hangs on `/bin/true` — prerequisites met, backend
///   broken → this is a RESULT, so cells are `FAIL`, never `NOT_ATTEMPTED`.
fn probe_backend(hermit: &str, backend: &str, bound: Duration) -> Result<(), (bool, String)> {
    let argv = vec![
        hermit.to_string(),
        "--backend".into(),
        backend.into(),
        "run".into(),
        "--".into(),
        SMOKE_GUEST.into(),
    ];
    let (rc, out, timed_out) = run_bounded(&argv, bound);
    if rc == Some(0) {
        return Ok(());
    }
    if let Some(idx) = out.find("is unavailable: ") {
        let reason = out[idx + "is unavailable: ".len()..]
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        // `false` = prerequisites unmet ⇒ NOT_ATTEMPTED.
        return Err((false, reason));
    }
    let what = if timed_out {
        format!("smoke-failed: `{SMOKE_GUEST}` exceeded {}s", bound.as_secs())
    } else {
        format!(
            "smoke-failed: `{SMOKE_GUEST}` exited {}; {}",
            rc.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            out.lines().last().unwrap_or("").trim()
        )
    };
    // `true` = prerequisites met but non-functional ⇒ FAIL, not NOT_ATTEMPTED.
    Err((true, what))
}

#[allow(clippy::too_many_arguments)]
fn run_cell(
    hermit: &str,
    backend: &str,
    prog: &Program,
    strict_compare: bool,
    bound: Duration,
    legacy: &str,
) -> Cell {
    let json_path = std::env::temp_dir().join(format!(
        "bmx-{}-{}-{}.json",
        std::process::id(),
        backend,
        prog.label.replace(['/', ' '], "_")
    ));
    let _ = std::fs::remove_file(&json_path);
    let mut argv = vec![
        hermit.to_string(),
        "--backend".into(),
        backend.into(),
        "run".into(),
        "--strict".into(),
        "--verify".into(),
    ];
    if strict_compare {
        argv.push("--verify-strict".into());
    }
    argv.push("--verify-json".into());
    argv.push(json_path.to_string_lossy().to_string());
    argv.push("--".into());
    argv.extend(prog.argv.iter().cloned());

    let started = Instant::now();
    let (rc, out, timed_out) = run_bounded(&argv, bound);
    let duration_ms = started.elapsed().as_millis();

    let v: Option<serde_json::Value> = std::fs::read_to_string(&json_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());
    let _ = std::fs::remove_file(&json_path);

    let get_b = |k: &str| v.as_ref().and_then(|d| d.get(k)).and_then(|x| x.as_bool());
    let verified = get_b("verified");
    let parity = get_b("bitwise_parity");
    let verdict = v
        .as_ref()
        .and_then(|d| d.get("verdict"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let cmp = v.as_ref().and_then(|d| d.get("compared_log_messages"));
    let cl = cmp.and_then(|c| c.get("left")).and_then(|x| x.as_i64());
    let cr = cmp.and_then(|c| c.get("right")).and_then(|x| x.as_i64());
    let strictness = v
        .as_ref()
        .and_then(|d| d.get("comparison"))
        .and_then(|c| c.get("strictness"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let (state, reason) = if timed_out {
        (State::Timeout, format!("exceeded {}s wall bound", bound.as_secs()))
    } else if rc != Some(0) {
        (
            State::Fail,
            format!(
                "exit {}; {}",
                rc.map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
                out.lines().last().unwrap_or("").trim().chars().take(160).collect::<String>()
            ),
        )
    } else if verdict == "no_result" {
        // NO_RESULT IS NOT A RED. The product is saying it produced nothing
        // comparable, which is a different fact from "the runs disagreed", and
        // scoring it FAIL would manufacture reds out of an absence of evidence —
        // the exact inversion this four-state model exists to prevent.
        //
        // This is not hypothetical. `--backend dbt --verify-strict` on
        // `/bin/echo hi` exits 0, prints `:: Success: deterministic. Determinism
        // verified.` with matching guest-memory hashes, and reports
        // `{verified:false, bitwise_parity:false, verdict:"no_result",
        // comparison:null, compared_log_messages:null}`. DBT compares its own
        // memory hashes instead of the log path, so the standard verdict fields
        // are never populated. Grepping stdout scores that PASS; reading
        // `verified` alone scores it FAIL. Both are wrong: it ran, it self-reports
        // determinism, and it yields no qualifying comparison evidence.
        (
            State::AttemptedUnqualifiable,
            "verdict=no_result: ran without producing a comparable result (no log comparison \
             performed); self-reported determinism is not qualifying evidence"
                .into(),
        )
    } else if verified != Some(true) || (!verdict.is_empty() && verdict != "matched") {
        (
            State::Fail,
            format!("verified={verified:?} verdict={verdict}"),
        )
    } else if parity != Some(true) {
        // Ran, agreed, but at a tier that cannot certify.
        (
            State::AttemptedUnqualifiable,
            format!(
                "verified but bitwise_parity={}; comparator cannot certify L2",
                parity.map(|b| b.to_string()).unwrap_or_else(|| "absent".into())
            ),
        )
    } else if cl.unwrap_or(0) <= 0 || cr.unwrap_or(0) <= 0 {
        // A green over zero evidence is the zero-executed defect in a costume.
        (
            State::AttemptedUnqualifiable,
            "bitwise_parity true but ZERO log messages compared — no evidence".into(),
        )
    } else {
        (State::Pass, String::new())
    };

    Cell {
        program: prog.label.clone(),
        backend: backend.to_string(),
        state,
        reason,
        bitwise_parity: parity.map(|b| b.to_string()).unwrap_or_default(),
        verified: verified.map(|b| b.to_string()).unwrap_or_default(),
        verdict,
        compared_left: cl.map(|n| n.to_string()).unwrap_or_default(),
        compared_right: cr.map(|n| n.to_string()).unwrap_or_default(),
        strictness,
        rc: rc.map(|c| c.to_string()).unwrap_or_default(),
        duration_ms,
        legacy_modes: legacy.to_string(),
    }
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut hermit = std::env::var("STRICT_COMPAT_HERMIT_BIN")
        .unwrap_or_else(|_| "target/release/hermit".to_string());
    let mut out_csv = PathBuf::from("ignored/compat/backend-matrix.csv");
    let mut jobs: usize = 8;
    let mut bound = Duration::from_secs(90);
    let mut limit: usize = usize::MAX;
    let mut only_backends: Vec<String> = BACKENDS.iter().map(|s| s.to_string()).collect();
    let mut strict_compare = true;
    let mut fixtures_override: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut next = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_else(|| die(&format!("{a} needs a value")))
        };
        match a {
            "--hermit" => hermit = next(&mut i),
            "--out" => out_csv = PathBuf::from(next(&mut i)),
            "--jobs" => jobs = next(&mut i).parse().unwrap_or(8),
            "--timeout" => bound = Duration::from_secs(next(&mut i).parse().unwrap_or(90)),
            "--limit" => limit = next(&mut i).parse().unwrap_or(usize::MAX),
            "--backends" => {
                only_backends = next(&mut i).split(',').map(|s| s.trim().to_string()).collect()
            }
            "--no-verify-strict" => strict_compare = false,
            "--fixtures" => fixtures_override = Some(PathBuf::from(next(&mut i))),
            "--help" | "-h" => {
                println!(
                    "backend-matrix — one corpus x every backend, four-state cells\n\n\
                     --hermit PATH      hermit binary (or $STRICT_COMPAT_HERMIT_BIN)\n\
                     --out PATH         output CSV (default ignored/compat/backend-matrix.csv)\n\
                     --backends a,b     restrict the backend axis\n\
                     --limit N          first N programs (smoke runs)\n\
                     --jobs N           concurrency (default 8)\n\
                     --timeout S        per-cell wall bound (default 90)\n\
                     --no-verify-strict use the stripped comparator (cannot yield PASS)\n\
                     --fixtures PATH    prepared real_compat fixtures (see prepare_real_compat_fixtures.sh)\n"
                );
                return;
            }
            other => die(&format!("unknown argument `{other}` (try --help)")),
        }
        i += 1;
    }

    for b in &only_backends {
        if !BACKENDS.contains(&b.as_str()) {
            die(&format!("unknown backend `{b}`; known: {}", BACKENDS.join(", ")));
        }
    }

    let root = std::env::current_dir().unwrap();
    if !Path::new(&hermit).exists() {
        die(&format!("hermit binary not found: {hermit} (pass --hermit)"));
    }
    let hermit_abs = std::fs::canonicalize(&hermit).unwrap().to_string_lossy().to_string();

    let tmp = std::env::temp_dir().join(format!("bmx-{}", std::process::id()));
    // The heavyweight rows (java, cargo, gcc, git, cmake, sqlite3, ...) run through
    // tests/compat/real_compat_workload.sh, which needs prepared fixtures. Pointing
    // at an empty directory makes those ~20 programs report FAIL for a reason that
    // has nothing to do with the backend under test, so the path is overridable and
    // a missing override is called out rather than silently producing false reds.
    let fixtures = fixtures_override.clone().unwrap_or_else(|| tmp.join("fixtures"));
    if fixtures_override.is_none() {
        eprintln!(
            "backend-matrix: WARNING no --fixtures given; rows driven by \
             tests/compat/real_compat_workload.sh will FAIL for missing fixtures, \
             not for backend behaviour. Run tests/compat/prepare_real_compat_fixtures.sh <dir> first."
        );
    }
    let shell_build = tmp.join("shell-build");
    for d in [&tmp, &fixtures, &shell_build] {
        let _ = std::fs::create_dir_all(d);
    }

    let corpus_path = root.join("ci/compat/corpus.json");
    let (programs, exclusions) = load_corpus(&corpus_path, &root, &fixtures, &tmp, &shell_build);
    let programs: Vec<Program> = programs.into_iter().take(limit).collect();
    let n_prog = programs.len();

    eprintln!(
        "backend-matrix: {n_prog} programs x {} backends = {} cells; hermit {hermit_abs}",
        only_backends.len(),
        n_prog * only_backends.len()
    );

    // --- availability, once per backend, from the product's own refusal --------
    let mut unavailable: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for b in &only_backends {
        match probe_backend(&hermit_abs, b, Duration::from_secs(60)) {
            Ok(()) => eprintln!("  {b:9} FUNCTIONS (smoke {SMOKE_GUEST} rc=0)"),
            Err((prereq_met, reason)) => {
                eprintln!(
                    "  {b:9} {} — {reason}",
                    if prereq_met { "BROKEN (cells => FAIL)" } else { "UNAVAILABLE (cells => NOT_ATTEMPTED)" }
                );
                unavailable.insert(b.clone(), (prereq_met, reason));
            }
        }
    }

    // --- the matrix ------------------------------------------------------------
    let work: Vec<(usize, usize)> = (0..only_backends.len())
        .flat_map(|bi| (0..n_prog).map(move |pi| (bi, pi)))
        .collect();
    let total = work.len();
    let next = Arc::new(AtomicUsize::new(0));
    let cells: Arc<Mutex<Vec<Cell>>> = Arc::new(Mutex::new(Vec::with_capacity(total)));
    let done = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|scope| {
        for _ in 0..jobs.max(1) {
            let next = Arc::clone(&next);
            let cells = Arc::clone(&cells);
            let done = Arc::clone(&done);
            let programs = &programs;
            let only_backends = &only_backends;
            let unavailable = &unavailable;
            let exclusions = &exclusions;
            let hermit_abs = hermit_abs.clone();
            let work = &work;
            scope.spawn(move || loop {
                let k = next.fetch_add(1, Ordering::SeqCst);
                if k >= work.len() {
                    return;
                }
                let (bi, pi) = work[k];
                let backend = &only_backends[bi];
                let prog = &programs[pi];
                let legacy = prog.modes.join("|");

                let cell = if let Some((prereq_met, reason)) = unavailable.get(backend) {
                    // Prerequisites unmet ⇒ no result can exist ⇒ NOT_ATTEMPTED.
                    // Prerequisites met but broken ⇒ that IS a result ⇒ FAIL.
                    Cell {
                        program: prog.label.clone(),
                        backend: backend.clone(),
                        state: if *prereq_met { State::Fail } else { State::NotAttempted },
                        reason: reason.clone(),
                        bitwise_parity: String::new(),
                        verified: String::new(),
                        verdict: String::new(),
                        compared_left: String::new(),
                        compared_right: String::new(),
                        strictness: String::new(),
                        rc: String::new(),
                        duration_ms: 0,
                        legacy_modes: legacy.clone(),
                    }
                } else if let Some(r) = exclusions
                    .get(&prog.label)
                    .and_then(|e| e.get(backend))
                    .and_then(|x| x.as_str())
                {
                    Cell {
                        program: prog.label.clone(),
                        backend: backend.clone(),
                        state: State::NotAttempted,
                        reason: format!("corpus exclusion: {r}"),
                        bitwise_parity: String::new(),
                        verified: String::new(),
                        verdict: String::new(),
                        compared_left: String::new(),
                        compared_right: String::new(),
                        strictness: String::new(),
                        rc: String::new(),
                        duration_ms: 0,
                        legacy_modes: legacy.clone(),
                    }
                } else {
                    run_cell(&hermit_abs, backend, prog, strict_compare, bound, &legacy)
                };

                cells.lock().unwrap().push(cell);
                let d = done.fetch_add(1, Ordering::SeqCst) + 1;
                if d % 100 == 0 || d == work.len() {
                    eprintln!("  .. {d}/{}", work.len());
                }
            });
        }
    });

    let mut cells = Arc::try_unwrap(cells).ok().unwrap().into_inner().unwrap();
    cells.sort_by(|a, b| (&a.backend, &a.program).cmp(&(&b.backend, &b.program)));

    if let Some(p) = out_csv.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut f = std::fs::File::create(&out_csv)
        .unwrap_or_else(|e| die(&format!("cannot write {}: {e}", out_csv.display())));
    writeln!(
        f,
        "program,backend,state,reason,bitwise_parity,verified,verdict,compared_left,compared_right,comparison_strictness,rc,duration_ms,legacy_modes"
    )
    .unwrap();
    for c in &cells {
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{},{},{},{},{}",
            csv_escape(&c.program),
            csv_escape(&c.backend),
            c.state.as_str(),
            csv_escape(&c.reason),
            c.bitwise_parity,
            c.verified,
            csv_escape(&c.verdict),
            c.compared_left,
            c.compared_right,
            csv_escape(&c.strictness),
            c.rc,
            c.duration_ms,
            csv_escape(&c.legacy_modes)
        )
        .unwrap();
    }
    let _ = std::fs::remove_dir_all(&tmp);

    // --- summary: every ratio carries BOTH TERMS -------------------------------
    println!("\nbackend-matrix: {} cells -> {}", cells.len(), out_csv.display());
    println!(
        "\n{:<9} {:>5} {:>5} {:>5} {:>5} {:>7} {:>7}   {:<24} {}",
        "backend", "PASS", "FAIL", "TMO", "UNQ", "N/A", "cells", "PASS/ATTEMPTED", "ATTEMPTED/CORPUS"
    );
    for b in &only_backends {
        let mine: Vec<&Cell> = cells.iter().filter(|c| &c.backend == b).collect();
        let n = |s: State| mine.iter().filter(|c| c.state == s).count();
        let (p, fl, t, u, na) = (
            n(State::Pass),
            n(State::Fail),
            n(State::Timeout),
            n(State::AttemptedUnqualifiable),
            n(State::NotAttempted),
        );
        let att = mine.iter().filter(|c| c.state.is_attempted()).count();
        println!(
            "{:<9} {:>5} {:>5} {:>5} {:>5} {:>7} {:>7}   {:<24} {}",
            b,
            p,
            fl,
            t,
            u,
            na,
            mine.len(),
            format!("{p}/{att}"),
            format!("{att}/{}", mine.len())
        );
    }
    println!(
        "\nPASS/ATTEMPTED and ATTEMPTED/CORPUS are DIFFERENT QUESTIONS and are never multiplied.\n\
         NOT_ATTEMPTED is excluded from PASS/ATTEMPTED and is NOT a zero score.\n\
         ATTEMPTED_UNQUALIFIABLE ran and agreed but cannot certify; it is never counted as PASS."
    );
}
