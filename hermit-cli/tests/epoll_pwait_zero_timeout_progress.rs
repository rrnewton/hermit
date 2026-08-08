/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test: a zero-timeout `epoll_pwait` polling loop must not starve
//! its producer under ordinary sequentialized strict execution.
//!
//! A zero-timeout `epoll_pwait` cannot block, so injecting it straight into
//! Linux looks free. Under sequentialized threads it is not: a poller that
//! never requests a resource is never descheduled between preemptions, and the
//! worker it is polling for never gets the single logical CPU. `handle_poll`
//! has always taken a scheduler turn for zero-timeout calls, and the
//! record/replay arm of `handle_epoll_pwait` does too; the plain-strict path
//! must match. Before the fix this guest hangs.
//!
//! This asserts PROGRESS, not a return value: a syscall-return assertion
//! passes happily while the producer starves. The failure signal is `timeout`
//! killing the run (exit 124).

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// A healthy run finishes in well under a second. A starved run spins forever,
/// so this bound only has to be generous enough to never fire on a loaded box.
const TIMEOUT_SECONDS: u64 = 60;

const SUCCESS_MARKER: &str = "epoll-pwait-zero-timeout-progress-ok";

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("epoll-pwait-zero-timeout-progress");
            fs::create_dir_all(&build_root).expect("failed to create build directory");
            let output = build_root.join("epoll_pwait_zero_timeout_progress");
            let mut command = Command::new("cc");
            command
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-pthread",
                    "-D_GNU_SOURCE",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/epoll_pwait_zero_timeout_progress.c"))
                .arg("-o")
                .arg(&output);
            let status = command
                .status()
                .expect("failed to run cc to build epoll_pwait_zero_timeout_progress guest");
            assert!(status.success(), "guest compilation failed: {command:?}");
            output
        })
        .as_path()
}

/// Run the guest under ordinary sequentialized strict mode -- the exact
/// configuration that routes a NULL-sigmask `epoll_pwait` into
/// `handle_internal_epoll_pwait`. Deliberately no `--chaos` and no
/// record/replay: those arms already took a scheduler turn before the fix, so
/// exercising them would not detect the regression.
fn run_plain_strict(extra: &[&str]) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--strict",
        ])
        .args(extra)
        .arg("--")
        .arg(guest());

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));

    // `timeout` exits 124 when it has to kill the child. That is precisely the
    // starvation this test guards: the poller monopolized the CPU and the
    // worker never ran.
    assert_ne!(
        output.status.code(),
        Some(124),
        "zero-timeout epoll_pwait starved its producer (timed out): {rendered}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        output.status.success(),
        "guest failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8(output.stdout).expect("guest stdout should be UTF-8");
    assert!(
        stdout.contains(SUCCESS_MARKER),
        "guest did not report progress: {rendered}\nstdout:\n{stdout}"
    );
}

/// THE REGRESSION DETECTOR. Timer preemption is disabled, which removes the
/// only other mechanism that can rescue a non-yielding poller, so the
/// scheduler turn taken inside the zero-timeout `epoll_pwait` path is the sole
/// reason this guest can finish.
///
/// Measured both ways against this exact guest: with the scheduler request
/// removed from `handle_internal_epoll_pwait`, this test FAILS with the guest
/// killed at 60s (`timeout` exit 124); with the request restored it passes in
/// well under a second. That bracket is what makes it a guard rather than a
/// hopeful assertion.
#[test]
fn zero_timeout_epoll_pwait_yields_without_timer_preemption() {
    run_plain_strict(&["--max-timeslice=disabled"]);
}

/// Smoke coverage of the ordinary configuration, with timer preemption left
/// on. Kept because it exercises the common path end to end.
///
/// DO NOT MISTAKE THIS FOR A REGRESSION GUARD. Measured: with the scheduler
/// request removed it still PASSES, because ordinary timer preemption
/// deschedules the poller often enough for the worker to run. It cannot detect
/// the bug it appears to be about. The detector is
/// `zero_timeout_epoll_pwait_yields_without_timer_preemption` above; if that
/// one is ever weakened or deleted, this test does not cover for it.
#[test]
fn zero_timeout_epoll_pwait_ordinary_strict_smoke() {
    run_plain_strict(&[]);
}
