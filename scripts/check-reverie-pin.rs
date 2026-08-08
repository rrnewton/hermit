#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Block Hermit testing when its Reverie dependency pin is not exactly
//! `rrnewton/reverie:main`.
//!
//! The recorded SHA makes historical Hermit builds reproducible. It is not
//! permission to test against an old Reverie: every testing path requires the
//! recorded SHA to equal the live `main` tip. All Reverie revisions across
//! tracked Cargo dependency metadata must also be identical.
//!
//! Scope is derived with `git ls-files`: every tracked `Cargo.toml` and
//! `Cargo.lock` is inspected, including tracked vendored paths. Untracked or
//! generated files and files inside nested submodules are outside this check;
//! their contents are not tracked by the Hermit repository.
//!
//! Local use on Meta hosts:
//!
//! ```text
//! with-proxy ./ci/run-reverie-pin-check.sh
//! ```
//!
//! Repair every derived manifest and lockfile site with one command:
//!
//! ```text
//! with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest
//! ```

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const DEFAULT_REMOTE: &str = "https://github.com/rrnewton/reverie.git";
const MAIN_REF: &str = "refs/heads/main";
/// How to treat FRESHNESS — "is the pin equal to the ref `rrnewton/reverie:main`
/// currently points at" — as distinct from CONSISTENCY.
///
/// The distinction is the whole point, so it is a type rather than a bool:
/// consistency (the 46 revision entries agreeing with each other, and the
/// LiteInst cache keys prefixing the pin) is a statement about THE TREE BEING
/// COMMITTED and is never negotiable. Freshness is a statement about a remote
/// ref that moves under you, and about whether this host has egress at all.
/// Gating an unrelated commit on the second one is how a stale pin came to block
/// every commit in the repository and taught the fleet to pass `--no-verify` —
/// which discards consistency too, along with the nested-lockfile guard.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Freshness {
    /// Stale or unverifiable freshness is fatal. The default, so CI, `make lint`
    /// and every existing caller keep exactly today's behaviour.
    #[default]
    Block,
    /// Report freshness advisorily and never fail on it. Does NOT contact the
    /// network: a commit that touches no pin site must not be blockable by a
    /// hanging or unreachable remote.
    Warn,
    /// Decide from the staged change: `Block` if it touches a pin site,
    /// `Warn` if it does not. Resolves to one of the two above before use.
    Auto,
}

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    #[cfg(test)]
    remote: Option<String>,
    print_pin: bool,
    update_to_latest: bool,
    freshness: Freshness,
}

#[derive(Debug)]
struct PinOccurrence {
    path: PathBuf,
    line: usize,
    rev: String,
}

struct PinScan {
    occurrences: Vec<PinOccurrence>,
    tracked_files: Vec<PathBuf>,
}

fn usage() -> &'static str {
    "Usage: check-reverie-pin.rs [OPTIONS]\n\
     \n\
     Options:\n\
       --repo PATH                         Hermit checkout (default: git root)\n\
       --print-pin                         Print the single locally recorded pin; no network\n\
       --update-to-latest                  Update every derived Cargo pin site to latest main\n\
       --freshness block|warn|auto         Is a stale/unverifiable pin fatal? (default: block)\n\
                                           warn: advisory, and no network call\n\
                                           auto: block only if the STAGED change touches a pin site\n\
       -h, --help                          Show this help\n\
     \n\
     Scope: every tracked Cargo.toml and Cargo.lock from git ls-files.\n\
     Excludes non-Cargo files, untracked/generated files, and nested submodule contents."
}

/// Resolve [`Freshness::Auto`] against the staged change.
///
/// THE TRIGGER IS "DOES THIS CHANGE CARRY A PIN VALUE", and nothing wider.
/// I first also treated the pin MACHINERY — this checker, its launcher, the hook
/// — as a trigger, on a defence-in-depth argument, and it was wrong twice over.
/// It buys no protection: the hook compiles and runs the checker from the
/// WORKING TREE, so an edit that weakened the check would be judged by the
/// weakened check either way; freshness blocking cannot detect that. And it
/// recreates the exact deadlock this scoping exists to remove, aimed precisely
/// at whoever is repairing the pin machinery — including the commit that
/// introduced this function, which could then only have landed with the
/// `--no-verify` it exists to make unnecessary. A gate its own maintainer must
/// bypass is the failure mode, not a stricter version of the fix.
///
/// A genuine pin BUMP is unaffected and still strict: it stages ten Cargo files,
/// so it is in scope by content, and if main moved again mid-bump the value being
/// committed really is already wrong.
///
/// THE SCOPE IS DERIVED, NEVER HARDCODED. It is the intersection of the staged
/// paths with the checker's OWN scanned file set — the same `scan.tracked_files`
/// the verdict is computed from. A glob written into
/// the hook would be a second, independently-drifting copy of the scope, and the
/// first manifest that stopped matching it would be silently exempted from a
/// check it still needs. Deriving it means the two cannot disagree.
///
/// A newly ADDED manifest is covered without special-casing: `tracked_files`
/// comes from `git ls-files`, which reads the index, so a staged addition is
/// already in the scanned set.
///
/// FAILS CONSERVATIVE. Any uncertainty — no HEAD yet, git unavailable, a path we
/// cannot interpret — resolves to [`Freshness::Block`], i.e. exactly today's
/// behaviour. The relaxation must be something we affirmatively established, not
/// a default we fell into when the question could not be answered.
fn resolve_freshness(root: &Path, scan: &PinScan, requested: Freshness) -> (Freshness, String) {
    if requested != Freshness::Auto {
        return (requested, String::new());
    }
    let output = match git_in(root, &["diff", "--cached", "--name-only", "-z"]) {
        Ok(output) if output.status.success() => output,
        // No HEAD, a broken index, git missing: we cannot prove the change is
        // unrelated, so we do not get to relax anything.
        _ => {
            return (
                Freshness::Block,
                "the staged path set could not be determined".to_string(),
            );
        }
    };
    let staged: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .collect();
    if staged.is_empty() {
        // An empty staged set is not evidence of an unrelated change; it usually
        // means this is not a normal commit at all.
        return (
            Freshness::Block,
            "nothing is staged".to_string(),
        );
    }
    let scanned: BTreeSet<&Path> = scan.tracked_files.iter().map(PathBuf::as_path).collect();
    let touched: Vec<String> = staged
        .iter()
        .filter(|path| {
            scanned.contains(root.join(path).as_path())
        })
        .map(|path| path.display().to_string())
        .collect();
    if touched.is_empty() {
        (
            Freshness::Warn,
            format!(
                "{} staged path(s), 0 of them pin sites",
                staged.len()
            ),
        )
    } else {
        (
            Freshness::Block,
            format!(
                "{} of {} staged path(s) are pin sites: {}",
                touched.len(),
                staged.len(),
                touched.join(", ")
            ),
        )
    }
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
            "--print-pin" => config.print_pin = true,
            "--update-to-latest" => config.update_to_latest = true,
            "--freshness" => {
                config.freshness = match take_value(&args, &mut i, "--freshness")?.as_str() {
                    "block" => Freshness::Block,
                    "warn" => Freshness::Warn,
                    "auto" => Freshness::Auto,
                    other => {
                        return Err(format!(
                            "--freshness expects block|warn|auto, got {other:?}"
                        ));
                    }
                }
            }
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    if config.print_pin && config.update_to_latest {
        return Err("--print-pin and --update-to-latest are mutually exclusive".to_string());
    }
    Ok(config)
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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

fn tracked_cargo_metadata(root: &Path) -> Result<Vec<PathBuf>, String> {
    let output = git_in(
        root,
        &[
            "ls-files",
            "-z",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            ":(glob)**/Cargo.toml",
            ":(glob)**/Cargo.lock",
        ],
    )?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files for Cargo dependency metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut paths: Vec<PathBuf> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(|path| root.join(path))
        .collect();
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Err("git tracks no Cargo.toml or Cargo.lock files".to_string());
    }
    Ok(paths)
}

fn extract_rev(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    for index in 0..bytes.len().saturating_sub(2) {
        if &bytes[index..index + 3] != b"rev" {
            continue;
        }
        let before_is_key_char = index > 0
            && (bytes[index - 1].is_ascii_alphanumeric()
                || matches!(bytes[index - 1], b'_' | b'-'));
        if before_is_key_char {
            continue;
        }
        let mut cursor = index + 3;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'"') {
            return None;
        }
        cursor += 1;
        let end = bytes[cursor..].iter().position(|byte| *byte == b'"')? + cursor;
        return Some(line[cursor..end].to_string());
    }
    None
}

fn extract_lock_rev(line: &str) -> Option<String> {
    let start = line.find("?rev=")? + "?rev=".len();
    let rev: String = line[start..]
        .chars()
        .take_while(|character| character.is_ascii_hexdigit())
        .collect();
    (!rev.is_empty()).then_some(rev)
}

fn is_reverie_git_source(line: &str) -> bool {
    line.contains("github.com/") && line.contains("/reverie.git")
}

fn read_pins(root: &Path) -> Result<PinScan, String> {
    let tracked_files = tracked_cargo_metadata(root)?;

    let mut pins = Vec::new();
    for path in &tracked_files {
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let file_name = path.file_name().and_then(|name| name.to_str());
        for (line_index, line) in contents.lines().enumerate() {
            if !is_reverie_git_source(line) {
                continue;
            }
            let rev = match file_name {
                Some("Cargo.toml") => extract_rev(line),
                Some("Cargo.lock") => extract_lock_rev(line),
                _ => None,
            }
            .ok_or_else(|| {
                format!(
                    "{}:{} is a Reverie git dependency/source without a pinned rev",
                    path.display(),
                    line_index + 1
                )
            })?;
            if !is_full_sha(&rev) {
                return Err(format!(
                    "{}:{} has non-40-hex Reverie rev {rev:?}",
                    path.display(),
                    line_index + 1
                ));
            }
            pins.push(PinOccurrence {
                path: path.to_path_buf(),
                line: line_index + 1,
                rev,
            });
        }
    }
    if pins.is_empty() {
        return Err(
            "no pinned GitHub Reverie dependencies found in tracked Cargo.toml/Cargo.lock files"
                .to_string(),
        );
    }
    Ok(PinScan {
        occurrences: pins,
        tracked_files,
    })
}

fn query_main(remote: &str) -> Result<String, String> {
    let output = Command::new("git")
        .args(["ls-remote", "--exit-code", remote, MAIN_REF])
        .output()
        .map_err(|error| format!("could not run git ls-remote: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-remote {remote} {MAIN_REF} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let sha = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    if !is_full_sha(&sha) {
        return Err(format!("remote returned invalid main SHA {sha:?}"));
    }
    Ok(sha)
}

fn git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))
}

fn unique_pin(scan: &PinScan) -> Result<&str, String> {
    let pins: BTreeSet<&str> = scan
        .occurrences
        .iter()
        .map(|occurrence| occurrence.rev.as_str())
        .collect();
    if pins.len() != 1 {
        return Err(format!(
            "Reverie dependency metadata contains {} distinct revisions: {}",
            pins.len(),
            pins.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(scan.occurrences[0].rev.as_str())
}

fn run_cargo_update(root: &Path, args: &[&str]) -> Result<(), String> {
    let status = Command::new("cargo")
        .current_dir(root)
        .args(args)
        .status()
        .map_err(|error| format!("could not run cargo {}: {error}", args.join(" ")))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo {} failed with {status}; the manifest edits remain visible for repair",
            args.join(" ")
        ))
    }
}

fn rewrite_manifest_pins(scan: &PinScan, main: &str) -> Result<(usize, usize), String> {
    let old_revisions: BTreeSet<&str> = scan
        .occurrences
        .iter()
        .map(|occurrence| occurrence.rev.as_str())
        .filter(|revision| *revision != main)
        .collect();
    if old_revisions.is_empty() {
        return Ok((0, 0));
    }

    let manifest_paths: BTreeSet<&Path> = scan
        .occurrences
        .iter()
        .filter(|occurrence| {
            occurrence
                .path
                .file_name()
                .is_some_and(|name| name == "Cargo.toml")
        })
        .map(|occurrence| occurrence.path.as_path())
        .collect();
    let mut changed_files = 0;
    let mut changed_entries = 0;
    for path in manifest_paths {
        let original = fs::read_to_string(path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let mut updated = original.clone();
        for old in &old_revisions {
            let occurrences = updated.matches(*old).count();
            changed_entries += occurrences;
            updated = updated.replace(*old, main);
        }
        if updated != original {
            fs::write(path, updated)
                .map_err(|error| format!("could not update {}: {error}", path.display()))?;
            changed_files += 1;
        }
    }
    Ok((changed_files, changed_entries))
}

fn update_to_latest(root: &Path, scan: &PinScan, main: &str) -> Result<(), String> {
    if scan
        .occurrences
        .iter()
        .all(|occurrence| occurrence.rev == main)
    {
        println!("Reverie pin is already current: {main}");
        return Ok(());
    }

    let (changed_files, changed_entries) = rewrite_manifest_pins(scan, main)?;

    println!(
        "Updated {changed_entries} manifest revision entries in {changed_files} files; resolving tracked lockfiles."
    );
    run_cargo_update(root, &["update", "-p", "reverie-core"])?;
    let liteinst_manifest = root.join("liteinst-runtime-build/Cargo.toml");
    if liteinst_manifest.is_file() {
        let manifest = liteinst_manifest
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 manifest path: {}", liteinst_manifest.display()))?;
        run_cargo_update(
            root,
            &[
                "update",
                "--manifest-path",
                manifest,
                "-p",
                "reverie-liteinst",
            ],
        )?;
    }

    let updated = read_pins(root)?;
    let pin = unique_pin(&updated)?;
    if pin != main {
        return Err(format!(
            "update completed but derived Cargo metadata records {pin}, expected {main}"
        ));
    }
    println!(
        "Reverie pin updated to latest main {main} across {} derived revision entries.",
        updated.occurrences.len()
    );
    Ok(())
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("REVERIE PIN LINT: {title}");
    eprintln!("======================================================================");
}

fn blocked_instructions() {
    eprintln!();
    eprintln!("BLOCKED. Testing must use the latest rrnewton/reverie:main.");
    eprintln!("Update every derived manifest and lockfile site with:");
    eprintln!("  with-proxy ./ci/run-reverie-pin-check.sh --update-to-latest");
    eprintln!("Policy and recovery details: docs/updating-reverie.md");
}

/// Extract the short-SHA suffix of every LiteInst runtime cache-key token on a
/// line: `liteinst-runtime-build-<hex>` and `liteinst-runtime-<hex>` (6..=40
/// hex digits). Returns the captured short SHAs in order.
///
/// std-only on purpose: CI compiles this file with plain `rustc`
/// (`.github/workflows/ci-portable.yml`), not rust-script/cargo, so no external
/// crate (e.g. `regex`) is available. The nested-workspace path token
/// `liteinst-runtime-build/…` is deliberately NOT matched (it is a directory
/// name, not a revision key): after the optional `-build` the next byte must be
/// `-`, and the hex run must be at least 6 digits, so `-build/…` and the single
/// hex digit in `-build` are both rejected.
fn extract_cache_key_shas(line: &str) -> Vec<String> {
    const MARKER: &str = "liteinst-runtime";
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(rel) = line[from..].find(MARKER) {
        let idx = from + rel;
        let mut cursor = idx + MARKER.len();
        from = cursor;
        if line[cursor..].starts_with("-build") {
            cursor += "-build".len();
        }
        if bytes.get(cursor) != Some(&b'-') {
            continue;
        }
        cursor += 1;
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_hexdigit() {
            cursor += 1;
        }
        let sha = &line[start..cursor];
        if (6..=40).contains(&sha.len()) {
            found.push(sha.to_string());
        }
    }
    found
}

/// Bind every revision-keyed LiteInst runtime build/staging directory to the
/// canonical Reverie pin.
///
/// These directories (`target/liteinst-runtime-build-<short>`,
/// `build_root/liteinst-runtime-<short>`) embed a short prefix of the Reverie
/// pin so the staged runtime cache busts when the pin moves. If a bump updates
/// the Cargo manifests/locks but misses one of these string literals, the stale
/// directory silently reuses a runtime built against the OLD Reverie —
/// `hermit-install/build.rs` carried exactly this drift at `d973a85` after the
/// pin had advanced to `79517704…`. Rather than compare these heterogeneous
/// short forms to each other, bind each to the pin the manifests already agree
/// on: its short SHA MUST be a prefix of the full 40-hex rev. That also makes
/// them mutually consistent (all prefixes of one rev). Hard, offline (no
/// network), and shared by all three enforcement paths (hook, validate.sh, CI)
/// because every consumer already invokes this one checker.
fn check_liteinst_cache_keys(root: &Path, pin: &str) -> Result<i32, String> {
    // Exclude this checker's own source: it embeds deliberately-drifted example
    // tokens in its docstring and test fixtures (a check must not scan the file
    // that defines it). No real revision-keyed cache directory is named here.
    let output = git_in(
        root,
        &[
            "grep",
            "-I",
            "-n",
            "-E",
            "-e",
            r"liteinst-runtime(-build)?-[0-9a-f]{6,40}",
            "--",
            ".",
            ":(exclude,top)scripts/check-reverie-pin.rs",
        ],
    )?;
    // git grep exit codes: 0 = matches, 1 = no matches (fine here), >1 = error.
    match output.status.code() {
        Some(0) | Some(1) => {}
        _ => {
            return Err(format!(
                "git grep for LiteInst cache keys failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    let short = &pin[..7.min(pin.len())];
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut violations: Vec<(String, String)> = Vec::new();
    let mut checked = 0usize;
    for entry in stdout.lines() {
        // `git grep -n` prints `path:line:content`.
        let mut parts = entry.splitn(3, ':');
        let path = parts.next().unwrap_or("");
        let lineno = parts.next().unwrap_or("");
        let content = parts.next().unwrap_or("");
        for sha in extract_cache_key_shas(content) {
            checked += 1;
            if !pin.starts_with(&sha) {
                violations.push((format!("{path}:{lineno}"), sha));
            }
        }
    }
    if !violations.is_empty() {
        loud_header("LITEINST CACHE KEY DRIFT - BLOCKED");
        eprintln!("Canonical Reverie pin: {pin}");
        eprintln!("These revision-keyed LiteInst cache keys are NOT a prefix of the pin:");
        for (location, sha) in &violations {
            eprintln!("  {location}: ...liteinst-runtime[...]-{sha}");
        }
        eprintln!(
            "Update each stale key to the pin's short prefix ({short}) so the staged runtime"
        );
        eprintln!("cache busts when the Reverie pin moves. See docs/updating-reverie.md.");
        return Ok(1);
    }
    eprintln!("LiteInst cache keys: {checked} revision-keyed token(s) all track the pin ({short}).");
    Ok(0)
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let root = config.repo.clone().map_or_else(git_root, Ok)?;
    let scan = read_pins(&root)?;
    let pins = &scan.occurrences;

    if config.print_pin {
        println!("{}", unique_pin(&scan)?);
        return Ok(0);
    }

    let tracked_manifests = scan
        .tracked_files
        .iter()
        .filter(|path| path.file_name().is_some_and(|name| name == "Cargo.toml"))
        .count();
    let tracked_locks = scan.tracked_files.len() - tracked_manifests;
    let pinned_file_count: BTreeSet<&Path> = pins.iter().map(|item| item.path.as_path()).collect();
    eprintln!(
        "Scope: scanned {tracked_manifests} tracked Cargo.toml and {tracked_locks} tracked Cargo.lock files; {} files contain {} Reverie revision entries.",
        pinned_file_count.len(),
        pins.len()
    );
    eprintln!(
        "Scope exclusions: non-Cargo tracked files, untracked/generated files, and nested submodule contents; tracked vendored Cargo metadata is included."
    );

    let mut by_rev: BTreeMap<&str, Vec<&PinOccurrence>> = BTreeMap::new();
    for pin in pins {
        by_rev.entry(&pin.rev).or_default().push(pin);
    }
    if by_rev.len() != 1 && !config.update_to_latest {
        loud_header("INCONSISTENT HERMIT REVERIE REVISIONS - BLOCKED");
        for (rev, occurrences) in by_rev {
            eprintln!("  {rev}");
            for occurrence in occurrences {
                let path = occurrence
                    .path
                    .strip_prefix(&root)
                    .unwrap_or(&occurrence.path);
                eprintln!("    {}:{}", path.display(), occurrence.line);
            }
        }
        return Ok(1);
    }

    // LOCAL CONSISTENCY RUNS BEFORE THE NETWORK, and the order is the fix.
    //
    // This call used to sit AFTER `query_main`, so a failed `ls-remote` — a fact
    // about egress, not about this tree — masked the LiteInst cache-key check,
    // which is a real correctness defect in the tree being committed. The thing
    // that cannot be wrong about your commit must not hide the thing that can.
    // Consistency is checked at every freshness setting: it is never advisory.
    let pin = unique_pin(&scan)?;
    let cache_code = check_liteinst_cache_keys(&root, pin)?;
    if cache_code != 0 {
        return Ok(cache_code);
    }

    let (freshness, why) = resolve_freshness(&root, &scan, config.freshness);
    // `--update-to-latest` exists precisely to change the pin, so it always needs
    // the remote regardless of what is staged.
    if freshness == Freshness::Warn && !config.update_to_latest {
        // Deliberately NO network call. A commit that touches no pin site must
        // not be blockable — or hangable — by an unreachable remote, which is
        // how this gate would wedge every commit on a host without egress.
        println!(
            "Reverie pin freshness: NOT CHECKED ({why}). Local consistency PASSED: \
             pin {pin} is uniform across {} revision entries in {} tracked Cargo file(s), \
             and every LiteInst cache key matches it.",
            pins.len(),
            pinned_file_count.len()
        );
        println!(
            "  Freshness against rrnewton/reverie:main is enforced by CI, and by this hook \
             on any commit that touches a pin site. To check it now: \
             with-proxy ./ci/run-reverie-pin-check.sh"
        );
        return Ok(0);
    }

    // Production has no CLI/env/recorded-value override for the authority.
    // Tests substitute only the remote transport, then exercise this same
    // refs/heads/main dereference rather than injecting a well-shaped SHA.
    #[cfg(not(test))]
    let remote = DEFAULT_REMOTE;
    #[cfg(test)]
    let remote = config.remote.as_deref().unwrap_or(DEFAULT_REMOTE);
    let main_result = query_main(remote);

    let main = match main_result {
        Ok(main) => main,
        Err(error) => {
            loud_header("COULD NOT VERIFY LATEST REVERIE MAIN - BLOCKED");
            eprintln!("Hermit pin: {pin}");
            eprintln!("Lookup error: {error}");
            if !why.is_empty() {
                eprintln!("Freshness is fatal here because {why}.");
            }
            blocked_instructions();
            return Ok(1);
        }
    };

    if config.update_to_latest {
        update_to_latest(&root, &scan, &main)?;
        let updated = read_pins(&root)?;
        let updated_pin = unique_pin(&updated)?;
        let cache_code = check_liteinst_cache_keys(&root, updated_pin)?;
        if cache_code != 0 {
            return Ok(cache_code);
        }
        return Ok(0);
    }

    let entries = pins.len();
    let pin_files = pinned_file_count.len();

    if pin == main {
        println!(
            "Reverie pin is current: {pin} ({entries} revision entries across {pin_files} tracked Cargo metadata files)"
        );
        Ok(0)
    } else {
        loud_header("REVERIE PIN DOES NOT EQUAL LATEST MAIN - BLOCKED");
        eprintln!("Hermit pin:  {pin}");
        eprintln!("Latest main: {main}");
        eprintln!(
            "Affected metadata: {entries} revision entries across {pin_files} tracked Cargo files."
        );
        if !why.is_empty() {
            eprintln!("Freshness is fatal here because {why}.");
        }
        blocked_instructions();
        Ok(1)
    }
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
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    #[test]
    fn extracts_rev_key_not_reverie_prefix() {
        let line = r#"reverie = { git = "https://github.com/rrnewton/reverie.git", rev = "0123456789abcdef0123456789abcdef01234567" }"#;
        assert_eq!(
            extract_rev(line).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[test]
    fn validates_full_sha() {
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha("01234567"));
        assert!(!is_full_sha("z123456789abcdef0123456789abcdef01234567"));
    }

    #[test]
    fn help_states_the_checker_scope() {
        let help = usage();
        assert!(help.contains("every tracked Cargo.toml and Cargo.lock"));
        assert!(help.contains("Excludes non-Cargo files"));
    }

    #[test]
    fn extracts_rev_from_lock_source() {
        let line = r#"source = "git+https://github.com/rrnewton/reverie.git?rev=0123456789abcdef0123456789abcdef01234567#0123456789abcdef0123456789abcdef01234567""#;
        assert_eq!(
            extract_lock_rev(line).as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    fn temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "check-reverie-pin-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn init_fixture_repo(root: &Path) {
        fs::create_dir_all(root).expect("create fixture repository");
        assert!(git_in(root, &["init", "-q"]).unwrap().status.success());
        assert!(
            git_in(root, &["config", "user.email", "pin-test@example.com"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(root, &["config", "user.name", "Reverie Pin Test"])
                .unwrap()
                .status
                .success()
        );
    }

    #[test]
    fn exact_latest_pin_passes() {
        let root = temp_path("current");
        let remote = temp_path("current-reverie");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        fs::write(remote.join("revision"), "current\n").expect("write Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "current"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        let current =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{current}\" }}\n"
            ),
        )
        .expect("write fixture manifest");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            ..Config::default()
        })
        .expect("current pin should be classified");
        assert_eq!(code, 0, "an exact latest-main pin must pass");
        fs::remove_dir_all(root).expect("remove fixture repository");
        fs::remove_dir_all(remote).expect("remove Reverie fixture repository");
    }

    #[test]
    fn ancestor_behind_latest_fails() {
        let root = temp_path("behind-hermit");
        let remote = temp_path("behind-reverie");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);

        fs::write(remote.join("revision"), "old\n").expect("write old Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "old"])
                .unwrap()
                .status
                .success()
        );
        let old = String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
            .trim()
            .to_string();
        fs::write(remote.join("revision"), "latest\n").expect("write latest Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "latest"])
                .unwrap()
                .status
                .success()
        );
        let latest =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();
        assert_ne!(old, latest);
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );

        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{old}\" }}\n"
            ),
        )
        .expect("write stale Hermit fixture");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            ..Config::default()
        })
        .expect("behind pin should be classified");
        assert_eq!(code, 1, "an ancestor behind latest main must fail closed");

        fs::remove_dir_all(root).expect("remove Hermit fixture repository");
        fs::remove_dir_all(remote).expect("remove Reverie fixture repository");
    }

    #[test]
    fn mechanical_update_rewrites_derived_manifest_sites() {
        let root = temp_path("update");
        init_fixture_repo(&root);
        let old = "0123456789abcdef0123456789abcdef01234567";
        let latest = "89abcdef0123456789abcdef0123456789abcdef";
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{old}\" }}\n"
            ),
        )
        .expect("write stale fixture manifest");
        assert!(
            git_in(&root, &["add", "Cargo.toml"])
                .unwrap()
                .status
                .success()
        );
        let scan = read_pins(&root).expect("scan fixture manifest");
        assert_eq!(rewrite_manifest_pins(&scan, latest).unwrap(), (1, 1));
        let updated = read_pins(&root).expect("rescan updated fixture manifest");
        assert_eq!(unique_pin(&updated).unwrap(), latest);
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn tracked_stale_lockfile_fails_the_checker_path() {
        let root = temp_path("stale-lock");
        let remote = temp_path("stale-lock-reverie");
        let runtime = root.join("runtime");
        init_fixture_repo(&root);
        init_fixture_repo(&remote);
        fs::create_dir_all(&runtime).expect("create fixture directories");
        fs::write(remote.join("revision"), "current\n").expect("write Reverie fixture");
        assert!(
            git_in(&remote, &["add", "revision"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["commit", "-qm", "current"])
                .unwrap()
                .status
                .success()
        );
        assert!(
            git_in(&remote, &["branch", "-M", "main"])
                .unwrap()
                .status
                .success()
        );
        let current =
            String::from_utf8_lossy(&git_in(&remote, &["rev-parse", "HEAD"]).unwrap().stdout)
                .trim()
                .to_string();
        let stale = "89abcdef0123456789abcdef0123456789abcdef";
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{current}\" }}\n"
            ),
        )
        .expect("write fixture manifest");
        fs::write(
            runtime.join("Cargo.lock"),
            format!(
                "[[package]]\nname = \"reverie-core\"\nsource = \"git+https://github.com/rrnewton/reverie.git?rev={stale}#{stale}\"\n"
            ),
        )
        .expect("write planted stale lockfile");
        assert!(
            git_in(&root, &["add", "Cargo.toml", "runtime/Cargo.lock"])
                .unwrap()
                .status
                .success()
        );

        let scan = read_pins(&root).expect("scan tracked fixture metadata");
        assert_eq!(scan.tracked_files.len(), 2);
        assert!(
            scan.occurrences
                .iter()
                .any(|pin| pin.path.ends_with("runtime/Cargo.lock") && pin.rev == stale)
        );
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(remote.to_string_lossy().into_owned()),
            ..Config::default()
        })
        .expect("checker should classify the planted inconsistency");
        assert_eq!(code, 1, "a tracked stale Cargo.lock must fail closed");

        fs::remove_dir_all(root).expect("remove fixture repository");
        fs::remove_dir_all(remote).expect("remove Reverie fixture repository");
    }

    #[test]
    fn extract_cache_key_shas_handles_both_schemes() {
        assert_eq!(
            extract_cache_key_shas("$PWD/target/liteinst-runtime-build-7951770 arg"),
            vec!["7951770".to_string()]
        );
        assert_eq!(
            extract_cache_key_shas("build_root.join(\"liteinst-runtime-d973a85\")"),
            vec!["d973a85".to_string()]
        );
        // Multiple keys on one line, both schemes.
        assert_eq!(
            extract_cache_key_shas("a/liteinst-runtime-build-7951770 b/liteinst-runtime-abcdef1"),
            vec!["7951770".to_string(), "abcdef1".to_string()]
        );
        // The nested-workspace directory path is NOT a revision key.
        assert!(extract_cache_key_shas("liteinst-runtime-build/Cargo.lock").is_empty());
        // A too-short suffix (<6 hex) is not a revision key.
        assert!(extract_cache_key_shas("liteinst-runtime-ab12").is_empty());
    }

    #[test]
    fn cache_key_drift_fails_and_consistent_passes() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "check-reverie-pin-cachekey-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create fixture directory");
        let pin = "0123456789abcdef0123456789abcdef01234567";
        // Consistent: every cache key is a prefix of the pin.
        fs::write(
            root.join("portable.json"),
            "cmd = target/liteinst-runtime-build-0123456\n",
        )
        .expect("write consistent cache key");
        fs::write(
            root.join("build.rs"),
            "let t = build_root.join(\"liteinst-runtime-0123456789ab\");\n",
        )
        .expect("write consistent cache key");
        assert!(git_in(&root, &["init", "-q"]).unwrap().status.success());
        assert!(
            git_in(&root, &["add", "portable.json", "build.rs"])
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            check_liteinst_cache_keys(&root, pin).expect("scan consistent tree"),
            0,
            "cache keys that are prefixes of the pin must pass"
        );

        // Drift: plant a key that is not a prefix of the pin.
        fs::write(
            root.join("portable.json"),
            "cmd = target/liteinst-runtime-build-deadbee\n",
        )
        .expect("write drifted cache key");
        assert!(
            git_in(&root, &["add", "portable.json"])
                .unwrap()
                .status
                .success()
        );
        assert_eq!(
            check_liteinst_cache_keys(&root, pin).expect("scan drifted tree"),
            1,
            "a cache key that is not a prefix of the pin must fail closed"
        );

        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    // ---------------- freshness scoping (the --no-verify decoupling) -----------

    /// A committed fixture repo whose manifest pins `pin`, plus one extra
    /// tracked file to stage. The remote is deliberately a path that does not
    /// exist, so ANY attempt to dereference it fails — which is what makes
    /// "warn does not touch the network" an observation rather than a promise.
    fn staged_fixture(label: &str, pin: &str) -> PathBuf {
        let root = temp_path(label);
        init_fixture_repo(&root);
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{pin}\" }}\n"
            ),
        )
        .expect("write fixture manifest");
        fs::write(root.join("README.md"), "seed\n").expect("write fixture doc");
        assert!(git_in(&root, &["add", "Cargo.toml", "README.md"]).unwrap().status.success());
        assert!(git_in(&root, &["commit", "-qm", "seed"]).unwrap().status.success());
        root
    }

    const UNREACHABLE_REMOTE: &str = "/nonexistent/definitely-not-a-git-remote";
    const PIN_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const PIN_B: &str = "89abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn warn_freshness_passes_a_stale_pin_and_never_calls_the_network() {
        let root = staged_fixture("warn-stale", PIN_A);
        // Not merely "stale": the remote CANNOT be reached at all. Under
        // `Block` this is the COULD NOT VERIFY path and must fail (asserted
        // below), so a pass here can only mean no lookup was attempted.
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(UNREACHABLE_REMOTE.to_string()),
            freshness: Freshness::Warn,
            ..Config::default()
        })
        .expect("warn should classify");
        assert_eq!(code, 0, "warn must not fail on freshness it did not check");

        let blocked = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(UNREACHABLE_REMOTE.to_string()),
            freshness: Freshness::Block,
            ..Config::default()
        })
        .expect("block should classify");
        assert_eq!(
            blocked, 1,
            "the same fixture MUST fail under block, or the test above proves nothing"
        );
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn warn_freshness_still_blocks_inconsistent_revisions() {
        // Consistency is about the tree being committed and is never advisory.
        let root = staged_fixture("warn-inconsistent", PIN_A);
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\na = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{PIN_A}\" }}\nb = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{PIN_B}\" }}\n"
            ),
        )
        .expect("write inconsistent manifest");
        let code = run_with_config(Config {
            repo: Some(root.clone()),
            remote: Some(UNREACHABLE_REMOTE.to_string()),
            freshness: Freshness::Warn,
            ..Config::default()
        })
        .expect("warn should classify");
        assert_eq!(code, 1, "disagreeing revision entries must block at every freshness setting");
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn auto_warns_only_when_no_staged_path_is_a_pin_site() {
        let root = staged_fixture("auto-unrelated", PIN_A);
        fs::write(root.join("README.md"), "unrelated edit\n").expect("edit doc");
        assert!(git_in(&root, &["add", "README.md"]).unwrap().status.success());
        let scan = read_pins(&root).expect("scan fixture");
        let (resolved, why) = resolve_freshness(&root, &scan, Freshness::Auto);
        assert_eq!(resolved, Freshness::Warn, "a doc-only change is not a pin change: {why}");
        assert!(why.contains("0 of them pin sites"), "the reason must carry counts: {why}");
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn auto_blocks_when_a_staged_path_is_a_pin_site() {
        let root = staged_fixture("auto-manifest", PIN_A);
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[dependencies]\nreverie = {{ git = \"https://github.com/rrnewton/reverie.git\", rev = \"{PIN_A}\" }}\n# touched\n"
            ),
        )
        .expect("edit manifest");
        assert!(git_in(&root, &["add", "Cargo.toml"]).unwrap().status.success());
        let scan = read_pins(&root).expect("scan fixture");
        let (resolved, why) = resolve_freshness(&root, &scan, Freshness::Auto);
        assert_eq!(resolved, Freshness::Block, "a staged manifest must keep the strict reading");
        assert!(why.contains("Cargo.toml"), "the refusal must name the pin site: {why}");
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn auto_fails_conservative_when_the_scope_is_unknowable() {
        // Nothing staged is not evidence that the change is unrelated, so the
        // relaxation must not be reachable by simply failing to answer.
        let root = staged_fixture("auto-empty", PIN_A);
        let scan = read_pins(&root).expect("scan fixture");
        let (resolved, why) = resolve_freshness(&root, &scan, Freshness::Auto);
        assert_eq!(resolved, Freshness::Block, "an empty staged set must not relax anything");
        assert!(why.contains("nothing is staged"), "{why}");
        fs::remove_dir_all(root).expect("remove fixture repository");
    }

    #[test]
    fn explicit_settings_are_not_reinterpreted_by_scope() {
        // `--freshness block` and `--freshness warn` mean what they say; only
        // `auto` consults the index. CI passes neither and gets `block`.
        let root = staged_fixture("explicit", PIN_A);
        let scan = read_pins(&root).expect("scan fixture");
        assert_eq!(resolve_freshness(&root, &scan, Freshness::Block).0, Freshness::Block);
        assert_eq!(resolve_freshness(&root, &scan, Freshness::Warn).0, Freshness::Warn);
        assert_eq!(Config::default().freshness, Freshness::Block, "the default must stay strict");
        fs::remove_dir_all(root).expect("remove fixture repository");
    }
}
