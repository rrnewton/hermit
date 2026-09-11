/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::env;
use std::fs;
use std::hash::DefaultHasher;
use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[path = "../hermit-cli/src/liteinst_artifact.rs"]
// The install builder reuses only provenance validation; the shared module's
// source-capture and staging entry points belong to the producer build script.
#[allow(dead_code)]
mod liteinst_artifact;
mod liteinst_inputs;

const DYNAMORIO_FILES: &[&str] = &[
    "bin64/drrun",
    "lib64/release/libdynamorio.so",
    "lib64/release/libdrpreload.so",
    "ext/lib64/release/libdrx.so",
    "ext/lib64/release/libdrmgr.so",
    "ext/lib64/release/libdrreg.so",
    "ext/lib64/release/libdrwrap.so",
];

fn run(command: &mut Command, description: &str) {
    eprintln!("hermit-install: {description}: {command:?}");
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(status.success(), "failed to {description}: {status}");
}

fn output(command: &mut Command, description: &str) -> String {
    let result = command
        .output()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(
        result.status.success(),
        "failed to {description}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout)
        .unwrap_or_else(|error| panic!("non-UTF-8 output while trying to {description}: {error}"))
        .trim()
        .to_owned()
}

fn copy_file(source: &Path, destination: &Path) {
    assert!(
        source.is_file(),
        "required installation resource is missing: {}",
        source.display()
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", parent.display()));
    }
    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

fn ensure_submodule(
    repository: &Path,
    name: &str,
    relative: &str,
    marker: &str,
) -> (PathBuf, String) {
    let source = repository.join(relative);
    if !source.join(marker).is_file() {
        run(
            Command::new("git").arg("-C").arg(repository).args([
                "-c",
                &format!("submodule.{relative}.update=checkout"),
                "submodule",
                "update",
                "--init",
                "--checkout",
                "--depth",
                "1",
                "--recursive",
                "--",
                relative,
            ]),
            &format!("initialize the pinned {name} source"),
        );
    }

    let expected = output(
        Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["rev-parse", &format!(":{relative}")]),
        &format!("read the pinned {name} revision"),
    );
    let actual = output(
        Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"]),
        &format!("read the checked-out {name} revision"),
    );
    assert_eq!(
        actual, expected,
        "{name} source is not at the pinned revision"
    );
    (source, expected)
}

fn build_sabre(repository: &Path, build_root: &Path, resources: &Path) {
    let (source, revision) =
        ensure_submodule(repository, "SaBRe", "third-party/sabre", "CMakeLists.txt");
    // The target directory is restored by CI caches, while the installed
    // package is a Cargo-external side effect. Include both the verified
    // gitlink and the checkout path: the revision keeps stale SaBRe builds
    // unreachable, while the path keeps CMakeCache.txt bound to its original
    // absolute source directory.
    let mut source_hash = DefaultHasher::new();
    source.hash(&mut source_hash);
    let build = build_root.join(format!("sabre-{revision}-{:016x}", source_hash.finish()));
    run(
        Command::new("cmake")
            .arg("-S")
            .arg(&source)
            .arg("-B")
            .arg(&build)
            .arg("-DCMAKE_BUILD_TYPE=Release"),
        "configure SaBRe",
    );
    let mut command = Command::new("cmake");
    command
        .arg("--build")
        .arg(&build)
        .args(["--config", "Release", "--parallel"]);
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        command.arg(jobs);
    }
    run(&mut command, "build SaBRe");
    copy_file(&build.join("sabre"), &resources.join("sabre"));
    fs::write(resources.join("sabre.revision"), format!("{revision}\n"))
        .expect("failed to write SaBRe revision provenance");
}

fn build_e9patch(repository: &Path, build_root: &Path, resources: &Path) {
    let (source, _) = ensure_submodule(repository, "e9patch", "third-party/e9patch", "Makefile");
    let build = build_root.join("e9patch");
    if build.exists() {
        fs::remove_dir_all(&build)
            .unwrap_or_else(|error| panic!("failed to reset {}: {error}", build.display()));
    }
    fs::create_dir_all(&build)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", build.display()));
    run(
        Command::new("cp")
            .arg("-a")
            .arg(source.join("."))
            .arg(&build),
        "copy the pinned e9patch source into the target directory",
    );

    let mut command = Command::new("make");
    command.arg("-C").arg(&build).arg("release");
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        command.arg(format!("--jobs={}", jobs.to_string_lossy()));
    }
    run(&mut command, "build e9patch");
    copy_file(&build.join("e9tool"), &resources.join("e9tool"));
    copy_file(&build.join("e9patch"), &resources.join("e9patch"));
}

fn copy_licenses(repository_root: &Path, reverie_root: &Path, install: &Path) {
    copy_file(&repository_root.join("LICENSE"), &install.join("LICENSE"));
    let licenses = install.join("licenses");
    for name in [
        "LICENSE",
        "LICENSE.BSD-3",
        "LICENSE.GPL-2",
        "LICENSE.GPL-3",
        "LICENSE.MIT",
    ] {
        copy_file(
            &reverie_root.join("third-party/sabre").join(name),
            &licenses.join("sabre").join(name),
        );
    }
    copy_file(
        &reverie_root.join("third-party/dynamorio/License.txt"),
        &licenses.join("dynamorio/License.txt"),
    );
    copy_file(
        &reverie_root.join("third-party/e9patch/LICENSE"),
        &licenses.join("e9patch/LICENSE"),
    );
}

fn copy_dynamorio(resources: &Path) -> PathBuf {
    let drrun = reverie_dbt::bundled_drrun_path();
    let root = drrun
        .parent()
        .and_then(Path::parent)
        .expect("bundled drrun path has no DynamoRIO root");
    for relative in DYNAMORIO_FILES {
        copy_file(
            &root.join(relative),
            &resources.join("dynamorio").join(relative),
        );
    }
    reverie_dbt::bundled_dynamorio_cmake_dir().to_path_buf()
}

fn build_dbt_client(
    manifest_dir: &Path,
    build_root: &Path,
    resources: &Path,
    dynamorio_cmake: &Path,
) {
    let source = reverie_dbt::native_client_source_dir().join("client.c");
    let build = build_root.join("dbt-client");
    run(
        Command::new("cmake")
            .arg("-S")
            .arg(manifest_dir.join("native-client"))
            .arg("-B")
            .arg(&build)
            .arg("-DCMAKE_BUILD_TYPE=Release")
            .arg(format!("-DDynamoRIO_DIR={}", dynamorio_cmake.display()))
            .arg(format!("-DREVERIE_DBT_NATIVE_SOURCE={}", source.display()))
            .arg(format!("-DHERMIT_RESOURCE_DIR={}", resources.display())),
        "configure the relocatable Detcore DBT client",
    );
    let mut command = Command::new("cmake");
    command.arg("--build").arg(&build).args([
        "--config",
        "Release",
        "--target",
        "reverie_dbt_client",
        "--parallel",
    ]);
    if let Some(jobs) = env::var_os("NUM_JOBS") {
        command.arg(jobs);
    }
    run(&mut command, "build the relocatable Detcore DBT client");
    assert!(
        resources.join("libreverie_dbt_client.so").is_file(),
        "DBT client build did not produce libreverie_dbt_client.so"
    );
}

fn replace_symlink(destination: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(destination)?,
        Ok(_) => fs::remove_file(destination)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    symlink(target, destination)
}

fn build_liteinst_runtime(
    repository: &Path,
    reverie_root: &Path,
    source_record: &Path,
    caller_source_identity: &str,
    build_root: &Path,
    profile_dir: &Path,
    resources: &Path,
) {
    assert!(
        env::var("HERMIT_LITEINST_DIAGNOSTIC").as_deref() != Ok("1"),
        "diagnostic LiteInst artifacts must not be installed as product resources"
    );
    let runtime_name = match env::var("HERMIT_LITEINST_RUNTIME_KIND").as_deref() {
        Err(env::VarError::NotPresent) | Ok("preload") => "libhermit_liteinst_detcore.so",
        Ok("private-crt") => "hermit_liteinst_detcore_private.elf",
        Ok(other) => panic!("unsupported LiteInst runtime build kind {other}"),
        Err(error) => panic!("cannot read LiteInst runtime build kind: {error}"),
    };
    // stage-liteinst-runtime.sh appends the canonical Reverie pin to this
    // target root, so cache invalidation has the same source of truth as the
    // source and dependency checks.
    let target = build_root.join("liteinst-runtime");
    let runtime = profile_dir.join(runtime_name);
    run(
        Command::new(repository.join("scripts/stage-liteinst-runtime.sh"))
            .current_dir(repository)
            .env("HERMIT_LITEINST_HERMIT_ROOT", repository)
            .env("HERMIT_LITEINST_REVERIE_ROOT", reverie_root)
            .env("HERMIT_LITEINST_SOURCE_RECORD", source_record)
            .arg("release")
            .arg(&runtime)
            .arg(&target),
        "build the shared Detcore LiteInst runtime",
    );
    let provenance = PathBuf::from(format!("{}.provenance.json", runtime.display()));
    assert!(
        runtime.is_file() && provenance.is_file(),
        "standalone build did not stage the runtime/provenance pair at {}",
        runtime.display(),
    );
    let pin = reverie_pin(repository);
    let validate = |bytes: &[u8], provenance: &[u8]| {
        liteinst_artifact::validate_provenance(
            bytes,
            provenance,
            &pin,
            caller_source_identity,
            false,
        )
    };
    let pair = liteinst_inputs::RuntimePair::read(&runtime, validate)
        .expect("staged LiteInst runtime/provenance validation failed");
    let installed = resources.join(runtime_name);
    pair.install(&installed, validate)
        .expect("failed to install exact validated LiteInst runtime/provenance bytes");
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(env::var_os(name).unwrap_or_else(|| {
        panic!(
            "{name} is required; stage the LiteInst source record before building the Hermit caller"
        )
    }))
}

fn validate_liteinst_caller(
    repository: &Path,
    reverie_root: &Path,
    profile_dir: &Path,
) -> (PathBuf, String) {
    assert!(
        env::var("HERMIT_LITEINST_DIAGNOSTIC").as_deref() != Ok("1"),
        "diagnostic LiteInst artifacts must not be installed as product resources"
    );
    let source_record = required_path("HERMIT_LITEINST_SOURCE_RECORD");
    assert!(
        fs::symlink_metadata(&source_record).is_ok_and(|metadata| metadata.file_type().is_file()),
        "HERMIT_LITEINST_SOURCE_RECORD must name an existing regular file before building hermit-install: {}",
        source_record.display()
    );
    println!("cargo:rerun-if-changed={}", source_record.display());
    let configured_hermit = required_path("HERMIT_LITEINST_HERMIT_ROOT")
        .canonicalize()
        .expect("canonicalize HERMIT_LITEINST_HERMIT_ROOT");
    let configured_reverie = required_path("HERMIT_LITEINST_REVERIE_ROOT")
        .canonicalize()
        .expect("canonicalize HERMIT_LITEINST_REVERIE_ROOT");
    assert_eq!(
        configured_hermit,
        repository.canonicalize().expect("canonicalize Hermit root"),
        "HERMIT_LITEINST_HERMIT_ROOT differs from the repository being installed"
    );
    assert_eq!(
        configured_reverie,
        reverie_root
            .canonicalize()
            .expect("canonicalize Reverie root"),
        "HERMIT_LITEINST_REVERIE_ROOT differs from the resolved Reverie source"
    );

    let source_bytes = fs::read(&source_record).expect("read HERMIT_LITEINST_SOURCE_RECORD");
    let source_identity = liteinst_artifact::digest(&source_bytes);
    let caller = profile_dir.join("hermit");
    let caller_bytes = fs::read(&caller).unwrap_or_else(|error| {
        panic!(
            "Hermit caller must be built with the LiteInst source record before hermit-install: {}: {error}",
            caller.display()
        )
    });
    assert!(
        caller_bytes
            .windows(source_identity.len())
            .any(|window| window == source_identity.as_bytes()),
        "Hermit caller was not built with HERMIT_LITEINST_SOURCE_RECORD {}; rebuild it with the same source record and both source roots before hermit-install",
        source_record.display()
    );
    (source_record, source_identity)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=HERMIT_INSTALL_FORCE_RESTAGE");
    for name in [
        "HERMIT_LITEINST_DIAGNOSTIC",
        "HERMIT_LITEINST_RUNTIME_KIND",
        "HERMIT_LITEINST_SOURCE_RECORD",
        "HERMIT_LITEINST_CARGO_CONFIG",
        "HERMIT_LITEINST_HERMIT_ROOT",
        "HERMIT_LITEINST_REVERIE_ROOT",
        "HERMIT_LITEINST_CLI_MANIFEST",
        "HERMIT_LITEINST_DSO_MANIFEST",
        "HERMIT_LITEINST_PRIVATE_INPUTS",
        "CARGO",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }
    println!("cargo:rerun-if-changed=../scripts/stage-liteinst-runtime.sh");
    println!("cargo:rerun-if-changed=native-client/CMakeLists.txt");
    println!("cargo:rerun-if-changed=native-client/detcore_dbt_link_stub.c");
    // ⚠️ THE PIN FILES. Without these the staged runtime NEVER REBUILDS WHEN THE
    // REVERIE PIN MOVES: none of the triggers above changes when a pin bump
    // lands, so Cargo considers this script fresh and the stale `.so` stays.
    // A cell then measures the old binary and reports a verdict about the new
    // pin.
    println!("cargo:rerun-if-changed=../detcore/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/Cargo.lock");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/build.rs");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/artifact.rs");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/private_build.rs");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/detcore-runtime/Cargo.toml");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/detcore-runtime/Cargo.lock");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/detcore-runtime/src");
    println!("cargo:rerun-if-changed=../liteinst-runtime-build/private-native");

    let profile = env::var("PROFILE");
    if profile.as_deref() != Ok("release")
        || env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        return;
    }
    let profile = profile.expect("Cargo did not set PROFILE");

    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let profile_dir = out_dir
        .ancestors()
        .find(|ancestor| {
            ancestor.file_name().and_then(|name| name.to_str()) == Some(profile.as_str())
        })
        .expect("Cargo OUT_DIR does not have the active profile ancestor")
        .to_path_buf();
    let target_dir = profile_dir
        .parent()
        .expect("Cargo profile directory has no target parent");
    let repository = manifest_dir
        .parent()
        .expect("hermit-install is not inside the Hermit repository");
    let reverie_root = reverie_dbt::native_client_source_dir()
        .parent()
        .and_then(Path::parent)
        .expect("reverie-dbt source is not inside the Reverie repository");
    let (source_record, caller_source_identity) =
        validate_liteinst_caller(repository, reverie_root, &profile_dir);

    let install = target_dir.join("install_pkg");
    let resources = install.join("rsrcs");
    let build_root = target_dir.join("install-build");

    if install.exists() {
        fs::remove_dir_all(&install)
            .unwrap_or_else(|error| panic!("failed to reset {}: {error}", install.display()));
    }
    fs::create_dir_all(&resources)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", resources.display()));
    fs::create_dir_all(&build_root)
        .unwrap_or_else(|error| panic!("failed to create {}: {error}", build_root.display()));

    for library in ["libdetcore_dbt.so", "libdetcore_sabre.so"] {
        replace_symlink(
            &resources.join(library),
            &Path::new("../../release").join(library),
        )
        .unwrap_or_else(|error| panic!("failed to link packaged {library}: {error}"));
    }

    let dynamorio_cmake = copy_dynamorio(&resources);
    build_dbt_client(&manifest_dir, &build_root, &resources, &dynamorio_cmake);

    build_liteinst_runtime(
        repository,
        reverie_root,
        &source_record,
        &caller_source_identity,
        &build_root,
        &profile_dir,
        &resources,
    );

    build_sabre(reverie_root, &build_root, &resources);
    build_e9patch(reverie_root, &build_root, &resources);
    copy_licenses(repository, reverie_root, &install);

    replace_symlink(&install.join("hermit"), Path::new("../release/hermit"))
        .unwrap_or_else(|error| panic!("failed to link install_pkg/hermit: {error}"));
    fs::write(
        install.join("README.txt"),
        "Hermit release staging package. Copy with symlink dereferencing (for example, cp -aL) to create a standalone installation.\n",
    )
    .expect("failed to write install package README");
}

include!("../hermit-cli/reverie_pin.rs");

fn reverie_pin(repository: &Path) -> String {
    let text = fs::read_to_string(repository.join("detcore/Cargo.toml"))
        .expect("failed to read canonical Reverie pin manifest");
    parse_reverie_pin(&text).expect("canonical Reverie pin is missing or ambiguous")
}
