#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Derive per-step `cpu_timeout` (CPU-time budget, seconds) for the DAG
//! manifests from REAL measurements — never hand-written guesses.
//!
//! # Why CPU-time, not wall-time
//!
//! A wall `timeout` inflates under host load and flakes; a CPU-second is
//! physical work and is load-invariant, so a `cpu_timeout` catches a genuine
//! runaway (a hang that BURNS CPU, like a reap spin) without flaking when the
//! box is merely busy. The wall `timeout` stays as the backstop for the
//! stuck-and-idle case. Enforcement lives in `safe-ci-dag-runner`
//! (`scheduler.py`: a 1 Hz cgroup monitor reaps a step once
//! `cpu.stat usage_usec / 1e6 >= cpu_timeout`), and fires only under
//! `--cgroups`. See `ci/dag/README.md`.
//!
//! # The derivation rule (the same one validated for e2e/metadata)
//!
//!   cpu_timeout = round( max(observed CPU-seconds) * headroom )
//!
//! with `headroom` defaulting to 1.5 (the owner's "<=50% above what it normally
//! takes"). The bound is anchored on the DISTRIBUTION MAX, not the median, so a
//! legitimately heavy tail cannot trip it. A step is assigned a value ONLY when
//! at least `--min-samples` (default 5) successful CPU-time observations exist;
//! otherwise it is left WITHOUT a `cpu_timeout` and reported as such. We never
//! invent a number for an under-measured node.
//!
//! # Inputs (two measurement sources, auto-detected per CSV)
//!
//! * `safe-ci-dag-runner --perf-dir` step-profile CSVs: one row per DAG node per
//!   run, with a `step` column and a `cpu.usage_usec` column (microseconds of
//!   child CPU from the step's cgroup `cpu.stat`). This is the durable source —
//!   it accumulates automatically whenever the boxed runner runs, so the
//!   pipeline does not rot as nodes are added: re-run this tool after any
//!   `--cgroups --perf-dir` run and newly-covered nodes get derived values.
//! * `/usr/bin/time -v`-style CSVs with a `cpu_s` (or `user_s`+`sys_s`) column
//!   and NO `step` column: a single node measured in isolation. Name it with
//!   `--step <group>/<job>`. (This is how e2e/metadata's 35-sample study was
//!   taken before the boxed runner path existed.)
//!
//! Only rows that clearly succeeded are counted: a `returncode`/`exit` of 0 (or
//! an `ok` of true) when such a column is present; rows lacking any status
//! column are counted (the isolation studies record only clean runs).
//!
//! # The forcing-function flow (breach table -> declarations)
//!
//! Under the opt-out boxing model (`safe-ci-dag-runner` default-on cgroups, a
//! small default cap of 1 core / 1 GB / 10 s CPU per undeclared node, escape
//! hatch `--unsafe-no-cgroups`), running the full DAG once yields a BREACH
//! TABLE: every node that exceeds the 10 s default reports its real CPU-second
//! peak. That table is this tool's input — feed it as `--samples` and each
//! breaching node gets `round(max_cpu * headroom)`; nodes that never breach stay
//! UNSET because the default already suffices. So declarations are derived from
//! measured breaches, not guessed, and the node set comes from the manifests —
//! neither is hand-maintained. See task `cgroups-opt-out-with-small-default-cap`.
//!
//! # Usage
//!
//!   # Report only (no manifest change): derive from a runner perf CSV.
//!   ci/dag/derive-cpu-timeouts.rs --samples /path/step_profiles_*.csv
//!
//!   # Name a single-node isolation study and report.
//!   ci/dag/derive-cpu-timeouts.rs \
//!       --samples ambient.csv --samples load.csv --step e2e/metadata
//!
//!   # Apply derived values into the manifests (minimal per-line textual insert,
//!   # right after each step's "timeout" line; idempotent).
//!   ci/dag/derive-cpu-timeouts.rs --samples ... --step e2e/metadata --apply
//!
//!   # Machine-readable mapping for downstream tooling.
//!   ci/dag/derive-cpu-timeouts.rs --samples ... --format json
//!
//!   ci/dag/derive-cpu-timeouts.rs --self-test
//!
//! Flags: `--min-samples N` (default 5), `--headroom F` (default 1.5),
//! `--floor S` (default 0; a minimum applied only when a value is derived),
//! `--manifest PATH` (repeatable; default `ci/dag/portable.json` and
//! `ci/dag/privileged.json`), `--apply` (edit the manifests in place),
//! `--format human|json`.
//!
//! ```cargo
//! [dependencies]
//! serde_json = { version = "1.0", features = ["preserve_order"] }
//! ```

use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::exit;

/// A derived (or undeliverable) result for one DAG node.
struct Derived {
    key: String, // "group/job"
    n: usize,
    max_cpu_s: Option<f64>,
    cpu_timeout: Option<i64>,
    reason: String, // why unset, when cpu_timeout is None
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--self-test") {
        self_test();
        return;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_help();
        return;
    }

    let mut samples: Vec<PathBuf> = Vec::new();
    let mut manifests: Vec<PathBuf> = Vec::new();
    let mut step: Option<String> = None;
    let mut min_samples: usize = 5;
    let mut headroom: f64 = 1.5;
    let mut floor: i64 = 0;
    let mut apply = false;
    let mut format = "human".to_string();

    let mut i = 1;
    while i < args.len() {
        let a = &args[i];
        let mut next = || {
            i += 1;
            args.get(i).cloned().unwrap_or_else(|| {
                eprintln!("derive-cpu-timeouts: missing value after {a}");
                exit(2);
            })
        };
        match a.as_str() {
            "--samples" => samples.push(PathBuf::from(next())),
            "--manifest" => manifests.push(PathBuf::from(next())),
            "--step" => step = Some(normalize_key(&next())),
            "--min-samples" => min_samples = next().parse().unwrap_or(5),
            "--headroom" => headroom = next().parse().unwrap_or(1.5),
            "--floor" => floor = next().parse().unwrap_or(0),
            "--apply" => apply = true,
            "--format" => format = next(),
            other => {
                eprintln!("derive-cpu-timeouts: unknown argument {other}");
                exit(2);
            }
        }
        i += 1;
    }

    let root = repo_root();
    if manifests.is_empty() {
        manifests.push(root.join("ci/dag/portable.json"));
        manifests.push(root.join("ci/dag/privileged.json"));
    }

    // 1. The node universe: every (group/job) present in any manifest.
    let universe = read_universe(&manifests);
    if universe.is_empty() {
        eprintln!("derive-cpu-timeouts: no steps found in {manifests:?}");
        exit(3);
    }

    // 2. Ingest all sample CSVs into per-step CPU-second observations.
    let mut obs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for csv in &samples {
        ingest_csv(csv, step.as_deref(), &mut obs);
    }

    // 3. Derive per node.
    let mut results: Vec<Derived> = Vec::new();
    for key in &universe {
        let v = obs.get(key);
        match v {
            Some(cpu) if cpu.len() >= min_samples => {
                let max = cpu.iter().cloned().fold(f64::MIN, f64::max);
                let mut t = (max * headroom).round() as i64;
                if t < floor {
                    t = floor;
                }
                results.push(Derived {
                    key: key.clone(),
                    n: cpu.len(),
                    max_cpu_s: Some(max),
                    cpu_timeout: Some(t),
                    reason: String::new(),
                });
            }
            Some(cpu) => {
                let max = cpu.iter().cloned().fold(f64::MIN, f64::max);
                results.push(Derived {
                    key: key.clone(),
                    n: cpu.len(),
                    max_cpu_s: Some(max),
                    cpu_timeout: None,
                    reason: format!("insufficient samples ({} < {min_samples})", cpu.len()),
                });
            }
            None => results.push(Derived {
                key: key.clone(),
                n: 0,
                max_cpu_s: None,
                cpu_timeout: None,
                reason: "no CPU-time samples".to_string(),
            }),
        }
    }

    if format == "json" {
        emit_json(&results);
    } else {
        emit_human(&results, min_samples, headroom, floor);
    }

    if apply {
        let mapping: BTreeMap<&str, i64> = results
            .iter()
            .filter_map(|d| d.cpu_timeout.map(|t| (d.key.as_str(), t)))
            .collect();
        if mapping.is_empty() {
            eprintln!("\n--apply: nothing to write (no node had >= {min_samples} samples).");
        } else {
            for m in &manifests {
                let changed = apply_to_manifest(m, &mapping);
                eprintln!("--apply: {} step(s) updated in {}", changed, m.display());
            }
        }
    }
}

/// Normalize a step key to `group/job` (runner tags use `.`; manifests use
/// separate fields; studies may pass either).
fn normalize_key(s: &str) -> String {
    s.trim().replacen('.', "/", 1)
}

fn repo_root() -> PathBuf {
    // This script lives at <root>/ci/dag/derive-cpu-timeouts.rs.
    let here = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));
    // rust-script runs from a temp binary; prefer CWD-relative discovery.
    for base in [env::current_dir().ok(), here].into_iter().flatten() {
        let mut b = base.clone();
        loop {
            if b.join("ci/dag/portable.json").exists() {
                return b;
            }
            match b.parent() {
                Some(p) => b = p.to_path_buf(),
                None => break,
            }
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn read_universe(manifests: &[PathBuf]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for m in manifests {
        let raw = match fs::read_to_string(m) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let v: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("derive-cpu-timeouts: {} is not valid JSON: {e}", m.display());
                exit(3);
            }
        };
        if let Some(steps) = v.get("steps").and_then(|s| s.as_array()) {
            for s in steps {
                if let (Some(g), Some(j)) = (
                    s.get("group").and_then(|x| x.as_str()),
                    s.get("job").and_then(|x| x.as_str()),
                ) {
                    let key = format!("{g}/{j}");
                    if !seen.contains(&key) {
                        seen.push(key);
                    }
                }
            }
        }
    }
    seen
}

/// Parse one CSV, auto-detecting the two accepted shapes, appending CPU-second
/// observations keyed by `group/job` into `obs`.
fn ingest_csv(path: &Path, forced_step: Option<&str>, obs: &mut BTreeMap<String, Vec<f64>>) {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("derive-cpu-timeouts: cannot read {}: {e}", path.display());
            exit(3);
        }
    };
    let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
    let header = match lines.next() {
        Some(h) => h,
        None => return,
    };
    let cols: Vec<String> = header.split(',').map(|c| c.trim().to_string()).collect();
    let idx = |name: &str| cols.iter().position(|c| c == name);

    let step_i = idx("step");
    let usage_i = idx("cpu.usage_usec");
    let user_us_i = idx("cpu.user_usec");
    let sys_us_i = idx("cpu.system_usec");
    let cpu_s_i = idx("cpu_s");
    let user_s_i = idx("user_s");
    let sys_s_i = idx("sys_s");
    let rc_i = idx("returncode").or_else(|| idx("exit"));
    let ok_i = idx("ok");

    if step_i.is_none() && forced_step.is_none() {
        eprintln!(
            "derive-cpu-timeouts: {} has no `step` column; pass --step <group>/<job> \
             to name this single-node study",
            path.display()
        );
        exit(2);
    }

    for line in lines {
        let f: Vec<&str> = line.split(',').collect();
        let get = |o: Option<usize>| -> Option<&str> { o.and_then(|k| f.get(k).copied()) };

        // Success filter: honor a status column when present.
        if let Some(rc) = get(rc_i) {
            if rc.trim() != "0" {
                continue;
            }
        }
        if let Some(ok) = get(ok_i) {
            let ok = ok.trim().to_lowercase();
            if ok == "false" || ok == "0" {
                continue;
            }
        }

        // CPU seconds for this row, in priority order.
        let cpu_s: Option<f64> = if let Some(u) = get(usage_i).and_then(|s| s.trim().parse::<f64>().ok())
        {
            Some(u / 1_000_000.0)
        } else if let (Some(u), Some(s)) = (
            get(user_us_i).and_then(|s| s.trim().parse::<f64>().ok()),
            get(sys_us_i).and_then(|s| s.trim().parse::<f64>().ok()),
        ) {
            Some((u + s) / 1_000_000.0)
        } else if let Some(c) = get(cpu_s_i).and_then(|s| s.trim().parse::<f64>().ok()) {
            Some(c)
        } else if let (Some(u), Some(s)) = (
            get(user_s_i).and_then(|s| s.trim().parse::<f64>().ok()),
            get(sys_s_i).and_then(|s| s.trim().parse::<f64>().ok()),
        ) {
            Some(u + s)
        } else {
            None
        };

        let cpu_s = match cpu_s {
            Some(c) if c > 0.0 => c,
            _ => continue,
        };

        let key = match (get(step_i), forced_step) {
            (Some(s), _) if !s.trim().is_empty() => normalize_key(s),
            (_, Some(fs)) => fs.to_string(),
            _ => continue,
        };
        obs.entry(key).or_default().push(cpu_s);
    }
}

fn emit_human(results: &[Derived], min_samples: usize, headroom: f64, floor: i64) {
    let set: Vec<&Derived> = results.iter().filter(|d| d.cpu_timeout.is_some()).collect();
    let unset: Vec<&Derived> = results.iter().filter(|d| d.cpu_timeout.is_none()).collect();

    println!(
        "cpu_timeout derivation  (rule: round(max_cpu_s * {headroom}), floor {floor}s, \
         min {min_samples} samples)\n"
    );
    println!("{:<34} {:>4} {:>11} {:>12}", "step", "n", "max_cpu_s", "cpu_timeout");
    println!("{}", "-".repeat(64));
    for d in &set {
        println!(
            "{:<34} {:>4} {:>11.2} {:>12}",
            d.key,
            d.n,
            d.max_cpu_s.unwrap_or(0.0),
            d.cpu_timeout.unwrap()
        );
    }
    println!(
        "\nSET (justified by measurement): {}/{}",
        set.len(),
        results.len()
    );
    println!("UNSET (left without cpu_timeout, honestly): {}", unset.len());
    for d in &unset {
        let m = d
            .max_cpu_s
            .map(|x| format!(" (max seen {x:.2}s)"))
            .unwrap_or_default();
        println!("  - {:<32} {}{}", d.key, d.reason, m);
    }
}

fn emit_json(results: &[Derived]) {
    let arr: Vec<Value> = results
        .iter()
        .map(|d| {
            serde_json::json!({
                "step": d.key,
                "samples": d.n,
                "max_cpu_s": d.max_cpu_s,
                "cpu_timeout": d.cpu_timeout,
                "status": if d.cpu_timeout.is_some() { "set" } else { "unset" },
                "reason": if d.cpu_timeout.is_some() { Value::Null } else { Value::String(d.reason.clone()) },
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&arr).unwrap());
}

/// Insert or update `"cpu_timeout": N,` in each targeted step, right after that
/// step's `"timeout":` line, preserving the file's hand-formatting elsewhere.
/// Anchors on the unique `"group": "g", "job": "j",` first line of each step.
/// Idempotent: an existing `cpu_timeout` line is replaced in place.
fn apply_to_manifest(path: &Path, mapping: &BTreeMap<&str, i64>) -> usize {
    let raw = match fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("--apply: cannot read {}: {e}", path.display());
            return 0;
        }
    };
    let (out, changed) = edit_manifest_text(&raw, mapping);
    if changed > 0 {
        if let Err(e) = fs::write(path, out) {
            eprintln!("--apply: cannot write {}: {e}", path.display());
            return 0;
        }
    }
    changed
}

/// Pure text transform (unit-tested). Returns (new_text, steps_changed).
fn edit_manifest_text(raw: &str, mapping: &BTreeMap<&str, i64>) -> (String, usize) {
    let lines: Vec<&str> = raw.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + mapping.len());
    let mut changed = 0usize;

    // Value for the step we are currently inside, when that step is a target.
    let mut cur_target: Option<i64> = None;
    // Whether we have already inserted the line for the current target step
    // (guards against a second "timeout" match, though steps have only one).
    let mut inserted = false;

    for line in lines {
        // A step header line: `  "group": "g", "job": "j",` starts a new step.
        if let Some((g, j)) = parse_group_job(line) {
            let key = format!("{g}/{j}");
            cur_target = mapping.get(key.as_str()).copied();
            inserted = false;
        }

        // Inside a target step, drop any pre-existing cpu_timeout line so a
        // re-apply replaces rather than duplicates it (idempotent). Untouched
        // in non-target steps.
        if cur_target.is_some() && line.trim_start().starts_with("\"cpu_timeout\":") {
            continue;
        }

        out.push(line.to_string());

        // Insert right after the target step's "timeout": line.
        if let (Some(val), false) = (cur_target, inserted) {
            if let Some(indent) = timeout_line_indent(line) {
                out.push(format!("{indent}\"cpu_timeout\": {val},"));
                changed += 1;
                inserted = true;
            }
        }
    }
    (out.join("\n"), changed)
}

/// Parse `  "group": "g", "job": "j",` -> (g, j).
fn parse_group_job(line: &str) -> Option<(String, String)> {
    let t = line.trim_start();
    if !t.starts_with("\"group\":") {
        return None;
    }
    let g = between(t, "\"group\":")?;
    let j = between(t, "\"job\":")?;
    Some((g, j))
}

/// Extract the first double-quoted string value appearing after `after` in `s`.
fn between(s: &str, after: &str) -> Option<String> {
    let start = s.find(after)? + after.len();
    let rest = &s[start..];
    let q1 = rest.find('"')? + 1;
    let q2 = rest[q1..].find('"')? + q1;
    Some(rest[q1..q2].to_string())
}

/// If `line` is a top-level `"timeout": N,` line, return its leading indent.
fn timeout_line_indent(line: &str) -> Option<String> {
    let t = line.trim_start();
    if t.starts_with("\"timeout\":") {
        let indent_len = line.len() - t.len();
        return Some(line[..indent_len].to_string());
    }
    None
}

fn print_help() {
    println!(
        "derive-cpu-timeouts.rs — derive per-step cpu_timeout from measurements\n\n\
         See the header docs. Common forms:\n\
         \x20 --samples step_profiles.csv                 report from a runner perf CSV\n\
         \x20 --samples a.csv --samples b.csv --step g/j  single-node isolation study\n\
         \x20 ... --apply                                 write into the manifests\n\
         \x20 ... --format json                           machine-readable mapping\n\
         \x20 --self-test                                 run built-in tests\n"
    );
}

fn self_test() {
    // 1. edit_manifest_text inserts one line after the matching step's timeout,
    //    leaves others alone, and is idempotent.
    let src = "{\n  \"steps\": [\n    {\n      \"group\": \"e2e\", \"job\": \"metadata\",\n      \"cmd\": \"x\",\n      \"timeout\": 60,\n      \"hint\": {}\n    },\n    {\n      \"group\": \"check\", \"job\": \"portability_paths\",\n      \"timeout\": 60,\n      \"hint\": {}\n    }\n  ]\n}";
    let mut map: BTreeMap<&str, i64> = BTreeMap::new();
    map.insert("e2e/metadata", 18);
    let (out, changed) = edit_manifest_text(src, &map);
    assert_eq!(changed, 1, "exactly one step updated");
    assert!(
        out.contains("      \"timeout\": 60,\n      \"cpu_timeout\": 18,\n      \"hint\": {}"),
        "cpu_timeout inserted after e2e/metadata timeout with matching indent:\n{out}"
    );
    // The other step (check/portability_paths) must be untouched.
    assert_eq!(
        out.matches("\"cpu_timeout\":").count(),
        1,
        "only the targeted step gets a cpu_timeout"
    );
    // Idempotent: re-applying yields the same text and one change.
    let (out2, changed2) = edit_manifest_text(&out, &map);
    assert_eq!(out2, out, "re-apply is idempotent");
    assert_eq!(changed2, 1);
    // Updating the value replaces in place (no duplicate line).
    map.insert("e2e/metadata", 20);
    let (out3, _) = edit_manifest_text(&out, &map);
    assert!(out3.contains("\"cpu_timeout\": 20,"));
    assert_eq!(out3.matches("\"cpu_timeout\":").count(), 1);

    // 2. parse_group_job + between.
    assert_eq!(
        parse_group_job("      \"group\": \"e2e\", \"job\": \"metadata\","),
        Some(("e2e".to_string(), "metadata".to_string()))
    );
    assert_eq!(parse_group_job("      \"cmd\": \"x\","), None);

    // 3. CSV ingest: both shapes. Write temp files.
    let dir = env::temp_dir();
    let runner = dir.join("dct_runner_selftest.csv");
    fs::write(
        &runner,
        "step,returncode,ok,cpu.usage_usec\ne2e.metadata,0,True,7000000\ne2e.metadata,0,True,12240000\ncheck.foo,1,False,999000\n",
    )
    .unwrap();
    let mut obs: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    ingest_csv(&runner, None, &mut obs);
    let m = obs.get("e2e/metadata").expect("runner rows grouped by step");
    assert_eq!(m.len(), 2, "failed row (rc=1) excluded");
    let max = m.iter().cloned().fold(f64::MIN, f64::max);
    assert!((max - 12.24).abs() < 1e-6, "usage_usec -> seconds");
    assert!(obs.get("check/foo").is_none(), "failed-only step excluded");

    let study = dir.join("dct_study_selftest.csv");
    fs::write(
        &study,
        "phase,idx,wall_s,user_s,sys_s,cpu_s,exit\nseq,1,6.8,3.7,3.6,7.39,0\nseq,2,9.3,4.3,5.5,12.24,0\n",
    )
    .unwrap();
    let mut obs2: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    ingest_csv(&study, Some("e2e/metadata"), &mut obs2);
    let m2 = obs2.get("e2e/metadata").expect("study rows named by --step");
    assert_eq!(m2.len(), 2);
    let max2 = m2.iter().cloned().fold(f64::MIN, f64::max);
    // Derivation rule: round(12.24 * 1.5) = round(18.36) = 18.
    assert_eq!((max2 * 1.5).round() as i64, 18, "the validated e2e/metadata number");

    let _ = fs::remove_file(&runner);
    let _ = fs::remove_file(&study);

    println!("derive-cpu-timeouts: all self-tests passed");
}
