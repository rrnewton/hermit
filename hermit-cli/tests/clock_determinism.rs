/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod common;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

use common::hermit_binary;
use common::nondeterminism::NondeterminismCase;

const DETERMINISM_RUNS: usize = 5;
const REPEATABLE_EPOCH: &str = "2000-12-31T23:59:59.123456789Z";

static HERMIT_CLOCK_LOCK: Mutex<()> = Mutex::new(());
static CLOCK_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn command_output(mut command: Command, label: &str) -> Output {
    hermit_binary::configure_guest_execution(&mut command);
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

fn hermit_clock_lock() -> MutexGuard<'static, ()> {
    HERMIT_CLOCK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clock_guest() -> &'static Path {
    CLOCK_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("clock-determinism");
            fs::create_dir_all(&build_root)
                .expect("failed to create clock determinism build directory");
            let binary = build_root.join("clock_determinism");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-D_GNU_SOURCE",
                    "-std=c11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/clock_determinism.c"))
                .arg("-o")
                .arg(&binary);
            command_output(command, "clock determinism guest compilation");
            binary
        })
        .as_path()
}

fn run_clock_matrix(iteration: usize) -> Vec<u8> {
    let mut command = Command::new(hermit_binary::hermit_binary());
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    command.arg(format!("--epoch={REPEATABLE_EPOCH}")).arg("--");
    command.arg(clock_guest());
    let output = command_output(
        command,
        &format!("clock determinism matrix, iteration {}", iteration + 1),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "CLOCK_REALTIME ",
        "CLOCK_MONOTONIC ",
        "CLOCK_PROCESS_CPUTIME_ID ",
        "CLOCK_THREAD_CPUTIME_ID ",
        "CLOCK_BOOTTIME ",
        "gettimeofday consistent ",
        "clock matrix success\n",
    ] {
        assert!(
            stdout.contains(expected),
            "clock matrix iteration {} omitted {expected:?}\nstdout:\n{stdout}\nstderr:\n{}",
            iteration + 1,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    output.stdout
}

fn run_date_at_epoch(epoch: Option<&str>) -> Output {
    let mut command = Command::new(hermit_binary::hermit_binary());
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
    ]);
    if let Some(epoch) = epoch {
        command.arg(format!("--epoch={epoch}"));
    }
    command.args(["--", "/bin/date", "+%s.%N"]);
    command_output(command, "virtual epoch date probe")
}

#[test]
fn default_virtual_epoch_tracks_invocation_start_and_is_reported() {
    let _guard = hermit_clock_lock();
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let output = run_date_at_epoch(None);
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let observed = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .unwrap();
    assert!(
        observed >= before && observed <= after + 1.0,
        "default virtual epoch {observed} was not captured near host now [{before}, {after}]"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("virtual-time epoch="), "{stderr}");
    assert!(stderr.contains("source=host-now"), "{stderr}");
    assert!(stderr.contains("reproduce with --epoch="), "{stderr}");
}

#[test]
fn explicit_virtual_epoch_reproduces_identical_observed_time() {
    let _guard = hermit_clock_lock();
    let first = run_date_at_epoch(Some(REPEATABLE_EPOCH));
    let second = run_date_at_epoch(Some(REPEATABLE_EPOCH));
    assert_eq!(first.stdout, second.stdout);
    let rendered = String::from_utf8_lossy(&first.stdout);
    let (seconds, nanos) = rendered.trim().split_once('.').unwrap();
    let observed = seconds.parse::<u64>().unwrap() * 1_000_000_000 + nanos.parse::<u64>().unwrap();
    let epoch = 978_307_199_123_456_789;
    assert!(
        (epoch..epoch + 1_000_000_000).contains(&observed),
        "explicit epoch did not seed the expected virtual-time trajectory: {observed}"
    );
    for output in [first, second] {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("source=explicit"), "{stderr}");
        assert!(
            stderr.contains("2000-12-31T23:59:59.123456789+00:00"),
            "{stderr}"
        );
    }
}

#[test]
fn clock_apis_are_deterministic_across_five_runs() {
    let _guard = hermit_clock_lock();
    let baseline = run_clock_matrix(0);

    for iteration in 1..DETERMINISM_RUNS {
        assert_eq!(
            run_clock_matrix(iteration),
            baseline,
            "clock matrix changed output on iteration {}",
            iteration + 1,
        );
    }
}

// NONDET_SOURCE: timestamp
#[test]
fn strict_mode_eliminates_native_clock_nondeterminism() {
    let _guard = hermit_clock_lock();
    let case =
        NondeterminismCase::new("timestamp", Path::new("/bin/date"), &["+%s%N"]).with_retries(5);

    case.assert_nondeterministic_without_hermit();
    case.assert_nondeterministic_with_noop_verify();
    case.assert_deterministic_with_strict();
}
