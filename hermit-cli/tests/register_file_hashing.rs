/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end, two-direction proof for register-file hashing in `--verify`.
//!
//! Detcore samples the guest general-purpose register file at each
//! syscall-commit boundary and emits it as a `DETLOG [regs] ... hash=h<sha256>`
//! record (INFO-gated, canonicalized: host addresses become ordinals, other
//! values are emitted verbatim). This test exercises both required directions
//! against the running binary:
//!
//! * **Positive / benign:** two runs of the same guest with the same argument
//!   produce a byte-identical register-hash stream. Determinism holds.
//! * **Negative / planted:** the `register_marker` guest pins a distinct value
//!   into %r15 (a callee-saved register) across a `getpid` syscall while keeping
//!   stdout, exit status, and the syscall sequence identical. Two runs with
//!   different markers therefore diverge ONLY in the register file, and the
//!   register-hash stream catches it (unequal) even though stdout/exit are
//!   equal. This is a hard catch of a real register-state divergence, not a
//!   softer strip.
//!
//! Mirrors the shape of the memory/env determinism e2e tests: compile a tiny C
//! guest, run it under `hermit --log=info run --strict`, and inspect the DETLOG
//! stream on stderr.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static HERMIT_REGS_LOCK: Mutex<()> = Mutex::new(());
static REGS_GUEST: OnceLock<PathBuf> = OnceLock::new();

fn hermit_regs_lock() -> MutexGuard<'static, ()> {
    HERMIT_REGS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Compile `tests/c/register_marker.c` once and cache the binary path.
fn regs_guest() -> &'static Path {
    REGS_GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("register-file-hashing");
            fs::create_dir_all(&build_root)
                .expect("failed to create register-file-hashing build directory");
            let binary = build_root.join("register_marker");

            let mut command = Command::new("cc");
            command
                .args([
                    "-O0",
                    "-g",
                    "-D_GNU_SOURCE",
                    // gnu11: the guest uses GNU register-asm (`register ... __asm__("r15")`).
                    "-std=gnu11",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/register_marker.c"))
                .arg("-o")
                .arg(&binary);
            let output = command
                .output()
                .expect("failed to launch cc for register_marker guest");
            assert!(
                output.status.success(),
                "register_marker guest compilation failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            binary
        })
        .as_path()
}

/// Run the guest once under `hermit --log=info run --strict -- guest <marker>`.
fn run_under_hermit(marker: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.args([
        // `--verify` requires info-level logging; the register digest is emitted
        // through the same INFO determinism stream.
        "--log=info",
        "run",
        "--strict",
        // Match the other strict e2e tests: keep the test usable on VMs without
        // CPUID interception / PMU without weakening strict mode.
        "--no-virtualize-cpuid",
        "--max-timeslice=disabled",
        "--base-env=minimal",
        "--",
    ]);
    command.arg(regs_guest()).arg(marker);
    command
        .output()
        .expect("failed to launch hermit run for register_marker guest")
}

/// Extract the ordered list of `hash=h<hex>` tokens from the `DETLOG [regs]`
/// lines on stderr.
fn register_hashes(output: &Output) -> Vec<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr
        .lines()
        .filter(|line| line.contains("DETLOG [regs]"))
        .filter_map(|line| {
            line.split_whitespace()
                .find(|tok| tok.starts_with("hash=h"))
                .map(str::to_owned)
        })
        .collect()
}

#[test]
#[ignore = "e2e: requires hermit + mount namespaces + cc"]
fn register_hash_is_deterministic_and_catches_divergence() {
    let _guard = hermit_regs_lock();

    // The guest must at least reach its getpid syscall in every run.
    let first = run_under_hermit("101");
    assert!(
        first.status.success(),
        "guest failed under hermit:\nstatus: {}\nstderr:\n{}",
        first.status,
        String::from_utf8_lossy(&first.stderr),
    );
    let hashes_101_a = register_hashes(&first);
    assert!(
        !hashes_101_a.is_empty(),
        "expected at least one `DETLOG [regs]` line; stderr:\n{}",
        String::from_utf8_lossy(&first.stderr),
    );

    // ---- POSITIVE / benign: same marker => identical register-hash stream ----
    let second = run_under_hermit("101");
    let hashes_101_b = register_hashes(&second);
    assert_eq!(
        hashes_101_a, hashes_101_b,
        "register-hash stream was not deterministic across two identical runs",
    );

    // ---- NEGATIVE / planted: different %r15 marker => streams differ ----
    let other = run_under_hermit("202");
    let hashes_202 = register_hashes(&other);

    // The planted divergence is register-only: stdout and exit status are
    // identical, so a stdout/exit-based verify would see NO difference...
    assert_eq!(
        first.stdout, other.stdout,
        "guest stdout must be identical across markers (divergence is register-only)",
    );
    assert_eq!(
        first.status.code(),
        other.status.code(),
        "guest exit status must be identical across markers",
    );
    // ...but the register-file hash catches it. Same number of samples (same
    // syscall sequence), at least one hash differs.
    assert_eq!(
        hashes_101_a.len(),
        hashes_202.len(),
        "marker must not change the syscall sequence (only a register value)",
    );
    assert_ne!(
        hashes_101_a, hashes_202,
        "register-file hash failed to catch a real register-state divergence \
         (r15 marker 101 vs 202) that stdout/exit could not see",
    );
}
