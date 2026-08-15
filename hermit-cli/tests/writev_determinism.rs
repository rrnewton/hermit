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
use std::process::Stdio;

use nix::fcntl::OFlag;
use nix::unistd::pipe2;

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
fn writev_uses_fd_aware_scheduling_and_verifies() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("writev-determinism");
    fs::create_dir_all(&build_root).expect("failed to create writev guest build directory");
    let guest = build_root.join("writev_determinism");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/writev_determinism.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "writev guest compilation");

    // Both executions receive a duplicate of the same host pipe as standard input. This
    // compares pass-through behavior on one pipe object instead of comparing independently
    // selected capacities from two newly created pipes. O_CLOEXEC keeps the parent copies
    // out of both children; Stdio installs only the requested duplicate as fd 0.
    let (capacity_read, capacity_write) =
        pipe2(OFlag::O_CLOEXEC).expect("failed to create inherited capacity pipe");
    let mut native_capacity = Command::new(&guest);
    native_capacity.arg("pipe-capacity").stdin(Stdio::from(
        capacity_read
            .try_clone()
            .expect("failed to duplicate native capacity pipe"),
    ));
    let native_capacity_output = command_output(native_capacity, "native pipe capacity report");
    let native_capacity_stdout = String::from_utf8_lossy(&native_capacity_output.stdout);
    assert!(
        native_capacity_stdout.contains("pipe-max-size=")
            && native_capacity_stdout.contains("get=")
            && native_capacity_stdout.contains("set-current="),
        "native pipe capacity report omitted successful F_GETPIPE_SZ/F_SETPIPE_SZ evidence:\n\
         {native_capacity_stdout}",
    );

    let mut host_backed_capacity = Command::new("timeout");
    host_backed_capacity
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--no-sequentialize-threads",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("pipe-capacity")
        .stdin(Stdio::from(
            capacity_read
                .try_clone()
                .expect("failed to duplicate Hermit capacity pipe"),
        ));
    let host_backed_capacity_output = command_output(
        host_backed_capacity,
        "non-sequentialized pipe capacity report",
    );
    assert_eq!(
        host_backed_capacity_output.stdout,
        native_capacity_output.stdout,
        "non-sequentialized Hermit did not preserve the host pipe limit and F_SETPIPE_SZ behavior\n\
         native stdout:\n{}\nHermit stdout:\n{}\nHermit stderr:\n{}",
        native_capacity_stdout,
        String::from_utf8_lossy(&host_backed_capacity_output.stdout),
        String::from_utf8_lossy(&host_backed_capacity_output.stderr),
    );
    drop(capacity_read);
    drop(capacity_write);

    for (mode, diagnostic) in [
        (
            "inherited-pipe-get",
            "cannot expose host-selected capacity for inherited pipe fd 0",
        ),
        (
            "inherited-pipe-set",
            "cannot resize inherited pipe fd 0 from host-selected state",
        ),
    ] {
        let mut inherited = Command::new("timeout");
        inherited
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
            .arg(&guest)
            .arg(mode)
            .stdin(Stdio::piped());
        let output = inherited
            .output()
            .unwrap_or_else(|error| panic!("failed to start inherited-pipe {mode}: {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success() && stderr.contains(diagnostic),
            "inherited-pipe {mode} was not refused by the typed capacity path\n\
             status: {}\nstdout:\n{}\nstderr:\n{stderr}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
        );
    }

    let mut trace = Command::new("timeout");
    trace
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=trace",
            "run",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("4096")
        .stdin(Stdio::piped());
    let trace_output = command_output(trace, "strict writev trace");
    let trace_stdout = String::from_utf8_lossy(&trace_output.stdout);
    let trace_stderr = String::from_utf8_lossy(&trace_output.stderr);
    assert!(
        trace_stdout.contains("writev-determinism-ok"),
        "writev guest omitted its success marker\nstdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );
    assert!(
        trace_stderr.contains("inbound syscall: writev")
            && trace_stderr.contains(
                "NonblockableSyscall: converting to nonblocking syscall (internal polling): writev",
            )
            && trace_stderr.contains("Retry #1 for atomic blocking pipe writev after Err(EAGAIN)"),
        "writev did not reach typed dispatch and internal-fd scheduling\n\
         stdout:\n{trace_stdout}\nstderr:\n{trace_stderr}",
    );

    let strict_report = build_root.join("strict-verify.json");
    let _ = fs::remove_file(&strict_report);
    let mut strict_verify = Command::new("timeout");
    strict_verify
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--verify",
            "--verify-strict",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env=minimal",
        ])
        .arg("--verify-json")
        .arg(&strict_report)
        .arg("--")
        .arg(&guest)
        .arg("4096");
    command_output(strict_verify, "canonical strict writev verification");
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&strict_report).expect("strict verification omitted its JSON report"),
    )
    .expect("strict verification report was not valid JSON");
    assert_eq!(report["verdict"], serde_json::json!("matched"));
    assert_eq!(report["verified"], serde_json::json!(true));
    assert_eq!(report["bitwise_parity"], serde_json::json!(true));
    assert_eq!(
        report["comparison"]["strictness"],
        serde_json::json!("canonical")
    );
    for side in ["left", "right"] {
        assert!(
            report["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "canonical verification reported no compared {side} messages: {report}"
        );
    }

    // This compatibility case intentionally exercises the Stripped comparator. It is not
    // evidence for bitwise parity and its JSON verdict must keep saying so.
    let passthru_report = build_root.join("passthru-verify.json");
    let _ = fs::remove_file(&passthru_report);
    let mut passthru_verify = Command::new("timeout");
    passthru_verify
        .args(["--kill-after", "5s", "30s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--verify",
            "--passthru-opt",
            "--base-env=minimal",
        ])
        .arg("--verify-json")
        .arg(&passthru_report)
        .arg("--")
        .arg(&guest);
    command_output(passthru_verify, "Stripped passthru-opt writev verification");
    let passthru: serde_json::Value = serde_json::from_slice(
        &fs::read(&passthru_report).expect("passthru verification omitted its JSON report"),
    )
    .expect("passthru verification report was not valid JSON");
    assert_eq!(passthru["verified"], serde_json::json!(true));
    assert_eq!(passthru["bitwise_parity"], serde_json::json!(false));
    assert_eq!(
        passthru["comparison"]["strictness"],
        serde_json::json!("stripped")
    );

    // Exercise record mode on the smallest pipe-retry workload separately before the
    // full fixed-capacity workload is recorded and replayed below.
    let pipe_recording = build_root.join("pipe-recording");
    let _ = fs::remove_dir_all(&pipe_recording);
    let mut pipe_record = Command::new("timeout");
    pipe_record
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log=off", "record", "start", "--record-timeout=30"])
        .arg("--data-dir")
        .arg(&pipe_recording)
        .arg("--")
        .arg(&guest)
        .arg("record-pipe");
    let pipe_record_output = command_output(pipe_record, "writev pipe recording");
    let pipe_record_stdout = String::from_utf8_lossy(&pipe_record_output.stdout);
    let pipe_record_stderr = String::from_utf8_lossy(&pipe_record_output.stderr);
    assert!(
        pipe_record_stdout.contains("writev-determinism-ok")
            || pipe_record_stderr.contains("writev-determinism-ok"),
        "recorded writev pipe workload omitted its success marker\n\
         stdout:\n{pipe_record_stdout}\nstderr:\n{pipe_record_stderr}",
    );

    let recording = build_root.join("recording");
    let _ = fs::remove_dir_all(&recording);
    let mut record = Command::new("timeout");
    record
        .args(["--kill-after", "5s", "60s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "record",
            "start",
            "--verify",
            "--record-timeout=30",
        ])
        .arg("--data-dir")
        .arg(&recording)
        .arg("--")
        .arg(&guest)
        .arg("4096");
    let record_output = command_output(record, "writev record/replay verification");
    let record_stdout = String::from_utf8_lossy(&record_output.stdout);
    let record_stderr = String::from_utf8_lossy(&record_output.stderr);
    assert!(
        record_stdout.contains("Success: replay matched recording")
            || record_stderr.contains("Success: replay matched recording"),
        "Hermit omitted its replay-match marker\n\
         stdout:\n{record_stdout}\nstderr:\n{record_stderr}",
    );
}
