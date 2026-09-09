/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

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

fn compile_guest() -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("preadv2-pwritev2-pipe");
    fs::create_dir_all(&build_root).expect("failed to create p*v2 guest build directory");
    let guest = build_root.join("preadv2_pwritev2_pipe");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/preadv2_pwritev2_pipe.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "p*v2 pipe guest compilation");
    guest
}

#[test]
fn current_position_preadv2_and_pwritev2_match_blocking_pipe_semantics() {
    let guest = compile_guest();

    let mut trace = Command::new("timeout");
    trace
        .args(["--kill-after", "5s", "40s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=trace",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let trace_output = command_output(trace, "strict p*v2 pipe trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    for marker in [
        "preadv2-pwritev2-nowait-and-errors-ok",
        "preadv2-snapshot-ok",
        "pwritev2-atomic-snapshot-ok",
        "pwritev2-large-ok",
        "preadv2-signal-ok",
        "pwritev2-signal-ok",
        "preadv2-pwritev2-pipe-ok",
    ] {
        assert!(
            trace_stdout.contains(marker),
            "p*v2 guest omitted {marker}\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
        );
    }
    for evidence in [
        "inbound syscall: preadv2",
        "inbound syscall: pwritev2",
        "Retry #1 for blocking pipe preadv2",
        "Retry #1 for atomic blocking pipe pwritev2 after EAGAIN",
        "Retry #1 for blocking pipe pwritev2 after EAGAIN",
        " preadv2 in fd=",
        " pwritev2 out fd=",
    ] {
        assert!(
            trace_stderr.contains(evidence),
            "p*v2 trace omitted {evidence}\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
        );
    }

    let report_dir = tempfile::tempdir().expect("failed to create verification directory");
    let report_path = report_dir.path().join("verify.json");
    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "run",
            "--verify",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--verify-strict",
            "--base-env=minimal",
        ])
        .arg(format!("--verify-json={}", report_path.display()))
        .arg("--")
        .arg(&guest);
    command_output(verify, "strict p*v2 pipe verification");
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_path).expect("p*v2 verification report was not written"),
    )
    .expect("p*v2 verification report is valid JSON");
    assert_eq!(report["verdict"], "matched", "verify report: {report}");
    assert_eq!(report["verified"], true, "verify report: {report}");
    assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_io_buffers"], true,
        "verify report: {report}"
    );
}

#[test]
fn pwritev2_pipe_retry_refuses_a_replaced_descriptor() {
    let guest = compile_guest();
    let report_dir = tempfile::tempdir().expect("failed to create verification directory");
    let report_path = report_dir.path().join("verify.json");
    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "40s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "run",
            "--verify",
            "--verify-strict",
            "--allow-unsupported-syscalls",
            "--base-env=minimal",
        ])
        .arg(format!("--verify-json={}", report_path.display()))
        .arg("--")
        .arg(&guest)
        .arg("fd-replacement");
    let output = command_output(verify, "pwritev2 descriptor replacement verification");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("pwritev2-fd-replacement-ok"),
        "pwritev2 descriptor replacement probe omitted its marker\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_path).expect("descriptor replacement report was not written"),
    )
    .expect("descriptor replacement report is valid JSON");
    assert_eq!(report["verdict"], "matched", "verify report: {report}");
    assert_eq!(report["verified"], true, "verify report: {report}");
    assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
}

#[test]
fn current_position_pipe_attempts_run_through_record_mode() {
    let guest = compile_guest();
    let build_root = guest.parent().expect("p*v2 guest should have a parent");
    let recording = build_root.join("recording");
    let _ = fs::remove_dir_all(&recording);

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit_test::hermit_binary())
        .args(["--log=trace", "record", "start", "--record-timeout=30"])
        .arg("--data-dir")
        .arg(&recording)
        .arg("--")
        .arg(&guest)
        .arg("record-pipe");
    let output = command_output(record, "p*v2 pipe recording");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("preadv2-pwritev2-record-pipe-ok")
            || stderr.contains("preadv2-pwritev2-record-pipe-ok"),
        "recorded p*v2 pipe guest omitted its marker\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    for evidence in [
        "Retry #1 for blocking pipe preadv2",
        "Retry #1 for atomic blocking pipe pwritev2 after EAGAIN",
        "Retry #1 for blocking pipe pwritev2 after EAGAIN",
    ] {
        assert!(
            stderr.contains(evidence),
            "recorded p*v2 pipe trace omitted {evidence}\nstderr:\n{stderr}"
        );
    }
}
