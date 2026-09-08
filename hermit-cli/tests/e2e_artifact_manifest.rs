/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde_json::Value;

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn temporary_directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "hermit-e2e-artifact-manifest-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("create artifact-manifest temporary directory");
    path
}

fn write_file(path: &Path, contents: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().expect("fixture path has a parent"))
        .unwrap_or_else(|error| panic!("create parent for {}: {error}", path.display()));
    fs::write(path, contents).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

fn run_git(root: &Path, args: &[&str]) -> Output {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("run git {:?} in {}: {error}", args, root.display()));
    assert!(
        output.status.success(),
        "git {:?} failed in {}:\n{}",
        args,
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_text(root: &Path, args: &[&str]) -> String {
    String::from_utf8(run_git(root, args).stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_owned()
}

fn initialize_git_repository(root: &Path) {
    fs::create_dir_all(root).expect("create Git fixture root");
    run_git(root, &["init", "--quiet"]);
    run_git(root, &["config", "user.name", "Hermit artifact test"]);
    run_git(
        root,
        &["config", "user.email", "artifact-test@example.invalid"],
    );
    run_git(root, &["config", "commit.gpgsign", "false"]);
}

fn commit_all(root: &Path, message: &str) {
    run_git(root, &["add", "--all"]);
    run_git(root, &["commit", "--quiet", "--no-verify", "-m", message]);
}

fn write_fake_hermit(path: &Path, git_sha: &str) -> String {
    let version = serde_json::json!({
        "schema": 1,
        "version": "artifact-test",
        "build_date": null,
        "git_sha": git_sha,
        "features": {"dbt": true, "e9patch": true, "sabre": true},
    });
    let raw_version = format!("{version}\n");
    assert!(!raw_version.contains('\''));
    let script = format!(
        "#!/bin/sh\nif [ \"$#\" -eq 2 ] && [ \"$1\" = version ] && [ \"$2\" = --json ]; then\n  printf '%s\\n' '{}'\nelse\n  printf 'expected-identity\\n'\nfi\n",
        raw_version.trim_end()
    );
    write_file(path, script.as_bytes(), 0o755);
    raw_version
}

struct Fixture {
    scratch: PathBuf,
    source: PathBuf,
    agent_utils: PathBuf,
    publisher: PathBuf,
    verifier: PathBuf,
    binary: PathBuf,
    install: PathBuf,
    bundles: PathBuf,
    pointer: PathBuf,
    short_head: String,
    raw_version: String,
}

impl Fixture {
    fn new() -> Self {
        let scratch = temporary_directory();
        let source = scratch.join("source");
        let agent_utils = source.join("agent-utils");
        let ci = source.join("ci");
        let product_root = repository_root();

        initialize_git_repository(&agent_utils);
        write_file(&agent_utils.join("anchor"), b"agent-utils source\n", 0o644);
        commit_all(&agent_utils, "agent-utils fixture");

        fs::create_dir_all(&ci).expect("create fixture ci directory");
        for name in [
            "publish-hermit-e2e-artifact.sh",
            "verify-hermit-e2e-artifact.sh",
        ] {
            fs::copy(product_root.join("ci").join(name), ci.join(name))
                .unwrap_or_else(|error| panic!("copy fixture {name}: {error}"));
            fs::set_permissions(ci.join(name), fs::Permissions::from_mode(0o755))
                .unwrap_or_else(|error| panic!("chmod fixture {name}: {error}"));
        }
        write_file(&source.join("tracked-source"), b"Hermit source\n", 0o644);
        initialize_git_repository(&source);
        commit_all(&source, "Hermit fixture");

        let short_head = git_text(&source, &["rev-parse", "--short=12", "HEAD"]);
        let binary = scratch.join("hermit");
        let raw_version = write_fake_hermit(&binary, &short_head);
        let install = scratch.join("install");
        for name in [
            "libdetcore_dbt.so",
            "libdetcore_sabre.so",
            "libreverie_dbt_client.so",
            "libreverie_liteinst.so",
        ] {
            write_file(
                &install.join("rsrcs").join(name),
                format!("fixture {name}\n").as_bytes(),
                0o644,
            );
        }
        for name in ["dynamorio/bin64/drrun", "sabre", "e9patch", "e9tool"] {
            write_file(
                &install.join("rsrcs").join(name),
                b"#!/bin/sh\nexit 0\n",
                0o755,
            );
        }
        write_file(
            &install.join("rsrcs/libreverie_liteinst.so.revision"),
            b"0123456789abcdef0123456789abcdef01234567\n",
            0o640,
        );

        Self {
            publisher: ci.join("publish-hermit-e2e-artifact.sh"),
            verifier: ci.join("verify-hermit-e2e-artifact.sh"),
            binary,
            install,
            bundles: scratch.join("bundles"),
            pointer: scratch.join("artifact.path"),
            scratch,
            source,
            agent_utils,
            short_head,
            raw_version,
        }
    }

    fn publish(&self, binary: &Path, bundles: &Path, pointer: &Path) -> Output {
        Command::new(&self.publisher)
            .args([binary, bundles, pointer, self.install.as_path()])
            .output()
            .expect("run fixture artifact publisher")
    }

    fn publish_valid(&self) -> PathBuf {
        let output = self.publish(&self.binary, &self.bundles, &self.pointer);
        assert!(
            output.status.success(),
            "valid artifact publication failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        PathBuf::from(
            fs::read_to_string(&self.pointer)
                .expect("read artifact pointer")
                .trim(),
        )
    }

    fn verify(&self, input: &Path) -> Output {
        Command::new(&self.verifier)
            .arg(input)
            .output()
            .expect("run fixture artifact verifier")
    }
}

fn assert_refused(output: Output, reason: &str) {
    assert_eq!(
        output.status.code(),
        Some(2),
        "refusal returned the wrong status:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(reason),
        "refusal did not name {reason:?}:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256_file(path: &Path) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .unwrap_or_else(|error| panic!("hash {}: {error}", path.display()));
    assert!(
        output.status.success(),
        "sha256sum failed for {}",
        path.display()
    );
    String::from_utf8(output.stdout)
        .expect("sha256sum output is UTF-8")
        .split_whitespace()
        .next()
        .expect("sha256sum output has a digest")
        .to_owned()
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination)
        .unwrap_or_else(|error| panic!("create {}: {error}", destination.display()));
    let output = Command::new("cp")
        .arg("-a")
        .arg(source.join("."))
        .arg(destination)
        .output()
        .expect("copy artifact fixture");
    assert!(
        output.status.success(),
        "copy artifact fixture failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn copied_bundle(scratch: &Path, source: &Path, label: &str) -> PathBuf {
    let parent = scratch.join(label);
    let destination = parent.join(source.file_name().expect("artifact has an identity"));
    copy_tree(source, &destination);
    destination
}

fn write_manifest(path: &Path, value: &Value) {
    let mut bytes = serde_json::to_vec_pretty(value).expect("serialize tampered manifest");
    bytes.push(b'\n');
    fs::write(path, bytes).expect("write tampered manifest");
}

fn readdressed_bundle(scratch: &Path, source: &Path, label: &str, manifest: &Value) -> PathBuf {
    let parent = scratch.join(label);
    let work = parent.join("work");
    copy_tree(source, &work);
    write_manifest(&work.join("manifest.json"), manifest);
    let destination = parent.join(sha256_file(&work.join("manifest.json")));
    fs::rename(&work, &destination).expect("address tampered fixture by its manifest");
    destination
}

#[test]
fn artifact_manifest_binds_source_version_and_every_payload_file() {
    let fixture = Fixture::new();
    let bundle = fixture.publish_valid();
    let manifest_path = bundle.join("manifest.json");
    let manifest: Value = serde_json::from_slice(
        &fs::read(&manifest_path).expect("read published artifact manifest"),
    )
    .expect("parse published artifact manifest");

    assert_eq!(manifest["schema"], 1);
    assert_eq!(manifest["kind"], "complete");
    assert_eq!(
        manifest["source"]["hermit"]["head"],
        git_text(&fixture.source, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        manifest["source"]["hermit"]["tree"],
        git_text(&fixture.source, &["show", "-s", "--format=%T", "HEAD"])
    );
    let gitlink = git_text(&fixture.source, &["ls-tree", "HEAD", "--", "agent-utils"])
        .split_whitespace()
        .nth(2)
        .expect("agent-utils gitlink record has an object ID")
        .to_owned();
    let agent_head = git_text(&fixture.agent_utils, &["rev-parse", "HEAD"]);
    assert_eq!(manifest["source"]["agent_utils"]["gitlink"], gitlink);
    assert_eq!(manifest["source"]["agent_utils"]["head"], agent_head);
    assert_eq!(manifest["hermit_version_json"], fixture.raw_version);
    assert_eq!(
        bundle.file_name().expect("bundle has an identity"),
        sha256_file(&manifest_path).as_str(),
        "directory identity must be the manifest digest"
    );
    let paths: Vec<_> = manifest["files"]
        .as_array()
        .expect("manifest files are an array")
        .iter()
        .map(|entry| entry["path"].as_str().expect("file path is a string"))
        .collect();
    assert!(paths.contains(&"hermit"));
    assert!(paths.contains(&"install/rsrcs/libreverie_liteinst.so"));
    assert!(paths.contains(&"install/rsrcs/libreverie_liteinst.so.revision"));
    for entry in manifest["files"].as_array().unwrap() {
        assert!(entry["mode"].as_str().is_some());
        assert!(entry["size"].as_u64().is_some());
        assert_eq!(entry["sha256"].as_str().unwrap().len(), 64);
    }

    let verified = fixture.verify(&fixture.pointer);
    assert!(
        verified.status.success(),
        "valid artifact verification failed:\n{}",
        String::from_utf8_lossy(&verified.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&verified.stdout).trim(),
        bundle.to_string_lossy()
    );

    let wrong_binary = fixture.scratch.join("wrong-source-hermit");
    write_fake_hermit(&wrong_binary, "000000000000");
    assert_refused(
        fixture.publish(
            &wrong_binary,
            &fixture.scratch.join("wrong-source-bundles"),
            &fixture.scratch.join("wrong-source.path"),
        ),
        "Hermit binary/source revision mismatch",
    );
    for reported in [
        "unknown".to_owned(),
        format!("{}-dirty", fixture.short_head),
    ] {
        let bad = fixture.scratch.join(format!("inadmissible-{reported}"));
        write_fake_hermit(&bad, &reported);
        assert_refused(
            fixture.publish(
                &bad,
                &fixture
                    .scratch
                    .join(format!("inadmissible-{reported}-bundles")),
                &fixture
                    .scratch
                    .join(format!("inadmissible-{reported}.path")),
            ),
            "inadmissible source revision",
        );
    }

    fs::write(
        fixture.source.join("tracked-source"),
        b"dirty Hermit source\n",
    )
    .expect("dirty source fixture");
    assert_refused(
        fixture.publish(
            &fixture.binary,
            &fixture.scratch.join("dirty-source-bundles"),
            &fixture.scratch.join("dirty-source.path"),
        ),
        "Hermit source worktree is not clean",
    );
    run_git(&fixture.source, &["checkout", "--", "tracked-source"]);

    let source_side_effect = fixture.scratch.join("source-side-effect-hermit");
    let source_side_effect_script = format!(
        "#!/bin/sh\nif [ \"$#\" -eq 2 ] && [ \"$1\" = version ] && [ \"$2\" = --json ]; then\n  printf 'mutated by version probe\\n' > \"{}\"\n  printf '%s\\n' '{}'\nelse\n  exit 1\nfi\n",
        fixture.source.join("tracked-source").display(),
        fixture.raw_version.trim_end()
    );
    write_file(
        &source_side_effect,
        source_side_effect_script.as_bytes(),
        0o755,
    );
    assert_refused(
        fixture.publish(
            &source_side_effect,
            &fixture.scratch.join("source-side-effect-bundles"),
            &fixture.scratch.join("source-side-effect.path"),
        ),
        "Hermit source worktree is not clean",
    );
    run_git(&fixture.source, &["checkout", "--", "tracked-source"]);

    write_file(
        &fixture.agent_utils.join("anchor"),
        b"different agent-utils source\n",
        0o644,
    );
    commit_all(&fixture.agent_utils, "move agent-utils fixture HEAD");
    assert_refused(
        fixture.publish(
            &fixture.binary,
            &fixture.scratch.join("gitlink-mismatch-bundles"),
            &fixture.scratch.join("gitlink-mismatch.path"),
        ),
        "agent-utils gitlink/HEAD mismatch",
    );
    run_git(&fixture.agent_utils, &["checkout", "--quiet", &gitlink]);

    let manifest_tamper = copied_bundle(&fixture.scratch, &bundle, "manifest-tamper");
    OpenOptions::new()
        .append(true)
        .open(manifest_tamper.join("manifest.json"))
        .and_then(|mut file| file.write_all(b" \n"))
        .expect("tamper manifest bytes");
    assert_refused(
        fixture.verify(&manifest_tamper),
        "content-addressed artifact manifest mismatch",
    );

    let mut source_manifest = manifest.clone();
    source_manifest["source"]["hermit"]["tree"] =
        Value::String("0000000000000000000000000000000000000000".into());
    let source_tamper =
        readdressed_bundle(&fixture.scratch, &bundle, "source-tamper", &source_manifest);
    assert_refused(
        fixture.verify(&source_tamper),
        "artifact Hermit tree does not match live source",
    );

    let mut version_manifest = manifest.clone();
    let mut version: Value = serde_json::from_str(
        version_manifest["hermit_version_json"]
            .as_str()
            .expect("manifest has raw version JSON"),
    )
    .expect("raw version JSON parses");
    version["version"] = Value::String("tampered-version".into());
    version_manifest["hermit_version_json"] =
        Value::String(format!("{}\n", serde_json::to_string(&version).unwrap()));
    let version_tamper = readdressed_bundle(
        &fixture.scratch,
        &bundle,
        "version-tamper",
        &version_manifest,
    );
    assert_refused(
        fixture.verify(&version_tamper),
        "live version JSON does not match its manifest",
    );

    let resource_tamper = copied_bundle(&fixture.scratch, &bundle, "resource-tamper");
    let resource = resource_tamper.join("install/rsrcs/libreverie_liteinst.so");
    let mut changed = fs::read(&resource).expect("read resource to tamper");
    changed[0] ^= 1;
    fs::write(&resource, changed).expect("tamper resource without changing its size");
    assert_refused(
        fixture.verify(&resource_tamper),
        "published artifact SHA-256 mismatch for install/rsrcs/libreverie_liteinst.so",
    );

    let mode_tamper = copied_bundle(&fixture.scratch, &bundle, "mode-tamper");
    fs::set_permissions(
        mode_tamper.join("install/rsrcs/libreverie_liteinst.so.revision"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("tamper resource mode");
    assert_refused(
        fixture.verify(&mode_tamper),
        "published artifact mode mismatch for install/rsrcs/libreverie_liteinst.so.revision",
    );

    let unlisted = copied_bundle(&fixture.scratch, &bundle, "unlisted-resource");
    write_file(
        &unlisted.join("install/rsrcs/unlisted"),
        b"unbound bytes\n",
        0o644,
    );
    assert_refused(
        fixture.verify(&unlisted),
        "published artifact file set does not match its manifest",
    );

    let side_effect_parent = fixture.scratch.join("version-side-effect");
    let side_effect_work = side_effect_parent.join("work");
    copy_tree(&bundle, &side_effect_work);
    let side_effect_script = format!(
        "#!/bin/sh\nif [ \"$#\" -eq 2 ] && [ \"$1\" = version ] && [ \"$2\" = --json ]; then\n  printf x >> \"$(dirname \"$0\")/install/rsrcs/libreverie_liteinst.so.revision\"\n  printf '%s\\n' '{}'\nelse\n  exit 1\nfi\n",
        fixture.raw_version.trim_end()
    );
    let side_effect_binary = side_effect_work.join("hermit");
    write_file(&side_effect_binary, side_effect_script.as_bytes(), 0o755);
    let mut side_effect_manifest = manifest.clone();
    let binary_entry = side_effect_manifest["files"]
        .as_array_mut()
        .expect("manifest files are mutable")
        .iter_mut()
        .find(|entry| entry["path"] == "hermit")
        .expect("manifest has a Hermit entry");
    binary_entry["mode"] = Value::String("755".into());
    binary_entry["size"] = Value::from(
        fs::metadata(&side_effect_binary)
            .expect("stat side-effecting binary")
            .len(),
    );
    binary_entry["sha256"] = Value::String(sha256_file(&side_effect_binary));
    write_manifest(
        &side_effect_work.join("manifest.json"),
        &side_effect_manifest,
    );
    let side_effect_bundle =
        side_effect_parent.join(sha256_file(&side_effect_work.join("manifest.json")));
    fs::rename(&side_effect_work, &side_effect_bundle)
        .expect("address side-effecting executable fixture");
    assert_refused(
        fixture.verify(&side_effect_bundle),
        "published artifact size mismatch for install/rsrcs/libreverie_liteinst.so.revision",
    );

    fs::remove_dir_all(&fixture.scratch).expect("remove artifact-manifest fixture");
}
