// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved. Licensed under the BSD-style license in LICENSE.

//! A worker survives exec as the leader twice, retaining its clock and PMU work.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use detcore::Digest;
use hermit::canonical_verdict::ComparedLogScope;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::canonical_verdict::VerificationReport;
use regex::Regex;

use super::kvm_cancellation::bounded_command_with_timeout;
use super::kvm_cancellation::bounded_read;

const MIB: u64 = 1024 * 1024;

fn assert_pmu_handoffs(log: &str, stdout: &str) {
    let identity =
        Regex::new(r"(?m)^before round=(\d+) pid=(\d+) worker=(\d+) peer=(\d+)$").unwrap();
    let identities: Vec<_> = identity.captures_iter(stdout).collect();
    assert_eq!(identities.len(), 2, "two actual nonleader exec boundaries");
    let lines: Vec<_> = log.lines().collect();
    for (round, identity) in identities.iter().enumerate() {
        assert_eq!(identity[1].parse::<usize>().unwrap(), round);
        let leader = &identity[2];
        let worker = &identity[3];
        assert_ne!(leader, worker);
        let exec = lines
            .iter()
            .position(|line| {
                line.contains(&format!(
                    "[detcore, dtid {worker}] inbound syscall: execve("
                ))
            })
            .expect("the identified worker really called execve");
        let raw_clock = |line: &str| {
            line.split_once("local rcb clock_value ")
                .expect("actual PMU counter")
                .1
                .parse::<u64>()
                .expect("numeric PMU counter")
        };
        let before = lines[..exec]
            .iter()
            .rfind(|line| line.contains(&format!("[dtid {worker}] updated rcb clock,")))
            .expect("worker clock accounted before exec");
        let after = lines[exec + 1..]
            .iter()
            .find(|line| line.contains(&format!("[dtid {leader}] updated rcb clock,")))
            .expect("replacement image resumes clock accounting as the leader");
        assert!(raw_clock(before) > 0, "worker executed counted branches");
        assert!(
            raw_clock(after) >= raw_clock(before),
            "exec must retain the actual PMU counter: {before}\n{after}"
        );
        let logical_rcbs = |line: &str| {
            line.split_once(", rcbs: ")
                .expect("logical branch accounting")
                .1
                .split_once(',')
                .unwrap()
                .0
                .parse::<u64>()
                .expect("numeric logical branch count")
        };
        // update_logical_time_rcbs charges exactly the raw-counter delta. The
        // surviving logical clock must retain prior work without charging it
        // twice, even though its scheduler identity becomes the leader.
        assert_eq!(
            logical_rcbs(after),
            logical_rcbs(before) + raw_clock(after) - raw_clock(before),
            "exec must preserve logical branch accounting: {before}\n{after}"
        );
    }
}

pub(super) fn run() {
    run_fixture(false);
}

pub(super) fn run_exit_only() {
    run_fixture(true);
}

fn run_fixture(exit_only: bool) {
    let _lock = super::hermit_run_guard();
    let start = Instant::now();
    let remaining = || {
        Duration::from_secs(55)
            .checked_sub(start.elapsed())
            .expect("the complete regression must fit the existing 57-second test budget")
    };
    fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).expect("fixture parent");
    let prefix = if exit_only {
        "ptrace-nonleader-exec-exit-"
    } else {
        "ptrace-nonleader-exec-"
    };
    let root = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("retained fixture directory")
        .keep();
    eprintln!(
        "ptrace nonleader exec artifacts retained at {}",
        root.display()
    );
    let fixture_name = if exit_only {
        "nonleader_exec_exit.c"
    } else {
        "nonleader_exec.c"
    };
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture_name);
    fs::copy(&fixture, root.join("guest.c")).expect("retain exact fixture");
    let guest = root.join("program");
    let compile = root.join("compile");
    let status = bounded_command_with_timeout(
        Command::new("cc")
            .args([
                "-std=gnu11",
                "-O0",
                "-g",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-pthread",
            ])
            .arg(&fixture)
            .arg("-o")
            .arg(&guest),
        &compile,
        remaining(),
    );
    assert!(
        status.success(),
        "fixture compilation failed: {}",
        compile.display()
    );
    let mut previous_stdout = None;
    for pair in 0..3 {
        let directory = root.join(format!("pair-{pair}"));
        let logs = directory.join("verify-logs");
        fs::create_dir_all(&logs).unwrap();
        let report_path = directory.join("verification.json");
        let args = [
            "--log=trace",
            "run",
            "--backend=ptrace",
            "--base-env=minimal",
            // CARGO_TARGET_TMPDIR may itself be under host /tmp. Keep that
            // fixture visible just as the neighboring CLI guest tests do.
            "--tmp=/tmp",
            "--strict",
            "--epoch=2026-01-01T00:00:00.123456789+00:00",
            "--max-timeslice=200000000",
            "--verify",
            "--verify-strict",
            "--verify-json",
            report_path.to_str().unwrap(),
            "--keep-logs",
            "--verify-log-dir",
            logs.to_str().unwrap(),
            "--",
            guest.to_str().unwrap(),
        ];
        let mut command = super::hermit_command(&args);
        command.env("HERMIT_LOG_MAX_BYTES", (64 * MIB).to_string());
        let status = bounded_command_with_timeout(&mut command, &directory, remaining());
        assert_eq!(
            status.code(),
            Some(0),
            "ptrace pair {pair}: {}",
            directory.display()
        );
        let output = bounded_read(&directory.join("stdout"), MIB);
        let stdout = std::str::from_utf8(&output).expect("guest trajectory text");
        if exit_only {
            // The replacement only exits successfully. Its success must not
            // depend on an external supervisor noticing missing output; the
            // skipped-reconnect mutation must fail in the runtime itself.
            assert!(stdout.is_empty(), "the exit-only guest has no output");
        } else {
            assert_eq!(stdout.lines().count(), 23, "complete two-exec trajectory");
            assert_eq!(stdout.matches("sample round=").count(), 16);
            assert_eq!(stdout.matches("gone=ESRCH").count(), 2);
            assert_eq!(stdout.matches(" running\n").count(), 2);
            assert!(stdout.ends_with("nonleader-exec-ok rounds=2 final=73 reaped=once\n"));
        }
        if let Some(previous) = &previous_stdout {
            assert_eq!(
                &output, previous,
                "all three full nanosecond trajectories agree"
            );
        }
        let report = VerificationReport::from_current_json_slice(&bounded_read(&report_path, MIB))
            .expect("complete current typed verification receipt");
        report
            .require_canonical_match()
            .expect("nonempty canonical INFO match");
        report
            .require_exact_output_match()
            .expect("exact status/stdout/stderr match");
        assert_eq!(report.guest_exit_code, Some(0));
        assert!(report.guest_signal.is_none());
        let policy = report.comparison.as_ref().unwrap();
        assert_eq!(policy.display_name.as_deref(), Some("BitwiseInfoV1"));
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
        let counts = report.compared_log_messages.as_ref().unwrap();
        assert_eq!(counts.left, counts.right);
        let outputs = report.compared_outputs.as_ref().unwrap();
        for operand in [&outputs.left, &outputs.right] {
            assert_eq!(operand.exit_code, Some(0));
            assert!(operand.signal.is_none());
            assert_eq!(operand.stdout_bytes, output.len() as u64);
            assert_eq!(operand.stdout_sha256, Digest::new(&output).to_string());
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
            assert_eq!(matches.len(), 1, "one complete retained log per execution");
            let log = String::from_utf8(bounded_read(&matches[0], 64 * MIB)).unwrap();
            if !exit_only {
                assert_pmu_handoffs(&log, stdout);
            }
        }
        previous_stdout = Some(output);
        eprintln!("ptrace nonleader exec pair {pair}: two full canonical executions");
    }
}
