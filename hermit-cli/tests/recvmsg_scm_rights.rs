/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression tests for `recvmsg` SCM_RIGHTS ancillary file descriptors under
//! plain `hermit run`.
//!
//! Detcore must register descriptors received through an SCM_RIGHTS control
//! message in its fd table. Before that fix, the first operation on a received
//! descriptor (e.g. `read`) failed with `EBADF` because Detcore had never seen
//! the fd. These tests compile the C guests and confirm they succeed under
//! `hermit run`.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static GUESTS: OnceLock<PathBuf> = OnceLock::new();

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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

fn build_root() -> &'static Path {
    GUESTS.get_or_init(|| {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("recvmsg-scm-rights");
        fs::create_dir_all(&root).expect("failed to create guest build directory");
        root
    })
}

fn guest(name: &str, source: &str) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output = build_root().join(name);
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread"])
        .arg(repository.join("tests/c").join(source))
        .arg("-o")
        .arg(&output);
    command_output(command, &format!("{name} compilation"));
    output
}

fn run_guest(name: &str, source: &str, expected_marker: &str) {
    let _guard = hermit_run_lock();
    let program = guest(name, source);

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["run", "--base-env=minimal", "--"])
        .arg(&program);
    let output = command_output(command, &format!("hermit run {name}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(expected_marker),
        "{name} did not report success under hermit run:\nstdout:\n{}\nstderr:\n{}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn single_scm_rights_fd_is_mappable() {
    run_guest(
        "recvmsg_scm_rights_mmap",
        "recvmsg_scm_rights_mmap.c",
        "recvmsg-scm-rights-mmap-ok",
    );
}

#[test]
fn multiple_scm_rights_fds_are_readable() {
    run_guest(
        "recvmsg_scm_rights_multi",
        "recvmsg_scm_rights_multi.c",
        "recvmsg-scm-rights-multi-ok",
    );
}
