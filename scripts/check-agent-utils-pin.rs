#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Require Hermit's agent-utils gitlink to equal `rrnewton/agent-utils:main`.
//!
//! The gitlink keeps historical Hermit commits reproducible, but validation and
//! new commits must use the current agent-utils main revision. This checker is
//! deliberately separate from the Reverie checker: each remote authority has
//! one verifier and one independently attributable failure.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const DEFAULT_REMOTE: &str = "https://github.com/rrnewton/agent-utils.git";
const MAIN_REF: &str = "refs/heads/main";

#[derive(Default)]
struct Config {
    repo: Option<PathBuf>,
    #[cfg(test)]
    remote: Option<String>,
}

fn usage() -> &'static str {
    "Usage: check-agent-utils-pin.rs [--repo PATH]\n\
     \n\
     Require the tracked agent-utils gitlink to equal the live\n\
     rrnewton/agent-utils:main tip. The submodule need not be initialized."
}

fn parse_args() -> Result<Config, String> {
    let args: Vec<String> = env::args().collect();
    let mut config = Config::default();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--repo" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--repo requires a Hermit checkout path".to_string())?;
                config.repo = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                println!("{}", usage());
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
        index += 1;
    }
    Ok(config)
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

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_in(root: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))
}

fn read_gitlink(root: &Path) -> Result<String, String> {
    let output = git_in(root, &["ls-tree", "HEAD", "--", "agent-utils"])?;
    if !output.status.success() {
        return Err(format!(
            "git ls-tree HEAD -- agent-utils failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let (metadata, path) = line
        .split_once('\t')
        .ok_or_else(|| "HEAD does not contain an agent-utils gitlink".to_string())?;
    let fields: Vec<&str> = metadata.split_whitespace().collect();
    if path != "agent-utils" || fields.len() != 3 || fields[0] != "160000" || fields[1] != "commit"
    {
        return Err(format!(
            "HEAD agent-utils entry is not a submodule gitlink: {line:?}"
        ));
    }
    let sha = fields[2].to_string();
    if !is_full_sha(&sha) {
        return Err(format!(
            "HEAD contains invalid agent-utils gitlink SHA {sha:?}"
        ));
    }
    Ok(sha)
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
        return Err(format!(
            "remote returned invalid agent-utils main SHA {sha:?}"
        ));
    }
    Ok(sha)
}

fn loud_header(title: &str) {
    eprintln!("======================================================================");
    eprintln!("AGENT-UTILS PIN LINT: {title}");
    eprintln!("======================================================================");
}

fn blocked_instructions() {
    eprintln!();
    eprintln!("BLOCKED. Hermit must pin the exact latest rrnewton/agent-utils:main.");
    eprintln!(
        "Use the serialized agent-utils workflow, then re-pin Hermit to the exact landed SHA."
    );
    eprintln!("Do not push agent-utils changes straight to main.");
}

fn check_repo(root: &Path, remote: &str) -> Result<i32, String> {
    let pin = read_gitlink(root)?;
    let main = query_main(remote)?;
    if pin == main {
        println!("agent-utils pin is current: {pin}");
        return Ok(0);
    }

    loud_header("PIN DOES NOT EQUAL LATEST MAIN - BLOCKED");
    eprintln!("Hermit gitlink: {pin}");
    eprintln!("Latest main:    {main}");
    blocked_instructions();
    Ok(1)
}

fn run_with_config(config: Config) -> Result<i32, String> {
    let root = match config.repo {
        Some(root) => root,
        None => git_root()?,
    };
    #[cfg(test)]
    let remote = config.remote.as_deref().unwrap_or(DEFAULT_REMOTE);
    #[cfg(not(test))]
    let remote = DEFAULT_REMOTE;
    check_repo(&root, remote)
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
    use std::fs;
    use std::process::Output;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use super::*;

    struct Fixture {
        root: PathBuf,
        remote: PathBuf,
        stale: String,
        current: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
            let _ = fs::remove_dir_all(&self.remote);
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before epoch")
            .as_nanos();
        env::temp_dir().join(format!(
            "check-agent-utils-pin-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    fn git(repo: &Path, args: &[&str]) -> Output {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("launch git");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn sha(repo: &Path) -> String {
        String::from_utf8(git(repo, &["rev-parse", "HEAD"]).stdout)
            .expect("utf8 SHA")
            .trim()
            .to_string()
    }

    fn write_commit(repo: &Path, contents: &str) -> String {
        fs::write(repo.join("payload"), contents).expect("write payload");
        git(repo, &["add", "payload"]);
        git(
            repo,
            &[
                "-c",
                "user.name=Pin Lint Test",
                "-c",
                "user.email=pin-lint@example.com",
                "commit",
                "-m",
                contents,
            ],
        );
        sha(repo)
    }

    fn set_gitlink(repo: &Path, revision: &str, message: &str) {
        let cacheinfo = format!("160000,{revision},agent-utils");
        git(repo, &["update-index", "--add", "--cacheinfo", &cacheinfo]);
        git(
            repo,
            &[
                "-c",
                "user.name=Pin Lint Test",
                "-c",
                "user.email=pin-lint@example.com",
                "commit",
                "-m",
                message,
            ],
        );
    }

    fn fixture(label: &str) -> Fixture {
        let remote = temp_path(&format!("{label}-remote"));
        let root = temp_path(&format!("{label}-root"));
        fs::create_dir_all(&remote).expect("create remote");
        fs::create_dir_all(&root).expect("create root");
        git(&remote, &["init", "-b", "main"]);
        let stale = write_commit(&remote, "stale");
        let current = write_commit(&remote, "current");

        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("README"), "fixture\n").expect("write README");
        git(&root, &["add", "README"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Pin Lint Test",
                "-c",
                "user.email=pin-lint@example.com",
                "commit",
                "-m",
                "fixture root",
            ],
        );
        set_gitlink(&root, &stale, "pin stale agent-utils");
        Fixture {
            root,
            remote,
            stale,
            current,
        }
    }

    #[test]
    fn planted_stale_pin_is_refused() {
        let fixture = fixture("stale");
        assert_ne!(fixture.stale, fixture.current);
        assert_eq!(
            check_repo(&fixture.root, fixture.remote.to_str().unwrap()).unwrap(),
            1
        );
    }

    #[test]
    fn planted_current_pin_passes() {
        let fixture = fixture("current");
        set_gitlink(&fixture.root, &fixture.current, "pin current agent-utils");
        assert_eq!(
            check_repo(&fixture.root, fixture.remote.to_str().unwrap()).unwrap(),
            0
        );
    }

    #[test]
    fn rejects_a_non_gitlink_entry() {
        let root = temp_path("ordinary-directory");
        fs::create_dir_all(root.join("agent-utils")).unwrap();
        git(&root, &["init", "-b", "main"]);
        fs::write(root.join("agent-utils/file"), "not a submodule\n").unwrap();
        git(&root, &["add", "agent-utils/file"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Pin Lint Test",
                "-c",
                "user.email=pin-lint@example.com",
                "commit",
                "-m",
                "ordinary directory",
            ],
        );
        let error = read_gitlink(&root).unwrap_err();
        assert!(error.contains("not a submodule gitlink"));
        fs::remove_dir_all(root).unwrap();
    }
}
