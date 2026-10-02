/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test: reading a pipe that the guest REOPENED BY PATH must not
//! deadlock the scheduler.
//!
//! `/dev/stdin`, `/proc/self/fd/N` and bash's process substitution
//! (`< <(cmd)` reads `/dev/fd/63`) all reach an existing pipe through
//! `openat`, which creates a new open file description. `handle_pipe2` made the
//! original description physically nonblocking so that a read waiting for its
//! writer takes the deterministic nonblockize-and-retry path; the reopened
//! description had neither that O_NONBLOCK nor the Pipe type, so the read
//! blocked in the kernel while holding the scheduler turn and the writer never
//! ran (https://github.com/rrnewton/hermit/issues/1850). Every nixpkgs
//! fixupPhase does this (`while read ...; done < <(find ...)`).
//!
//! Each scenario makes the writer wait first, so the reader is certain to reach
//! an empty pipe. This asserts PROGRESS: the failure signal is `timeout`
//! killing the run (exit 124).

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::process::Command;

/// A healthy run finishes in about a second. A deadlocked run never finishes,
/// so this bound only has to be generous enough to never fire on a loaded box.
const TIMEOUT_SECONDS: u64 = 60;

fn run_bash(extra: &[&str], script: &str, expected_stdout: &str) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(["run", "--base-env=minimal", "--no-virtualize-cpuid"])
        .args(extra)
        .args(["--", "/bin/bash", "-c", script]);

    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));

    assert_ne!(
        output.status.code(),
        Some(124),
        "read from a reopened pipe deadlocked the scheduler (timed out): {rendered}\nstdout:\n{}\nstderr:\n{}",
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
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        expected_stdout,
        "wrong guest output: {rendered}"
    );
}

const DEV_STDIN: &str = "{ /bin/sleep 0.2; echo hi; } | /bin/cat /dev/stdin";
const PROC_SELF_FD: &str = "{ /bin/sleep 0.2; echo hi; } | /bin/cat /proc/self/fd/0";
const PROCESS_SUBSTITUTION: &str = "n=0; while read -r line; do n=$((n+1)); done \
     < <(/bin/sleep 0.2; printf 'a\\nb\\nc\\n'); echo n=$n";

/// Measured against hermit 3629a4ec954d (before the fix): all three scenarios
/// in this file hang until `timeout` kills them; with the fix each exits 0.
#[test]
fn cat_dev_stdin_reads_a_pipe_whose_writer_is_late() {
    run_bash(&[], DEV_STDIN, "hi\n");
}

#[test]
fn cat_proc_self_fd_reads_a_pipe_whose_writer_is_late() {
    run_bash(&[], PROC_SELF_FD, "hi\n");
}

#[test]
fn process_substitution_reads_a_pipe_whose_writer_is_late() {
    run_bash(&[], PROCESS_SUBSTITUTION, "n=3\n");
}

#[test]
fn process_substitution_completes_under_strict() {
    run_bash(&["--strict"], PROCESS_SUBSTITUTION, "n=3\n");
}
