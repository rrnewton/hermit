#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Tier 2 of the two-tier `Cargo.lock` gate (Reverie pin-flow design,
//! Finding 4): the omission lint. A change that touches any `Cargo.toml`
//! must also touch the owning Cargo root's `Cargo.lock`, or the author
//! must attach the Tier 1 freshness signal
//! (`scripts/check-cargo-lock-freshness.rs`) showing the committed lock
//! is already fresh. This catches "bumped the manifest, forgot the
//! lock" at diff time, in milliseconds, with no Cargo run — which is
//! the only place an fbsource-first edit can be caught, because the
//! exported lockfiles cannot be certified in place inside fbsource
//! (the internal workspace swaps roots and legitimately resolves
//! differently; see the design).
//!
//! Root ownership, in either tree shape (GitHub/export paths like
//! `reverie/reverie/Cargo.toml`, or fbsource paths like
//! `fbcode/hermetic_infra/reverie/reverie/Cargo.toml`): a manifest under
//! `reverie/` is owned by `reverie/Cargo.lock`; under
//! `liteinst-runtime-build/` by `liteinst-runtime-build/Cargo.lock`;
//! any other Hermit manifest by the Hermit root `Cargo.lock`. In
//! fbsource this same path logic is registered as an `arc lint` under
//! `fbcode//hermetic_infra` (changed files from arc, fed via
//! `--files-from-stdin`); the rule lives here so both trees run one
//! implementation and one test suite.
//!
//! Changed files come from `--changed-from-git <base>` (`git diff
//! --name-only <base>` plus uncommitted changes when the base is a
//! commit) or from stdin with `--files-from-stdin`, one path per line.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

/// The three owning lockfiles, as Hermit-checkout-relative paths.
const HERMIT_LOCK: &str = "Cargo.lock";
const REVERIE_LOCK: &str = "reverie/Cargo.lock";
const LITEINST_LOCK: &str = "liteinst-runtime-build/Cargo.lock";

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    base: Option<String>,
    files_from_stdin: bool,
}

fn usage() -> &'static str {
    "Usage: check-cargo-lock-cochange.rs [OPTIONS]\n\
     \n\
     Refuse a change that edits a Cargo.toml without its owning root's\n\
     Cargo.lock. Remedies: regenerate and include the lockfile, or attach\n\
     the Tier 1 signal (scripts/check-cargo-lock-freshness.rs passing).\n\
     \n\
     Options:\n\
       --changed-from-git BASE   Changed files via `git diff --name-only BASE`\n\
       --files-from-stdin        Read changed paths from stdin, one per line\n\
       --repo PATH               Hermit checkout (default: git root)\n\
       -h, --help                Show this help"
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => config.repo = Some(PathBuf::from(take_value(&args, &mut i, "--repo")?)),
            "--changed-from-git" => {
                config.base = Some(take_value(&args, &mut i, "--changed-from-git")?)
            }
            "--files-from-stdin" => config.files_from_stdin = true,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    if config.base.is_some() == config.files_from_stdin {
        return Err(format!(
            "exactly one of --changed-from-git or --files-from-stdin is required\n{}",
            usage()
        ));
    }
    Ok(config)
}

/// Normalize a changed path from either tree shape to a
/// Hermit-checkout-relative path: strip everything through
/// `hermetic_infra/hermit/` or `hermetic_infra/` (the latter keeps the
/// `hermit/`/`reverie/` project segment, mapped below), and backslashes
/// never occur in these trees.
fn normalize_path(raw: &str) -> String {
    let path = raw.trim().trim_start_matches("./");
    if let Some(rest) = path.split("hermetic_infra/hermit/").nth(1) {
        if path.contains("hermetic_infra/hermit/") {
            return rest.to_string();
        }
    }
    if let Some(rest) = path.strip_prefix("hermetic_infra/") {
        // fbcode/hermetic_infra/reverie/... with the fbcode/ prefix
        // already stripped by the hermit/ arm's split fallback below.
        return rest.to_string();
    }
    if let Some(idx) = path.find("hermetic_infra/") {
        return path[idx + "hermetic_infra/".len()..].to_string();
    }
    path.to_string()
}

/// The owning root lockfile for a normalized manifest path, or `None`
/// when the path is not a manifest this lint owns. Paths here may still
/// carry the fbsource project segment (`hermit/...`, `reverie/...`)
/// after normalization; `hermit/` is peeled for the Hermit-owned cases.
fn owning_lock(normalized: &str) -> Option<&'static str> {
    if !normalized.ends_with("Cargo.toml") {
        return None;
    }
    let path = normalized.strip_prefix("hermit/").unwrap_or(normalized);
    if path.starts_with("reverie/") || path == "reverie/Cargo.toml" {
        Some(REVERIE_LOCK)
    } else if path.starts_with("liteinst-runtime-build/") {
        Some(LITEINST_LOCK)
    } else {
        Some(HERMIT_LOCK)
    }
}

/// True when the changed set contains the given owning lockfile, in
/// either tree shape.
fn lock_changed(changed: &BTreeSet<String>, lock: &str) -> bool {
    changed.iter().any(|path| {
        let normalized = normalize_path(path);
        let peeled = normalized.strip_prefix("hermit/").unwrap_or(&normalized);
        peeled == lock
    })
}

/// Apply the co-change rule to a changed-path set. Returns the list of
/// `(manifest, owning lock)` violations, sorted for stable output.
fn violations(changed: &BTreeSet<String>) -> Vec<(String, &'static str)> {
    let mut found = Vec::new();
    for path in changed {
        let normalized = normalize_path(path);
        if let Some(lock) = owning_lock(&normalized) {
            if !lock_changed(changed, lock) {
                found.push((normalized, lock));
            }
        }
    }
    found.sort();
    found
}

fn git_root() -> Result<PathBuf, String> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|error| format!("could not run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

fn changed_from_git(repo: &PathBuf, base: &str) -> Result<BTreeSet<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--name-only", base])
        .output()
        .map_err(|error| format!("could not run git diff: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff --name-only {base} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("CARGO LOCK CO-CHANGE LINT: {title}");
    eprintln!("======================================================================");
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let repo = config.repo.clone().map_or_else(git_root, Ok)?;
    let changed = if config.files_from_stdin {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .map_err(|error| format!("could not read changed paths from stdin: {error}"))?;
        input
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        changed_from_git(&repo, config.base.as_deref().unwrap_or("HEAD"))?
    };
    eprintln!(
        "Scope: checking {} changed path(s) for Cargo.toml/Cargo.lock co-change.",
        changed.len()
    );

    let found = violations(&changed);
    if found.is_empty() {
        println!("OK: every changed Cargo.toml is accompanied by its owning Cargo.lock.");
        return Ok(0);
    }
    loud_header("MANIFEST CHANGED WITHOUT ITS LOCKFILE - BLOCKED");
    for (manifest, lock) in &found {
        eprintln!("  {manifest} changed without {lock}");
    }
    eprintln!();
    eprintln!(
        "Either regenerate and include the owning lockfile(s) above \
         (`cargo metadata --format-version 1` at that root), or attach the \
         Tier 1 signal showing the committed lock is already fresh: \
         scripts/check-cargo-lock-freshness.rs passing on this change."
    );
    Ok(1)
}

fn run() -> Result<i32, String> {
    run_with_config(parse_args()?)
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            loud_header("CHECKER ERROR - BLOCKED");
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[test]
    fn manifest_only_change_is_refused_for_each_root() {
        assert_eq!(
            violations(&changed(&["detcore/Cargo.toml"])),
            [("detcore/Cargo.toml".to_string(), HERMIT_LOCK)]
        );
        assert_eq!(
            violations(&changed(&["reverie/reverie/Cargo.toml"])),
            [("reverie/reverie/Cargo.toml".to_string(), REVERIE_LOCK)]
        );
        assert_eq!(
            violations(&changed(&["liteinst-runtime-build/runtime/Cargo.toml"])),
            [(
                "liteinst-runtime-build/runtime/Cargo.toml".to_string(),
                LITEINST_LOCK
            )]
        );
    }

    #[test]
    fn manifest_with_owning_lock_passes_and_lock_only_passes() {
        assert!(violations(&changed(&["detcore/Cargo.toml", "Cargo.lock"])).is_empty());
        assert!(
            violations(&changed(&["reverie/reverie/Cargo.toml", "reverie/Cargo.lock"]))
                .is_empty()
        );
        assert!(violations(&changed(&[
            "liteinst-runtime-build/Cargo.toml",
            "liteinst-runtime-build/Cargo.lock",
        ]))
        .is_empty());
        assert!(violations(&changed(&["Cargo.lock"])).is_empty());
        assert!(violations(&changed(&["docs/Users.md"])).is_empty());
        assert!(violations(&changed(&[])).is_empty());
    }

    #[test]
    fn wrong_roots_lock_does_not_satisfy_the_rule() {
        // The Hermit root lock moving does not cover a Reverie manifest:
        // each root's lock is resolved independently.
        assert_eq!(
            violations(&changed(&["reverie/reverie-ptrace/Cargo.toml", "Cargo.lock"])).len(),
            1
        );
    }

    #[test]
    fn fbsource_path_shapes_map_to_the_same_roots() {
        assert_eq!(
            violations(&changed(&["fbcode/hermetic_infra/hermit/detcore/Cargo.toml"])),
            [("detcore/Cargo.toml".to_string(), HERMIT_LOCK)]
        );
        assert!(violations(&changed(&[
            "fbcode/hermetic_infra/reverie/reverie/Cargo.toml",
            "fbcode/hermetic_infra/reverie/Cargo.lock",
        ]))
        .is_empty());
        assert!(violations(&changed(&[
            "fbcode/hermetic_infra/hermit/liteinst-runtime-build/Cargo.toml",
            "fbcode/hermetic_infra/hermit/liteinst-runtime-build/Cargo.lock",
        ]))
        .is_empty());
    }
}
