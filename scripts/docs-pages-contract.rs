#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Guard the GitHub Pages compatibility-site publication contract.
//!
//! The reviewed snapshot has two public paths: its pinned content-addressed
//! path and `compatibility/latest`. The latter must be a complete regular-file
//! copy, not a redirect or symlink, so deep links and query/hash navigation work
//! on static Pages. This checker deliberately lives apart from the Actions
//! trigger-policy checker: publication integrity and workflow scheduling are
//! different contracts.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::fs;
use std::path::Path;
use std::path::PathBuf;

const WORKFLOW: &str = ".github/workflows/docs.yml";
const LANDING_PAGE: &str = "docs/site/index.html";
const PUBLICATION_STEP: &str = "Add reviewed compatibility website";
const LANDING_ALIAS: &str = "href=\"compatibility/latest/\"";
const LANDING_WORDING: &str = "<strong>Compatibility snapshot:</strong>";

const PIN_VALUES: &[(&str, &str)] = &[
    ("COMPATIBILITY_SITE_RELEASE_REPOSITORY", "rrnewton/hermit"),
    (
        "COMPATIBILITY_SITE_IDENTITY",
        "e75fe49e3b55c6357b32e0110d65731575fe287413f3239a92d9d4522e15a4fb",
    ),
    (
        "COMPATIBILITY_SITE_RELEASE_TAG",
        "compatibility-website-e75fe49e3b55c6357b32e0110d65731575fe287413f3239a92d9d4522e15a4fb",
    ),
    (
        "COMPATIBILITY_SITE_RELEASE_TITLE",
        "Compatibility website snapshot e75fe49e3b55c6357b32e0110d65731575fe287413f3239a92d9d4522e15a4fb",
    ),
    (
        "COMPATIBILITY_SITE_ASSET",
        "compatibility-website-e75fe49e3b55c6357b32e0110d65731575fe287413f3239a92d9d4522e15a4fb.tar.gz",
    ),
    (
        "COMPATIBILITY_SITE_ARCHIVE_SHA256",
        "98eb2459286865081566242780056997a28a21d467ebe311a398ba4f77b84b46",
    ),
    ("COMPATIBILITY_SITE_ARCHIVE_BYTES", "'13929857'"),
    (
        "COMPATIBILITY_SITE_BUILD_SHA256",
        "ada1279a5fbf4565efaf9cc5fdfcd5522c92e7a61c8b52a8bf43ab19420da695",
    ),
    (
        "COMPATIBILITY_SITE_MANIFEST_TREE_SHA256",
        "413f9fee6fc98f5335ab01441f9a61844d0568fe754f9cf373aea1b96239cca6",
    ),
    (
        "COMPATIBILITY_SITE_ARTIFACTS_SHA256",
        "b06eb90e88b243d6af2bf45f7c53d694e988a7d538e77891e2baebc9983598a4",
    ),
    (
        "COMPATIBILITY_SITE_CONTENT_TREE_SHA256",
        "cb1736cb2fc93a7962656b7a5a85d3f7c3095864996dad441bdc36b7e5a84883",
    ),
    (
        "COMPATIBILITY_SITE_MODE_SHA256",
        "bb176a41b861c4db27edf1b6e8a8254d1cbdba5fc232588f09ffa1d8ddee2a85",
    ),
    (
        "COMPATIBILITY_SITE_RECURSIVE_IDENTITY_SHA256",
        "d8e604d8ba3cf846a5599ab4e64628fa18bb2b803980e306f4416a6f269bd912",
    ),
    ("COMPATIBILITY_SITE_FILE_COUNT", "'94'"),
    ("COMPATIBILITY_SITE_FILE_BYTES", "'29222036'"),
];

const REVIEWED_ARTIFACT_VALUES: &[&str] = &[
    "DIRECTORIES = (\"assets\", \"cells\", \"data\", \"runs\", \"tests\")",
    "\"current_validation_run_row_count\": 60,",
    "\"green_cell_count\": 596,",
    "\"in_manifest_cell_count\": 5744,",
    "\"never_measured_cell_count\": 5103,",
    "\"physical_row_count\": 14541,",
    "\"red_cell_count\": 30,",
    "\"represented_run_count\": 20724,",
    "\"selected_by_full_custom_command_count\": 3,",
    "\"test_count\": 359,",
    "\"validation_history_row_count\": 64,",
    "len(members) != 100",
];

const SHELL_VERIFICATION: &[&str] = &[
    "repos/$COMPATIBILITY_SITE_RELEASE_REPOSITORY/releases/tags/$COMPATIBILITY_SITE_RELEASE_TAG",
    "--arg tag \"$COMPATIBILITY_SITE_RELEASE_TAG\"",
    "--arg title \"$COMPATIBILITY_SITE_RELEASE_TITLE\"",
    "--arg name \"$COMPATIBILITY_SITE_ASSET\"",
    "--arg digest \"sha256:$COMPATIBILITY_SITE_ARCHIVE_SHA256\"",
    "--argjson size \"$COMPATIBILITY_SITE_ARCHIVE_BYTES\"",
    ".tag_name != $tag or .name != $title",
    ".draft != false or .prerelease != false",
    "[.assets[] | select(.name == $name)]",
    "if length == 1 then .[0]",
    ".state == \"uploaded\"",
    ".size == $size",
    ".digest == $digest",
    "repos/$COMPATIBILITY_SITE_RELEASE_REPOSITORY/releases/assets/$asset_id",
    "sha256sum \"$archive\"",
    "stat -c %s \"$archive\"",
];

const PYTHON_VERIFICATION: &[&str] = &[
    "hashlib.sha256(build_bytes).hexdigest() != BUILD_SHA256",
    "manifest[\"counts\"] != EXPECTED_COUNTS",
    "manifest[\"freshness_sha256\"] != IDENTITY or TARGET.name != IDENTITY",
    "manifest[\"tree_sha256\"] != MANIFEST_TREE_SHA256",
    "manifest[\"artifacts_sha256\"] != ARTIFACTS_SHA256",
    "digest(artifacts) != ARTIFACTS_SHA256",
    "set(by_name) != expected_names",
    "observed[path] != (row[\"bytes\"], row[\"sha256\"])",
    "content.hexdigest() != CONTENT_TREE_SHA256",
    "hashlib.sha256(\"\".join(sorted(mode_rows)).encode()).hexdigest() != MODE_SHA256",
    "hashlib.sha256(\"\".join(identity_rows).encode()).hexdigest() != RECURSIVE_IDENTITY_SHA256",
];

const LATEST_COPY_CONTRACT: &[&str] = &[
    "LATEST = TARGET.parent / \"latest\"",
    "def tree_inventory(root: Path)",
    "inventory.append((relative, \"d\", mode, 0, \"\"))",
    "inventory.append((relative, \"f\", mode, size, sha256.hexdigest()))",
    "def copy_regular_tree(source_root: Path, destination_root: Path)",
    "destination_root.exists() or destination_root.is_symlink()",
    "stat.S_ISDIR(metadata.st_mode)",
    "stat.S_ISREG(metadata.st_mode)",
    "os.O_RDONLY | os.O_NOFOLLOW",
    "os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW",
    "os.chmod(destination, source_mode, follow_symlinks=False)",
    "pinned_inventory = tree_inventory(TARGET)",
    "copy_regular_tree(TARGET, LATEST)",
    "latest_inventory = tree_inventory(LATEST)",
    "if latest_inventory != pinned_inventory:",
    "latest copy differs from the pinned tree in path, type, size, content, or mode",
];

fn repository_root() -> PathBuf {
    let script = Path::new(file!());
    script
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn indentation(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn named_step(source: &str, name: &str) -> Result<String, String> {
    let lines = source.lines().collect::<Vec<_>>();
    let marker = format!("- name: {name}");
    let matches = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == marker)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "expected exactly one `{name}` step, found {}",
            matches.len()
        ));
    }
    let start = matches[0];
    let step_indent = indentation(lines[start]);
    let end = lines[start + 1..]
        .iter()
        .position(|line| {
            !line.trim().is_empty()
                && indentation(line) == step_indent
                && line.trim_start().starts_with("- ")
        })
        .map_or(lines.len(), |offset| start + 1 + offset);
    Ok(lines[start..end].join("\n"))
}

fn literal_block(step: &str, key: &str) -> Result<String, String> {
    let lines = step.lines().collect::<Vec<_>>();
    let marker = format!("{key}: |");
    let matches = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim() == marker)
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(format!(
            "publication step must contain exactly one `{marker}`, found {}",
            matches.len()
        ));
    }
    let start = matches[0] + 1;
    let parent_indent = indentation(lines[matches[0]]);
    let end = lines[start..]
        .iter()
        .position(|line| !line.trim().is_empty() && indentation(line) <= parent_indent)
        .map_or(lines.len(), |offset| start + offset);
    let body = &lines[start..end];
    let body_indent = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indentation(line))
        .min()
        .ok_or_else(|| format!("publication `{key}` block is empty"))?;
    if body_indent <= parent_indent {
        return Err(format!("publication `{key}` block has invalid indentation"));
    }
    Ok(body
        .iter()
        .map(|line| line.get(body_indent..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn heredoc(source: &str, delimiter: &str) -> Result<String, String> {
    let quoted_marker = format!("<<'{delimiter}'");
    let lines = source.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| line.contains(&quoted_marker))
        .ok_or_else(|| format!("missing quoted {delimiter} heredoc"))?
        + 1;
    let end = lines[start..]
        .iter()
        .position(|line| *line == delimiter)
        .map(|offset| start + offset)
        .ok_or_else(|| format!("unterminated {delimiter} heredoc"))?;
    Ok(lines[start..end].join("\n"))
}

fn require_once(errors: &mut Vec<String>, scope: &str, source: &str, needle: &str) {
    let count = source.matches(needle).count();
    if count != 1 {
        errors.push(format!(
            "{scope} must contain `{needle}` exactly once, found {count}"
        ));
    }
}

fn require_all(errors: &mut Vec<String>, scope: &str, source: &str, needles: &[&str]) {
    for needle in needles {
        if !source.contains(needle) {
            errors.push(format!("{scope} is missing required contract `{needle}`"));
        }
    }
}

fn env_value<'a>(source: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}:");
    source.lines().find_map(|line| {
        let trimmed = line.trim();
        trimmed
            .strip_prefix(&prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn validate_contract(workflow: &str, landing: &str) -> Result<(), Vec<String>> {
    let mut errors = Vec::new();
    for (key, expected) in PIN_VALUES {
        let marker = format!("\n  {key}:");
        require_once(&mut errors, "workflow top-level pin set", workflow, &marker);
        match env_value(workflow, key) {
            Some(actual) if actual != *expected => errors.push(format!(
                "workflow pin `{key}` must equal `{expected}`, found `{actual}`"
            )),
            None => errors.push(format!("cannot read workflow pin `{key}`")),
            Some(_) => {}
        }
    }

    if env_value(workflow, "COMPATIBILITY_SITE_RELEASE_TITLE")
        .is_some_and(|title| title.contains("Historical"))
    {
        errors.push("release title must not use Historical wording".into());
    }

    let step = match named_step(workflow, PUBLICATION_STEP) {
        Ok(step) => step,
        Err(error) => {
            errors.push(error);
            return Err(errors);
        }
    };
    require_once(
        &mut errors,
        "publication step",
        &step,
        "if: github.ref == 'refs/heads/main'",
    );
    let shell = match literal_block(&step, "run") {
        Ok(shell) => shell,
        Err(error) => {
            errors.push(error);
            return Err(errors);
        }
    };
    require_all(
        &mut errors,
        "release/archive verification",
        &shell,
        SHELL_VERIFICATION,
    );
    let python = match heredoc(&shell, "PY") {
        Ok(python) => python,
        Err(error) => {
            errors.push(error);
            return Err(errors);
        }
    };
    require_all(
        &mut errors,
        "extracted artifact verification",
        &python,
        PYTHON_VERIFICATION,
    );
    for value in REVIEWED_ARTIFACT_VALUES {
        require_once(&mut errors, "reviewed artifact values", &python, value);
    }
    require_all(
        &mut errors,
        "latest full-copy verification",
        &python,
        LATEST_COPY_CONTRACT,
    );
    require_once(
        &mut errors,
        "workflow landing assertion",
        &python,
        LANDING_ALIAS,
    );
    require_once(
        &mut errors,
        "workflow landing assertion",
        &python,
        LANDING_WORDING,
    );
    for forbidden in ["os.symlink(", ".symlink_to(", "os.link("] {
        if python.contains(forbidden) {
            errors.push(format!(
                "latest publication must be a copied regular-file tree, found `{forbidden}`"
            ));
        }
    }

    require_once(&mut errors, "landing page", landing, LANDING_ALIAS);
    require_once(&mut errors, "landing page", landing, LANDING_WORDING);
    for source in [python.as_str(), landing] {
        if source.contains("Historical snapshot &mdash; not current:") {
            errors.push("landing wording must not claim the snapshot is not current".into());
        }
    }
    if let Some(identity) = env_value(workflow, "COMPATIBILITY_SITE_IDENTITY") {
        let pinned_href = format!("href=\"compatibility/{identity}/\"");
        if landing.contains(&pinned_href) {
            errors.push("landing page still links the content-addressed snapshot directly".into());
        }
    } else {
        errors.push("cannot read COMPATIBILITY_SITE_IDENTITY value".into());
    }
    require_once(
        &mut errors,
        "deployment step",
        workflow,
        "publish_dir: ./target/doc",
    );

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn read_inputs(root: &Path) -> Result<(String, String), String> {
    let workflow = fs::read_to_string(root.join(WORKFLOW))
        .map_err(|error| format!("cannot read {WORKFLOW}: {error}"))?;
    let landing = fs::read_to_string(root.join(LANDING_PAGE))
        .map_err(|error| format!("cannot read {LANDING_PAGE}: {error}"))?;
    Ok((workflow, landing))
}

fn main() {
    rust_script_prelude::init();
    let root = repository_root();
    let result = read_inputs(&root).and_then(|(workflow, landing)| {
        validate_contract(&workflow, &landing).map_err(|errors| errors.join("\n"))
    });
    match result {
        Ok(()) => println!(
            "docs Pages contract OK: pinned snapshot plus byte/mode-identical compatibility/latest"
        ),
        Err(error) => {
            eprintln!("docs-pages-contract: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    const COPY_FIXTURE: &str = r#"
import ast
import os
import pathlib
import tempfile

module = ast.parse(os.environ["DOCS_PAGES_EMBEDDED_PYTHON"])
wanted = {"refuse", "tree_inventory", "copy_regular_tree"}
nodes = [
    node for node in module.body
    if isinstance(node, (ast.Import, ast.ImportFrom))
    or isinstance(node, ast.FunctionDef) and node.name in wanted
]
namespace = {}
exec(compile(ast.Module(body=nodes, type_ignores=[]), "docs.yml:copy-functions", "exec"), namespace)

with tempfile.TemporaryDirectory(prefix="docs-pages-copy-", dir="/tmp") as temporary:
    base = pathlib.Path(temporary)
    pinned = base / "identity"
    latest = base / "latest"
    (pinned / "assets").mkdir(parents=True)
    (pinned / "cells" / "deep").mkdir(parents=True)
    (pinned / "index.html").write_bytes(b"index\x00bytes")
    (pinned / "assets" / "site.js").write_bytes(b"console.log(1)\n")
    (pinned / "cells" / "deep" / "index.html").write_bytes(b"deep")
    os.chmod(pinned, 0o750)
    os.chmod(pinned / "assets", 0o710)
    os.chmod(pinned / "cells", 0o755)
    os.chmod(pinned / "cells" / "deep", 0o711)
    os.chmod(pinned / "index.html", 0o440)
    os.chmod(pinned / "assets" / "site.js", 0o640)
    os.chmod(pinned / "cells" / "deep" / "index.html", 0o444)

    expected = namespace["tree_inventory"](pinned)
    namespace["copy_regular_tree"](pinned, latest)
    observed = namespace["tree_inventory"](latest)
    assert observed == expected
    assert all(
        not path.is_symlink() and (path.is_file() or path.is_dir())
        for path in [latest, *latest.rglob("*")]
    )

    (latest / "assets" / "site.js").write_bytes(b"changed")
    assert namespace["tree_inventory"](latest) != expected

    os.symlink("index.html", pinned / "alias")
    try:
        namespace["copy_regular_tree"](pinned, base / "refused")
    except SystemExit as error:
        assert "link or special" in str(error)
    else:
        raise AssertionError("symlinked source was accepted")
"#;

    fn actual() -> (String, String) {
        read_inputs(&repository_root()).expect("repository contract inputs should be readable")
    }

    fn assert_rejected(workflow: &str, landing: &str, expected: &str) {
        let errors = validate_contract(workflow, landing)
            .expect_err("weakened publication contract unexpectedly passed")
            .join("\n");
        assert!(errors.contains(expected), "unexpected errors: {errors}");
    }

    #[test]
    fn current_workflow_and_landing_page_satisfy_contract() {
        let (workflow, landing) = actual();
        validate_contract(&workflow, &landing).expect("current Pages contract should pass");
    }

    #[test]
    fn changing_any_pin_or_pinned_verification_is_rejected() {
        let (workflow, landing) = actual();
        for (key, expected) in PIN_VALUES {
            let declaration = format!("\n  {key}:");
            let weakened = workflow.replacen(&declaration, "\n  REMOVED_PIN:", 1);
            assert_rejected(&weakened, &landing, &declaration);

            let declaration = format!("\n  {key}: {expected}");
            let changed = format!("\n  {key}: MUTATED_PIN");
            let weakened = workflow.replacen(&declaration, &changed, 1);
            assert_rejected(&weakened, &landing, &format!("workflow pin `{key}`"));
        }
        for required in SHELL_VERIFICATION.iter().chain(PYTHON_VERIFICATION) {
            let weakened = workflow.replacen(required, "REMOVED_VERIFICATION", 1);
            assert_rejected(&weakened, &landing, required);
        }
        for value in REVIEWED_ARTIFACT_VALUES {
            let weakened = workflow.replacen(value, "MUTATED_ARTIFACT_VALUE", 1);
            assert_rejected(&weakened, &landing, value);
        }

        let identity = env_value(&workflow, "COMPATIBILITY_SITE_IDENTITY").unwrap();
        let current_title = format!("Compatibility website snapshot {identity}");
        let historical_title = format!("Historical compatibility website snapshot {identity}");
        let weakened = workflow.replacen(&current_title, &historical_title, 1);
        assert_rejected(
            &weakened,
            &landing,
            "release title must not use Historical wording",
        );
    }

    #[test]
    fn redirect_or_missing_full_copy_is_rejected() {
        let (workflow, landing) = actual();
        let redirected = workflow.replacen(
            "copy_regular_tree(TARGET, LATEST)",
            "LATEST.symlink_to(TARGET, target_is_directory=True)",
            1,
        );
        assert_rejected(&redirected, &landing, "copy_regular_tree(TARGET, LATEST)");
        assert_rejected(&redirected, &landing, ".symlink_to(");
    }

    #[test]
    fn embedded_copy_is_byte_and_mode_identical_and_refuses_symlinks() {
        let (workflow, _) = actual();
        let step = named_step(&workflow, PUBLICATION_STEP).unwrap();
        let shell = literal_block(&step, "run").unwrap();
        let python = heredoc(&shell, "PY").unwrap();
        let output = Command::new("python3")
            .args(["-c", COPY_FIXTURE])
            .env("DOCS_PAGES_EMBEDDED_PYTHON", python)
            .output()
            .expect("python3 should execute the embedded copy fixture");
        assert!(
            output.status.success(),
            "copy fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn omitting_full_inventory_equality_is_rejected() {
        let (workflow, landing) = actual();
        let weakened =
            workflow.replacen("if latest_inventory != pinned_inventory:", "if False:", 1);
        assert_rejected(
            &weakened,
            &landing,
            "if latest_inventory != pinned_inventory:",
        );
    }

    #[test]
    fn landing_page_must_use_the_stable_alias() {
        let (workflow, landing) = actual();
        let identity = env_value(&workflow, "COMPATIBILITY_SITE_IDENTITY").unwrap();
        let pinned = landing.replacen(
            "href=\"compatibility/latest/\"",
            &format!("href=\"compatibility/{identity}/\""),
            1,
        );
        assert_rejected(&workflow, &pinned, "href=\"compatibility/latest/\"");
        assert_rejected(
            &workflow,
            &pinned,
            "landing page still links the content-addressed snapshot directly",
        );
    }

    #[test]
    fn workflow_landing_assertion_must_use_the_stable_alias() {
        let (workflow, landing) = actual();
        let identity = env_value(&workflow, "COMPATIBILITY_SITE_IDENTITY").unwrap();
        let weakened = workflow.replacen(
            LANDING_ALIAS,
            &format!("href=\"compatibility/{identity}/\""),
            1,
        );
        assert_rejected(&weakened, &landing, "workflow landing assertion");
        assert_rejected(&weakened, &landing, LANDING_ALIAS);
    }
}
