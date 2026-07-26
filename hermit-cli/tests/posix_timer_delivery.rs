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
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static HERMIT_TIMER_LOCK: Mutex<()> = Mutex::new(());
static TIMER_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn timer_lock() -> MutexGuard<'static, ()> {
    HERMIT_TIMER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn timer_guest() -> &'static Path {
    TIMER_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("posix-timer-delivery");
            fs::create_dir_all(&build_root)
                .expect("failed to create POSIX timer guest build directory");
            let binary = build_root.join("posix_timer_delivery");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-D_GNU_SOURCE",
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/posix_timer_delivery.c"))
                .args(["-lrt", "-o"])
                .arg(&binary);
            command_output(command, "POSIX timer guest compilation");
            binary
        })
        .as_path()
}

fn run_timer_scenario(scenario: &str) {
    let _guard = timer_lock();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(["--log=off", "run", "--strict", "--verify", "--"]);
    command.arg(timer_guest()).arg(scenario);
    let output = command_output(command, &format!("POSIX timer {scenario} scenario"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Determinism verified"),
        "POSIX timer {scenario} did not report L2 verification:\n{stderr}",
    );
}

#[test]
fn one_shot_timer_reaches_sigtimedwait_with_metadata() {
    run_timer_scenario("sigtimedwait");
}

#[test]
fn blocked_timer_signal_survives_nanosleep_and_reaches_signalfd() {
    run_timer_scenario("signalfd");
}

#[test]
fn gnu_timeout_expires_under_strict_verify() {
    if !Path::new("/usr/bin/timeout").is_file() || !Path::new("/usr/bin/bash").is_file() {
        eprintln!("skipping GNU timeout regression: /usr/bin/timeout or /usr/bin/bash is absent");
        return;
    }

    let _guard = timer_lock();
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log=off",
        "run",
        "--strict",
        "--verify",
        "--",
        "/usr/bin/bash",
        "-c",
        "/usr/bin/timeout 1 /usr/bin/sleep 5; test \"$?\" -eq 124",
    ]);
    command_output(command, "GNU timeout strict verify regression");
}
