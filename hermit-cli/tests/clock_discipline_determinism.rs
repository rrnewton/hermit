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

const CASES: &[(&str, &str, &str)] = &[
    (
        "adjtimex_deterministic.c",
        "adjtimex-ok state=5 status=64 tick=10000",
        "adjtimex",
    ),
    (
        "clock_adjtime_deterministic.c",
        "clock-adjtime-ok state=5 status=64 tick=10000",
        "clock_adjtime",
    ),
    ("syslog_deterministic.c", "syslog-ok size=0", "syslog"),
];

fn assert_syslog_interception(phase: &str, output: &Output) {
    assert!(
        output.status.success(),
        "syslog {phase} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("syslog-ok size=0"),
        "syslog {phase} did not return Detcore's fixed result"
    );

    let trace = String::from_utf8_lossy(&output.stderr);
    assert!(
        trace
            .lines()
            .any(|line| line.contains("inbound syscall: syslog(")),
        "syslog {phase} bypassed Detcore's inbound syscall handler\nstderr:\n{trace}"
    );
    assert!(
        trace
            .lines()
            .any(|line| { line.contains("finish syscall") && line.contains("syslog(") }),
        "syslog {phase} bypassed Detcore's finished syscall handler\nstderr:\n{trace}"
    );
}

fn record_and_replay_syslog(hermit: &str, guest: &Path) {
    let data_dir = tempfile::tempdir().expect("failed to create syslog recording directory");

    let record = Command::new("timeout")
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit)
        .args(["--log=trace", "record", "start", "--record-timeout=45"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .arg("--")
        .arg(guest)
        .output()
        .expect("failed to record syslog guest");
    assert_syslog_interception("recording", &record);

    let replay = Command::new("timeout")
        .args(["--kill-after", "5s", "60s"])
        .arg(hermit)
        .args(["--log=trace", "replay", "--autopilot"])
        .arg(format!("--data-dir={}", data_dir.path().display()))
        .output()
        .expect("failed to replay syslog guest");
    assert_syslog_interception("replay", &replay);
    assert_eq!(
        record.stdout, replay.stdout,
        "syslog replay output differed from the recording"
    );
}

#[test]
fn clock_discipline_and_kernel_log_are_host_independent() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-discipline-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    for (source, marker, syscall) in CASES {
        let guest = build_root.join(source.trim_end_matches(".c"));
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tests/c").join(source))
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile {source}: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {source}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&compile.stdout),
            String::from_utf8_lossy(&compile.stderr),
        );

        let trace = Command::new("timeout")
            .args(["--kill-after", "5s", "60s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=trace",
                "run",
                "--backend=ptrace",
                "--strict",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to trace {source}: {error}"));
        assert!(
            trace.status.success(),
            "{source} trace failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            trace.status,
            String::from_utf8_lossy(&trace.stdout),
            String::from_utf8_lossy(&trace.stderr),
        );
        assert!(
            String::from_utf8_lossy(&trace.stdout).contains(marker),
            "{source} omitted marker {marker}"
        );
        assert!(
            String::from_utf8_lossy(&trace.stderr)
                .contains(&format!("inbound syscall: {syscall}(")),
            "{source} trace omitted {syscall}"
        );

        let verify = Command::new("timeout")
            .args(["--kill-after", "5s", "60s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=debug",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to verify {source}: {error}"));
        assert!(
            verify.status.success(),
            "{source} verification failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            verify.status,
            String::from_utf8_lossy(&verify.stdout),
            String::from_utf8_lossy(&verify.stderr),
        );
        let combined = [verify.stdout, verify.stderr].concat();
        assert!(
            String::from_utf8_lossy(&combined).contains("Determinism verified"),
            "{source} omitted determinism marker"
        );

        if *syscall == "syslog" {
            record_and_replay_syslog(env!("CARGO_BIN_EXE_hermit"), &guest);
        }
    }
}
