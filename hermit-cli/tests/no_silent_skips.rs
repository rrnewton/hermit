/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;

const SOURCE_ROOTS: &[&str] = &[
    ".github/workflows",
    "common",
    "detcore",
    "flaky-tests",
    "hermit-cli",
    "hermit-verify",
    "tests",
    "validate.sh",
];

const FORBIDDEN_MARKERS: &[(&str, &str)] = &[
    ("ignored test attribute", "#[ignore"),
    ("silent PMU return macro", "ret_without_perf!"),
    ("ignored-only Cargo filter", "--ignored"),
    ("successful test skip", "Skipping test"),
    ("successful future test skip", "will be skipped"),
    ("optional CI skip", "skipping optional"),
    ("unit-only CI fallback", "running Hermit unit tests only"),
];

fn is_control_source(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "validate.sh")
        || path.extension().is_some_and(|extension| {
            matches!(
                extension.to_str(),
                Some("rs" | "sh" | "test" | "yml" | "yaml")
            )
        })
}

fn collect_control_sources(path: &Path, sources: &mut Vec<PathBuf>) {
    if path.is_file() {
        if is_control_source(path) {
            sources.push(path.to_owned());
        }
        return;
    }

    let entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
    for entry in entries {
        let entry =
            entry.unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()));
        if entry
            .file_type()
            .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", entry.path().display()))
            .is_symlink()
        {
            continue;
        }
        collect_control_sources(&entry.path(), sources);
    }
}

#[test]
fn test_control_sources_have_no_silent_skip_markers() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let this_file = repository.join("hermit-cli/tests/no_silent_skips.rs");
    let mut sources = Vec::new();
    for root in SOURCE_ROOTS {
        collect_control_sources(&repository.join(root), &mut sources);
    }

    let mut violations = Vec::new();
    for source in sources {
        if source == this_file {
            continue;
        }
        let contents = fs::read_to_string(&source)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", source.display()));
        for (description, marker) in FORBIDDEN_MARKERS {
            for (index, line) in contents.lines().enumerate() {
                if line.contains(marker) {
                    violations.push(format!(
                        "{}:{}: {description}: {line}",
                        source.strip_prefix(repository).unwrap_or(&source).display(),
                        index + 1,
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "ERROR: test coverage must fail instead of silently skipping:\n{}",
        violations.join("\n")
    );
}
