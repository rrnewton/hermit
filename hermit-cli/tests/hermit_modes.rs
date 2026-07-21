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
static WORKLOADS: OnceLock<Workloads> = OnceLock::new();

#[derive(Debug)]
struct Workload {
    name: &'static str,
    path: PathBuf,
}

struct Workloads {
    stable: Vec<Workload>,
    hello_race: Workload,
}

#[derive(Clone, Copy)]
enum RunMode {
    Default,
    Strict,
    Chaos,
    Verify,
}

impl RunMode {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Strict => "strict",
            Self::Chaos => "chaos",
            Self::Verify => "verify",
        }
    }
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

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn compile_c(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation");
}

fn compile_rust(source: &Path, output: &Path) {
    let mut command = Command::new("rustc");
    command
        .args(["--edition=2024", "-C", "debuginfo=1"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "Rust workload compilation");
}

fn workloads() -> &'static Workloads {
    WORKLOADS.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-wave1-workloads");
        fs::create_dir_all(&build_root).expect("failed to create workload build directory");

        let c_sources = [
            ("getpid", "getpid.c"),
            ("uname", "uname.c"),
            ("sysinfo", "sysinfo.c"),
            ("wait_on_child", "wait_on_child.c"),
            ("nanosleep_parallel", "nanosleep-par.c"),
        ];
        let stable = c_sources
            .into_iter()
            .map(|(name, source_name)| {
                let path = build_root.join(name);
                compile_c(&repository.join("tests/c").join(source_name), &path);
                Workload { name, path }
            })
            .collect();

        let hello_race = Workload {
            name: "hello_race",
            path: build_root.join("hello_race"),
        };
        compile_rust(
            &repository.join("flaky-tests/hello_race.rs"),
            &hello_race.path,
        );

        Workloads { stable, hello_race }
    })
}

fn hermit_run(mode: RunMode, workload: &Workload) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        "run",
        "--base-env=minimal",
        "--no-virtualize-cpuid",
        "--preemption-timeout=disabled",
    ]);
    match mode {
        RunMode::Default => {
            command.args(["--no-sequentialize-threads", "--no-deterministic-io"]);
        }
        RunMode::Strict => {}
        RunMode::Chaos => {
            command.arg("--chaos");
        }
        RunMode::Verify => {
            command.arg("--verify");
        }
    }
    command
        .arg(format!("--env=HERMIT_MODE={}", mode.name()))
        .arg(&workload.path);
    command_output(
        command,
        &format!("{} mode for {}", mode.name(), workload.name),
    );
}

fn run_stable_matrix(mode: RunMode) {
    let _guard = hermit_run_lock();
    for workload in &workloads().stable {
        hermit_run(mode, workload);
    }
}

#[test]
fn default_mode_matrix() {
    run_stable_matrix(RunMode::Default);
}

#[test]
fn strict_mode_matrix() {
    run_stable_matrix(RunMode::Strict);
}

#[test]
fn chaos_mode_matrix() {
    run_stable_matrix(RunMode::Chaos);
}

#[test]
fn verify_mode_matrix() {
    run_stable_matrix(RunMode::Verify);
}

#[test]
fn hello_race_chaos_verify() {
    let _guard = hermit_run_lock();
    let workload = &workloads().hello_race;
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args([
            "run",
            "--verify",
            "--verify-allow=both",
            "--chaos",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--env=HERMIT_MODE=chaos",
        ])
        .arg(&workload.path);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Hermit propagates the guest status even when --verify-allow=both accepts it.
    assert!(
        output.status.code().is_some() && stderr.contains("Success: deterministic."),
        "chaos verification for hello_race failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{stderr}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
    );
}
