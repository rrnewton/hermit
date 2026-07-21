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
use std::sync::OnceLock;

static WORKLOADS: OnceLock<Vec<Workload>> = OnceLock::new();

#[derive(Debug)]
struct Workload {
    name: &'static str,
    path: PathBuf,
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

fn compile_c(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C record workload compilation");
}

// Reuse Cargo's Nix artifact so this test can compile the existing Rust guest
// without a generated manifest edit or a recursive Cargo invocation.
fn nix_rlibs() -> Vec<PathBuf> {
    let dependency_dir = std::env::current_exe()
        .expect("failed to locate the record/replay test binary")
        .parent()
        .expect("integration test binary should be inside Cargo's deps directory")
        .to_path_buf();
    let mut candidates = fs::read_dir(&dependency_dir)
        .expect("failed to read Cargo's dependency directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("libnix-") && name.ends_with(".rlib"))
        })
        .collect::<Vec<_>>();
    candidates.sort();
    assert!(
        !candidates.is_empty(),
        "Cargo did not build a Nix rlib in {}",
        dependency_dir.display()
    );
    candidates
}

fn compile_rust_clock(source: &Path, output: &Path) {
    let dependency_dir = std::env::current_exe()
        .expect("failed to locate the record/replay test binary")
        .parent()
        .expect("integration test binary should be inside Cargo's deps directory")
        .to_path_buf();
    let mut failures = Vec::new();

    for nix_rlib in nix_rlibs() {
        let mut command = Command::new("rustc");
        command
            .args(["--edition=2024", "-C", "debuginfo=1", "-L"])
            .arg(format!("dependency={}", dependency_dir.display()))
            .arg("--extern")
            .arg(format!("nix={}", nix_rlib.display()))
            .arg(source)
            .arg("-o")
            .arg(output);
        let rendered = format!("{command:?}");
        let result = command
            .output()
            .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
        if result.status.success() {
            return;
        }
        failures.push(format!(
            "{rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            result.status,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr),
        ));
    }

    panic!(
        "failed to compile the Rust clock_gettime workload with any Cargo-built Nix rlib:\n{}",
        failures.join("\n\n")
    );
}

fn workloads() -> &'static [Workload] {
    WORKLOADS.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("record-replay-workloads");
        fs::create_dir_all(&build_root).expect("failed to create workload build directory");

        let c_sources = [
            ("c_getpid", "getpid.c"),
            ("c_uname", "uname.c"),
            ("c_sysinfo", "sysinfo.c"),
            ("c_wait_on_child", "wait_on_child.c"),
            ("c_nanosleep_parallel", "nanosleep-par.c"),
        ];
        let mut workloads = c_sources
            .into_iter()
            .map(|(name, source_name)| {
                let path = build_root.join(name);
                compile_c(&repository.join("tests/c").join(source_name), &path);
                Workload { name, path }
            })
            .collect::<Vec<_>>();

        let clock_gettime = Workload {
            name: "rs_clock_gettime",
            path: build_root.join("rs_clock_gettime"),
        };
        compile_rust_clock(
            &repository.join("tests/rust/clock_gettime.rs"),
            &clock_gettime.path,
        );
        workloads.push(clock_gettime);
        workloads
    })
}

#[test]
fn record_replay_matrix() {
    // Record/replay does not enable PMU-backed preemption, so these workloads
    // also run on GitHub-hosted runners without performance-counter access.
    for workload in workloads() {
        let data_dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("record-replay-data")
            .join(workload.name);
        fs::create_dir_all(&data_dir).expect("failed to create Hermit recording directory");

        let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
        command
            .env("HERMIT_MODE", "record")
            .args(["record", "start", "--verify"])
            .arg(format!("--data-dir={}", data_dir.display()))
            .arg("--")
            .arg(&workload.path);
        let output = command_output(command, &format!("record/replay for {}", workload.name));
        let combined_output = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            combined_output.contains("Success: replay matched recording."),
            "Hermit did not report deterministic replay for {}:\n{}",
            workload.name,
            combined_output
        );
    }
}
