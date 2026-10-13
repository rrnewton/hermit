/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::path::Path;
use std::process::Command;

#[test]
fn native_callback_scope_retires_authority_and_refuses_live_or_nested_borrows() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let directory = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let binary = directory.path().join("native_callback_scope");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let compile = Command::new("timeout")
        .args(["--signal=TERM", "--kill-after=2s", "30s"])
        .arg(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(manifest.join("native"))
        .arg(manifest.join("tests/fixtures/native_callback_scope.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile the actual native callback scope helper");
    assert!(compile.status.success(), "{compile:?}");
    let result = Command::new("timeout")
        .args(["--signal=TERM", "--kill-after=2s", "30s"])
        .arg(binary)
        .output()
        .expect("run native callback scope lifetime assertions");
    assert!(result.status.success(), "{result:?}");
}
