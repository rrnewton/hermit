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

#[path = "src/liteinst_artifact.rs"]
pub mod liteinst_artifact;

use build_support::build_date;
use build_support::git_short_sha;
use build_support::git_watch_paths;

fn main() {
    let sha = git_short_sha();
    let date = build_date();

    println!("cargo:rustc-env=HERMIT_BUILD_GIT_SHA={sha}");
    println!("cargo:rustc-env=HERMIT_BUILD_DATE={date}");

    // EMBED THE REVERIE PIN THIS BINARY WAS BUILT AGAINST.
    //
    // The staged LiteInst/DBT runtimes are built separately and can be
    // ARBITRARILY STALE relative to the pin the tree declares, with nothing
    // reporting it. A cell then measures whichever `.so` happens to be on disk
    // and publishes a verdict about the pin it believes it is testing.
    //
    // Embedding the pin here is what lets the loader compare, at the moment it
    // resolves a staged runtime, the revision the runtime was built from against
    // the revision this binary was built from. Provenance recorded beside the
    // artifact is not enough on its own -- `sabre.revision` has been written for
    // some time and is read by nothing, which is provenance rather than
    // authority.
    let reverie_pin = build_support::reverie_pin();
    println!("cargo:rustc-env=HERMIT_REVERIE_PIN={reverie_pin}");
    println!("cargo:rerun-if-changed=../detcore/Cargo.toml");

    // Re-run when the checked-out revision or index moves. Arbitrary tracked
    // worktree files are intentionally not added as explicit watches: avoiding
    // one Cargo dependency per file keeps incremental builds fast, and staging
    // an edit refreshes the embedded dirty marker through the watched index.
    for path in git_watch_paths() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    emit_liteinst_identity(&reverie_pin);
}

fn emit_liteinst_identity(pin: &str) {
    use std::path::PathBuf;
    for name in [
        "HERMIT_LITEINST_SOURCE_RECORD",
        "HERMIT_LITEINST_DIAGNOSTIC",
        "HERMIT_LITEINST_RUNTIME_KIND",
        "HERMIT_LITEINST_PRIVATE_INPUTS",
        "HERMIT_LITEINST_HERMIT_ROOT",
        "HERMIT_LITEINST_REVERIE_ROOT",
        "HERMIT_LITEINST_CLI_MANIFEST",
        "HERMIT_LITEINST_DSO_MANIFEST",
        "HERMIT_LITEINST_CARGO_CONFIG",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    let Some(source) = std::env::var_os("HERMIT_LITEINST_SOURCE_RECORD") else {
        println!("cargo:rustc-env=HERMIT_LITEINST_SOURCE_SHA256=unknown");
        println!("cargo:rustc-env=HERMIT_LITEINST_DIAGNOSTIC_BUILD=0");
        println!("cargo:rustc-env=HERMIT_LITEINST_RESOLVED_REVERIE_REV=unknown");
        return;
    };
    let source = PathBuf::from(source)
        .canonicalize()
        .expect("source record path");
    let required = |name| {
        PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is required")))
    };
    let hermit = required("HERMIT_LITEINST_HERMIT_ROOT");
    let reverie = required("HERMIT_LITEINST_REVERIE_ROOT");
    let cli = std::env::var_os("HERMIT_LITEINST_CLI_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| hermit.join("hermit-cli/Cargo.toml"));
    let dso = std::env::var_os("HERMIT_LITEINST_DSO_MANIFEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| hermit.join("liteinst-runtime-build/detcore-runtime/Cargo.toml"));
    let config = std::env::var_os("HERMIT_LITEINST_CARGO_CONFIG").map(PathBuf::from);
    let diagnostic = std::env::var("HERMIT_LITEINST_DIAGNOSTIC").as_deref() == Ok("1");
    let inputs = liteinst_artifact::SourceInputs {
        hermit: &hermit,
        reverie: &reverie,
        cli_manifest: &cli,
        dso_manifest: &dso,
        config: config.as_deref(),
        evidence: source.parent().unwrap(),
        pin,
        diagnostic,
    };
    let (identity, record) = liteinst_artifact::verify_source_record(&source, &inputs)
        .expect("CLI source/dependency identity");
    if liteinst_artifact::private::requested().expect("runtime build kind") {
        let native = required("HERMIT_LITEINST_PRIVATE_INPUTS");
        println!("cargo:rerun-if-changed={}", native.display());
        let inputs = liteinst_artifact::private::native_inputs(&native).expect("native inputs");
        for path in inputs.files.values().chain(inputs.headers.values()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    println!("cargo:rustc-env=HERMIT_LITEINST_SOURCE_SHA256={identity}");
    println!(
        "cargo:rustc-env=HERMIT_LITEINST_RESOLVED_REVERIE_REV={}",
        record["resolved_reverie_rev"].as_str().unwrap()
    );
    println!(
        "cargo:rustc-env=HERMIT_LITEINST_DIAGNOSTIC_BUILD={}",
        u8::from(diagnostic)
    );
    println!("cargo:rerun-if-changed={}", source.display());
    for (role, root) in [("hermit_files", &hermit), ("reverie_files", &reverie)] {
        for file in record[role].as_object().unwrap().keys() {
            println!("cargo:rerun-if-changed={}", root.join(file).display());
        }
    }
    for file in [&cli, &dso].into_iter().chain(config.iter()) {
        println!("cargo:rerun-if-changed={}", file.display());
    }
}
