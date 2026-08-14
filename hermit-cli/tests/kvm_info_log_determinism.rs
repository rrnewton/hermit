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

fn compile_guest(name: &str, extra_args: &[&str]) -> PathBuf {
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kvm-info-log-determinism");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let source = build_root.join(format!("{name}.c"));
    fs::write(&source, "int main(void) { return 0; }\n").expect("failed to write guest source");
    let binary = build_root.join(name);
    let output = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .args(extra_args)
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap_or_else(|error| panic!("failed to compile {name}: {error}"));
    assert!(
        output.status.success(),
        "failed to compile {name}:\n{}",
        String::from_utf8_lossy(&output.stderr),
    );
    binary
}

fn run_kvm_info(binary: &Path, extra_run_flags: &[&str]) -> String {
    let output = Command::new("timeout")
        .args(["--kill-after", "10s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args(["--log", "info", "--backend", "kvm"])
        .args(["run", "--strict"])
        .args(extra_run_flags)
        .arg("--")
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

    let binary = compile_guest("guest", &[]);
    let first = run_kvm_info(&binary, &[]);
    let second = run_kvm_info(&binary, &[]);

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

/// Every `DETLOG [memory]` record, in order, with the wall-clock prefix removed.
fn memory_records(log: &str) -> Vec<&str> {
    strip_wall_clock_prefix(log)
        .into_iter()
        .filter(|line| line.contains("DETLOG [memory]"))
        .collect()
}

/// KVM must produce identical stack and heap CONTENT hashes across two runs of
/// the same guest.
///
/// SCOPE, stated because it is deliberately narrow and must not be read as more
/// than it is: this uses a **statically linked** guest. A dynamically linked
/// guest FAILS this property today, and not marginally — measured on devbig030,
/// `/bin/echo hello` under KVM has 98 of 113 stack-content hashes differing run
/// to run, while ptrace has 0 of 193 differing.
///
/// The cause is known and tracked, not papered over here: the KVM backend never
/// delivers `rdtsc` to the Reverie `Tool`, so Detcore's existing virtualization
/// never runs and the guest reads a raw host-derived cycle counter, which the
/// dynamic loader then leaves on the stack. See
/// <https://github.com/rrnewton/reverie/issues/448>. A static binary executes
/// zero `rdtsc` (measured: 0, versus 10 for every dynamically linked guest
/// tested), which is exactly why the property holds here and only here.
///
/// So this test pins the boundary of what is true rather than asserting a
/// broader claim. When #448 lands, the static restriction should be removed and
/// this test should pass for a dynamic guest too — that is the intended signal.
#[test]
fn kvm_memory_hashes_repeat_for_a_static_guest() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP kvm_memory_hashes_repeat_for_a_static_guest: /dev/kvm is not present");
        return;
    }
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let binary = compile_guest("static_guest", &["-static"]);
    let flags = ["--detlog-stack", "--detlog-heap"];
    let first = run_kvm_info(&binary, &flags);
    let second = run_kvm_info(&binary, &flags);

    let first_records = memory_records(&first);
    let second_records = memory_records(&second);

    // Without this guard the equality below would pass vacuously if the flags
    // stopped emitting records at all — which is precisely how a memory
    // determinism check would silently stop checking anything.
    assert!(
        !first_records.is_empty(),
        "no DETLOG [memory] records were emitted under --detlog-stack --detlog-heap; \
         the comparison below would not discriminate anything",
    );
    assert!(
        first_records.iter().any(|r| r.contains("Stack"))
            && first_records.iter().any(|r| r.contains("Heap")),
        "expected both Stack and Heap records; got {} record(s): {:?}",
        first_records.len(),
        first_records,
    );
    assert_eq!(
        first_records.len(),
        second_records.len(),
        "KVM emitted a different number of memory records between two runs",
    );

    let divergences: Vec<(usize, &str, &str)> = first_records
        .iter()
        .zip(second_records.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, *a, *b))
        .collect();

    assert!(
        divergences.is_empty(),
        "{} of {} KVM memory content hashes differ across two runs of the same \
         static guest:\n{}",
        divergences.len(),
        first_records.len(),
        divergences
            .iter()
            .map(|(index, a, b)| format!("  record {index}:\n    run 1: {a}\n    run 2: {b}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
