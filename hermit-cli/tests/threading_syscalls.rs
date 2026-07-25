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
fn threading_syscalls_reach_strict_verify_l2() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("threading-syscalls");
    fs::create_dir_all(&build_root)
        .expect("failed to create threading syscall guest build directory");
    let guest = build_root.join("threading_syscalls");

    let mut compile = Command::new("cc");
    compile
        .args([
            "-O0", "-g", "-pthread", "-std=c11", "-Wall", "-Wextra", "-Werror",
        ])
        .arg(repository.join("tests/bin/robust_futex_test.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "threading syscall guest compilation");

    let mut trace_command = Command::new("timeout");
    trace_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--base-env=minimal",
            "--max-timeslice=disabled",
            "--tmp=/tmp",
            "--",
        ])
        .arg(&guest);
    let trace_output = command_output(trace_command, "strict threading syscall run");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("PASS: blocked and failed signals preserved live owner")
            && trace_stdout.contains("PASS: pending owner-zero robust wake preserved word")
            && trace_stdout.contains("PASS: robust mutex waiter received EOWNERDEAD")
            && trace_stdout.contains("PASS: sibling robust-list lookup and ESRCH semantics")
            && trace_stdout.contains("PASS: legacy and futex2 variants handled deterministically")
            && trace_stdout.contains("PASS: exit_group and fatal-signal owner death recovered"),
        "threading syscall guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    let mut verify_command = Command::new("timeout");
    verify_command
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--verify",
            "--base-env=minimal",
            "--max-timeslice=disabled",
            "--tmp=/tmp",
            "--",
        ])
        .arg(&guest);
    let verify_output = command_output(verify_command, "strict threading syscall verification");
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
}
