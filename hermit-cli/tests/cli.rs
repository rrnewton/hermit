/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::process::Command;
use std::process::Output;

fn hermit(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("failed to run hermit with {args:?}: {error}"))
}

fn assert_success(output: &Output, args: &[&str]) {
    assert!(
        output.status.success(),
        "hermit {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("hermit stdout should be UTF-8")
}

#[test]
fn top_level_help_lists_user_facing_commands() {
    let args = ["--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("Usage: hermit [OPTIONS] <COMMAND>"));
    for command in ["run", "record", "replay", "log-diff", "analyze"] {
        assert!(help.contains(command), "missing {command:?} in:\n{help}");
    }
}

#[test]
fn replay_help_accepts_optional_recording_id() {
    let args = ["replay", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("Usage: hermit replay [OPTIONS] [ID]"));
    assert!(help.contains("--autopilot"));
    assert!(help.contains("--data-dir <DIR>"));
    assert!(help.contains("--gdbserver-port"));
}

#[test]
fn run_help_exposes_determinism_modes() {
    let args = ["run", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    for option in [
        "--sequentialize-threads",
        "--chaos",
        "--verify",
        "--record-preemptions",
        "--replay-preemptions-from",
        "--preemption-timeout",
        "--no-namespace",
        "--core-only",
    ] {
        assert!(help.contains(option), "missing {option:?} in run help");
    }
}

#[test]
fn incompatible_run_modes_fail_during_argument_parsing() {
    let args = ["run", "--namespace-only", "--chaos", "/bin/true"];
    let output = hermit(&args);

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("--namespace-only"),
        "unexpected error:\n{stderr}"
    );
    assert!(stderr.contains("--chaos"), "unexpected error:\n{stderr}");
    assert!(
        stderr.contains("cannot be used with"),
        "unexpected error:\n{stderr}"
    );
}

#[test]
fn no_namespace_rejects_container_only_options() {
    let cases = [
        "--namespace-only",
        "--analyze-networking",
        "--mount=type=bind,source=/tmp,target=/tmp",
        "--bind=/tmp",
    ];

    for incompatible in cases {
        let args = ["run", "--no-namespace", incompatible, "/bin/true"];
        let output = hermit(&args);
        assert_eq!(
            output.status.code(),
            Some(2),
            "hermit {args:?} unexpectedly ran"
        );

        let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
        assert!(
            stderr.contains("--no-namespace"),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains(incompatible.split_once("=").map_or(incompatible, |x| x.0)),
            "unexpected error:\n{stderr}"
        );
        assert!(
            stderr.contains("cannot be used with"),
            "unexpected error:\n{stderr}"
        );
    }
}

#[test]
fn no_namespace_runs_without_container_setup() {
    let args = [
        "run",
        "--no-namespace",
        "--preemption-timeout=disabled",
        "--",
        "/bin/echo",
        "hello",
    ];
    let output = hermit(&args);
    assert_success(&output, &args);

    assert_eq!(stdout(&output), "hello\n");
    let stderr = String::from_utf8(output.stderr).expect("hermit stderr should be UTF-8");
    assert!(
        stderr.contains("WARNING: --no-namespace"),
        "unexpected stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("less deterministic"),
        "unexpected stderr:\n{stderr}"
    );
}

#[test]
fn record_help_lists_management_commands() {
    let args = ["record", "--help"];
    let output = hermit(&args);
    assert_success(&output, &args);
    let help = stdout(&output);

    assert!(help.contains("Usage: hermit record <COMMAND>"));
    for command in ["list", "rm", "clean", "start"] {
        assert!(help.contains(command), "missing {command:?} in:\n{help}");
    }
}

#[test]
fn record_list_json_reports_an_empty_inventory() {
    let data_dir = tempfile::tempdir().expect("failed to create recording data directory");
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args(["record", "list", "--json", "--data-dir"])
        .arg(data_dir.path())
        .output()
        .expect("failed to run hermit record list");
    assert!(
        output.status.success(),
        "hermit record list failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let value: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("record list should emit JSON");
    assert_eq!(value, serde_json::json!([]));
}
