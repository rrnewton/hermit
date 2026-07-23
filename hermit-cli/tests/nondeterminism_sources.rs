/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Killer-demo tests: each guest is bitwise NONDETERMINISTIC when run natively
//! and DETERMINISTIC under `hermit run --strict --verify`. Every case pairs
//! [`NondeterminismCase::assert_nondeterministic_without_hermit`] (proof the
//! program really is nondeterministic) with
//! [`NondeterminismCase::assert_deterministic_with_strict`] (proof Hermit
//! removes it). Sources that the `--strace-only` passthrough preset does not
//! determinize also assert
//! [`NondeterminismCase::assert_nondeterministic_with_noop_verify`].
//!
//! Covered NONDET sources: `thread-race`, `hashmap-order`, `aslr`, `urandom`.
//! `timestamp` is covered separately in `clock_determinism.rs`.

mod common;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;

use common::nondeterminism::NondeterminismCase;

/// Serializes the Hermit runs in this binary so overlapping guests do not
/// contend for scheduler and performance-counter resources.
static HERMIT_NONDET_LOCK: Mutex<()> = Mutex::new(());

fn hermit_nondet_lock() -> MutexGuard<'static, ()> {
    HERMIT_NONDET_LOCK
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

/// Compiles a guest from `tests/c/<source>` into the test temp directory and
/// returns the path to the resulting executable.
fn compile_guest(source: &str, binary_name: &str, extra_args: &[&str]) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("nondeterminism-sources");
    fs::create_dir_all(&build_root).expect("failed to create nondeterminism build directory");
    let binary = build_root.join(binary_name);

    let mut command = Command::new("cc");
    command
        .args(["-O2", "-g"])
        .args(extra_args)
        .arg(repository.join("tests/c").join(source))
        .arg("-o")
        .arg(&binary);
    command_output(command, &format!("compiling {source}"));
    binary
}

// NONDET_SOURCE: thread-race
#[test]
fn strict_mode_eliminates_thread_scheduling_race() {
    let _guard = hermit_nondet_lock();
    let guest = compile_guest(
        "thread_race_counter.c",
        "thread_race_counter",
        &["-pthread"],
    );
    // The race is probabilistic, so allow extra retries to observe a divergence.
    let case = NondeterminismCase::new("thread-race", &guest, &[]).with_retries(20);

    // Native lost updates make the total vary; --strace-only passes scheduling
    // through, so noop verification also observes the race.
    case.assert_nondeterministic_without_hermit();
    case.assert_nondeterministic_with_noop_verify();
    case.assert_deterministic_with_strict();
}

// NONDET_SOURCE: hashmap-order
#[test]
fn strict_mode_eliminates_hashmap_iteration_order() {
    let _guard = hermit_nondet_lock();
    let guest = compile_guest(
        "hashmap_iteration_order.c",
        "hashmap_iteration_order",
        &["-D_GNU_SOURCE"],
    );
    let case = NondeterminismCase::new("hashmap-order", &guest, &[]);

    // The seed comes from getrandom, which Hermit determinizes even under
    // --strace-only, so only the naked baseline exposes the varying order.
    case.assert_nondeterministic_without_hermit();
    case.assert_deterministic_with_strict();
}

// NONDET_SOURCE: aslr
#[test]
fn strict_mode_eliminates_aslr_addresses() {
    let _guard = hermit_nondet_lock();
    let guest = compile_guest("print_memaddrs.c", "print_memaddrs", &[]);
    let case = NondeterminismCase::new("aslr", &guest, &[]);

    // Hermit normalizes address bits during verification even in --strace-only,
    // so the naked baseline is the source of the observed nondeterminism.
    case.assert_nondeterministic_without_hermit();
    case.assert_deterministic_with_strict();
}

// NONDET_SOURCE: urandom
#[test]
fn strict_mode_eliminates_urandom_reads() {
    let _guard = hermit_nondet_lock();
    let guest = compile_guest(
        "random_sources.c",
        "random_sources",
        &["-pthread", "-Wall", "-Wextra", "-Werror"],
    );
    let case = NondeterminismCase::new("urandom", &guest, &[]);

    // Reads from /dev/urandom and /dev/random pass through under --strace-only,
    // so noop verification observes the nondeterminism directly.
    case.assert_nondeterministic_without_hermit();
    case.assert_nondeterministic_with_noop_verify();
    case.assert_deterministic_with_strict();
}
