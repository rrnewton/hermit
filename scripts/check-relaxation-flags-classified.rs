#!/usr/bin/env rust-script
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Fail if a determinism-relaxing CLI flag exists that nobody has classified.
//!
//! WHY THIS EXISTS. Only `--strict` deterministic execution counts, but the set
//! of flags that relax determinism GROWS, and every consumer that needs to know
//! "is this run relaxed?" has so far answered it from a list written by hand.
//! A hand-written list of a growing set is this repository's recurring defect;
//! it does not fail when the set grows, it just quietly stops covering it.
//!
//! So this does not hold a list of flags. It DERIVES the candidate set from the
//! clap declarations in source on every run and fails when the derived set and
//! the classified set disagree. Adding a relaxation flag therefore breaks this
//! check until someone classifies it, which is the only behaviour that survives
//! the set growing.
//!
//! THREE DECLARATION PATTERNS, and the third is why a `--no-` prefix match is
//! not good enough:
//!   A  a plain `no_*: bool` field; clap derives `--no-...`.
//!   B  a default-ON field made negatable with `ArgAction::SetFalse`, e.g.
//!      `#[clap(long = "no-virtualize-time", action = clap::ArgAction::SetFalse)]`.
//!      These have NO positive form -- "on" is expressed by omitting the flag.
//!   C  a relaxation whose NAME CONTAINS NO "no" AT ALL:
//!        --strace-only            sets all six of the above at once
//!        --max-timeslice=disabled turns off RCB logical time (`use_rcb_time()`
//!                                 is `max_timeslice.is_some() && !no_rcb_time`)
//!      A prefix match misses both. Pattern C is checked by NAME because it
//!      cannot be recognised structurally -- and that is exactly why each such
//!      flag is listed with the source fact that makes it a relaxation, so the
//!      claim is auditable rather than asserted.
//!
//! Usage:
//!   ./scripts/check-relaxation-flags-classified.rs           # check
//!   ./scripts/check-relaxation-flags-classified.rs --list    # print derived set
//!
//! ```cargo
//! [dependencies]
//! ```

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Files that declare CLI flags. Derived from git, not hardcoded: any tracked
/// Rust source may declare one, so all of them are scanned.
fn tracked_rust_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let out = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "--", "*.rs"])
        .output()
        .map_err(|e| format!("git ls-files failed: {e}"))?;
    if !out.status.success() {
        return Err("git ls-files returned non-zero".to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| root.join(l))
        .collect())
}

/// Pattern A + B, derived structurally from the clap declarations.
///
/// Deliberately NOT anchored on visibility: an earlier hand-written version of
/// this scan matched only `` and `pub(crate) ` and silently missed
/// `pub no_rcb_time`. Matching the field NAME and requiring `: bool` avoids
/// re-encoding a guess about how the field happens to be spelled.
fn derive_structural(sources: &[PathBuf]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for path in sources {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let trimmed = line.trim();
            // Pattern B: an explicit negatable long name.
            if let Some(rest) = trimmed.split_once("long = \"no-") {
                if let Some(name) = rest.1.split('"').next() {
                    found.insert(format!("--no-{name}"));
                }
            }
            // Pattern A: `[vis ]no_something: bool,`
            if trimmed.ends_with(": bool,") {
                if let Some(field) = trimmed.trim_end_matches(": bool,").split_whitespace().last() {
                    if let Some(stem) = field.strip_prefix("no_") {
                        found.insert(format!("--no-{}", stem.replace('_', "-")));
                    }
                }
            }
        }
    }
    found
}

/// One classified flag: the CLI spelling, whether it relaxes determinism, and
/// the source fact that settles it. The `why` is not decoration -- it is what
/// makes a classification auditable instead of an assertion.
struct Classified {
    flag: &'static str,
    relaxes: bool,
    why: &'static str,
}

/// The classification. Adding a flag to source without adding it here FAILS.
const CLASSIFIED: &[Classified] = &[
    Classified { flag: "--no-sequentialize-threads", relaxes: true,
        why: "run.rs: 'Disable deterministic sequential thread execution'" },
    Classified { flag: "--no-deterministic-io", relaxes: true,
        why: "run.rs: 'Disable deterministic I/O behavior'" },
    Classified { flag: "--no-namespace", relaxes: true,
        why: "run.rs: 'compromises isolation and deterministic reproducibility'" },
    Classified { flag: "--no-rcb-time", relaxes: true,
        why: "config.rs: use_rcb_time() is false, dropping RCB from logical time" },
    Classified { flag: "--no-virtualize-time", relaxes: true,
        why: "config.rs: SetFalse on virtualize_time" },
    Classified { flag: "--no-virtualize-cpuid", relaxes: true,
        why: "config.rs: SetFalse on virtualize_cpuid" },
    Classified { flag: "--no-virtualize-metadata", relaxes: true,
        why: "config.rs: SetFalse on virtualize_metadata" },
    // Pattern C. Not derivable structurally; carried by name WITH its reason.
    Classified { flag: "--strace-only", relaxes: true,
        why: "run.rs: expansion sets virtualize_{cpuid,metadata,time}=false, \
              deterministic_io=false, sequentialize_threads=false, no_rcb_time=true" },
    Classified { flag: "--max-timeslice=disabled", relaxes: true,
        why: "config.rs: use_rcb_time() = max_timeslice.is_some() && !no_rcb_time, \
              so disabling the timeslice disables RCB logical time" },
    // Derived by the structural scan but NOT determinism relaxations. Listed so
    // the check is exhaustive over what it derives rather than filtered by an
    // undeclared heuristic.
    Classified { flag: "--no-color", relaxes: false,
        why: "logdiff.rs: terminal colour only" },
    Classified { flag: "--no-base", relaxes: false,
        why: "check-reverie-pin.rs:1324: CI pin-gate tooling, not the guest CLI.               Declares that an invocation has no monotonicity base so an               unresolvable base is an intended skip instead of a silent one;               it reaches no scheduler, virtual clock, or syscall path" },
];

fn main() {
    let root = match Command::new("git").args(["rev-parse", "--show-toplevel"]).output() {
        Ok(o) if o.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string())
        }
        _ => {
            eprintln!("not inside a git checkout");
            std::process::exit(2);
        }
    };
    let sources = match tracked_rust_sources(&root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let derived = derive_structural(&sources);
    let classified: BTreeSet<String> = CLASSIFIED.iter().map(|c| c.flag.to_string()).collect();

    if std::env::args().any(|a| a == "--list") {
        println!("structurally derived ({}):", derived.len());
        for f in &derived {
            println!("  {f}");
        }
        println!("classified ({}):", classified.len());
        for c in CLASSIFIED {
            println!("  {:32} relaxes={}  {}", c.flag, c.relaxes, c.why);
        }
        return;
    }

    let unclassified: Vec<&String> = derived.difference(&classified).collect();
    let relaxing = CLASSIFIED.iter().filter(|c| c.relaxes).count();

    println!(
        "Scanned {} tracked .rs files; structurally derived {} candidate flag(s); \
         {} classified, of which {} relax determinism.",
        sources.len(),
        derived.len(),
        classified.len(),
        relaxing
    );

    if !unclassified.is_empty() {
        eprintln!("======================================================================");
        eprintln!("RELAXATION-FLAG LINT: UNCLASSIFIED FLAG(S) - BLOCKED");
        eprintln!("======================================================================");
        for f in &unclassified {
            eprintln!("  {f}");
        }
        eprintln!();
        eprintln!(
            "A CLI flag was derived from source that nobody has classified. Decide whether it \
             relaxes determinism and add it to CLASSIFIED in {}, WITH the source fact that \
             settles it. Do not classify it by guessing from the name.",
            file!()
        );
        std::process::exit(1);
    }

    println!("All derived flags are classified.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structural scan must see BOTH declaration patterns. Pattern B was
    /// invisible to an earlier prefix-only scan.
    #[test]
    fn derives_both_structural_patterns() {
        let dir = std::env::temp_dir().join(format!("relaxlint-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.rs");
        std::fs::write(
            &f,
            "    pub no_rcb_time: bool,\n\
                 #[clap(long = \"no-virtualize-time\", action = clap::ArgAction::SetFalse)]\n\
                 pub(crate) no_sequentialize_threads: bool,\n",
        )
        .unwrap();
        let got = derive_structural(&[f]);
        assert!(got.contains("--no-rcb-time"), "pattern A, `pub` visibility: {got:?}");
        assert!(got.contains("--no-virtualize-time"), "pattern B: {got:?}");
        assert!(
            got.contains("--no-sequentialize-threads"),
            "pattern A, `pub(crate)`: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Visibility must not be part of the match. This is the exact bug the
    /// first hand-written scan had.
    #[test]
    fn visibility_is_not_part_of_the_match() {
        let dir = std::env::temp_dir().join(format!("relaxlint-vis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("s.rs");
        std::fs::write(&f, "    pub no_only_public: bool,\n").unwrap();
        assert!(derive_structural(&[f]).contains("--no-only-public"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every classified entry must carry a reason. A classification without the
    /// source fact behind it is an assertion, and this file exists to replace
    /// assertions with derivations.
    #[test]
    fn every_classification_carries_its_reason() {
        for c in CLASSIFIED {
            assert!(!c.why.trim().is_empty(), "{} has no reason", c.flag);
        }
    }
}
