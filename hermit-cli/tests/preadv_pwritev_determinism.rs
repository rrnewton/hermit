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

/// Positioned vectored I/O (preadv/preadv2/pwritev/pwritev2) must run under
/// strict mode (no fail-closed abort on an "unsupported" syscall) and verify
/// bitwise-identically across two runs.
#[test]
fn preadv_pwritev_run_strict_and_verify() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("preadv-pwritev-determinism");
    fs::create_dir_all(&build_root).expect("failed to create preadv/pwritev guest build directory");
    let guest = build_root.join("preadv_pwritev_determinism");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/preadv_pwritev_determinism.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "preadv/pwritev guest compilation");

    // Strict run with the fail-closed guard armed: proves the four syscalls are
    // now handled rather than aborting the sandbox.
    let mut strict = Command::new("timeout");
    strict
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let strict_output = command_output(strict, "strict preadv/pwritev run");
    let strict_stdout = String::from_utf8_lossy(&strict_output.stdout);
    let strict_stderr = String::from_utf8_lossy(&strict_output.stderr);
    assert!(
        strict_stdout.contains("preadv-pwritev-determinism-ok"),
        "preadv/pwritev guest omitted its success marker\nstdout:\n{strict_stdout}\nstderr:\n{strict_stderr}",
    );
    // Confirm each syscall actually reached typed dispatch (not silently elided).
    for needle in ["preadv(", "preadv2(", "pwritev(", "pwritev2("] {
        assert!(
            strict_stderr.contains(needle),
            "expected {needle} in the deterministic trace\nstderr:\n{strict_stderr}",
        );
    }

    // Bitwise-identical repeat run (L2).
    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--verify",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let verify_output = command_output(verify, "strict preadv/pwritev verification");
    let verify_stdout = String::from_utf8_lossy(&verify_output.stdout);
    let verify_stderr = String::from_utf8_lossy(&verify_output.stderr);
    assert!(
        verify_stdout.contains("Determinism verified")
            || verify_stderr.contains("Determinism verified"),
        "Hermit omitted its determinism marker\nstdout:\n{verify_stdout}\nstderr:\n{verify_stderr}",
    );
}
