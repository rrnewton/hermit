/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

static LITEINST_RUNTIME: OnceLock<()> = OnceLock::new();

pub(super) fn hermit_binary() -> PathBuf {
    std::env::var_os("HERMIT_LITEINST_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_hermit")))
}

pub(super) fn liteinst_runtime_library() -> PathBuf {
    hermit_binary()
        .parent()
        .expect("Hermit test binary should have a profile directory")
        .join("libhermit.so")
}

pub(super) fn ensure_liteinst_runtime() {
    LITEINST_RUNTIME.get_or_init(|| {
        let hermit = hermit_binary();
        let profile_dir = hermit
            .parent()
            .expect("Hermit test binary should have a profile directory");
        let profile = profile_dir
            .file_name()
            .expect("Hermit profile directory should have a name");
        let cargo_profile = if profile == OsStr::new("debug") {
            OsStr::new("dev")
        } else {
            profile
        };
        let target_dir = profile_dir
            .parent()
            .expect("Hermit profile should be inside a target directory");
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let runtime_target = target_dir.join("liteinst-tool-build");
        let runtime = liteinst_runtime_library();
        if runtime.is_file() {
            return;
        }
        let output = Command::new(env!("CARGO"))
            .current_dir(repository)
            .args(["build", "--package", "hermit", "--lib", "--profile"])
            .arg(cargo_profile)
            .env("CARGO_TARGET_DIR", &runtime_target)
            .output()
            .expect("failed to build the LiteInst Detcore tool DSO");
        assert!(
            output.status.success(),
            "LiteInst tool DSO build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let artifact_profile = if cargo_profile == OsStr::new("dev") {
            OsStr::new("debug")
        } else {
            cargo_profile
        };
        let built = runtime_target.join(artifact_profile).join("libhermit.so");
        std::fs::copy(&built, &runtime).unwrap_or_else(|error| {
            panic!(
                "failed to stage LiteInst tool {} as {}: {error}",
                built.display(),
                runtime.display()
            )
        });
        assert!(
            runtime.is_file(),
            "LiteInst tool build did not stage {}",
            runtime.display(),
        );
    });
}
