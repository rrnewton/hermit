#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Verify the cross-client skill layout without duplicating instruction bodies.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const FLAT_SKILLS: &[&str] = &[
    "benchmark",
    "ci-debugging",
    "deadlock-debugging",
    "presenting-quantitative-data",
    "repo-cleanliness",
    "ux-tester",
];

const PACKAGED_SKILLS: &[&str] = &[
    "backend-reality-reviewer",
    "continuous-virtual-time-is-sacred",
    "determinism-regression-debugging",
    "fabler",
    "hermit-debugging",
    "human-review-first",
    "post-facto-review",
    "progress-rubric",
    "test-shrink-optimization",
];

const PARENT_ONLY_ROLES: &[&str] = &[
    "hermit-ci",
    "hermit-coord",
    "hermit-dbi",
    "hermit-kvm",
    "hermit-lander",
    "hermit-liteinst",
    "hermit-opt",
    "hermit-sabre",
];

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

fn require_symlink(path: &Path, expected: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_symlink() {
        return Err(format!("{} must be a symlink", path.display()));
    }
    let actual =
        fs::read_link(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    if actual != expected {
        return Err(format!(
            "{} points to {:?}, expected {:?}",
            path.display(),
            actual,
            expected
        ));
    }
    Ok(())
}

fn frontmatter<'a>(contents: &'a str, path: &Path) -> Result<&'a str, String> {
    let rest = contents
        .strip_prefix("---\n")
        .ok_or_else(|| format!("{} lacks YAML frontmatter", path.display()))?;
    let closing = rest
        .find("\n---\n")
        .ok_or_else(|| format!("{} has unterminated YAML frontmatter", path.display()))?;
    Ok(&contents[..4 + closing + 5])
}

fn expected_wrapper(name: &str, canonical: &str, path: &Path) -> Result<String, String> {
    let metadata = frontmatter(canonical, path)?;
    Ok(format!(
        "{metadata}\n# Codex discovery entrypoint\n\n\
         Read and follow [the canonical `{name}` skill](../../../.claude/skills/{name}.md) \
         completely. Resolve further relative links from the canonical file's directory.\n"
    ))
}

fn entry_names(path: &Path) -> Result<BTreeSet<String>, String> {
    fs::read_dir(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?
        .map(|entry| {
            let entry = entry.map_err(|error| format!("cannot read directory entry: {error}"))?;
            entry
                .file_name()
                .into_string()
                .map_err(|name| format!("non-UTF-8 skill entry: {name:?}"))
        })
        .collect()
}

fn check(root: &Path) -> Result<(), String> {
    require_symlink(&root.join(".llms/skills"), Path::new("../.claude/skills"))?;

    let codex_root = root.join(".agents/skills");
    let codex_metadata = fs::symlink_metadata(&codex_root)
        .map_err(|error| format!("cannot inspect {}: {error}", codex_root.display()))?;
    if !codex_metadata.is_dir() || codex_metadata.file_type().is_symlink() {
        return Err(format!(
            "{} must be a real directory so flat skills can use SKILL.md entrypoints",
            codex_root.display()
        ));
    }

    let mut expected_entries = BTreeSet::from(["README.md".to_owned()]);
    expected_entries.extend(FLAT_SKILLS.iter().map(|name| (*name).to_owned()));
    expected_entries.extend(PACKAGED_SKILLS.iter().map(|name| (*name).to_owned()));
    let actual_entries = entry_names(&codex_root)?;
    if actual_entries != expected_entries {
        return Err(format!(
            "stock-Codex skill entries differ:\n  actual: {actual_entries:?}\n  expected: {expected_entries:?}"
        ));
    }

    let mut expected_canonical = BTreeSet::new();
    expected_canonical.extend(FLAT_SKILLS.iter().map(|name| format!("{name}.md")));
    expected_canonical.extend(PACKAGED_SKILLS.iter().map(|name| (*name).to_owned()));
    let canonical_root = root.join(".claude/skills");
    let actual_canonical = entry_names(&canonical_root)?;
    if actual_canonical != expected_canonical {
        return Err(format!(
            "canonical skill entries differ:\n  actual: {actual_canonical:?}\n  expected: {expected_canonical:?}"
        ));
    }

    for name in FLAT_SKILLS {
        let canonical_path = canonical_root.join(format!("{name}.md"));
        let canonical = fs::read_to_string(&canonical_path)
            .map_err(|error| format!("cannot read {}: {error}", canonical_path.display()))?;
        let wrapper_dir = codex_root.join(name);
        let wrapper_metadata = fs::symlink_metadata(&wrapper_dir)
            .map_err(|error| format!("cannot inspect {}: {error}", wrapper_dir.display()))?;
        if !wrapper_metadata.is_dir() || wrapper_metadata.file_type().is_symlink() {
            return Err(format!(
                "{} must be a real directory",
                wrapper_dir.display()
            ));
        }
        if entry_names(&wrapper_dir)? != BTreeSet::from(["SKILL.md".to_owned()]) {
            return Err(format!(
                "{} must contain only SKILL.md",
                wrapper_dir.display()
            ));
        }
        let wrapper_path = wrapper_dir.join("SKILL.md");
        let wrapper = fs::read_to_string(&wrapper_path)
            .map_err(|error| format!("cannot read {}: {error}", wrapper_path.display()))?;
        let expected = expected_wrapper(name, &canonical, &canonical_path)?;
        if wrapper != expected {
            return Err(format!(
                "{} is stale; regenerate it from {}",
                wrapper_path.display(),
                canonical_path.display()
            ));
        }
    }

    for name in PACKAGED_SKILLS {
        let entry = codex_root.join(name);
        require_symlink(
            &entry,
            &PathBuf::from(format!("../../.claude/skills/{name}")),
        )?;
        let skill_file = entry.join("SKILL.md");
        if !skill_file.is_file() {
            return Err(format!(
                "{} does not resolve to a file",
                skill_file.display()
            ));
        }
    }

    for name in PARENT_ONLY_ROLES {
        let path = canonical_root.join(format!("{name}.md"));
        if fs::symlink_metadata(&path).is_ok() {
            return Err(format!(
                "parent coordinator role leaked into product skills: {}",
                path.display()
            ));
        }
    }

    Ok(())
}

fn main() {
    rust_script_prelude::init();
    let root = match env::args().nth(1) {
        Some(path) => PathBuf::from(path),
        None => match git_root() {
            Ok(path) => path,
            Err(error) => {
                eprintln!("check-skill-discovery: ERROR: {error}");
                std::process::exit(1);
            }
        },
    };
    if let Err(error) = check(&root) {
        eprintln!("check-skill-discovery: ERROR: {error}");
        std::process::exit(1);
    }
    println!(
        "check-skill-discovery: PASS ({} flat adapters, {} packaged skills)",
        FLAT_SKILLS.len(),
        PACKAGED_SKILLS.len()
    );
}
