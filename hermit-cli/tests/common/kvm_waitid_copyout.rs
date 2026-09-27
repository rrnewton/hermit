// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Exercise raw waitid through the real KVM CLI and Detcore. The guest emits
//! complete caller arenas because the generic waitid IO hook does not observe
//! every error write, padding byte, or overlapping output. Exact stdout plus
//! the full canonical INFO comparison covers these specific observations.
//! Immediate logical-child and CPU accounting invariants have component tests;
//! eventual guest ECHILD by itself would not establish their ordering.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::canonical_verdict::VerificationReport;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

const TERMINAL_CASES: &[&str] = &[
    "writable",
    "null-info",
    "null-usage",
    "both-null",
    "protected-unused-tail",
    "read-only-info",
    "inaccessible-info",
    "read-only-usage",
    "inaccessible-usage",
    "usage-prefix-1",
    "usage-prefix-8",
    "usage-prefix-64",
    "first-scalar-split",
    "pid-scalar-split",
    "status-scalar-split",
    "aliased-outputs",
    "wnowait-writable",
    "wnowait-info-fault",
    "wnowait-usage-fault",
    "high-option-register-bits",
    "p-all-ignores-id",
    "fault-preserves-sibling",
];

const ERROR_CASES: &[&str] = &[
    "already-reaped",
    "invalid-options",
    "invalid-options-null",
    "invalid-options-protected",
    "pid-zero",
    "pid-zero-null",
    "pid-zero-protected",
    "pid-negative",
    "pgid-negative",
    "invalid-selector",
    "live-no-event",
    "live-no-event-null",
    "live-no-event-protected",
    "live-usage-protected",
    "live-high-options",
    "live-p-all-ignores-id",
    "interrupted-protected-info",
];

fn assert_observations(stdout: &[u8], mode: &str, expected: &[&str], children: u64) {
    let text = std::str::from_utf8(stdout).expect("complete fixture UTF-8");
    assert!(text.ends_with('\n'));
    let rows: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).expect("one complete JSON observation per line"))
        .collect();
    let mut names = BTreeSet::new();
    let mut checks = 0;
    let mut summaries = 0;
    for (index, row) in rows.iter().enumerate() {
        match row["type"].as_str().expect("observation type") {
            "case" => {
                let name = row["name"].as_str().expect("case name");
                assert!(names.insert(name), "duplicate case {name}");
                for (before, after) in [("info_before", "info_after"), ("aux_before", "aux_after")]
                {
                    let before = row[before].as_str().expect("entire before arena");
                    let after = row[after].as_str().expect("entire after arena");
                    assert!(!before.is_empty() && before.len() <= 4 * 65536);
                    assert_eq!(before.len(), after.len());
                    assert_eq!(before.len() % 2, 0);
                    assert!(before.as_bytes().chunks_exact(2).all(|pair| pair == b"a5"));
                    assert!(after.bytes().all(|byte| byte.is_ascii_hexdigit()));
                }
                assert!(row["rc"].is_i64());
                assert!(row["errno"].is_i64());
            }
            "check" => {
                checks += 1;
                let arena = row["arena"].as_str().expect("full guarded follow-up arena");
                assert_eq!(arena.len(), 320);
                assert!(arena.bytes().all(|byte| byte.is_ascii_hexdigit()));
            }
            "retained" => {
                assert_eq!(row["rc"], 0);
                assert_eq!(row["errno"], 0);
            }
            "summary" => {
                summaries += 1;
                assert_eq!(
                    index + 1,
                    rows.len(),
                    "summary must be the final observation"
                );
                assert_eq!(row["mode"], mode);
                assert_eq!(row["cases"].as_u64(), Some(expected.len() as u64));
                assert_eq!(row["children"].as_u64(), Some(children));
                assert_eq!(row["passed"], true);
                assert!(row["assertions"].as_u64().unwrap() > expected.len() as u64);
            }
            other => panic!("unexpected observation {other}"),
        }
    }
    let expected_names: BTreeSet<_> = expected.iter().copied().collect();
    assert_eq!(names, expected_names);
    assert!(
        checks > 0,
        "actual child peeks and reap checks must execute"
    );
    assert_eq!(summaries, 1);
}

pub(super) fn run(mode: &str) {
    let (expected, children) = match mode {
        "terminal" => (TERMINAL_CASES, 23),
        "errors" => (ERROR_CASES, 2),
        _ => panic!("unknown waitid fixture mode"),
    };
    let _lock = super::hermit_run_guard();
    // One shared deadline includes fixture compilation and the two-guest
    // verification, inside the existing 57-second outer test timeout.
    let deadline = Instant::now() + Duration::from_secs(50);
    let _kvm = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .expect("selected waitid regression requires /dev/kvm; absence is not a pass");
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let root = tempfile::Builder::new()
        .prefix("kvm-waitid-copyout-")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "KVM waitid copyout artifacts retained at {}",
        root.display()
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kvm_waitid_copyout.c");
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture source");
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
    assert!(status.success(), "waitid fixture compilation failed");
    assert!(bounded_read(&compile.join("stdout"), 64 * MIB).is_empty());
    assert!(bounded_read(&compile.join("stderr"), 16 * MIB).is_empty());
    let bytes = bounded_read(&guest, 16 * MIB);
    let elf = goblin::elf::Elf::parse(&bytes).expect("actual fixture ELF");
    assert_eq!(elf.header.e_type, goblin::elf::header::ET_EXEC);
    assert_eq!(elf.header.e_machine, goblin::elf::header::EM_X86_64);

    let directory = root.join(mode);
    let logs = directory.join("verify-logs");
    fs::create_dir_all(&logs).unwrap();
    let report_path = directory.join("verification.json");
    let args = [
        "--log=info",
        "run",
        "--base-env=minimal",
        "--backend=kvm",
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
        mode,
    ];
    let mut command = super::hermit_command(&args);
    command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .expect("test deadline");
    let status = bounded_command_with_timeout(&mut command, &directory, remaining);
    assert_eq!(status.code(), Some(0), "both guests must actually succeed");
    let stdout = bounded_read(&directory.join("stdout"), 64 * MIB);
    assert_observations(&stdout, mode, expected, children);
    let report = VerificationReport::from_current_json_slice(&bounded_read(&report_path, 16 * MIB))
        .expect("complete current typed verification report");
    report
        .require_canonical_match()
        .expect("full nonempty canonical INFO match");
    report
        .require_exact_output_match()
        .expect("exact output/status parity");
    assert_eq!(report.guest_exit_code, Some(0));
    assert!(report.guest_signal.is_none());
    let counts = report.compared_log_messages.unwrap();
    assert!(counts.left > 0);
    assert_eq!(counts.left, counts.right);
    let policy = report.comparison.as_ref().unwrap();
    assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
    assert!(policy.compare_logs);
    assert_eq!(policy.record_envelope, RecordEnvelopeReport::AllRecordsV1);
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
    assert_eq!(
        policy.stripped_prefixes.as_deref(),
        Some(["real-wall-clock-prefix/v1".to_owned()].as_slice())
    );
    assert_eq!(
        policy.canonicalizations.as_deref(),
        Some(["host-address-to-first-appearance-ordinal/v1".to_owned()].as_slice())
    );
    let outputs = report.compared_outputs.as_ref().unwrap();
    for operand in [&outputs.left, &outputs.right] {
        assert_eq!(operand.exit_code, Some(0));
        assert!(operand.signal.is_none());
        assert_eq!(operand.stdout_bytes, stdout.len() as u64);
        assert_eq!(operand.stdout_sha256, Digest::new(&stdout).to_string());
        assert_eq!(operand.stderr_bytes, 0);
        assert_eq!(operand.stderr_sha256, Digest::new(b"").to_string());
    }
    for prefix in ["run1_log_", "run2_log_"] {
        let paths: Vec<_> = fs::read_dir(&logs)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(prefix)
            })
            .collect();
        assert_eq!(paths.len(), 1, "one retained full log per actual guest");
        assert!(!bounded_read(&paths[0], 64 * MIB).is_empty());
    }
    assert!(
        Instant::now() < deadline,
        "complete test stays inside its shared bound"
    );
}
