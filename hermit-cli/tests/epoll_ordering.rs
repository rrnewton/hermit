/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test for deterministic `epoll_wait` event ordering.
//!
//! The `epoll_ordering` guest makes several fds readable in a scrambled order
//! before a single `epoll_wait`, tagging each with an ascending `data.u64`.
//! `epoll_wait(2)` does not define the order of returned events, so a native
//! kernel reports them in a host-dependent order (registration order here).
//! Hermit determinizes the result by sorting on the caller-supplied `data`, so
//! under Hermit the guest must observe the canonical ascending order on every
//! run.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

const RUNS: usize = 5;

/// The canonical order Hermit must impose regardless of the kernel's native
/// (scrambled) delivery order. Matches the ascending `data.u64` tags the guest
/// registers.
const CANONICAL_ORDER: &str = "0 1 2 3 4 5 6 7";

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static EPOLL_GUEST: OnceLock<PathBuf> = OnceLock::new();

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

fn epoll_guest() -> &'static Path {
    EPOLL_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("epoll-ordering");
        fs::create_dir_all(&build_root).expect("failed to create epoll guest build directory");
        let output = build_root.join("epoll_ordering");

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
            .arg(repository.join("tests/c/epoll_ordering.c"))
            .arg("-o")
            .arg(&output);
        command_output(command, "epoll ordering guest compilation");
        output
    })
}

fn run_guest(run: usize) -> String {
    // NOTE: `--strict` would be ideal, but glibc's startup `rseq` currently
    // aborts under strict mode in this environment; the standard deterministic
    // flags below still keep `sequentialize_threads` on, which is what routes
    // `epoll_wait` through Detcore's determinized internal-polling path.
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(epoll_guest());

    let output = command_output(command, &format!("epoll ordering run {run}/{RUNS}"));
    assert!(
        output.stdout.ends_with(b"epoll_ordering success\n"),
        "run {run}/{RUNS} omitted its success marker:\n{}",
        String::from_utf8_lossy(&output.stdout),
    );
    String::from_utf8(output.stdout).expect("guest output should be UTF-8")
}

#[test]
fn epoll_wait_returns_events_in_canonical_order() {
    let _guard = hermit_run_lock();

    let first = run_guest(1);
    let order = first
        .lines()
        .next()
        .expect("guest should print the delivery order");
    assert_eq!(
        order, CANONICAL_ORDER,
        "epoll_wait did not return events in canonical (data-sorted) order",
    );

    // The canonical order must also be stable across runs.
    for run in 2..=RUNS {
        let actual = run_guest(run);
        assert_eq!(
            actual, first,
            "epoll_wait ordering changed on run {run}/{RUNS}:\nexpected:\n{first}actual:\n{actual}",
        );
    }
}
