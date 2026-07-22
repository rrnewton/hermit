/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_REDIS_LOCK: Mutex<()> = Mutex::new(());

fn hermit_redis_lock() -> MutexGuard<'static, ()> {
    HERMIT_REDIS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn executable(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().find_map(|candidate| {
        let path = PathBuf::from(candidate);
        let metadata = fs::metadata(&path).ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(path)
    })
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

fn run_strict_workload(
    redis_server: &Path,
    redis_cli: &Path,
    mode: &str,
    iteration: usize,
) -> Output {
    let workload = repository().join("experiments/redis-strict/workload.sh");
    let instance = format!("cargo-{}-{iteration}", std::process::id());
    let mut command = Command::new("timeout");
    command
        .arg("90")
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", "off", "run", "--strict", "--", "/bin/sh"])
        .arg(workload)
        .arg(redis_server)
        .arg(redis_cli)
        .arg(mode)
        .arg(instance);
    command_output(
        command,
        &format!("strict Redis {mode} workload, iteration {iteration}"),
    )
}

#[test]
fn redis_small_subset_is_deterministic_under_strict_hermit() {
    let _guard = hermit_redis_lock();
    let redis_server = executable(&["/usr/bin/redis-server", "/usr/local/bin/redis-server"])
        .expect("redis-server is required; the self-hosted CI job installs it");
    let redis_cli = executable(&["/usr/bin/redis-cli", "/usr/local/bin/redis-cli"])
        .expect("redis-cli is required; the self-hosted CI job installs it");

    let first = run_strict_workload(&redis_server, &redis_cli, "small", 1);
    let second = run_strict_workload(&redis_server, &redis_cli, "small", 2);
    assert_eq!(
        first.stdout, second.stdout,
        "Redis stdout changed between runs"
    );
    assert_eq!(
        first.stderr, second.stderr,
        "Redis stderr changed between runs"
    );

    let stdout = String::from_utf8_lossy(&first.stdout);
    for marker in [
        "ping=PONG\n",
        "string=hermit\n",
        "counter=2\n",
        "list=alpha,beta,gamma\n",
        "redis-strict-small-ok\n",
    ] {
        assert!(
            stdout.contains(marker),
            "Redis output omitted {marker:?}:\n{stdout}"
        );
    }
}

#[test]
#[ignore = "downloads and builds pinned Redis, then runs the extended strict suite"]
fn redis_source_build_and_extended_suite_under_strict_hermit() {
    let _guard = hermit_redis_lock();
    let runner = repository().join("experiments/redis-strict/run.sh");
    let mut command = Command::new("timeout");
    command
        .arg("900")
        .arg(runner)
        .env("HERMIT_BIN", env!("CARGO_BIN_EXE_hermit"));
    let output = command_output(command, "pinned Redis source-build strict suite");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("redis-source-build-strict-ok\n"),
        "source-build runner omitted its success marker:\n{stdout}"
    );
}
