/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test: a blocking open of a named FIFO must not deadlock the
//! scheduler (https://github.com/rrnewton/hermit/issues/2203).
//!
//! Linux makes `open(fifo, O_RDONLY)` wait until a writer opens the FIFO and
//! `open(fifo, O_WRONLY)` wait until a reader holds it open. Issued as is, that
//! open waits in the kernel while holding the scheduler turn, so the process
//! that would open the other end never runs. nixpkgs' `audit-tmpdir.sh`, run in
//! every stdenv fixupPhase, does this with two FIFOs.
//!
//! Each scenario delays one end so the other is certain to open first. This
//! asserts PROGRESS and the Linux result: the failure signal is `timeout`
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
        "a blocking FIFO open deadlocked the scheduler (timed out): {rendered}\nstdout:\n{}\nstderr:\n{}",
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

const SETUP: &str = "set -e; d=$(/bin/mktemp -d); /bin/mkfifo \"$d/a\" \"$d/b\"; ";

/// The reader opens first and must wait for the late writer.
const LATE_WRITER: &str = "{ /bin/sleep 0.2; echo hi > \"$d/a\"; } & /bin/cat \"$d/a\"; wait";

/// The writer opens first and must wait for the late reader.
const LATE_READER: &str = "echo hi > \"$d/a\" & /bin/sleep 0.2; /bin/cat \"$d/a\"; wait";

/// A writer that opens and closes without writing: the reader's open completes
/// and its read sees end of file.
const SILENT_WRITER: &str =
    "{ /bin/sleep 0.2; : > \"$d/a\"; } & /bin/cat \"$d/a\"; echo \"cat=$?\"; wait";

/// A reader's open completes when the writer OPENS, not when it writes: each
/// side opens one FIFO for reading and the other for writing, and the first
/// write depends on both opens having returned. Waiting for data instead of a
/// writer deadlocks here.
const HANDSHAKE: &str = "{ exec 3>\"$d/a\" 4<\"$d/b\"; read -r x <&4; echo \"got-$x\" >&3; } & \
     exec 3<\"$d/a\" 4>\"$d/b\"; echo ping >&4; read -r y <&3; echo \"$y\"; wait";

/// nixpkgs `audit-tmpdir.sh`: one writer opens two FIFOs in turn while a
/// separate reader drains each.
const AUDIT_TMPDIR: &str = "/bin/cat \"$d/a\" > \"$d/a.out\" & /bin/cat \"$d/b\" > \"$d/b.out\" & \
     { echo elf >&3; echo script >&4; } 3>\"$d/a\" 4>\"$d/b\"; wait; \
     /bin/cat \"$d/a.out\" \"$d/b.out\"";

/// Measured against hermit e3a699b4a8d0 (before this fix): every scenario in
/// this file hangs until `timeout` kills it; with the fix each exits 0.
#[test]
fn reader_open_waits_for_a_late_writer() {
    run_bash(&[], &format!("{SETUP}{LATE_WRITER}"), "hi\n");
}

#[test]
fn writer_open_waits_for_a_late_reader() {
    run_bash(&[], &format!("{SETUP}{LATE_READER}"), "hi\n");
}

#[test]
fn writer_that_never_writes_gives_the_reader_end_of_file() {
    run_bash(&[], &format!("{SETUP}{SILENT_WRITER}"), "cat=0\n");
}

#[test]
fn reader_open_returns_when_the_writer_opens_not_when_it_writes() {
    run_bash(&[], &format!("{SETUP}{HANDSHAKE}"), "got-ping\n");
}

#[test]
fn audit_tmpdir_fifo_pair_completes() {
    run_bash(&[], &format!("{SETUP}{AUDIT_TMPDIR}"), "elf\nscript\n");
}

#[test]
fn audit_tmpdir_fifo_pair_completes_under_strict() {
    run_bash(
        &["--strict"],
        &format!("{SETUP}{AUDIT_TMPDIR}"),
        "elf\nscript\n",
    );
}
