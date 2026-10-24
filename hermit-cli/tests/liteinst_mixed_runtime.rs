/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Requires a separately finalized OFF installation from this exact source.
//! The producer runs before this bounded test; this test never builds a runtime
//! and never substitutes an inert DSO for the genuine Detcore preload.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[path = "common/hermit_binary.rs"]
mod hermit_binary;

#[test]
fn liteinst_cli_refuses_an_off_runtime_unless_patching_is_explicitly_disabled() {
    let installation = PathBuf::from(
        std::env::var_os("HERMIT_MIXED_LITEINST_OFF_INSTALLATION")
            .expect("stage a genuine OFF Detcore installation before the mixed-runtime test"),
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("Hermit repository root");
    let verified = Command::new("python3")
        .arg(root.join("ci/verify-packaged-detcore.py"))
        .arg("--without-liteinst")
        .arg(&installation)
        .output()
        .expect("verify the separately produced OFF runtime receipt");
    assert!(verified.status.success(), "{verified:?}");
    let resources = installation.join("rsrcs");
    let receipt: serde_json::Value = serde_json::from_slice(
        &fs::read(resources.join("libdetcore_liteinst.package.json"))
            .expect("read the verified OFF receipt"),
    )
    .expect("parse the verified OFF receipt");
    assert_eq!(receipt["reverie_pin"], env!("HERMIT_REVERIE_PIN"));
    assert_eq!(receipt["cargo_artifact"]["features"], serde_json::json!([]));

    for selector in [None, Some("1"), Some("0")] {
        let mut command = Command::new(hermit_binary::hermit_binary());
        command.args([
            "--backend=liteinst",
            "run",
            "--strict",
            "--max-timeslice=disabled",
            "--base-env=minimal",
        ]);
        if let Some(selector) = selector {
            command.arg(format!("--env=REVERIE_LITEINST_SITE_PATCHING={selector}"));
        }
        command.args(["--", "/bin/echo", "mixed-runtime-guest"]);
        hermit_binary::configure_guest_execution(&mut command);
        command.env_remove("REVERIE_LITEINST_SITE_PATCHING").env(
            "HERMIT_LITEINST_TOOL_RUNTIME",
            resources.join("libdetcore_liteinst.so"),
        );
        let output = command
            .output()
            .expect("run the real mixed CLI/runtime pair");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!(
            "mixed-runtime selector={selector:?} status={:?}\nstdout={stdout}\nstderr={stderr}",
            output.status
        );
        if selector == Some("0") {
            assert!(output.status.success(), "{output:?}");
            assert_eq!(stdout, "mixed-runtime-guest\n");
        } else {
            assert!(!output.status.success(), "{output:?}");
            assert!(
                stderr.contains("built without LiteInst; enable the `liteinst` Cargo feature"),
                "{stderr}"
            );
            assert!(stdout.is_empty(), "the refused guest ran: {output:?}");
        }
    }
}
