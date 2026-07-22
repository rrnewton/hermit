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

const DETERMINISM_RUNS: usize = 5;

static HERMIT_PROCESS_LOCK: Mutex<()> = Mutex::new(());
static PROCESS_FIXTURES: OnceLock<ProcessFixtures> = OnceLock::new();

struct ProcessFixtures {
    driver: PathBuf,
    chain: [PathBuf; 3],
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

fn hermit_process_lock() -> MutexGuard<'static, ()> {
    HERMIT_PROCESS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn compile_c(source: &Path, output: &Path, define: Option<&str>) {
    let mut command = Command::new("cc");
    command.args([
        "-O0",
        "-g",
        "-pthread",
        "-D_GNU_SOURCE",
        "-std=c11",
        "-Wall",
        "-Wextra",
        "-Werror",
    ]);
    if let Some(define) = define {
        command.arg(format!("-D{define}"));
    }
    command.arg(source).arg("-o").arg(output);
    command_output(command, "fork/exec guest compilation");
}

fn process_fixtures() -> &'static ProcessFixtures {
    PROCESS_FIXTURES.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("fork-exec-determinism");
        fs::create_dir_all(&build_root).expect("failed to create fork/exec build directory");

        let driver = build_root.join("fork_exec_determinism");
        compile_c(
            &repository.join("tests/c/fork_exec_determinism.c"),
            &driver,
            None,
        );

        let chain_source = repository.join("tests/c/exec_chain.c");
        let chain = [
            build_root.join("exec_chain_a"),
            build_root.join("exec_chain_b"),
            build_root.join("exec_chain_c"),
        ];
        for (index, binary) in chain.iter().enumerate() {
            compile_c(
                &chain_source,
                binary,
                Some(&format!("CHAIN_STAGE={}", index + 1)),
            );
        }

        ProcessFixtures { driver, chain }
    })
}

fn run_process_scenario(program: &Path, args: &[&str], expected_stdout: &str) {
    let _guard = hermit_process_lock();
    let mut baseline = None;

    for iteration in 0..DETERMINISM_RUNS {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command.args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--preemption-timeout=disabled",
            "--",
        ]);
        command.arg(program).args(args);
        let output = command_output(
            command,
            &format!("process scenario, iteration {}", iteration + 1),
        );
        assert_eq!(
            output.stdout,
            expected_stdout.as_bytes(),
            "unexpected process output on iteration {}\nstderr:\n{}",
            iteration + 1,
            String::from_utf8_lossy(&output.stderr),
        );

        if let Some(first) = &baseline {
            assert_eq!(
                &output.stdout,
                first,
                "process output changed on iteration {}",
                iteration + 1,
            );
        } else {
            baseline = Some(output.stdout);
        }
    }
}

fn run_driver_scenario(scenario: &str, expected_stdout: &str) {
    run_process_scenario(&process_fixtures().driver, &[scenario], expected_stdout);
}

#[test]
fn fork_exec_inherits_fd_environment_and_cwd() {
    run_driver_scenario(
        "inherited-exec",
        "exec inherited env cwd fd\nfd contents=parent+child\n",
    );
}

#[test]
fn clone_vfork_exec_is_deterministic() {
    run_driver_scenario(
        "vfork-exec",
        "vfork child reached exec=1\nvfork child status=0\n",
    );
}

#[test]
fn multi_fork_wait_order_is_deterministic() {
    run_driver_scenario(
        "multi-fork",
        "wait child=0 status=10\nwait child=1 status=11\nwait child=2 status=12\nwait child=3 status=13\n",
    );
}

#[test]
fn exec_chain_across_distinct_binaries_is_deterministic() {
    let fixtures = process_fixtures();
    let chain_b = fixtures.chain[1]
        .to_str()
        .expect("exec chain B path is not UTF-8");
    let chain_c = fixtures.chain[2]
        .to_str()
        .expect("exec chain C path is not UTF-8");
    run_process_scenario(
        &fixtures.chain[0],
        &[chain_b, chain_c],
        "chain a\nchain b\nchain c\n",
    );
}

#[test]
fn posix_spawn_patterns_are_deterministic() {
    run_driver_scenario(
        "posix-spawn",
        "posix_spawn output=spawn child env=spawned\nposix_spawnp status=7\n",
    );
}

#[test]
fn forked_signal_delivery_order_is_deterministic() {
    run_driver_scenario(
        "fork-signal",
        "fork signal handler\nfork signal phase=2 deliveries=1 child=0\n",
    );
}
