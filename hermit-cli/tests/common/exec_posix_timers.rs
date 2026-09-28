// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Linux deletes POSIX timers at successful exec, but preserves ITIMER_REAL.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;
const EXPECTED: &[u8] = b"PASS exec deleted POSIX timers; failed exec retained timers; ITIMER_REAL survived; clock advanced\n";

pub(super) fn run(backend: &str) {
    let _lock = super::hermit_run_guard();
    if backend == "kvm" {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .expect("selected KVM exec timer regression requires /dev/kvm; absence is not a pass");
    }
    let root = tempfile::Builder::new()
        .prefix(&format!("exec-posix-timers-{backend}-"))
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained exec timer fixture directory")
        .keep();
    eprintln!(
        "{backend} exec timer artifacts retained at {}",
        root.display()
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exec_posix_timers.c");
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture");
    let guest = root.join("program");
    let compile = root.join("compile");
    let status = bounded_command_with_timeout(
        Command::new("cc")
            .args([
                "-std=gnu11",
                "-O2",
                "-g",
                "-Wall",
                "-Wextra",
                "-Wpedantic",
                "-Wformat=2",
                "-Werror",
                "-fno-pie",
                "-no-pie",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &compile,
        Duration::from_secs(20),
    );
    assert!(status.success(), "exec timer fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());

    let directory = root.join("verify");
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).expect("retained verification logs");
    let report_path = directory.join("verification.json");
    let args = [
        "--log=info",
        "run",
        "--base-env=minimal",
        "--backend",
        backend,
        "--strict",
        "--verify-strict",
        "--verify",
        "--verify-json",
        report_path.to_str().unwrap(),
        "--keep-logs",
        "--verify-log-dir",
        logs.to_str().unwrap(),
        "--mount=type=tmpfs,target=/test",
        "--workdir=/test",
        "--env=LC_ALL=C",
        "--env=TZ=UTC",
        "--",
        guest.to_str().unwrap(),
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let status = bounded_command_with_timeout(&mut command, &directory, Duration::from_secs(57));
    assert_eq!(status.code(), Some(0), "{backend} exec timer guest status");
    assert_eq!(
        bounded_read(&directory.join("stdout"), 64 * MIB),
        EXPECTED,
        "{backend} exec timer guest must complete every assertion"
    );
    let report = VerificationReport::from_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact two-run status/stdout/stderr match");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    let policy = report.comparison.as_ref().unwrap();
    assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert_eq!(policy.compare_io_buffers, Some(true));
    assert_eq!(policy.virtualize_time, Some(true));
    assert_eq!(policy.strip_lines, Some(false));
    assert_eq!(policy.canonicalize_addresses, Some(true));
    assert_eq!(policy.full_trace, Some(true));
    assert_eq!(policy.exact_remainder, Some(true));
    assert_eq!(policy.ignore_lines, Some(false));
    assert_eq!(policy.skip_commit, Some(false));
    assert_eq!(policy.skip_detlog, Some(false));
    assert_eq!(policy.log_scope, Some(ComparedLogScope::Info));
    let outputs = report.compared_outputs.as_ref().unwrap();
    for operand in [&outputs.left, &outputs.right] {
        assert_eq!(operand.exit_code, Some(0));
        assert!(operand.signal.is_none());
        assert_eq!(operand.stdout_bytes, EXPECTED.len() as u64);
        assert_eq!(operand.stdout_sha256, Digest::new(EXPECTED).to_string());
        assert_eq!(operand.stderr_bytes, 0);
        assert_eq!(operand.stderr_sha256, Digest::new(b"").to_string());
    }
    for prefix in ["run1_log_", "run2_log_"] {
        let matches: Vec<_> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect();
        assert_eq!(matches.len(), 1, "one retained log per actual guest");
        assert!(!bounded_read(&matches[0], 64 * MIB).is_empty());
    }
}
