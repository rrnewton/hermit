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
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static POLICY_GUEST: OnceLock<PathBuf> = OnceLock::new();
const RUNS: usize = 5;

const MINIMAL_FILES: [(&str, &[u8]); 6] = [
    (
        "/proc/self/maps",
        b"00400000-00401000 r-xp 00000000 00:00 0 [hermit]\n",
    ),
    (
        "/proc/self/stat",
        b"1 (hermit) R 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n",
    ),
    (
        "/proc/self/status",
        b"Name:\thermit\nState:\tR (running)\nPid:\t1\nPPid:\t0\nThreads:\t1\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n",
    ),
    ("/proc/self/cmdline", b"hermit-guest\0"),
    (
        "/proc/cpuinfo",
        b"processor\t: 0\nvendor_id\t: Hermit\nmodel name\t: Hermit Virtual CPU\ncpu MHz\t\t: 0.000\ncpu cores\t: 1\nsiblings\t: 1\nflags\t\t:\n",
    ),
    ("/proc/sys/kernel/random/entropy_avail", b"256\n"),
];

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn hermit_output(program: &Path, arguments: &[&str]) -> Output {
    hermit_output_with_options(program, arguments, &[])
}

fn hermit_output_with_options(program: &Path, arguments: &[&str], run_options: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "--log=error",
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--preemption-timeout=disabled",
    ]);
    command
        .args(run_options)
        .arg("--")
        .arg(program)
        .args(arguments);
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {command:?}: {error}"))
}

fn assert_minimal_file(path: &str, expected: &[u8]) {
    let _guard = hermit_run_lock();
    for run in 1..=RUNS {
        let output = hermit_output(Path::new("/bin/cat"), &[path]);
        assert!(
            output.status.success(),
            "minimal procfs read {run} failed for {path}: status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            output.stdout, expected,
            "minimal procfs content changed for {path} on run {run}"
        );
    }
}

fn policy_guest() -> &'static Path {
    POLICY_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("procfs-minimal");
            fs::create_dir_all(&build_root).expect("failed to create procfs test directory");
            let output = build_root.join("procfs-minimal");
            let mut command = Command::new("cc");
            command
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-D_GNU_SOURCE",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/procfs_minimal.c"))
                .arg("-o")
                .arg(&output);
            let result = command
                .output()
                .unwrap_or_else(|error| panic!("failed to start {command:?}: {error}"));
            assert!(
                result.status.success(),
                "procfs policy guest compilation failed: {command:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr),
            );
            output
        })
        .as_path()
}

#[test]
fn procfs_exposes_only_fixed_minimal_content() {
    for (path, contents) in MINIMAL_FILES {
        assert_minimal_file(path, contents);
    }
}

#[test]
fn procfs_blocks_unlisted_entries() {
    let _guard = hermit_run_lock();
    for path in [
        "/proc/meminfo",
        "/proc/self/environ",
        "/proc/sys/kernel/hostname",
        "/proc/sys/vm/swappiness",
    ] {
        for run in 1..=RUNS {
            let output = hermit_output(Path::new("/bin/cat"), &[path]);
            assert!(
                !output.status.success(),
                "hidden procfs path {path} unexpectedly opened on run {run}"
            );
            assert!(output.stdout.is_empty(), "hidden path leaked bytes: {path}");
        }
    }
}

#[test]
fn procfs_policy_covers_metadata_and_modern_open_calls() {
    let _guard = hermit_run_lock();
    for run in 1..=RUNS {
        let output = hermit_output(policy_guest(), &[]);
        assert!(
            output.status.success(),
            "procfs policy guest failed on run {run}: status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(output.stdout, b"procfs-policy:ok\n");
    }
}

#[test]
fn procfs_policy_is_independent_of_metadata_virtualization() {
    let _guard = hermit_run_lock();
    let output = hermit_output_with_options(policy_guest(), &[], &["--no-virtualize-metadata"]);
    assert!(
        output.status.success(),
        "minimal procfs policy was disabled with metadata virtualization: status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(output.stdout, b"procfs-policy:ok\n");
}
