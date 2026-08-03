use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use flate2::Compression;
use flate2::GzBuilder;
use hermit_plugin_protocol::PayloadManifest;
use hermit_plugin_protocol::PluginIdentity;
use sha2::Digest as _;
use sha2::Sha256;
use tar::Builder;
use tar::Header;

// Provenance: the 2026-08-03 measured stripped SaBRe loader plus Detcore DSO
// compressed to 1,733,462 bytes. Four MiB is more than twice that observation
// while still making accidental payload growth fail during a release build.
const MAX_EMBEDDED_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=STRIP");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        panic!("hermit-sabre currently supports only x86_64 Linux");
    }

    let out = PathBuf::from(required_env("OUT_DIR"));
    let payload = out.join("payload");
    if payload.exists() {
        fs::remove_dir_all(&payload).expect("failed to reset the payload directory");
    }
    fs::create_dir_all(payload.join("bin")).expect("failed to create payload bin directory");
    fs::create_dir_all(payload.join("lib")).expect("failed to create payload lib directory");
    fs::create_dir_all(payload.join("licenses"))
        .expect("failed to create payload license directory");

    let sabre = payload.join("bin/sabre");
    copy_file(reverie_sabre::bundled_sabre_path(), &sabre);
    strip_file(&sabre);

    let runtime = payload.join("lib/libdetcore_sabre.so");
    build_detcore_runtime(&out, &runtime);
    strip_file(&runtime);

    copy_licenses(&payload);

    let identity = PluginIdentity::with_abi(
        "sabre",
        &env::var("CARGO_PKG_VERSION").expect("Cargo did not set CARGO_PKG_VERSION"),
        hermit_plugin_protocol::SABRE_DETCORE_ABI_TAG,
        detcore::DETCORE_BUILD_ID,
    );
    let files = payload_hashes(&payload);
    let manifest = PayloadManifest {
        plugin: identity,
        release_dir: PathBuf::new(),
        drrun: PathBuf::new(),
        client: PathBuf::new(),
        detcore_runtime: PathBuf::from("lib/libdetcore_sabre.so"),
        sabre: PathBuf::from("bin/sabre"),
        e9tool: PathBuf::new(),
        e9patch: PathBuf::new(),
        files,
    };
    fs::write(
        payload.join("plugin.json"),
        serde_json::to_vec_pretty(&manifest).expect("failed to encode payload manifest"),
    )
    .expect("failed to write payload manifest");

    let archive = out.join("payload.tar.gz");
    write_archive(&payload, &archive).expect("failed to write embedded payload archive");
    let archive_bytes = fs::metadata(&archive)
        .expect("failed to inspect embedded payload archive")
        .len();
    let profile = env::var("PROFILE").expect("Cargo did not set PROFILE");
    if profile == "release" {
        assert!(
            archive_bytes <= MAX_EMBEDDED_ARCHIVE_BYTES,
            "embedded SaBRe payload is {archive_bytes} bytes, exceeding the documented {MAX_EMBEDDED_ARCHIVE_BYTES}-byte release ratchet"
        );
    }
    println!("cargo:warning=embedded SaBRe payload ({profile} profile): {archive_bytes} bytes");
}

fn required_env(name: &str) -> std::ffi::OsString {
    env::var_os(name).unwrap_or_else(|| panic!("Cargo did not set {name}"))
}

fn build_detcore_runtime(out: &Path, destination: &Path) {
    let profile = env::var("PROFILE").expect("Cargo did not set PROFILE");
    let profile_dir = out
        .ancestors()
        .find(|path| path.file_name().and_then(|name| name.to_str()) == Some(profile.as_str()))
        .expect("Cargo OUT_DIR has no active profile ancestor");
    let cdylib = profile_dir.join("deps/libdetcore_sabre.so");
    assert!(
        cdylib.is_file(),
        "Cargo did not expose the detcore-sabre build-dependency cdylib at {}",
        cdylib.display()
    );
    copy_file(&cdylib, destination);
}

fn copy_licenses(payload: &Path) {
    let manifest = PathBuf::from(required_env("CARGO_MANIFEST_DIR"));
    copy_file(
        &manifest.parent().unwrap().join("LICENSE"),
        &payload.join("licenses/hermit-LICENSE"),
    );
    let sabre = reverie_sabre::bundled_sabre_source_dir();
    copy_file(
        &sabre.join("LICENSE"),
        &payload.join("licenses/sabre-LICENSE"),
    );
    copy_file(
        &sabre.join("LICENSE.BSD-3"),
        &payload.join("licenses/sabre-LICENSE.BSD-3"),
    );
}

fn copy_file(source: &Path, destination: &Path) {
    assert!(
        source.is_file(),
        "required payload file is missing: {}",
        source.display()
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).expect("failed to create payload directory");
    }
    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "failed to copy {} to {}: {error}",
            source.display(),
            destination.display()
        )
    });
}

fn strip_file(path: &Path) {
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("failed to inspect {}: {error}", path.display()))
        .permissions();
    permissions.set_mode(permissions.mode() | 0o200);
    fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("failed to make {} writable: {error}", path.display()));
    let strip = env::var_os("STRIP").unwrap_or_else(|| "strip".into());
    run(
        Command::new(strip).arg("--strip-unneeded").arg(path),
        &format!("strip {}", path.display()),
    );
}

fn run(command: &mut Command, description: &str) {
    eprintln!("hermit-dynamorio: {description}: {command:?}");
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to {description}: {error}"));
    assert!(status.success(), "failed to {description}: {status}");
}

fn payload_files(root: &Path) -> Vec<PathBuf> {
    fn visit(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
            .map(|entry| entry.expect("failed to read payload entry").path())
            .collect::<Vec<_>>();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.push(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files
}

fn payload_hashes(root: &Path) -> BTreeMap<String, String> {
    payload_files(root)
        .into_iter()
        .map(|relative| {
            let bytes = fs::read(root.join(&relative)).expect("failed to hash payload file");
            (
                relative.to_string_lossy().into_owned(),
                hex::encode(Sha256::digest(bytes)),
            )
        })
        .collect()
}

fn write_archive(root: &Path, output: &Path) -> io::Result<()> {
    let file = fs::File::create(output)?;
    let gzip = GzBuilder::new().mtime(0).write(file, Compression::best());
    let mut archive = Builder::new(gzip);
    for relative in payload_files(root) {
        let path = root.join(&relative);
        let bytes = fs::read(&path)?;
        let executable = fs::metadata(&path)?.permissions().mode() & 0o111 != 0;
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(if executable { 0o755 } else { 0o644 });
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive.append_data(&mut header, &relative, bytes.as_slice())?;
    }
    archive.into_inner()?.finish()?;
    Ok(())
}
