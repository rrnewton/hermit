/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Limited regression for the shared physical launch migration: real PID and
//! UTS separation, the private init, and a fresh /tmp. Every other required
//! isolation assertion is retained in the same report and remains independently
//! required by backend_isolation.rs and the full standalone matrix.

#[path = "common/bounded_command.rs"]
pub mod command_capture;
#[path = "../../tests/isolation/driver.rs"]
pub mod driver;
#[path = "common/hermit_binary.rs"]
mod hermit_binary;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use driver::Cell;
use driver::conformance::NamespacePolicy;
use driver::conformance::Verdict;

fn capture_command(
    command: &mut Command,
    directory: &Path,
    wall: Duration,
    stream_bytes: u64,
) -> driver::Capture {
    let receipt = command_capture::run(
        command,
        directory,
        command_capture::Limits {
            wall,
            stdout: stream_bytes,
            stderr: stream_bytes,
        },
        || {},
    );
    driver::Capture {
        status: receipt.status,
        stdout_read: receipt.stdout_read,
        stdout_written: receipt.stdout_written,
        stderr_read: receipt.stderr_read,
        stderr_written: receipt.stderr_written,
        stdout_eof: receipt.stdout_eof,
        stderr_eof: receipt.stderr_eof,
        reaped: receipt.reaped,
        cleanup_complete: receipt.cleanup_complete,
        elapsed: receipt.elapsed,
        failure: receipt.failure,
    }
}

fn launch_migration(backend: &str) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let artifacts = driver::integration_artifacts(repository, "launch", backend)
        .expect("declared artifact input");
    let prepared = driver::Prepared::build_integration(
        repository,
        hermit_binary::hermit_binary(),
        &artifacts,
        capture_command,
        Path::new("hermit-cli/tests/backend_isolation_launch.rs"),
    )
    .expect("required isolation fixture admission failed: no_result");
    let result = prepared.collect(Cell {
        backend: backend.to_owned(),
        root: "host".to_owned(),
        path: "run-raw".to_owned(),
        policy: NamespacePolicy::Private,
    });
    let failures: Vec<_> = result
        .assertions
        .iter()
        .filter(|row| {
            (driver::LAUNCH_MIGRATION_ASSERTIONS.contains(&row.name.as_str())
                || row.name == "execution.cli_exit"
                || row.name == "receipt.input_stability"
                || row.name == "receipt.executor_boundary"
                || row.name == "receipt.typed_report")
                && row.verdict != Verdict::Match
        })
        .map(|row| format!("{} {} {}", row.name, row.verdict.name(), row.detail))
        .collect();
    assert!(
        result.launch_migration_matches(),
        "{backend} host/run-raw PID/UTS/private-tmp launch regression failed: {failures:?}; full required report: {}",
        result.directory.join("assertions.json").display()
    );
}

#[test]
fn ptrace_host_pid_uts_and_private_tmp_launch_contract() {
    launch_migration("ptrace");
}

// Enabling the DBT feature requests the real Detcore DBT launch. Missing
// installed resources fail that request; they never select a different backend.
#[cfg(feature = "dbt")]
#[test]
fn dbt_host_pid_uts_and_private_tmp_launch_contract() {
    launch_migration("dbt");
}

fn capture_control_directory(name: &str) -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("control timestamp")
        .as_nanos();
    let directory = repository.join("ignored").join(format!(
        "backend-isolation-capture-{name}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).expect("owned capture control directory");
    directory
}

#[test]
fn complete_capture_retains_both_streams_and_actual_status() {
    let directory = capture_control_directory("complete");
    let receipt = command_capture::run(
        Command::new("sh").args(["-c", "printf stdout; printf stderr >&2"]),
        &directory,
        command_capture::Limits {
            wall: Duration::from_secs(2),
            stdout: 32,
            stderr: 32,
        },
        || {},
    );
    std::fs::write(directory.join("receipt"), format!("{receipt:#?}\n")).expect("control receipt");
    assert!(receipt.complete(), "{receipt:?}");
    assert!(receipt.status.unwrap().success());
    assert_eq!(std::fs::read(directory.join("stdout")).unwrap(), b"stdout");
    assert_eq!(std::fs::read(directory.join("stderr")).unwrap(), b"stderr");
    assert_eq!((receipt.stdout_read, receipt.stderr_read), (6, 6));
}

#[test]
fn overflowing_capture_retains_prefix_and_cannot_complete() {
    let directory = capture_control_directory("overflow");
    let receipt = command_capture::run(
        Command::new("sh").args(["-c", "printf 123456789"]),
        &directory,
        command_capture::Limits {
            wall: Duration::from_secs(2),
            stdout: 4,
            stderr: 32,
        },
        || {},
    );
    std::fs::write(directory.join("receipt"), format!("{receipt:#?}\n")).expect("control receipt");
    assert!(!receipt.complete());
    assert!(receipt.reaped && receipt.cleanup_complete);
    assert_eq!(std::fs::read(directory.join("stdout")).unwrap(), b"1234");
    assert_eq!((receipt.stdout_read, receipt.stdout_written), (9, 4));
    assert!(receipt.failure.unwrap().contains("output bound"));
}

#[test]
fn exited_child_with_descendant_pipe_is_not_complete_capture() {
    let directory = capture_control_directory("descendant-pipe");
    let receipt = command_capture::run(
        Command::new("sh").args(["-c", "sleep 10 & exit 0"]),
        &directory,
        command_capture::Limits {
            wall: Duration::from_secs(1),
            stdout: 32,
            stderr: 32,
        },
        || {},
    );
    std::fs::write(directory.join("receipt"), format!("{receipt:#?}\n")).expect("control receipt");
    assert!(!receipt.complete());
    assert!(
        receipt.status.unwrap().success(),
        "direct child exited zero before its descendant closed the pipe"
    );
    assert!(receipt.reaped && receipt.cleanup_complete);
    assert!(receipt.failure.unwrap().contains("wall limit"));
}
