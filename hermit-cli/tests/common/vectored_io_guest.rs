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

pub fn command_output(mut command: Command, label: &str) -> Output {
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

pub fn compile_guest() -> (tempfile::TempDir, std::path::PathBuf) {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(build_root).expect("failed to create guest artifact root");
    let directory = tempfile::Builder::new()
        .prefix("preadv2-pwritev2-pipe-")
        .tempdir_in(build_root)
        .expect("failed to create private p*v2 guest build directory");
    let guest = directory.path().join("preadv2_pwritev2_pipe");

    let mut compile = Command::new("cc");
    compile
        .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
        .arg(repository.join("tests/c/preadv2_pwritev2_pipe.c"))
        .arg("-o")
        .arg(&guest);
    command_output(compile, "p*v2 pipe guest compilation");
    (directory, guest)
}
