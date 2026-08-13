/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

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

#[test]
fn blocking_scalar_pipe_write_completes_positive_short() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("scalar-pipe-write");
    fs::create_dir_all(&build_root).expect("failed to create scalar-write build directory");
    let guest = build_root.join("scalar_pipe_write");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/scalar_pipe_write.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "scalar pipe write guest compilation");

    let mut run = Command::new("timeout");
    run.args(["--kill-after=5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=trace",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let run_output = command_output(run, "blocking scalar pipe write");
    let run_combined = format!(
        "{}{}",
        String::from_utf8_lossy(&run_output.stdout),
        String::from_utf8_lossy(&run_output.stderr)
    );
    assert!(
        run_combined.contains("scalar-pipe-write-ok"),
        "guest omitted its success marker:\n{run_combined}"
    );
    assert!(
        run_combined.contains("Retry #1 for blocking pipe write after Ok(4096)"),
        "test did not exercise positive-short completion:\n{run_combined}"
    );

    for case in ["close-reuse", "sigpipe"] {
        let mut bracket = Command::new("timeout");
        bracket
            .args(["--kill-after=5s", "30s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=off",
                "run",
                "--strict",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .arg(case);
        command_output(bracket, case);
    }

    let recording = build_root.join("recording");
    let _ = fs::remove_dir_all(&recording);
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after=5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg("--data-dir")
        .arg(&recording)
        .arg("--")
        .arg(&guest)
        .arg("stdout");
    let record_output = command_output(record, "scalar pipe write recording");

    let mut replay = Command::new("timeout");
    replay
        .args(["--kill-after=5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "replay", "--autopilot", "--data-dir"])
        .arg(&recording);
    let replay_output = command_output(replay, "scalar pipe write replay");

    let expected = (0..8190usize)
        .map(|index| ((index * 37 + 11) & 0xff) as u8)
        .collect::<Vec<_>>();
    assert_eq!(
        record_output.stdout, expected,
        "recording did not emit all 8190 bytes from the one scalar write"
    );
    assert_eq!(
        replay_output.stdout, expected,
        "replay did not reproduce all 8190 recorded bytes"
    );
}
