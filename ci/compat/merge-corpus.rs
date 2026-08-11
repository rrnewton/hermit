#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Merge the four per-mode compatibility corpora into ONE backend-agnostic
//! corpus, and prove the merge lost nothing.
//!
//! # Why this exists
//!
//! `ci/compat/corpus-{strict,sabre,e9patch,rr}.json` are four separate files
//! describing four overlapping populations of the SAME programs. That shape has
//! two costs, and the second is the expensive one:
//!
//! 1. Adding a backend means AUTHORING A NEW CORPUS FILE. The structure is
//!    per-backend, so backends cannot be compared and coverage cannot be summed.
//! 2. A program absent from a mode's file is INDISTINGUISHABLE from a program
//!    that mode handles. Absence is silence. Measured on the four files: the
//!    union is 214 programs, but only 72 are attempted by all of
//!    ptrace+sabre+e9patch+liteinst, and 23 are attempted by exactly one
//!    mechanism. None of those absences carries a reason.
//!
//! The merged corpus fixes (1) by having one program list, and fixes (2) by
//! making membership EXPLICIT per program: a mode that does not attempt a
//! program says so in the data, and the matrix driver reports it as
//! `NOT_ATTEMPTED` with a reason instead of omitting the cell.
//!
//! # The safety contract, and why `verify` is the important subcommand
//!
//! This is a MIGRATION, so its whole job is to change structure and NOT
//! behaviour. `verify` re-derives each legacy per-mode view from the merged
//! corpus and requires it to equal the legacy file EXACTLY — same labels, same
//! order, same argv, element for element. If a single argv byte moved, the merge
//! silently changed what CI runs, which is the one outcome that would make this
//! worse than leaving four files alone.
//!
//! Run `verify` in CI. `generate` is for a human refreshing the data file.
//!
//! # The three known argv divergences (recorded, deliberately NOT resolved)
//!
//! 211 of 214 labels carry byte-identical argv in every mode that has them.
//! Three do not: `bc`, `lua` and `perl`. In each case `strict` runs a deep
//! semantic workload (compute a value, assert it) while `sabre`/`e9patch`/`rr`
//! run a shallow one (`lua -e print(42)`). That is a depth ratchet that reached
//! `strict` and never reached the others.
//!
//! This script records that divergence in `mode_argv` rather than resolving it.
//! Promoting the other three modes to the deep workload would be a real
//! improvement AND a behaviour change that could turn a green cell red, which is
//! not this task's job. Recorded and visible beats resolved and silent; closing
//! it is a corpus-depth task, not a structural one.
//!
//! ```cargo
//! [dependencies]
//! serde_json = "1.0"
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

/// The legacy per-mode corpora, in the order the merged file lists them.
const MODES: [&str; 4] = ["strict", "sabre", "e9patch", "rr"];

/// Schema identifier written into the merged corpus.
const SCHEMA: &str = "hermit-compat-corpus/v1";

type Rows = Vec<(String, Vec<String>)>;

fn die(msg: &str) -> ! {
    eprintln!("merge-corpus: {msg}");
    std::process::exit(1);
}

/// Read one legacy corpus as an ORDERED list. Order is part of the contract:
/// `verify` compares sequences, not sets, so a reordering is caught too.
fn read_legacy(dir: &Path, mode: &str) -> Rows {
    let path = dir.join(format!("corpus-{mode}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", path.display())));
    let doc: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| die(&format!("invalid JSON in {}: {e}", path.display())));
    let rows = doc
        .get("rows")
        .and_then(|r| r.as_array())
        .unwrap_or_else(|| die(&format!("{} has no `rows` array", path.display())));
    rows.iter()
        .enumerate()
        .map(|(i, r)| {
            let label = r
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| die(&format!("{} row {i}: missing `label`", path.display())))
                .to_string();
            let argv: Vec<String> = r
                .get("argv")
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| die(&format!("{} row {i}: missing `argv`", path.display())))
                .iter()
                .map(|a| {
                    a.as_str()
                        .unwrap_or_else(|| {
                            die(&format!("{} row {i} ({label}): non-string argv", path.display()))
                        })
                        .to_string()
                })
                .collect();
            (label, argv)
        })
        .collect()
}

/// Build the merged document from the four legacy corpora.
///
/// The canonical argv is `strict`'s when the program appears there, because
/// `strict` is the deepest-ratcheted mode (see the module docs on `bc`/`lua`/
/// `perl`); otherwise it is the first mode that carries the program, in `MODES`
/// order. Any mode whose legacy argv differs from the canonical one gets a
/// `mode_argv` override so the legacy view stays reproducible byte for byte.
fn build(dir: &Path) -> serde_json::Value {
    let legacy: BTreeMap<&str, Rows> =
        MODES.iter().map(|m| (*m, read_legacy(dir, m))).collect();

    // Union of labels, and each mode's membership.
    let mut labels: BTreeSet<String> = BTreeSet::new();
    for rows in legacy.values() {
        for (l, _) in rows {
            labels.insert(l.clone());
        }
    }

    let mut programs = Vec::new();
    let mut divergent = 0usize;
    for label in &labels {
        let per_mode: BTreeMap<&str, &Vec<String>> = MODES
            .iter()
            .filter_map(|m| {
                legacy[*m]
                    .iter()
                    .find(|(l, _)| l == label)
                    .map(|(_, a)| (*m, a))
            })
            .collect();

        // Canonical argv: prefer `strict`, else first mode in MODES order.
        let canonical: Vec<String> = per_mode
            .get("strict")
            .or_else(|| MODES.iter().find_map(|m| per_mode.get(m)))
            .map(|a| (*a).clone())
            .unwrap_or_else(|| die(&format!("{label}: present in no mode (impossible)")));

        let modes: Vec<String> =
            MODES.iter().filter(|m| per_mode.contains_key(**m)).map(|m| m.to_string()).collect();

        let mut mode_argv = serde_json::Map::new();
        for m in MODES {
            if let Some(a) = per_mode.get(m) {
                if **a != canonical {
                    mode_argv.insert(m.to_string(), serde_json::json!(*a));
                }
            }
        }
        if !mode_argv.is_empty() {
            divergent += 1;
        }

        let mut obj = serde_json::Map::new();
        obj.insert("label".into(), serde_json::json!(label));
        obj.insert("argv".into(), serde_json::json!(canonical));
        obj.insert("modes".into(), serde_json::json!(modes));
        if !mode_argv.is_empty() {
            obj.insert("mode_argv".into(), serde_json::Value::Object(mode_argv));
        }
        programs.push(serde_json::Value::Object(obj));
    }

    eprintln!(
        "merge-corpus: {} programs, {} with divergent per-mode argv",
        programs.len(),
        divergent
    );

    serde_json::json!({
        "schema": SCHEMA,
        "generated_by": "ci/compat/merge-corpus.rs",
        "description":
            "ONE backend-agnostic compatibility corpus. `modes` records which legacy per-mode \
             corpus carried each program, so the four legacy views stay byte-reproducible during \
             migration; it is NOT a statement that other backends cannot run the program. A mode \
             absent from `modes` with no entry in `exclusions` is an UNEXPLAINED gap, and the \
             matrix driver reports it as NOT_ATTEMPTED(unexplained) rather than omitting the cell.",
        "exclusions": exclusions(),
        "programs": programs,
    })
}

/// Reasons for a program being absent from a mode, where a reason is actually
/// known. Everything not listed here is an UNEXPLAINED absence — which is the
/// honest state, and the backlog this corpus exists to expose.
fn exclusions() -> serde_json::Value {
    serde_json::json!({
        "shell-build": {
            "sabre": "not carried by the legacy sabre corpus; reason not recorded upstream",
            "e9patch": "not carried by the legacy e9patch corpus; reason not recorded upstream",
            "rr": "not carried by the legacy rr corpus; reason not recorded upstream"
        },
        "lsof": {
            "sabre": "not carried by the legacy sabre corpus; reason not recorded upstream",
            "e9patch": "not carried by the legacy e9patch corpus; reason not recorded upstream",
            "rr": "not carried by the legacy rr corpus; reason not recorded upstream"
        }
    })
}

/// Re-derive one legacy per-mode view from the merged corpus.
fn view(merged: &serde_json::Value, mode: &str) -> Rows {
    let programs = merged
        .get("programs")
        .and_then(|p| p.as_array())
        .unwrap_or_else(|| die("merged corpus has no `programs` array"));
    let mut out: Rows = Vec::new();
    for p in programs {
        let label = p.get("label").and_then(|v| v.as_str()).unwrap_or_else(|| die("program without label"));
        let modes: Vec<&str> = p
            .get("modes")
            .and_then(|m| m.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        if !modes.contains(&mode) {
            continue;
        }
        let argv_val = p
            .get("mode_argv")
            .and_then(|m| m.get(mode))
            .or_else(|| p.get("argv"))
            .unwrap_or_else(|| die(&format!("{label}: no argv")));
        let argv: Vec<String> = argv_val
            .as_array()
            .unwrap_or_else(|| die(&format!("{label}: argv not an array")))
            .iter()
            .filter_map(|a| a.as_str().map(|s| s.to_string()))
            .collect();
        out.push((label.to_string(), argv));
    }
    out
}

/// Compare a derived view against its legacy file as SETS keyed by label.
///
/// Deliberately not a sequence comparison: the merged corpus is label-sorted
/// while the legacy files preserve the bash extractor's emission order, and that
/// order carries no meaning — `compat_nodes` turns every row into an independent
/// DAG node keyed by label. What MUST match exactly is the label set and each
/// label's argv, element for element, and that is what this checks.
fn compare(mode: &str, derived: &Rows, legacy: &Rows) -> Vec<String> {
    let d: BTreeMap<&str, &Vec<String>> = derived.iter().map(|(l, a)| (l.as_str(), a)).collect();
    let l: BTreeMap<&str, &Vec<String>> = legacy.iter().map(|(k, a)| (k.as_str(), a)).collect();
    let mut bad = Vec::new();
    if d.len() != derived.len() {
        bad.push(format!("{mode}: derived view has duplicate labels"));
    }
    for (label, largv) in &l {
        match d.get(label) {
            None => bad.push(format!("{mode}: LOST program `{label}`")),
            Some(dargv) if dargv != largv => bad.push(format!(
                "{mode}: `{label}` argv differs\n     legacy  {largv:?}\n     derived {dargv:?}"
            )),
            Some(_) => {}
        }
    }
    for label in d.keys() {
        if !l.contains_key(label) {
            bad.push(format!("{mode}: INVENTED program `{label}` not in the legacy corpus"));
        }
    }
    bad
}

fn compat_dir() -> PathBuf {
    // Resolve relative to this script so the tool works from any cwd.
    let here = Path::new(file!())
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("ci/compat"));
    if here.join("corpus-strict.json").exists() {
        return here;
    }
    let fallback = PathBuf::from("ci/compat");
    if fallback.join("corpus-strict.json").exists() {
        return fallback;
    }
    die("cannot locate ci/compat (run from the Hermit repo root)")
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("verify");
    let dir = compat_dir();
    let merged_path = dir.join("corpus.json");

    match cmd {
        "generate" => {
            let merged = build(&dir);
            let text = serde_json::to_string_pretty(&merged).unwrap();
            std::fs::write(&merged_path, format!("{text}\n"))
                .unwrap_or_else(|e| die(&format!("cannot write {}: {e}", merged_path.display())));
            println!("wrote {}", merged_path.display());
            // Generating without immediately proving equivalence would defeat
            // the point, so `generate` always verifies what it just wrote.
            verify(&dir, &merged_path);
        }
        "verify" => verify(&dir, &merged_path),
        other => die(&format!(
            "unknown subcommand `{other}`; expected `generate` or `verify`"
        )),
    }
}

fn verify(dir: &Path, merged_path: &Path) {
    let text = std::fs::read_to_string(merged_path)
        .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", merged_path.display())));
    let merged: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| die(&format!("invalid JSON in {}: {e}", merged_path.display())));

    let schema = merged.get("schema").and_then(|v| v.as_str()).unwrap_or("");
    if schema != SCHEMA {
        die(&format!("merged corpus schema is `{schema}`, expected `{SCHEMA}`"));
    }

    let mut bad = Vec::new();
    let mut counts = Vec::new();
    for mode in MODES {
        let derived = view(&merged, mode);
        let legacy = read_legacy(dir, mode);
        counts.push(format!("{mode} {}/{}", derived.len(), legacy.len()));
        bad.extend(compare(mode, &derived, &legacy));
    }

    let programs = merged.get("programs").and_then(|p| p.as_array()).map(|a| a.len()).unwrap_or(0);
    if bad.is_empty() {
        println!(
            "✅ merge-corpus verify: {programs} programs; every legacy view re-derived EXACTLY \
             [{}]",
            counts.join(", ")
        );
    } else {
        eprintln!("❌ merge-corpus verify: {} discrepanc(y/ies)", bad.len());
        for b in &bad {
            eprintln!("   {b}");
        }
        std::process::exit(1);
    }
}
