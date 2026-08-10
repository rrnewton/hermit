#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Aggregate N `backend-matrix` sweeps into PER-CELL STABILITY.
//!
//! # Why a single sweep must not be published
//!
//! Two sweeps of the identical 1284-cell matrix were measured to differ by up to
//! **7 cells per backend**. So a cell's last observed state is not its state, and
//! a one-shot `FAIL` published against a NAMED PROGRAM is worse than an empty
//! cell: an empty cell reads as "we do not know", while a wrong `FAIL` reads as
//! knowledge and is correspondingly harder to undo.
//!
//! # The four-state argument, applied to repeatability instead of attempt
//!
//! `backend-matrix` refuses to collapse *not attempted* into *fine*. This refuses
//! the same collapse one axis over: **a cell that fails 5/5 and a cell that fails
//! 2/5 must not render identically.** One is a gap; the other is a flake, and
//! they call for completely different work — a product fix versus a harness or
//! host investigation.
//!
//! | stability | meaning |
//! | --- | --- |
//! | `STABLE` | every sweep agreed. The observed state is the cell's state. |
//! | `FLAKY` | the sweeps disagreed. There is NO single cell state, and the majority is reported only alongside the split. |
//! | `UNDERSAMPLED` | fewer than `--min-runs` observations exist. Not a result. |
//!
//! A `FLAKY` cell is **never** published as a named-program defect. It is
//! published as a flake, with its exact split, which is a different and honest
//! claim.
//!
//! # Reading the output
//!
//! `stability.csv` carries one row per `(program, backend)` with the count of
//! each state across sweeps, so a consumer can re-derive any view without
//! trusting this tool's summary. `dominant_state` is only meaningful when
//! `stability=STABLE`; for `FLAKY` rows the `n_*` columns are the result.
//!
//! ```cargo
//! [dependencies]
//! ```

#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;

fn die(msg: &str) -> ! {
    eprintln!("stability: {msg}");
    std::process::exit(1);
}

/// Minimal RFC4180-ish CSV line splitter (handles quoted fields with `""`).
fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_q && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => in_q = !in_q,
            ',' if !in_q => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

struct Obs {
    states: Vec<String>,
    reasons: Vec<String>,
}

fn main() {
    rust_script_prelude::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out = PathBuf::from("ignored/compat/stability.csv");
    let mut min_runs: usize = 3;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = PathBuf::from(args.get(i).unwrap_or_else(|| die("--out needs a value")));
            }
            "--min-runs" => {
                i += 1;
                min_runs = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(3);
            }
            "--help" | "-h" => {
                println!(
                    "stability — aggregate N backend-matrix sweeps into per-cell stability\n\n\
                     usage: stability.rs [--out PATH] [--min-runs N] sweep1.csv sweep2.csv ...\n"
                );
                return;
            }
            other => inputs.push(PathBuf::from(other)),
        }
        i += 1;
    }
    if inputs.is_empty() {
        die("no sweep CSVs given (try --help)");
    }

    let mut cells: BTreeMap<(String, String), Obs> = BTreeMap::new();
    let mut sweeps_seen = 0usize;
    for path in &inputs {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| die(&format!("cannot read {}: {e}", path.display())));
        let mut lines = text.lines();
        let header = lines.next().unwrap_or_else(|| die(&format!("{} is empty", path.display())));
        let cols: Vec<String> = split_csv(header);
        let idx = |name: &str| -> usize {
            cols.iter()
                .position(|c| c == name)
                .unwrap_or_else(|| die(&format!("{} has no `{name}` column", path.display())))
        };
        let (ip, ib, is, ir) = (idx("program"), idx("backend"), idx("state"), idx("reason"));
        let mut rows = 0usize;
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let f = split_csv(line);
            if f.len() <= ir {
                continue;
            }
            let e = cells
                .entry((f[ip].clone(), f[ib].clone()))
                .or_insert_with(|| Obs { states: Vec::new(), reasons: Vec::new() });
            e.states.push(f[is].clone());
            e.reasons.push(f[ir].clone());
            rows += 1;
        }
        eprintln!("stability: {} -> {rows} cells", path.display());
        sweeps_seen += 1;
    }

    if let Some(p) = out.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    let mut f = std::fs::File::create(&out)
        .unwrap_or_else(|e| die(&format!("cannot write {}: {e}", out.display())));
    writeln!(
        f,
        "program,backend,runs,stability,dominant_state,n_pass,n_fail,n_timeout,n_unqualifiable,n_not_attempted,distinct_states,observed_sequence,example_reason"
    )
    .unwrap();

    let mut per_backend: BTreeMap<String, BTreeMap<&'static str, usize>> = BTreeMap::new();
    let mut flaky_rows: Vec<(String, String, String)> = Vec::new();
    let mut stable_fail_by_backend: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for ((program, backend), obs) in &cells {
        let runs = obs.states.len();
        let count = |s: &str| obs.states.iter().filter(|x| x.as_str() == s).count();
        let (np, nf, nt, nu, nn) = (
            count("PASS"),
            count("FAIL"),
            count("TIMEOUT"),
            count("ATTEMPTED_UNQUALIFIABLE"),
            count("NOT_ATTEMPTED"),
        );
        let distinct: BTreeSet<&str> = obs.states.iter().map(|s| s.as_str()).collect();
        let stability = if runs < min_runs {
            "UNDERSAMPLED"
        } else if distinct.len() == 1 {
            "STABLE"
        } else {
            "FLAKY"
        };
        // Only meaningful for STABLE. Emitted for FLAKY too, but the summary and
        // the docs both say the split is the result there — a majority label on a
        // flaky cell is exactly the collapse this tool exists to prevent.
        let dominant = obs
            .states
            .iter()
            .fold(BTreeMap::<&str, usize>::new(), |mut m, s| {
                *m.entry(s.as_str()).or_default() += 1;
                m
            })
            .into_iter()
            .max_by_key(|(_, n)| *n)
            .map(|(s, _)| s.to_string())
            .unwrap_or_default();

        let bucket = per_backend.entry(backend.clone()).or_default();
        *bucket.entry(match stability {
            "STABLE" => match dominant.as_str() {
                "PASS" => "stable_pass",
                "FAIL" => "stable_fail",
                "TIMEOUT" => "stable_timeout",
                "ATTEMPTED_UNQUALIFIABLE" => "stable_unqual",
                _ => "stable_na",
            },
            "FLAKY" => "flaky",
            _ => "undersampled",
        })
        .or_default() += 1;

        if stability == "FLAKY" {
            flaky_rows.push((
                backend.clone(),
                program.clone(),
                obs.states
                    .iter()
                    .map(|s| match s.as_str() {
                        "PASS" => "P",
                        "FAIL" => "F",
                        "TIMEOUT" => "T",
                        "ATTEMPTED_UNQUALIFIABLE" => "U",
                        _ => "N",
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ));
        }
        if stability == "STABLE" && (dominant == "FAIL" || dominant == "TIMEOUT") {
            stable_fail_by_backend.entry(backend.clone()).or_default().push(program.clone());
        }

        let seq = obs
            .states
            .iter()
            .map(|s| match s.as_str() {
                "PASS" => "P",
                "FAIL" => "F",
                "TIMEOUT" => "T",
                "ATTEMPTED_UNQUALIFIABLE" => "U",
                _ => "N",
            })
            .collect::<Vec<_>>()
            .join("");
        let example = obs
            .reasons
            .iter()
            .find(|r| !r.is_empty())
            .cloned()
            .unwrap_or_default()
            .replace(['\n', '\r'], " ");
        let esc = |s: &str| {
            if s.contains([',', '"']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        };
        writeln!(
            f,
            "{},{},{runs},{stability},{dominant},{np},{nf},{nt},{nu},{nn},{},{seq},{}",
            esc(program),
            esc(backend),
            distinct.len(),
            esc(&example)
        )
        .unwrap();
    }

    // ---------------------------------------------------------------- summary
    println!("\nstability over {sweeps_seen} sweep(s), {} cells -> {}", cells.len(), out.display());
    println!("\n{:<9} {:>11} {:>11} {:>13} {:>10} {:>6}   {}", "backend", "stable_pass", "stable_fail", "stable_unqual", "stable_TMO", "FLAKY", "publishable-as-defect");
    let mut tot_flaky = 0usize;
    for (b, m) in &per_backend {
        let g = |k: &str| m.get(k).copied().unwrap_or(0);
        tot_flaky += g("flaky");
        println!(
            "{:<9} {:>11} {:>11} {:>13} {:>10} {:>6}   {}",
            b,
            g("stable_pass"),
            g("stable_fail"),
            g("stable_unqual"),
            g("stable_timeout"),
            g("flaky"),
            g("stable_fail") + g("stable_timeout")
        );
    }
    println!("\nFLAKY cells (NOT publishable as named-program defects): {tot_flaky}");
    if !flaky_rows.is_empty() {
        flaky_rows.sort();
        println!("\n{:<9} {:<22} sequence", "backend", "program");
        for (b, p, seq) in flaky_rows.iter().take(60) {
            println!("{b:<9} {p:<22} {seq}");
        }
        if flaky_rows.len() > 60 {
            println!("... and {} more (see the CSV)", flaky_rows.len() - 60);
        }
    }
    println!(
        "\nSTABLE means every sweep agreed. FLAKY means they did not, and a flaky cell has NO single\n\
         state — its split IS the result. Only stable_fail + stable_TMO may be published against a\n\
         named program."
    );
}
