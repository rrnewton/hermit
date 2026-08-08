/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! THE BEHAVIOURAL BRACKET for the determinized `chown` family (#1849).
//!
//! The unit tests in `detcore::syscall_classification` pin which syscalls are
//! in the ownership-change set. They cannot see what the dispatch arm returns:
//! measured, both mutating the arm's body to `Err(EPERM)` and deleting the arm
//! outright leave those tests green. This test is what fails.
//!
//! It asserts both halves of the contract in one guest run:
//!
//! * the identity half — a virtual root's ownership change succeeds for any
//!   uid, so an arm that returns `EPERM` (or that never emulates at all) fails
//!   here;
//! * the argument half — `ENOENT`, `EBADF`, `ENOTDIR` and the `fchownat` flag
//!   `EINVAL` still reach the guest, so an arm that returns an unconditional
//!   `Ok(0)` fails here.
//!
//! Run under both namespace configurations, because the pre-#1849 failure mode
//! was different in each: `EPERM` for everything with `--no-namespace`, and
//! `EINVAL` for an unmapped uid under the default one-uid `uid_map`.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

const SUCCESS_MARKER: &str = "chown-virtual-root-identity-ok";

/// A healthy run is a few syscalls; this only has to be generous enough never
/// to fire on a loaded box.
const TIMEOUT_SECONDS: u64 = 60;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

fn guest() -> &'static Path {
    GUEST
        .get_or_init(|| {
            let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("hermit-cli should be inside the repository");
            let build_root =
                Path::new(env!("CARGO_TARGET_TMPDIR")).join("chown-virtual-root-identity");
            fs::create_dir_all(&build_root).expect("failed to create build directory");
            let output = build_root.join("chown_virtual_root_identity");
            let mut command = Command::new("cc");
            command
                .args([
                    "-std=c11",
                    "-O2",
                    "-g",
                    "-D_GNU_SOURCE",
                    "-Wall",
                    "-Wextra",
                    "-Werror",
                ])
                .arg(repository.join("tests/c/chown_virtual_root_identity.c"))
                .arg("-o")
                .arg(&output);
            let status = command
                .status()
                .expect("failed to run cc to build chown_virtual_root_identity guest");
            assert!(status.success(), "guest compilation failed: {command:?}");
            output
        })
        .as_path()
}

/// The guest writes into its working directory, so give each configuration its
/// own scratch directory under the target dir rather than the repository.
fn scratch(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("chown-virtual-root-identity")
        .join(tag);
    fs::create_dir_all(&dir).expect("failed to create scratch directory");
    dir
}

fn run_guest(tag: &str, extra: &[&str]) {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"))
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--strict",
        ])
        .args(extra)
        .arg("--")
        .arg(guest())
        .current_dir(scratch(tag));

    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start guest: {rendered}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    // The guest prints one `ok`/`FAIL` line per check, so a failure report names
    // the exact call, the expected errno, and the observed one.
    assert!(
        output.status.success(),
        "chown virtual-root identity contract violated ({tag}): {rendered}\n\
         status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status,
    );
    assert!(
        stdout.contains(SUCCESS_MARKER),
        "guest did not report success ({tag}): {rendered}\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// Default configuration: a user namespace whose `uid_map` maps exactly one id.
/// Before #1849 this returned `EINVAL` for any uid other than 0.
#[test]
fn chown_family_keeps_virtual_root_identity_and_real_errors() {
    run_guest("default", &[]);
}

/// No user namespace at all. Before #1849 the whole family returned `EPERM`
/// here, including `chown(path, 0, 0)` — the configuration in which the old
/// model was most visibly incoherent (`getuid` reported 0 while `stat`
/// reported the real host uid).
#[test]
fn chown_family_keeps_virtual_root_identity_without_a_user_namespace() {
    run_guest("no-namespace", &["--no-namespace"]);
}
