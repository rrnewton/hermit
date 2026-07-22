/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Stress test for deterministic futex wake ordering.
//!
//! Several threads block in `FUTEX_WAIT` on one address; the main thread then
//! wakes them one at a time with `FUTEX_WAKE`. Each woken thread writes its id,
//! so the program's stdout *is* the exact order Detcore chose to wake the
//! waiters. Running under `hermit run --strict --verify` executes the guest
//! twice and fails unless both runs are bitwise-identical (assurance level L2),
//! which proves the wake order is deterministic.
//!
//! The test asserts L2 in three scheduling configurations — default, `--chaos`,
//! and `--fuzz-futexes` — because each selects wakees differently (see
//! `Scheduler::choose_futex_wakees`): the default takes a fixed slice of the
//! waiter list, `--fuzz-futexes` shuffles it with the seeded fuzz PRNG, and
//! `--chaos` perturbs the whole schedule. All three must remain deterministic.
//!
//! The guest coordinates with `nanosleep` (deterministic virtual time) rather
//! than a `sched_yield` spin loop, which would starve under `--chaos` (GH #81).

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;

/// Hermit serializes the whole guest, so only run one instance at a time.
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

/// Number of threads that block on the futex.
const WAITERS: usize = 6;

/// Repetitions per configuration, to shake out rare divergence (toward L4).
const REPETITIONS: usize = 3;

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const GUEST_SOURCE: &str = r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <linux/futex.h>
#include <sys/syscall.h>
#include <unistd.h>
#include <time.h>
#include <stdio.h>

#define N 6
static int futex_word = 0;
static atomic_int ready = 0;

static long fx(int *a, int op, int val) {
  return syscall(SYS_futex, a, op | FUTEX_PRIVATE_FLAG, val, NULL, NULL, 0);
}
static void nap(long ms) {
  struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
  nanosleep(&ts, NULL);
}
static void *waiter(void *arg) {
  long id = (long)arg;
  atomic_fetch_add(&ready, 1);
  fx(&futex_word, FUTEX_WAIT, 0);
  char buf[8];
  int n = snprintf(buf, sizeof buf, "%ld\n", id);
  write(1, buf, n);
  return NULL;
}
int main(void) {
  pthread_t t[N];
  for (long i = 0; i < N; i++) pthread_create(&t[i], NULL, waiter, (void *)i);
  while (atomic_load(&ready) < N) nap(1);
  nap(20);                        /* let every thread park in FUTEX_WAIT */
  futex_word = 1;
  int woken = 0;
  while (woken < N) {
    long w = fx(&futex_word, FUTEX_WAKE, 1);
    if (w > 0) woken += w; else nap(1);
  }
  for (int i = 0; i < N; i++) pthread_join(t[i], NULL);
  return 0;
}
"#;

/// Compiles the embedded guest once and returns the binary path.
fn guest_binary() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("futex-wake-order");
    fs::create_dir_all(&dir).expect("failed to create futex workload directory");
    let src = dir.join("futex_wake_order.c");
    fs::write(&src, GUEST_SOURCE).expect("failed to write futex guest source");
    let binary = dir.join("futex_wake_order");
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread"])
        .arg(&src)
        .arg("-o")
        .arg(&binary);
    let output = command.output().expect("failed to invoke cc");
    assert!(
        output.status.success(),
        "compiling futex guest failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

/// Runs the guest under `hermit run` with the given extra flags before `--`.
fn hermit_run(binary: &Path, extra_flags: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args(["--log=error", "run"]);
    command.args(extra_flags);
    command.arg("--").arg(binary);
    let rendered = format!("{command:?}");
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run {rendered}: {error}"))
}

/// Asserts the guest is bitwise-deterministic under `--strict --verify` plus the
/// given extra flags (L2), repeated `REPETITIONS` times.
fn assert_l2(extra_flags: &[&str]) {
    let _guard = hermit_run_lock();
    let binary = guest_binary();
    let mut flags = vec!["--strict", "--verify"];
    flags.extend_from_slice(extra_flags);
    for rep in 1..=REPETITIONS {
        let output = hermit_run(&binary, &flags);
        assert!(
            output.status.success(),
            "futex wake order not bitwise-deterministic under {flags:?} (repetition {rep}): \
             status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn futex_wake_order_is_deterministic_default() {
    assert_l2(&[]);
}

#[test]
fn futex_wake_order_is_deterministic_under_chaos() {
    assert_l2(&["--chaos"]);
}

#[test]
fn futex_wake_order_is_deterministic_under_fuzz_futexes() {
    assert_l2(&["--fuzz-futexes"]);
}

#[test]
fn futex_wakes_every_waiter_exactly_once() {
    let _guard = hermit_run_lock();
    let binary = guest_binary();
    let output = hermit_run(&binary, &["--strict"]);
    assert!(
        output.status.success(),
        "guest failed: status {}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).expect("stdout should be UTF-8");
    let mut ids: Vec<usize> = stdout
        .lines()
        .map(|line| {
            line.trim()
                .parse::<usize>()
                .expect("waiter id should be numeric")
        })
        .collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        (0..WAITERS).collect::<Vec<_>>(),
        "every waiter should be woken exactly once; wake sequence was:\n{stdout}"
    );
}
