/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const DETERMINISM_RUNS: usize = 10;
const ROOT_IDENTITY: &[u8] = b"parent pid=3 ppid=1 tid=3\n";

static HERMIT_PID_LOCK: Mutex<()> = Mutex::new(());
static PID_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn hermit_pid_lock() -> MutexGuard<'static, ()> {
    HERMIT_PID_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn pid_guest() -> &'static Path {
    PID_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pid-determinism");
            fs::create_dir_all(&build_root)
                .expect("failed to create PID determinism build directory");
            let binary = build_root.join("pid_namespace_determinism");

            let output = Command::new("cc")
                .args(["-O2", "-g", "-std=c11", "-Wall", "-Wextra", "-Werror"])
                .arg(repository.join("tests/c/pid_namespace_determinism.c"))
                .arg("-o")
                .arg(&binary)
                .output()
                .expect("failed to start PID guest compilation");
            assert!(
                output.status.success(),
                "PID guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            binary
        })
        .as_path()
}

fn run_pid_guest() -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--strict",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(pid_guest())
        .output()
        .expect("failed to run PID guest under Hermit");
    assert!(
        output.status.success(),
        "PID guest failed: status={}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

#[test]
fn fork_returns_repeatable_namespace_pids() {
    let _guard = hermit_pid_lock();
    let expected = run_pid_guest();
    assert!(
        expected.starts_with(ROOT_IDENTITY),
        "guest root did not receive the fixed namespace identity",
    );
    assert_eq!(
        expected.iter().filter(|byte| **byte == b'\n').count(),
        5,
        "guest did not report one parent and four children",
    );
    for run in 2..=DETERMINISM_RUNS {
        assert_eq!(
            run_pid_guest(),
            expected,
            "PID namespace allocation changed on run {run}",
        );
    }
}
