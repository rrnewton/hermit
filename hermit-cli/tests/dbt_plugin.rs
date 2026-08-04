/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#![cfg(not(feature = "dbi"))]

use std::process::Command;

use hermit_plugin_protocol::EX_UNAVAILABLE;

#[test]
fn absent_plugin_exits_with_exact_install_remedy() {
    let directory = tempfile::tempdir().unwrap();
    let hermit = directory.path().join("bin/hermit");
    std::fs::create_dir_all(hermit.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_hermit"), &hermit).unwrap();
    let output = Command::new(hermit)
        .args(["--backend", "dbt", "run", "/bin/true"])
        .env("HERMIT_DIR", directory.path().join("hermit"))
        .env("CARGO_HOME", directory.path().join("cargo"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run default Hermit without a plugin");

    assert_eq!(output.status.code(), Some(EX_UNAVAILABLE));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error: backend 'dbt' is unavailable: hermit-dynamorio was not found"));
    assert!(stderr.contains("install it with:\n  cargo install --locked hermit-dynamorio"));
    assert!(stderr.contains("searched:"));
}

fn assert_absent_plugin(backend: &str, helper: &str) {
    let directory = tempfile::tempdir().unwrap();
    let hermit = directory.path().join("bin/hermit");
    std::fs::create_dir_all(hermit.parent().unwrap()).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_hermit"), &hermit).unwrap();
    let output = Command::new(hermit)
        .args(["--backend", backend, "run", "/bin/true"])
        .env("HOME", directory.path().join("home"))
        .env("HERMIT_DIR", directory.path().join("hermit"))
        .env("CARGO_HOME", directory.path().join("cargo"))
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run default Hermit without a plugin");

    assert_eq!(output.status.code(), Some(EX_UNAVAILABLE));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&format!(
        "error: backend '{backend}' is unavailable: {helper} was not found"
    )));
    assert!(stderr.contains(&format!(
        "install it with:\n  cargo install --locked {helper}"
    )));
}

#[test]
fn absent_sabre_plugin_exits_with_exact_install_remedy() {
    assert_absent_plugin("sabre", "hermit-sabre");
}

#[test]
fn absent_e9patch_plugin_exits_with_exact_install_remedy() {
    assert_absent_plugin("e9patch", "hermit-e9patch");
}
