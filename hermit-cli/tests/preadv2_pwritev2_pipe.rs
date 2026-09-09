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
use std::sync::Mutex;

static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

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

fn snapshot_addresses(stdout: &str, marker: &str) -> [String; 3] {
    let line = stdout
        .lines()
        .find(|line| line.starts_with(marker))
        .unwrap_or_else(|| panic!("guest omitted {marker:?}\nstdout:\n{stdout}"));
    let mut fields = line.split_whitespace().skip(1).map(|field| {
        field
            .split_once('=')
            .unwrap_or_else(|| panic!("malformed address field {field:?} in {line:?}"))
            .1
            .to_owned()
    });
    let addresses = std::array::from_fn(|_| {
        fields
            .next()
            .unwrap_or_else(|| panic!("too few addresses in {line:?}"))
    });
    assert!(fields.next().is_none(), "too many addresses in {line:?}");
    addresses
}

fn assert_snapshot_iobuf_evidence(stdout: &str, stderr: &str) {
    let [pread_first, pread_second, pread_poison] =
        snapshot_addresses(stdout, "preadv2-iobuf-addresses");
    let pread_lines = stderr
        .lines()
        .filter(|line| line.contains("[iobuf]") && line.contains(" preadv2 in fd="))
        .collect::<Vec<_>>()
        .join("\n");
    for (address, byte) in [(pread_first, b'A'), (pread_second, b'B')] {
        let expected = format!(
            "{address}+1->{}",
            detcore::Digest::new(std::slice::from_ref(&byte))
        );
        assert!(
            pread_lines.contains(&expected),
            "preadv2 evidence omitted original extent/digest {expected}\n{pread_lines}"
        );
    }
    assert!(
        !pread_lines.contains(&format!("{pread_poison}+")),
        "preadv2 evidence followed the caller-mutated poison iovec\n{pread_lines}"
    );

    let [pwrite_first, pwrite_second, pwrite_poison] =
        snapshot_addresses(stdout, "pwritev2-iobuf-addresses");
    let pwrite_lines = stderr
        .lines()
        .filter(|line| line.contains("[iobuf]") && line.contains(" pwritev2 out fd="))
        .collect::<Vec<_>>()
        .join("\n");
    for (address, byte) in [(pwrite_first, b'A'), (pwrite_second, b'B')] {
        let expected = format!(
            "{address}+2048->{}",
            detcore::Digest::new(vec![byte; 2048].as_slice())
        );
        assert!(
            pwrite_lines.contains(&expected),
            "pwritev2 evidence omitted original extent/digest {expected}\n{pwrite_lines}"
        );
    }
    assert!(
        !pwrite_lines.contains(&format!("{pwrite_poison}+")),
        "pwritev2 evidence followed the caller-mutated poison iovec\n{pwrite_lines}"
    );
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
        "vectored-descriptor-matrix-ok",
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
        "Retry #1 for blocking socket readv",
        "Retry #1 for blocking eventfd readv",
        "Retry #1 for blocking socket preadv2",
        "Retry #1 for blocking eventfd preadv2",
        "Retry #1 for blocking socket writev after EAGAIN",
        "Retry #1 for blocking eventfd writev after EAGAIN",
        "Retry #1 for blocking socket pwritev2 after EAGAIN",
        "Retry #1 for blocking eventfd pwritev2 after EAGAIN",
        " preadv2 in fd=",
        " pwritev2 out fd=",
    ] {
        assert!(
            trace_stderr.contains(evidence),
            "p*v2 trace omitted {evidence}\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
        );
    }
    assert_snapshot_iobuf_evidence(&trace_stdout, &trace_stderr);

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
    let report_dir = tempfile::tempdir().expect("failed to create record verification directory");
    let report_path = report_dir.path().join("verify.json");

    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "record",
            "start",
            "--verify",
            "--verify-strict",
            "--record-timeout=30",
        ])
        .arg("--data-dir")
        .arg(&recording)
        .arg(format!("--verify-json={}", report_path.display()))
        .arg("--")
        .arg(&guest)
        .arg("record-pipe");
    let output = command_output(record, "p*v2 pipe recording");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("Success: replay matched recording")
            || stderr.contains("Success: replay matched recording"),
        "p*v2 record command did not perform a successful replay comparison\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_path).expect("record verification report was not written"),
    )
    .expect("record verification report is valid JSON");
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
    assert_eq!(
        report["comparison"]["record_envelope"], "all_records_v1",
        "verify report: {report}"
    );
}

#[test]
fn pwritev2_partial_progress_is_interrupted_on_a_non_root_writer() {
    let guest = compile_guest();
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "30s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=trace",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("partial-signal");
    let output = command_output(command, "pwritev2 partial-progress signal interruption");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("pwritev2-partial-signal-ok:4096"),
        "pwritev2 did not return its exact partial byte count after sibling signal\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
    assert!(
        stderr.contains("WaitidSignals") && stderr.contains("pwritev2"),
        "trace did not prove the parked pwritev2 was resumed through the signal path\n\
         stderr:\n{stderr}",
    );
}

#[test]
#[ignore = "requires combined PR529 per-thread scratch + PR538 vectored support"]
fn kvm_current_position_vectored_descriptor_matrix_requires_pr529_and_pr538() {
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let guest = compile_guest();
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "run",
            "--backend=kvm",
            "--strict",
            "--verify",
            "--verify-strict",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("matrix");
    let output = command_output(command, "KVM vectored descriptor matrix");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("vectored-descriptor-matrix-ok"));
}
