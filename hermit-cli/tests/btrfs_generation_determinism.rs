/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end coverage for host Btrfs transaction generation identities.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

struct ProgramCase {
    name: &'static str,
    candidates: &'static [&'static str],
    args: Vec<String>,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn is_lowercase_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
            }
        })
}

fn first_generation_path() -> Option<PathBuf> {
    let mut paths = fs::read_dir("/sys/fs/btrfs")
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_lowercase_uuid))
        .map(|entry| entry.path().join("generation"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().next()
}

fn required_program(case: &ProgramCase) -> PathBuf {
    case.candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "required program {} is missing; expected one of {:?}",
                case.name, case.candidates
            )
        })
}

fn assert_l2(case: &ProgramCase) {
    let program = required_program(case);
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "DEBUG",
            "run",
            "--backend",
            "ptrace",
            "--strict",
            "--verify",
            "--verify-logs",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
            "--",
        ])
        .arg(&program)
        .args(&case.args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "{} failed ptrace L2 strict verification ({rendered})\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
        output.status,
    );
    assert!(
        stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
        "{} omitted Hermit's verification marker ({rendered})\nstdout:\n{stdout}\nstderr:\n{stderr}",
        case.name,
    );
}

fn read_generation(path: &Path) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log",
            "ERROR",
            "run",
            "--backend",
            "ptrace",
            "--strict",
            "--panic-on-unsupported-syscalls",
            "--base-env",
            "minimal",
            "--",
            "/usr/bin/cat",
        ])
        .arg(path)
        .output()
        .expect("failed to read Btrfs generation through Hermit");
    assert!(
        output.status.success(),
        "Btrfs generation read failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output.stdout
}

#[test]
fn btrfs_generation_consumers_are_deterministic_under_strict_verify() {
    let Some(path) = first_generation_path() else {
        return;
    };
    let _guard = hermit_run_lock();
    assert_eq!(read_generation(&path), b"0\n");

    let path = path.display().to_string();
    let cases = [
        ProgramCase {
            name: "bash Btrfs generation",
            candidates: &["/usr/bin/bash", "/bin/bash"],
            args: vec![
                "-c".to_owned(),
                "for i in {1..100000}; do :; done; cat \"$1\"".to_owned(),
                "bash".to_owned(),
                path.clone(),
            ],
        },
        ProgramCase {
            name: "zsh Btrfs generation",
            candidates: &["/usr/bin/zsh", "/bin/zsh"],
            args: vec![
                "-c".to_owned(),
                "i=0; while [ \"$i\" -lt 5000 ]; do i=$((i+1)); done; cat \"$1\"".to_owned(),
                "zsh".to_owned(),
                path.clone(),
            ],
        },
        ProgramCase {
            name: "perl Btrfs generation",
            candidates: &["/usr/bin/perl", "/bin/perl"],
            args: vec![
                "-e".to_owned(),
                "$x += $_ for 1..5000000; open my $fh, \"<\", $ARGV[0] or die $!; print while <$fh>"
                    .to_owned(),
                path,
            ],
        },
    ];

    for case in &cases {
        assert_l2(case);
    }
}
