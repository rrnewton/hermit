/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for Linux's robust-futex owner-death protocol.
//!
//! `tests/bin/robust_futex_test.c` parks a waiter on a `PTHREAD_MUTEX_ROBUST`
//! mutex, sets `FUTEX_WAITERS`, and then lets the owning thread exit while still
//! holding the mutex. Linux marks the futex word `FUTEX_OWNER_DIED` and wakes one
//! waiter, which is how the waiter's `pthread_mutex_lock` returns `EOWNERDEAD`.
//!
//! Before Detcore modeled that protocol, the precise futex model parked the
//! waiter in the scheduler's own pool where the kernel's internal wake could not
//! reach it: the run hung and then aborted with
//! "Deadlock detected: thread(s) waiting on futex, but no runnable threads left".
//!
//! This test asserts full L2 canonical parity (`--verify-strict --verify-json`
//! with `bitwise_parity: true`), which is strictly stronger than the E2E
//! harness's `verify` mode: that mode runs plain `--verify` and therefore only
//! establishes the stripped comparison.

use std::fs;
use std::fs::File;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// Captured child output, always via files rather than pipes.
struct Captured {
    stdout: String,
    stderr: String,
}

/// Run `command` to completion, capturing its streams into `stdout_path` and
/// `stderr_path`.
///
/// Deliberately *not* `Command::output()`. When a Hermit run wedges, its
/// scheduler panics but leaves the guest in a ptrace stop, and `timeout` kills
/// only Hermit itself. An orphaned stopped tracee keeps an inherited pipe open
/// forever, so `output()` would block long past the `timeout` bound and turn a
/// clean regression failure into a hung test. Redirecting to files means the
/// wait ends when `timeout` exits, whatever the tracee does.
fn run_captured(
    mut command: Command,
    label: &str,
    stdout_path: &Path,
    stderr_path: &Path,
    expect_success: bool,
) -> Captured {
    let rendered = format!("{command:?}");
    let out = File::create(stdout_path).expect("failed to create stdout capture");
    let err = File::create(stderr_path).expect("failed to create stderr capture");
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .status()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    let captured = Captured {
        stdout: fs::read_to_string(stdout_path).unwrap_or_default(),
        stderr: fs::read_to_string(stderr_path).unwrap_or_default(),
    };
    if expect_success {
        assert!(
            status.success(),
            "{label} failed: {rendered}\nstatus: {status}\nstdout:\n{}\nstderr:\n{}",
            captured.stdout,
            captured.stderr,
        );
    }
    captured
}

fn build_guest(build_root: &Path) -> PathBuf {
    fs::create_dir_all(build_root).expect("failed to create guest build directory");
    let guest = build_root.join("robust_futex_test");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository().join("tests/bin/robust_futex_test.c"))
        .args(["-pthread", "-o"])
        .arg(&guest);
    run_captured(
        compile,
        "robust-futex guest compilation",
        &build_root.join("compile.out"),
        &build_root.join("compile.err"),
        true,
    );
    guest
}

/// The native control must pass, otherwise the guest itself is broken and any
/// Hermit result would be meaningless.
#[test]
fn robust_futex_owner_death_passes_natively() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-native");
    let guest = build_guest(&build_root);

    let mut native = Command::new("timeout");
    native.args(["--kill-after", "5s", "30s"]).arg(&guest);
    let captured = run_captured(
        native,
        "native robust-futex control",
        &build_root.join("native.out"),
        &build_root.join("native.err"),
        true,
    );
    assert!(
        captured
            .stdout
            .contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "native control did not report EOWNERDEAD\nstdout:\n{}",
        captured.stdout
    );
}

#[test]
fn robust_futex_owner_death_wakes_the_waiter_at_l2() {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("robust-futex-owner-death");
    let guest = build_guest(&build_root);
    let verify_json = build_root.join("ptrace.verify.json");
    let _ = fs::remove_file(&verify_json);

    let mut verify = Command::new("timeout");
    verify
        .args(["--kill-after", "5s", "120s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=info",
            "run",
            "--backend=ptrace",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
        ])
        .arg(&verify_json)
        .args(["--base-env=minimal", "--"])
        .arg(&guest);
    let captured = run_captured(
        verify,
        "robust-futex owner-death strict verification",
        &build_root.join("verify.out"),
        &build_root.join("verify.err"),
        true,
    );

    let stdout = captured.stdout;
    let stderr = captured.stderr;
    assert!(
        stdout.contains("PASS: robust mutex waiter received EOWNERDEAD"),
        "the waiter did not observe EOWNERDEAD under Hermit\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    // `--verify` alone prints an identical success line for the lossy stripped
    // comparison, so the JSON record is the only thing that distinguishes L2.
    let report = fs::read_to_string(&verify_json).unwrap_or_else(|error| {
        panic!(
            "missing verification report {}: {error}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            verify_json.display()
        )
    });
    for expected in [
        r#""verified":true"#,
        r#""bitwise_parity":true"#,
        r#""strictness":"canonical""#,
        r#""guest_exit_code":0"#,
    ] {
        assert!(
            report.contains(expected),
            "verification report lacks {expected}\nreport:\n{report}\nstderr:\n{stderr}"
        );
    }
}
