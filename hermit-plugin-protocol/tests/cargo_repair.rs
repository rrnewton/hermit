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

use hermit_plugin_protocol::cargo_install_repair;

fn write_package(root: &Path, version: &str, output: &str) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        format!(
            "[package]\nname = \"hermit-dynamorio\"\nversion = \"{version}\"\nedition = \"2024\"\n"
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/main.rs"),
        format!("fn main() {{ println!(\"{output}\"); }}\n"),
    )
    .unwrap();
}

fn cargo(cargo_home: &Path, arguments: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO"))
        .args(arguments)
        .env("CARGO_HOME", cargo_home)
        .output()
        .expect("run Cargo")
}

#[test]
fn cargo_root_repair_replaces_a_genuinely_stale_selected_helper() {
    let directory = tempfile::tempdir().unwrap();
    let stale = directory.path().join("stale");
    let fresh = directory.path().join("fresh");
    let hermit_dir = directory.path().join("hermit root");
    let cargo_home = directory.path().join("cargo-home");
    write_package(&stale, "0.1.0", "stale-0.1.0");
    write_package(&fresh, "0.2.0", "fresh-0.2.0");

    for package in [&stale, &fresh] {
        let output = cargo(
            &cargo_home,
            &[
                "generate-lockfile".as_ref(),
                "--manifest-path".as_ref(),
                package.join("Cargo.toml").as_os_str(),
            ],
        );
        assert!(
            output.status.success(),
            "generate lockfile failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let install = |package: &Path| {
        cargo(
            &cargo_home,
            &[
                "install".as_ref(),
                "--path".as_ref(),
                package.as_os_str(),
                "--root".as_ref(),
                hermit_dir.as_os_str(),
                "--force".as_ref(),
                "--locked".as_ref(),
            ],
        )
    };
    let first = install(&stale);
    assert!(
        first.status.success(),
        "stale install failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let selected = hermit_dir.join("bin/hermit-dynamorio");
    let before = Command::new(&selected).output().unwrap();
    assert_eq!(before.stdout, b"stale-0.1.0\n");
    let remedy = cargo_install_repair(&selected, "hermit-dynamorio", "0.2.0").unwrap();
    assert!(remedy.contains("--root"));
    assert!(remedy.contains("repaired selected plugin:"));

    // The unpublished test package uses --path; root, force, and locked are the
    // same placement/replacement semantics as the emitted registry command.
    let repaired = install(&fresh);
    assert!(
        repaired.status.success(),
        "repair install failed: {}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let report = String::from_utf8_lossy(&repaired.stderr);
    assert!(
        report.contains("Replacing") || report.contains("Replaced"),
        "Cargo did not report replacing the stale helper: {report}"
    );

    let after = Command::new(&selected).output().unwrap();
    assert_eq!(after.stdout, b"fresh-0.2.0\n");
    assert_ne!(before.stdout, after.stdout);
}
