/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end determinism tests that run real-world applications under
//! `hermit run --strict --verify` and require a bitwise-identical repeat run
//! (assurance level L2).
//!
//! These deliberately use the built-in `--verify` execution path (Hermit runs
//! the guest twice and diffs the two logs) rather than the manual "run N times
//! and compare stdout" style used elsewhere. They also cover applications that
//! previously had no strict-mode coverage at all:
//!
//! - `curl` and `nginx` were only exercised in default run mode
//!   (`arbitrary_binaries.rs`, `integration_matrix.rs`), never under `--strict`.
//! - `redis-server` and `java` have strict workloads elsewhere
//!   (`redis_strict.rs`, `language_runtime_determinism.rs`); the bounded
//!   version invocations here add a cheap, self-contained L2 smoke check for
//!   process startup and static initialization under the deterministic runtime.
//!
//! Each workload is a bounded, self-contained invocation (no network, no
//! long-running server) so the run terminates and its output depends only on
//! the guest, which is what lets `--verify` reach L2.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::MutexGuard;

/// Serialize the Hermit runs; the deterministic scheduler and PMU counters are
/// process-global resources on the self-hosted runner.
static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());

/// Wall-clock cap for a single `--verify` (two-run) invocation.
const HERMIT_VERIFY_TIMEOUT: &str = "120s";

/// Grace period before `timeout(1)` escalates from SIGTERM to SIGKILL.
const HERMIT_VERIFY_KILL_AFTER: &str = "10s";

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Resolve the first candidate path that names an existing regular file.
///
/// These applications are installed by the self-hosted CI job, so a missing
/// binary is a hard error rather than a silent skip.
fn required_app(name: &str, candidates: &[&str]) -> PathBuf {
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!(
                "ERROR: required application {name} is missing; expected an executable at one of \
                 {candidates:?} (the self-hosted CI job installs it)"
            )
        })
}

/// Run `hermit run --strict --verify -- <program> <args>` and assert that
/// Hermit's verifier reports a bitwise-identical repeat run (L2).
///
/// `--verify` exits non-zero when the two runs diverge, so a successful exit is
/// the primary determinism signal; the success banner is checked as a guard
/// against the flag silently becoming a no-op.
fn assert_l2_under_strict_verify(program: &Path, args: &[&str]) {
    let _guard = hermit_run_lock();

    let mut command = Command::new("timeout");
    command
        .args([
            "--kill-after",
            HERMIT_VERIFY_KILL_AFTER,
            HERMIT_VERIFY_TIMEOUT,
        ])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--strict",
            "--verify",
            // The self-hosted runner exposes real CPUID/PMU; these relaxations
            // keep the test usable on VMs without CPUID interception without
            // weakening determinism (they do not disable strict mode).
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ])
        .arg(program)
        .args(args);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "hermit run --strict --verify was not deterministic (L2) for {rendered}\n\
         status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stderr.contains("Determinism verified") || stdout.contains("Determinism verified"),
        "hermit --verify exited 0 but did not report determinism for {rendered}\n\
         stdout:\n{stdout}\nstderr:\n{stderr}",
    );
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the curl binary"]
fn curl_version_is_deterministic_under_strict_verify() {
    let curl = required_app("curl", &["/usr/bin/curl", "/usr/local/bin/curl"]);
    assert_l2_under_strict_verify(&curl, &["--version"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the nginx binary"]
fn nginx_version_is_deterministic_under_strict_verify() {
    let nginx = required_app("nginx", &["/usr/sbin/nginx", "/usr/bin/nginx"]);
    assert_l2_under_strict_verify(&nginx, &["-v"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + the redis-server binary"]
fn redis_server_version_is_deterministic_under_strict_verify() {
    let redis_server = required_app(
        "redis-server",
        &["/usr/bin/redis-server", "/usr/local/bin/redis-server"],
    );
    assert_l2_under_strict_verify(&redis_server, &["--version"]);
}

#[test]
#[ignore = "e2e: requires hermit + PMU/mount namespaces + a JVM"]
fn java_version_is_deterministic_under_strict_verify() {
    let java = required_app("java", &["/usr/local/bin/java", "/usr/bin/java"]);
    assert_l2_under_strict_verify(&java, &["-version"]);
}
