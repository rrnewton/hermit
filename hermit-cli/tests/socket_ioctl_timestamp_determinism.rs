/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::path::Path;
use std::process::Command;

/// The line Hermit prints when it runs the guest under in-guest LiteInst.
#[cfg(feature = "liteinst")]
const IN_GUEST_SELECTED: &str =
    "hermit: [liteinst in-guest] selected: the guest preload is to host the Detcore Tool";

#[cfg(feature = "liteinst")]
const IN_GUEST_BACKEND: &str = "liteinst";
#[cfg(all(feature = "in-guest-trap", not(feature = "liteinst")))]
const IN_GUEST_BACKEND: &str = "in-guest-trap";
#[cfg(all(feature = "in-guest-trap", not(feature = "liteinst")))]
const IN_GUEST_SELECTED: &str = "hermit: [in-guest-trap] selected: the guest preload is to host the Detcore Tool, \
     with syscall site patching off (REVERIE_LITEINST_SITE_PATCHING=0)";

/// Runs `guest` twice under the enabled in-guest backend and returns its stdout.
/// The existing LiteInst and trap-only entrypoints use the same epoch, status,
/// banner, nonempty-output and exact second-run parity assertions. It refuses a maximum timeslice
/// until it can deliver Detcore's preemption timer, so these runs have none,
/// and they do not pass `--verify` yet. The guest checks its own timestamps
/// against logical time and fails if one escapes; the second run must print the
/// same output. Both runs pin
/// the same `--epoch`, as `--verify` pins one epoch for its two runs; without
/// it each run starts its virtual clock at the host's current time.
#[cfg(feature = "in-guest-trap")]
fn run_in_guest_twice(guest: &Path, args: &[&str], label: &str) -> String {
    let run = || {
        let mut command = Command::new("timeout");
        command
            .args(["--kill-after", "5s", "90s"])
            .arg(hermit_test::hermit_binary())
            .arg("--log=info")
            .arg(format!("--backend={IN_GUEST_BACKEND}"))
            .args(["run", "--epoch=2026-01-01T00:00:00Z"])
            .args([
                "--strict",
                "--max-timeslice=disabled",
                "--base-env=minimal",
                "--",
            ])
            .arg(guest)
            .args(args);
        hermit_test::configure_guest_execution(&mut command);
        let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{IN_GUEST_BACKEND}/{label} failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            output.status
        );
        assert!(
            stderr.lines().any(|line| line == IN_GUEST_SELECTED),
            "{IN_GUEST_BACKEND}/{label} did not select its in-guest runtime\nstderr:\n{stderr}"
        );
        stdout
    };
    let first = run();
    assert!(
        !first.is_empty(),
        "{IN_GUEST_BACKEND}/{label} printed nothing"
    );
    assert_eq!(
        run(),
        first,
        "{IN_GUEST_BACKEND}/{label} printed different output on its second run"
    );
    first
}

fn build_socket_ioctl_guest() -> std::path::PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("socket-ioctl-timestamp-{}", std::process::id()));
    std::fs::create_dir_all(&build_root).expect("failed to create guest build directory");
    let guest = build_root.join("socket-ioctl-timestamp");

    let compile = Command::new("cc")
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/socket_ioctl_timestamp.c"))
        .arg("-o")
        .arg(&guest)
        .output()
        .expect("failed to compile socket ioctl timestamp guest");
    assert!(
        compile.status.success(),
        "guest compilation failed: {}",
        String::from_utf8_lossy(&compile.stderr)
    );

    guest
}

#[test]
fn socket_timestamp_ioctls_use_logical_time() {
    let guest = build_socket_ioctl_guest();

    // The DBT leg runs only in builds that include DBT; the ptrace leg always runs.
    let backends: &[&str] = if cfg!(feature = "dbt") {
        &["ptrace", "dbt"]
    } else {
        &["ptrace"]
    };
    for backend in backends {
        for mode in ["v4-us", "v4-ns", "v6-us"] {
            let mut command = Command::new("timeout");
            command
                .args(["--kill-after", "5s", "90s"])
                .arg(hermit_test::hermit_binary())
                .arg("--log=info")
                .arg(format!("--backend={backend}"))
                .arg("run")
                .args(["--strict", "--verify", "--base-env=minimal", "--"])
                .arg(&guest)
                .arg(mode);
            hermit_test::configure_guest_execution(&mut command);
            let verify = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
            let stdout = String::from_utf8_lossy(&verify.stdout);
            let stderr = String::from_utf8_lossy(&verify.stderr);
            assert!(
                verify.status.success(),
                "{backend}/{mode} strict verification failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
                verify.status
            );
            assert!(
                stdout.contains("Determinism verified") || stderr.contains("Determinism verified"),
                "{backend}/{mode} omitted Hermit's determinism marker\nstdout:\n{stdout}\nstderr:\n{stderr}"
            );
        }
    }

    #[cfg(feature = "liteinst")]
    for mode in ["v4-us", "v4-ns", "v6-us"] {
        run_in_guest_twice(&guest, &[mode], mode);
    }

    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "5s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=off",
            "--backend=ptrace",
            "run",
            "--no-virtualize-time",
            "--no-virtualize-metadata",
            "--base-env=minimal",
            "--",
        ])
        .arg(&guest)
        .arg("v4-us");
    hermit_test::configure_guest_execution(&mut command);
    let realtime = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    assert!(
        realtime.status.success(),
        "host-clock socket timestamp case failed: {}\nstdout:\n{}\nstderr:\n{}",
        realtime.status,
        String::from_utf8_lossy(&realtime.stdout),
        String::from_utf8_lossy(&realtime.stderr)
    );
    let seconds: i64 = String::from_utf8_lossy(&realtime.stdout)
        .trim()
        .split_once('.')
        .expect("guest should print a timeval")
        .0
        .parse()
        .expect("guest should print numeric timeval seconds");
    assert!(
        seconds >= 1_704_067_200,
        "--no-virtualize-time returned the fixed logical epoch: {seconds}"
    );
}

#[test]
#[cfg(all(feature = "in-guest-trap", not(feature = "liteinst")))]
fn socket_timestamp_ioctls_use_logical_time_in_guest_trap() {
    let guest = build_socket_ioctl_guest();
    for mode in ["v4-us", "v4-ns", "v6-us"] {
        run_in_guest_twice(&guest, &[mode], mode);
    }
}
