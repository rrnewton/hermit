/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! An mtime set explicitly through utimensat, futimens or utimes is the mtime
//! the guest's stat reports afterwards
//! (https://github.com/rrnewton/hermit/issues/3565). Before this, Hermit's
//! virtual mtime moved only on writes, so a build unpacked by `tar` saw its
//! files ordered by extraction and `make` re-ran autotools steps that a native
//! build skips.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_test::configure_guest_execution(&mut command);
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

#[test]
fn explicit_mtimes_are_reported_by_stat_and_verify_strictly() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("utimensat-mtime");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("utimensat_mtime");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/utimensat_mtime.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "utimensat guest compilation");

    // The guest checks Linux's own behavior, so it must pass without Hermit.
    let native = command_output(Command::new(&guest), "native utimensat guest");
    assert_eq!(
        String::from_utf8_lossy(&native.stdout),
        "explicit mtimes honored\n"
    );

    let report = build_root.join("verify.json");
    let mut run = Command::new("timeout");
    run.args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "--backend=ptrace",
            "run",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&report)
        .args(["--base-env=minimal", "--"])
        .arg(&guest);
    let output = command_output(run, "utimensat guest under hermit run --verify-strict");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("explicit mtimes honored"),
        "guest saw a virtual mtime that ignored utimensat\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report).expect("hermit did not write the verification report"),
    )
    .expect("verification report is not JSON");
    assert_eq!(report["bitwise_parity"], serde_json::Value::Bool(true));
    let compared = &report["compared_log_messages"];
    assert!(
        compared["left"].as_u64().is_some_and(|n| n > 0) && compared["left"] == compared["right"],
        "verification compared no INFO messages: {report}"
    );
}
