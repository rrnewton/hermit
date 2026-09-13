/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/vectored_io_guest.rs"]
mod vectored_io_guest;

use std::fs;
use std::process::Command;
use std::sync::Mutex;

use vectored_io_guest::command_output;
use vectored_io_guest::compile_guest;

static KVM_RUN_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn run_kvm_current_position_vectored_io_matches_linux() {
    let _guard = KVM_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (_guest_dir, guest) = compile_guest();
    let report_dir = tempfile::tempdir().expect("failed to create KVM verification directory");
    let report_path = report_dir.path().join("verify.json");
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after", "10s", "90s"])
        .arg(hermit_test::hermit_binary())
        .args([
            "--log=info",
            "run",
            "--backend=kvm",
            "--strict",
            "--verify",
            "--verify-strict",
            "--base-env=minimal",
        ])
        .arg(format!("--verify-json={}", report_path.display()))
        .arg("--")
        .arg(&guest);
    let output = command_output(command, "KVM current-position vectored I/O");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for marker in [
        "preadv2-pwritev2-nowait-and-errors-ok",
        "vectored-self-alias-ok",
        "preadv2-snapshot-ok",
        "pwritev2-atomic-snapshot-ok",
        "pwritev2-large-ok",
        "preadv2-signal-ok",
        "pwritev2-signal-ok",
        "vectored-descriptor-matrix-ok",
        "preadv2-pwritev2-pipe-ok",
    ] {
        assert!(
            stdout.contains(marker),
            "KVM guest omitted {marker}\n{stdout}"
        );
    }
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_path).expect("KVM vectored verification report was not written"),
    )
    .expect("KVM vectored verification report is valid JSON");
    assert_eq!(report["verdict"], "matched", "verify report: {report}");
    assert_eq!(report["verified"], true, "verify report: {report}");
    assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["compare_io_buffers"], true,
        "verify report: {report}"
    );
}
