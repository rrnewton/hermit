/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The KVM backend's INFO stream must not carry host-irreproducible values.
//!
//! `--verify-strict` compares INFO events under the canonical policy: it removes
//! the real wall-clock *prefix* of each line and compares the remainder exactly.
//! Any host wall-clock value inside a message *body* therefore makes two runs of
//! the same guest differ on that line alone, with no guest cause.
//!
//! This is currently invisible through `--verify` itself, because the KVM path
//! takes an output-only fallback (`compare_logs: false`, reported as
//! `bitwise_parity: false`) and never compares the internal stream. This test
//! compares it directly so the defect cannot hide behind that fallback.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// KVM runs are serialized for the same reason `kvm_harder.rs` serializes them.
static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

/// Strip the leading RFC3339 wall-clock timestamp that `tracing` prefixes to
/// every line. This is exactly the `real-wall-clock-prefix/v1` removal the
/// canonical comparison policy performs, and nothing more: the remainder of the
/// line is compared verbatim, so a nondeterministic value in a message body is
/// still caught.
fn strip_wall_clock_prefix(log: &str) -> Vec<&str> {
    log.lines()
        .map(|line| match line.split_once(char::is_whitespace) {
            // A timestamp token looks like `2026-08-14T03:41:30.792057Z`.
            Some((first, rest))
                if first.len() >= 20
                    && first.ends_with('Z')
                    && first.starts_with(|c: char| c.is_ascii_digit()) =>
            {
                rest
            }
            _ => line,
        })
        .collect()
}

fn compile_guest() -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-info-log-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let source = build_root.join("guest.c");
    fs::write(&source, "int main(void) { return 0; }\n").expect("failed to write guest source");
    let binary = build_root.join("guest");
    let output = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile guest: {error}"));
    assert!(
        output.status.success(),
        "failed to compile guest:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn run_kvm_info(binary: &Path) -> String {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", "info", "--backend", "kvm"])
        .args(["run", "--strict", "--"])
        .arg(binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to run guest under KVM: {error}"));
    assert!(
        output.status.success(),
        "KVM strict run failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn kvm_info_stream_repeats_exactly_across_two_runs() {
    // Host limitation, not a product result: without /dev/kvm there is no KVM
    // backend to measure. Report the skip rather than passing silently.
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_info_stream_repeats_exactly_across_two_runs: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest();
    let first = run_kvm_info(&binary);
    let second = run_kvm_info(&binary);

    let first_lines = strip_wall_clock_prefix(&first);
    let second_lines = strip_wall_clock_prefix(&second);

    // Guard the comparison itself: an empty or truncated capture would make the
    // equality below vacuously true, which is the classic false green.
    assert!(
        first_lines.len() > 50,
        "KVM INFO capture is implausibly short ({} lines); the comparison below \
         would not discriminate anything",
        first_lines.len(),
    );
    assert_eq!(
        first_lines.len(),
        second_lines.len(),
        "KVM INFO stream changed line count between two runs of the same guest",
    );

    let divergences: Vec<(usize, &str, &str)> = first_lines
        .iter()
        .zip(second_lines.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, *a, *b))
        .collect();

    assert!(
        divergences.is_empty(),
        "KVM INFO stream is not reproducible across two runs of the same guest. \
         Every line below differs with no guest cause, so it carries a host value \
         that must not be in the compared stream:\n{}",
        divergences
            .iter()
            .map(|(index, a, b)| format!("  line {index}:\n    run 1: {a}\n    run 2: {b}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
