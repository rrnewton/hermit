/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Emits build metadata consumed by `hermit --version`.
//!
//! The crate version is the single source of truth in `Cargo.toml`
//! (`CARGO_PKG_VERSION`); this script only augments it with the build date and
//! the source revision so a released binary can be traced back to a commit.
//! Both values are exposed to the crate through `cargo:rustc-env` and read with
//! `env!` in `src/bin/hermit/version.rs`.
//!
//! Only the Cargo/OSS build runs this script. The fbcode (Buck) build derives
//! its version from `build_info::BuildInfo` instead, so nothing here needs to
//! work under Buck.

#[path = "build_support.rs"]
mod build_support;

use build_support::build_date;
use build_support::git_short_sha;
use build_support::git_watch_paths;

fn main() {
    let sha = git_short_sha();
    let date = build_date();

    println!("cargo:rustc-env=HERMIT_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=HERMIT_BUILD_DATE={date}");

    // Re-run when the revision, index, or a tracked worktree file changes so
    // the embedded provenance stays accurate. Untracked generated output is
    // deliberately excluded by build_support.
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    // Runtime returns from #[test] functions are reported as passes by
    // libtest. Decide availability before compiling the KVM-only integration
    // tests so an unavailable device is an explicit ignored/SKIPPED result.
    // The override exists only to bracket the unavailable-device decision.
    let kvm_device = std::env::var_os("HERMIT_KVM_TEST_DEVICE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/dev/kvm"));
    println!("cargo:rustc-check-cfg=cfg(hermit_kvm_tests_available)");
    println!("cargo:rerun-if-env-changed=HERMIT_KVM_TEST_DEVICE");
    println!("cargo:rerun-if-changed={}", kvm_device.display());
    if std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&kvm_device)
        .is_ok()
    {
        println!("cargo:rustc-cfg=hermit_kvm_tests_available");
    }

    // Same defect, same cure, one level down. Six of the KVM-only tests also
    // shell out to an auxiliary tool and guarded it with a runtime `return`,
    // which libtest scores as a PASS -- so on a host without `setpriv` those
    // tests announced success having executed nothing. Decide availability
    // here instead, so a missing tool becomes an explicit ignored/SKIPPED.
    //
    // These requirements are SURFACED, not relaxed: nothing here lets a test
    // run without its tool, it only makes the absence visible.
    //
    // HERMIT_TEST_MISSING_TOOLS is a comma-separated list of tool names to
    // treat as absent. It exists only to bracket the unavailable-tool
    // decision on a host that happens to have everything, mirroring
    // HERMIT_KVM_TEST_DEVICE above. It can only make a present tool look
    // missing, never the reverse.
    let forced_missing = std::env::var("HERMIT_TEST_MISSING_TOOLS").unwrap_or_default();
    let forced_missing: Vec<&str> = forced_missing
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .collect();
    println!("cargo:rerun-if-env-changed=HERMIT_TEST_MISSING_TOOLS");
    for (tool, path) in [
        ("awk", "/usr/bin/awk"),
        ("perl", "/usr/bin/perl"),
        ("bash", "/bin/bash"),
        ("paste", "/usr/bin/paste"),
        ("diff", "/usr/bin/diff"),
        ("setpriv", "/usr/bin/setpriv"),
        ("date", "/bin/date"),
    ] {
        println!("cargo:rustc-check-cfg=cfg(hermit_test_{tool}_available)");
        println!("cargo:rerun-if-changed={path}");
        if !forced_missing.contains(&tool) && std::path::Path::new(path).exists() {
            println!("cargo:rustc-cfg=hermit_test_{tool}_available");
        }
    }
}
