/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Barrier;

use hermit_plugin_protocol::EX_CANTCREAT;
use hermit_plugin_protocol::EX_CONFIG;
use hermit_plugin_protocol::EnsureRequest;
use hermit_plugin_protocol::PayloadManifest;
use hermit_plugin_protocol::PluginIdentity;

fn identity() -> PluginIdentity {
    PluginIdentity::with_abi(
        "e9patch",
        env!("CARGO_PKG_VERSION"),
        hermit_plugin_protocol::E9PATCH_ABI_TAG,
        detcore::DETCORE_BUILD_ID,
    )
}

fn ensure(root: &Path, request: &EnsureRequest) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_hermit-e9patch"))
        .arg("ensure")
        .env("HERMIT_DIR", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start helper");
    serde_json::to_writer(child.stdin.as_mut().unwrap(), request).unwrap();
    drop(child.stdin.take());
    child.wait_with_output().expect("wait for helper")
}

fn assert_success(output: &Output) -> PayloadManifest {
    assert!(
        output.status.success(),
        "helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("valid helper response")
}

#[test]
fn concurrent_first_runs_publish_one_complete_release() {
    let directory = tempfile::tempdir().unwrap();
    let root = Arc::new(directory.path().to_owned());
    let barrier = Arc::new(Barrier::new(7));
    let workers = (0..6)
        .map(|_| {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                assert_success(&ensure(&root, &EnsureRequest { host: identity() }))
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let manifests = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect::<Vec<_>>();

    assert!(manifests.windows(2).all(|pair| pair[0] == pair[1]));
    let expected = root
        .join("plugins/e9patch/releases")
        .join(identity().release_key());
    assert_eq!(
        fs::canonicalize(&expected).unwrap(),
        manifests[0].release_dir
    );
    assert!(manifests[0].e9tool.is_file());
    assert!(manifests[0].e9patch.is_file());

    let plugin_root = root.join("plugins/e9patch");
    assert_eq!(
        fs::canonicalize(plugin_root.join("current")).unwrap(),
        fs::canonicalize(expected).unwrap()
    );
    assert!(
        fs::read_dir(plugin_root.join("releases"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".extract-"))
    );
}

#[test]
fn exact_release_key_invalidates_stale_current_symlink() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let plugin_root = root.join("plugins/e9patch");
    let stale = plugin_root.join("releases/0.1.0/stale");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("not-a-plugin"), b"stale").unwrap();
    symlink("releases/0.1.0/stale", plugin_root.join("current")).unwrap();

    let manifest = assert_success(&ensure(root, &EnsureRequest { host: identity() }));
    assert_ne!(
        fs::read_link(plugin_root.join("current")).unwrap(),
        Path::new("releases/0.1.0/stale")
    );
    assert_eq!(
        fs::canonicalize(plugin_root.join("current")).unwrap(),
        manifest.release_dir
    );
    assert!(stale.join("not-a-plugin").is_file());
}

#[test]
fn unwritable_install_root_has_actionable_exit() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("read-only");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o555)).unwrap();
    let output = ensure(&root, &EnsureRequest { host: identity() });
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();

    assert_eq!(output.status.code(), Some(EX_CANTCREAT));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cannot materialize backend 'e9patch' payload"));
    assert!(stderr.contains("set HERMIT_DIR to a writable directory"));
}

#[test]
fn exact_version_mismatch_fails_before_extraction() {
    let directory = tempfile::tempdir().unwrap();
    let mut host = identity();
    host.package_version = "0.2.1".to_owned();
    let output = ensure(directory.path(), &EnsureRequest { host });

    assert_eq!(output.status.code(), Some(EX_CONFIG));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("package version mismatch"));
    assert!(stderr.contains("refusing backend 'e9patch'"));
    assert!(stderr.contains("cargo install --force --locked hermit-e9patch@=0.2.1"));
    assert!(!directory.path().join("plugins").exists());
}
