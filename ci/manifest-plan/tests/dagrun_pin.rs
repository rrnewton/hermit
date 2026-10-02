/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Keep every `dagrun` library dependency on the commit the `agent-utils`
//! gitlink records.
//!
//! `dagrun` is a git dependency rather than a path into the `agent-utils`
//! submodule, so a clone without submodules can still load the root workspace
//! and build or install `hermit`. Validation still runs the `dagrun` executable
//! from the submodule checkout. The library that generates and reads the DAG and
//! the executable that runs it must therefore come from one commit; this test
//! refuses any declaration, or the lockfile entry, that names another one.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const AGENT_UTILS_GIT: &str = "https://github.com/rrnewton/agent-utils.git";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("ci/manifest-plan must sit two levels below the repository root")
        .to_path_buf()
}

/// Git exports these to hooks and `git rebase --exec` steps, and they override
/// the working directory (https://github.com/rrnewton/hermit/issues/3362).
fn git(root: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(name);
    }
    let output = command
        .args(args)
        .current_dir(root)
        .output()
        .unwrap_or_else(|e| panic!("cannot run git {args:?}: {e}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output must be UTF-8")
}

fn gitlink(ls_tree: &str) -> Result<String, String> {
    let (metadata, path) = ls_tree
        .trim_end()
        .split_once('\t')
        .ok_or_else(|| format!("malformed ls-tree output: {ls_tree:?}"))?;
    match metadata.split(' ').collect::<Vec<_>>()[..] {
        ["160000", "commit", sha] if path == "agent-utils" && sha.len() == 40 => Ok(sha.into()),
        _ => Err(format!(
            "HEAD does not record agent-utils as a gitlink: {ls_tree:?}"
        )),
    }
}

/// Every reason the declarations or the lockfile disagree with `pin`.
fn disagreements(pin: &str, declarations: &[(String, String)], lock: &str) -> Vec<String> {
    let expected = format!(r#"git = "{AGENT_UTILS_GIT}", rev = "{pin}""#);
    let mut problems: Vec<String> = declarations
        .iter()
        .filter(|(_, line)| !line.contains(&expected))
        .map(|(location, line)| format!("{location}: `{line}` does not contain `{expected}`"))
        .collect();
    let lock: toml::Value = match lock.parse() {
        Ok(lock) => lock,
        Err(e) => return vec![format!("Cargo.lock does not parse: {e}")],
    };
    let sources: Vec<Option<&str>> = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some("dagrun"))
        .map(|package| package.get("source").and_then(toml::Value::as_str))
        .collect();
    let locked = format!("git+{AGENT_UTILS_GIT}?rev={pin}#{pin}");
    if sources != [Some(locked.as_str())] {
        problems.push(format!(
            "Cargo.lock must hold exactly one dagrun package, from `{locked}`; found {sources:?}"
        ));
    }
    problems
}

#[test]
fn every_dagrun_dependency_uses_the_agent_utils_gitlink() {
    let root = repo_root();
    let pin = gitlink(&git(&root, &["ls-tree", "HEAD", "agent-utils"])).unwrap();
    let declarations: Vec<(String, String)> = git(
        &root,
        &[
            "grep",
            "-n",
            "-E",
            "^(//! )?dagrun = [{]",
            "--",
            ":!agent-utils",
        ],
    )
    .lines()
    .map(|line| {
        let mut fields = line.splitn(3, ':');
        let location = format!(
            "{}:{}",
            fields.next().unwrap_or_default(),
            fields.next().unwrap_or_default()
        );
        (location, fields.next().unwrap_or_default().to_string())
    })
    .collect();
    assert!(
        declarations
            .iter()
            .any(|(location, _)| location.starts_with("ci/manifest-plan/Cargo.toml:")),
        "the search must find the workspace's own dagrun dependency; found {declarations:?}"
    );
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).expect("read Cargo.lock");

    let problems = disagreements(&pin, &declarations, &lock);
    assert!(
        problems.is_empty(),
        "dagrun dependencies disagree with the agent-utils gitlink {pin}; bump each \
         `rev`, then run `cargo update -p dagrun`:\n{}",
        problems.join("\n")
    );

    // The same inputs must be refused against any other commit, and a
    // declaration that reverts to the submodule path must be refused too.
    let other = "0".repeat(40);
    assert_eq!(
        disagreements(&other, &declarations, &lock).len(),
        declarations.len() + 1,
        "a different gitlink must refuse every declaration and the lockfile"
    );
    let mut reverted = declarations.clone();
    reverted[0].1 = r#"dagrun = { path = "../../agent-utils/rs/dagrun" }"#.into();
    assert_eq!(
        disagreements(&pin, &reverted, &lock).len(),
        1,
        "a path declaration must be refused"
    );
}
