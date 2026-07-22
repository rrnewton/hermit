/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end record -> replay matrix driven through the `hermit-verify` binary.
//!
//! fbsource's internal Buck suite ran the workload matrix through
//! `hermit-verify trace-replay --strip-times` (categories
//! `hermit_run_tracereplay__` + `hermit_run_tracereplay_chaos__`) and
//! `hermit-verify chaos-replay` (`hermit_run_chaosreplay__`) -- 126 targets in
//! total. The OSS Cargo suite exercised record -> replay only through
//! `hermit record start --verify` (see `record_replay.rs`) and never drove the
//! standalone `trace-replay` / `chaos-replay` verification paths over the
//! matrix. This test closes that gap: it records a schedule for each known-good
//! workload and replays it, asserting the replay is deterministic (stdout,
//! stderr, exit status, and the guest-visible DETLOG stream all match).
//!
//! ## The bootstrap-execve exclusion
//!
//! `hermit-verify`'s replay use cases compare the raw TRACE-level DETLOG streams
//! without the numeric stripping that `hermit record --verify` applies. On the
//! very first (bootstrap) `execve` that launches the guest, the injected
//! argv/envp buffer lives in the reverie guest-agent injection region, whose
//! address can differ between the record and replay processes. Those pointers
//! appear inside the tracing span attached to every DETLOG line emitted during
//! that `execve`, so the lines differ even though all guest registers, syscall
//! results, and outputs are identical. This is instrumentation-only
//! nondeterminism -- directly analogous to the trap-flag / scheduler exclusions
//! already documented for chaos trace replay. We exclude it with the opt-in
//! `--ignore-line syscall=execve` flag. The workloads below do not themselves
//! call `execve`, so this only drops the bootstrap record.
//!
//! ## PMU gating
//!
//! Recording and replaying a preemption schedule uses PMU-backed deterministic
//! preemption. Following the convention of the chaos tests in
//! `hermit_modes.rs`, the whole matrix is skipped (with a visible message) when
//! hardware performance counters are unavailable, so `cargo test` stays green on
//! hosts without an accessible PMU.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

/// Serializes every `hermit-verify` invocation in this file: each one launches
/// two full Hermit containers, so running them concurrently would oversubscribe
/// the machine and perturb scheduling.
static REPLAY_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<Vec<Workload>> = OnceLock::new();

/// A guest program plus which replay tiers it is known to be deterministic
/// under. Every workload here passes the two `trace-replay` tiers; a subset also
/// passes the stricter `chaos-replay` tier (which additionally compares the full
/// scheduler DETLOG stream and the recorded schedules).
struct Workload {
    name: &'static str,
    path: PathBuf,
    /// Whether this workload is deterministic under `chaos-replay`.
    chaos_replay_ok: bool,
}

/// `(name, source-relative-to-repo, no_libc, chaos_replay_ok)`.
///
/// `nanosleep_parallel` is deliberately excluded from `chaos-replay`: its
/// parallel timed sleeps produce a genuinely divergent chaos schedule on replay
/// (a different number of scheduler turns), which is a real coverage gap rather
/// than instrumentation noise.
const WORKLOAD_SPECS: &[(&str, &str, bool, bool)] = &[
    ("getpid", "tests/c/getpid.c", false, true),
    ("uname", "tests/c/uname.c", false, true),
    ("sysinfo", "tests/c/sysinfo.c", false, true),
    ("sysinfo_uptime", "tests/c/sysinfo_uptime.c", false, true),
    ("wait_on_child", "tests/c/wait_on_child.c", false, true),
    ("hello_alarm", "tests/c/hello_alarm.c", false, true),
    ("clone", "tests/c/clone.c", false, true),
    (
        "printf_with_threads",
        "tests/c/printf_with_threads.c",
        false,
        true,
    ),
    (
        "thread_exhaustion",
        "tests/c/threadExhaustion.c",
        false,
        true,
    ),
    (
        "minimal_hello",
        "tests/c/simple/hello_nostdlib.c",
        true,
        true,
    ),
    (
        "nanosleep_parallel",
        "tests/c/nanosleep-par.c",
        false,
        false,
    ),
];

fn replay_lock() -> MutexGuard<'static, ()> {
    REPLAY_LOCK
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

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// Directory holding the freshly built `hermit` (and `hermit-verify`) binaries.
fn binary_directory() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hermit"))
        .parent()
        .expect("Hermit binary should have a parent directory")
}

fn compile_c(source: &Path, output: &Path, no_libc: bool) {
    let mut command = Command::new("cc");
    if no_libc {
        command.args(["-g", "-nostdlib"]);
    } else {
        command
            .args(["-O0", "-g", "-pthread", "-D_GNU_SOURCE"])
            .arg("-I")
            .arg(
                source
                    .parent()
                    .expect("C workload source should have a parent directory"),
            );
    }
    command.arg(source).arg("-o").arg(output);
    command_output(command, "replay-matrix C workload compilation");
}

fn workloads() -> &'static [Workload] {
    WORKLOADS.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("replay-matrix-workloads");
        fs::create_dir_all(&build_root).expect("failed to create replay-matrix build directory");
        WORKLOAD_SPECS
            .iter()
            .map(|&(name, source, no_libc, chaos_replay_ok)| {
                let path = build_root.join(name);
                compile_c(&repository().join(source), &path, no_libc);
                Workload {
                    name,
                    path,
                    chaos_replay_ok,
                }
            })
            .collect()
    })
}

/// Locate the `hermit-verify` binary that sits alongside the `hermit` binary,
/// building it on demand. `CARGO_BIN_EXE_*` is only defined for the current
/// package's own binaries, so -- like the guest-binary builds in the other
/// integration tests -- we invoke Cargo once to produce it.
fn hermit_verify_binary() -> &'static Path {
    static HERMIT_VERIFY: OnceLock<PathBuf> = OnceLock::new();
    HERMIT_VERIFY.get_or_init(|| {
        let binary = binary_directory().join("hermit-verify");
        if !binary.is_file() {
            let mut command = Command::new(env!("CARGO"));
            command.current_dir(repository()).args([
                "build",
                "-p",
                "hermit-verify",
                "--bin",
                "hermit-verify",
            ]);
            command_output(command, "hermit-verify build");
        }
        assert!(
            binary.is_file(),
            "missing hermit-verify binary: {}",
            binary.display()
        );
        binary
    })
}

/// Skip the matrix (returning `false`) when hardware performance counters are
/// not accessible, since preemption record/replay depends on the PMU.
fn perf_available(tier: &str) -> bool {
    if reverie_ptrace::is_perf_supported() {
        true
    } else {
        eprintln!(
            "SKIP {tier}: hardware perf counters are unavailable; preemption \
             record/replay requires an accessible PMU."
        );
        false
    }
}

/// Run one `hermit-verify` verification and assert it reports success. `HERMIT_BIN`
/// points the verifier at the freshly built `hermit`; the guest path is absolute
/// so it resolves inside the container.
fn verify(tier: &str, workload: &Workload, mode_args: &[&str]) {
    let mut command = Command::new(hermit_verify_binary());
    command.env("HERMIT_BIN", binary_directory().join("hermit"));
    command.args(mode_args);
    command.args(["--ignore-line", "syscall=execve"]);
    command.arg(&workload.path);

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {tier} for {}: {error}", workload.name));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains("Success!"),
        "{tier} verification for {} was not deterministic: {rendered}\nstatus: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        workload.name,
        output.status,
    );
}

#[test]
fn trace_replay_matrix() {
    let tier = "trace-replay";
    if !perf_available(tier) {
        return;
    }
    let _guard = replay_lock();
    for workload in workloads() {
        verify(
            tier,
            workload,
            &["trace-replay", "--strip-times", "--isolate-workdir"],
        );
    }
}

#[test]
fn trace_replay_chaos_matrix() {
    let tier = "trace-replay --chaos";
    if !perf_available(tier) {
        return;
    }
    let _guard = replay_lock();
    for workload in workloads() {
        verify(
            tier,
            workload,
            &[
                "trace-replay",
                "--chaos",
                "--strip-times",
                "--isolate-workdir",
            ],
        );
    }
}

#[test]
fn chaos_replay_matrix() {
    let tier = "chaos-replay";
    if !perf_available(tier) {
        return;
    }
    let _guard = replay_lock();
    for workload in workloads().iter().filter(|w| w.chaos_replay_ok) {
        verify(tier, workload, &["chaos-replay", "--isolate-workdir"]);
    }
}
