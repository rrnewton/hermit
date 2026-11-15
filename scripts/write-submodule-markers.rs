#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Carry `*.submodule.txt` markers for `hermit_import:prepare`
//! (Reverie pin-flow design, step 3), replacing hand edits in every
//! import.
//!
//! On the GitHub side a submodule pin is a gitlink. In fbsource the
//! same fact is a sibling marker file — `reverie` →
//! `reverie.submodule.txt` containing `Subproject commit <sha>` —
//! which CodeSync/ShipIt converts gitlink ↔ marker
//! (`fbcode/hermetic_infra/hermit_import/README.md`). The import's
//! prepare step historically did not carry those markers, so every
//! import diff hand-maintained them ("Until prepare carries them, the
//! import maintains them by hand", D131326210 summary) — one more
//! hand-copied hash that could be missed or left stale.
//!
//! This tool is the carry, kept in the Hermit tree so both sides run
//! one implementation with one test suite (the same arrangement as
//! the lock gate): given a source revision, it enumerates that
//! tree's gitlinks, cross-checks them against `.gitmodules`, and
//! writes each marker into the prepared tree. `hermit_import:prepare`
//! invokes it (Eden step) after preparing the tree, before codesync
//! translate, passing its `excluded_paths` gitlinks as `--exclude`
//! (excluded gitlinks are reported as skipped, never silently
//! dropped: a marker set narrower than the tree's gitlinks must be a
//! declared import-mapping decision, not an accident).
//!
//! Fail-closed, like the gates around it: an undeclared gitlink (no
//! `.gitmodules` path), a non-40-hex revision, a symlinked marker
//! path, or — in `--check` mode — any missing, differing, or stale
//! marker refuses. A stale marker (no matching gitlink) would
//! resurrect a removed submodule on export, so it refuses too rather
//! than being left for ShipIt to re-materialize.
//!
//! The trailing newline in marker content follows the recorded
//! marker spelling (`Subproject commit <sha>` as one line); the Eden
//! step byte-compares against the markers already in fbsource before
//! first use.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    rev: String,
    out: Option<PathBuf>,
    check: bool,
    excludes: BTreeSet<String>,
}

fn usage() -> &'static str {
    "Usage: write-submodule-markers.rs [OPTIONS] (--out DIR | --check --out DIR)\n\
     \n\
     Write (or with --check, verify) the *.submodule.txt markers for every\n\
     gitlink recorded at --rev, for hermit_import:prepare to carry.\n\
     \n\
     Options:\n\
       --repo PATH       Source Hermit checkout (default: git root)\n\
       --rev REF         Tree to read gitlinks/.gitmodules from (default: HEAD)\n\
       --out DIR         Prepared tree root to write/verify markers in (required)\n\
       --check           Verify only; refuse on missing/differing/stale markers\n\
       --exclude PATH    Gitlink path the import mapping excludes (repeatable)\n\
       -h, --help        Show this help"
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config {
        rev: "HEAD".to_string(),
        ..Config::default()
    };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--repo" => config.repo = Some(PathBuf::from(take_value(&args, &mut i, "--repo")?)),
            "--rev" => config.rev = take_value(&args, &mut i, "--rev")?,
            "--out" => config.out = Some(PathBuf::from(take_value(&args, &mut i, "--out")?)),
            "--exclude" => {
                config
                    .excludes
                    .insert(take_value(&args, &mut i, "--exclude")?);
            }
            "--check" => config.check = true,
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        i += 1;
    }
    if config.out.is_none() {
        return Err(format!(
            "--out DIR is required (markers are never written into the source checkout by default)\n{}",
            usage()
        ));
    }
    if config.rev.trim().is_empty() {
        return Err("--rev requires a non-empty revision".to_string());
    }
    Ok(config)
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// Gitlinks (`160000 commit <sha>\t<path>`) from `git ls-tree -r` output.
/// Non-gitlink lines are skipped; sha shape is judged in [`plan_markers`]
/// so a malformed entry is refused there, not silently dropped here.
fn parse_gitlinks(text: &str) -> Vec<(String, String)> {
    let mut links = Vec::new();
    for line in text.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        if fields.next() != Some("160000") || fields.next() != Some("commit") {
            continue;
        }
        if let Some(sha) = fields.next() {
            links.push((path.trim().to_string(), sha.to_string()));
        }
    }
    links
}

/// Declared submodule paths from `.gitmodules` text (`path = ...` lines
/// under `[submodule "..."]` tables).
fn parse_gitmodules_paths(text: &str) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    let mut in_submodule = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_submodule = trimmed.starts_with("[submodule ");
            continue;
        }
        if in_submodule {
            if let Some(rest) = trimmed.strip_prefix("path") {
                if let Some(rest) = rest.trim_start().strip_prefix('=') {
                    let path = rest.trim();
                    if !path.is_empty() {
                        paths.insert(path.to_string());
                    }
                }
            }
        }
    }
    paths
}

/// The marker path (repo-relative) for the gitlink at `path`.
fn marker_relpath(path: &str) -> String {
    format!("{path}.submodule.txt")
}

/// Exact marker bytes for one pin.
fn marker_content(sha: &str) -> String {
    format!("Subproject commit {sha}\n")
}

/// The markers to carry: marker relpath → content, plus the skipped
/// (excluded) gitlink paths. Refuses an undeclared gitlink, a bad
/// revision, or an exclude naming no gitlink (a stale exclusion must
/// not silently narrow tomorrow's carry either).
fn plan_markers(
    gitlinks: &[(String, String)],
    declared: &BTreeSet<String>,
    excludes: &BTreeSet<String>,
) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    let link_paths: BTreeSet<&str> = gitlinks.iter().map(|(p, _)| p.as_str()).collect();
    for exclude in excludes {
        if !link_paths.contains(exclude.as_str()) {
            return Err(format!(
                "--exclude {exclude:?} names no gitlink at this revision; refusing a stale exclusion"
            ));
        }
    }
    let mut markers = BTreeMap::new();
    let mut skipped = Vec::new();
    for (path, sha) in gitlinks {
        if excludes.contains(path) {
            skipped.push(path.clone());
            continue;
        }
        if !is_full_sha(sha) {
            return Err(format!(
                "gitlink {path} records non-40-hex revision {sha:?}"
            ));
        }
        if !declared.contains(path) {
            return Err(format!(
                "gitlink {path} is not declared in .gitmodules; refusing to carry an undeclared submodule"
            ));
        }
        markers.insert(marker_relpath(path), marker_content(sha));
    }
    Ok((markers, skipped))
}

/// Check-mode verdicts: every way the markers on disk disagree with
/// the plan (missing, differing, or stale), sorted for stable output.
fn marker_violations(
    expected: &BTreeMap<String, String>,
    existing: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut violations = Vec::new();
    for (path, content) in expected {
        match existing.get(path) {
            None => violations.push(format!("{path} is missing")),
            Some(found) if found != content => {
                violations.push(format!("{path} records {found:?}, expected {content:?}"))
            }
            Some(_) => {}
        }
    }
    for path in existing.keys() {
        if !expected.contains_key(path) {
            violations.push(format!("{path} is stale (no matching gitlink)"));
        }
    }
    violations.sort();
    violations
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

fn git_text(repo: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Existing marker files under `out`, as relpath → contents. A walk
/// that cannot read a marker is a checker error, never an omission.
fn read_existing_markers(out: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut found = BTreeMap::new();
    let mut stack = vec![out.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|error| format!("could not list {}: {error}", dir.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("could not list {}: {error}", dir.display()))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let kind = entry
                .file_type()
                .map_err(|error| format!("could not stat {}: {error}", path.display()))?;
            if kind.is_dir() {
                if name != ".git" {
                    stack.push(path);
                }
            } else if name.ends_with(".submodule.txt") {
                let rel = path
                    .strip_prefix(out)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let content = fs::read_to_string(&path)
                    .map_err(|error| format!("could not read {}: {error}", path.display()))?;
                found.insert(rel, content);
            }
        }
    }
    Ok(found)
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("SUBMODULE MARKERS: {title}");
    eprintln!("======================================================================");
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let repo = config.repo.clone().map_or_else(git_root, Ok)?;
    let out = config.out.clone().expect("--out checked in parse_args");
    let tree = git_text(&repo, &["ls-tree", "-r", &config.rev])?;
    let gitlinks = parse_gitlinks(&tree);
    // A tree with gitlinks but no .gitmodules cannot be judged declared;
    // a tree with neither simply carries nothing.
    let declared = match git_text(&repo, &["show", &format!("{}:.gitmodules", config.rev)]) {
        Ok(text) => parse_gitmodules_paths(&text),
        Err(error) if gitlinks.is_empty() => {
            let _ = error;
            BTreeSet::new()
        }
        Err(error) => return Err(error),
    };
    let (markers, skipped) = plan_markers(&gitlinks, &declared, &config.excludes)?;
    eprintln!(
        "Scope: {} gitlink(s) at {}, {} marker(s) to carry, {} excluded ({}).",
        gitlinks.len(),
        config.rev,
        markers.len(),
        skipped.len(),
        if skipped.is_empty() {
            "none".to_string()
        } else {
            skipped.join(", ")
        }
    );

    if config.check {
        let existing = read_existing_markers(&out)?;
        let violations = marker_violations(&markers, &existing);
        if violations.is_empty() {
            println!(
                "OK: {} marker(s) match the gitlinks at {}.",
                markers.len(),
                config.rev
            );
            return Ok(0);
        }
        loud_header("MARKERS DO NOT MATCH GITLINKS - BLOCKED");
        for violation in &violations {
            eprintln!("  {violation}");
        }
        return Ok(1);
    }

    // Write mode still refuses stale markers first: carrying forward a
    // marker whose submodule left the tree would resurrect it on export.
    if out.is_dir() {
        let existing = read_existing_markers(&out)?;
        let stale: Vec<&String> = existing
            .keys()
            .filter(|path| !markers.contains_key(*path))
            .collect();
        if !stale.is_empty() {
            loud_header("STALE MARKERS PRESENT - BLOCKED");
            for path in stale {
                eprintln!("  {path} has no matching gitlink at {}", config.rev);
            }
            eprintln!("Remove stale markers deliberately; this tool never deletes.");
            return Ok(1);
        }
    }
    let mut wrote = 0usize;
    for (rel, content) in &markers {
        let path = out.join(rel);
        if let Ok(meta) = fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() || meta.is_dir() {
                return Err(format!(
                    "marker path {} is a symlink or directory; refusing to write through it",
                    path.display()
                ));
            }
            if fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
                println!("UNCHANGED: {rel}");
                continue;
            }
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        fs::write(&path, content)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        wrote += 1;
        println!("WROTE: {rel}");
    }
    println!(
        "Carried {} marker(s) ({} written) for gitlinks at {}.",
        markers.len(),
        wrote,
        config.rev
    );
    Ok(0)
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

    const PIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const OTHER: &str = "0123456789abcdef0123456789abcdef01234567";

    fn declared(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn parses_gitlinks_and_gitmodules_paths() {
        let tree = format!(
            "100644 blob {OTHER}\tCargo.toml\n160000 commit {PIN}\treverie\n040000 tree {OTHER}\tdetcore\n160000 commit {OTHER}\tthird-party/rr\n"
        );
        assert_eq!(
            parse_gitlinks(&tree),
            [
                ("reverie".to_string(), PIN.to_string()),
                ("third-party/rr".to_string(), OTHER.to_string())
            ]
        );
        let modules = "[submodule \"reverie\"]\n\tpath = reverie\n\turl = https://example.invalid/r\n[submodule \"x\"]\npath=third-party/rr\n";
        assert_eq!(
            parse_gitmodules_paths(modules),
            declared(&["reverie", "third-party/rr"])
        );
    }

    #[test]
    fn plan_carries_declared_gitlinks_with_exact_marker_bytes() {
        let links = vec![("reverie".to_string(), PIN.to_string())];
        let (markers, skipped) =
            plan_markers(&links, &declared(&["reverie"]), &BTreeSet::new()).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(
            markers.get("reverie.submodule.txt").map(String::as_str),
            Some(format!("Subproject commit {PIN}\n").as_str())
        );
        assert_eq!(
            marker_relpath("third-party/rr"),
            "third-party/rr.submodule.txt"
        );
    }

    #[test]
    fn plan_refuses_undeclared_bad_sha_and_stale_exclude() {
        let links = vec![("reverie".to_string(), PIN.to_string())];
        assert!(plan_markers(&links, &BTreeSet::new(), &BTreeSet::new()).is_err());
        let bad = vec![("reverie".to_string(), "abc123".to_string())];
        assert!(plan_markers(&bad, &declared(&["reverie"]), &BTreeSet::new()).is_err());
        let stale: BTreeSet<String> = ["gone".to_string()].into_iter().collect();
        assert!(plan_markers(&links, &declared(&["reverie"]), &stale).is_err());
        // An exclusion for a real gitlink is honored and reported, not dropped.
        let excl: BTreeSet<String> = ["reverie".to_string()].into_iter().collect();
        let (markers, skipped) = plan_markers(&links, &declared(&["reverie"]), &excl).unwrap();
        assert!(markers.is_empty());
        assert_eq!(skipped, ["reverie"]);
    }

    #[test]
    fn check_mode_names_missing_differing_and_stale_markers() {
        let mut expected = BTreeMap::new();
        expected.insert("reverie.submodule.txt".to_string(), marker_content(PIN));
        assert!(marker_violations(&expected, &expected.clone()).is_empty());
        assert_eq!(marker_violations(&expected, &BTreeMap::new()).len(), 1);
        let mut wrong = expected.clone();
        wrong.insert("reverie.submodule.txt".to_string(), marker_content(OTHER));
        assert_eq!(marker_violations(&expected, &wrong).len(), 1);
        let mut stale = expected.clone();
        stale.insert("gone.submodule.txt".to_string(), marker_content(PIN));
        let violations = marker_violations(&expected, &stale);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("stale"), "{violations:?}");
    }
}
