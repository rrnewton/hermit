/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::Output;

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

#[test]
fn kernel_keyring_is_deterministically_unavailable() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kernel-keyring");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let cases = [
        ("add_key", repository.join("tests/c/add_key_enosys.c")),
        (
            "request_key",
            repository.join("tests/c/request_key_enosys.c"),
        ),
        ("keyctl", repository.join("tests/c/keyctl_enosys.c")),
    ];

    for (syscall, source) in cases {
        let guest = build_root.join(format!("{syscall}_enosys"));
        let mut compile = Command::new("cc");
        compile
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(source)
            .arg("-o")
            .arg(&guest);
        command_output(compile, &format!("{syscall} guest compilation"));

        let mut verify = Command::new("timeout");
        verify
            .args(["--kill-after", "5s", "90s"])
            .arg(env!("CARGO_BIN_EXE_hermit"))
            .args([
                "--log=info",
                "run",
                "--backend=ptrace",
                "--strict",
                "--verify",
                "--panic-on-unsupported-syscalls",
                "--base-env=minimal",
                "--",
            ])
            .arg(&guest);
        let output = command_output(verify, &format!("{syscall} strict verification"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
            "{syscall} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
}

/// Extracts the single `keyctl_enosys=<0|1>` line printed by the
/// `keyctl_passthrough` guest, panicking with full context if it is absent.
fn keyctl_enosys_flag(output: &Output, label: &str) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|line| line.starts_with("keyctl_enosys="))
        .unwrap_or_else(|| {
            panic!(
                "{label} did not report a keyctl_enosys flag\nstdout:\n{stdout}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        })
        .to_string()
}

/// Ordinary runs must keep host keyrings outside the deterministic model. The
/// explicit compatibility opt-out preserves the old host pass-through for rr
/// workloads that require it. The opt-out check is host-independent: it compares
/// the guest's ENOSYS-or-not verdict natively against the same guest under Hermit.
#[test]
fn kernel_keyring_fails_closed_by_default_and_opt_out_matches_host() {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("kernel-keyring-passthrough");
    fs::create_dir_all(&build_root).expect("failed to create guest build directory");

    let source = repository.join("tests/c/keyctl_passthrough.c");
    let guest = build_root.join("keyctl_passthrough");
    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(&source)
        .arg("-o")
        .arg(&guest);
    command_output(compile, "keyctl passthrough guest compilation");

    // Native baseline: whatever the host kernel reports for keyctl.
    let native = Command::new(&guest);
    let native_output = command_output(native, "native keyctl passthrough");
    let native_flag = keyctl_enosys_flag(&native_output, "native keyctl passthrough");

    let mut default = Command::new("timeout");
    default
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let default_output = command_output(default, "default Hermit keyctl boundary");
    assert_eq!(
        keyctl_enosys_flag(&default_output, "default Hermit keyctl boundary"),
        "keyctl_enosys=1",
        "default Hermit exposed the host keyring"
    );

    let mut compatibility = Command::new("timeout");
    compatibility
        .args(["--kill-after", "5s", "90s"])
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "--log=off",
            "run",
            "--backend=ptrace",
            "--allow-unsupported-syscalls",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest);
    let compatibility_output =
        command_output(compatibility, "explicit keyctl compatibility passthrough");
    let compatibility_flag = keyctl_enosys_flag(
        &compatibility_output,
        "explicit keyctl compatibility passthrough",
    );

    assert_eq!(
        compatibility_flag, native_flag,
        "compatibility opt-out did not preserve the host result: \
         native reported `{native_flag}`, Hermit reported `{compatibility_flag}`"
    );
}
