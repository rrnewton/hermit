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

const DYNAMORIO_FILES: &[&str] = &[
    "bin64/drrun",
    "lib64/release/libdynamorio.so",
    "lib64/release/libdrpreload.so",
    "ext/lib64/release/libdrx.so",
    "ext/lib64/release/libdrmgr.so",
    "ext/lib64/release/libdrreg.so",
    "ext/lib64/release/libdrwrap.so",
];
const MAX_PARALLEL_JOBS: usize = 16;

// Provenance: the 2026-08-03 release source-build measurement produced a
// 3,228,825-byte stripped gzip payload. Four MiB retains 29.9% headroom while
// making growth visible at install time. Debug builds intentionally carry an
// unoptimized build-dependency and are not a shipping-size measurement.
const MAX_EMBEDDED_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=native-client");
    println!("cargo:rerun-if-env-changed=STRIP");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux")
        || env::var("CARGO_CFG_TARGET_ARCH").as_deref() != Ok("x86_64")
    {
        panic!("hermit-dynamorio currently supports only x86_64 Linux");
    }

    let out = PathBuf::from(required_env("OUT_DIR"));
    let payload = out.join("payload");
    if payload.exists() {
        fs::remove_dir_all(&payload).expect("failed to reset the payload directory");
    }
    fs::create_dir_all(payload.join("lib/dynamorio"))
        .expect("failed to create payload DynamoRIO directory");
    fs::create_dir_all(payload.join("licenses"))
        .expect("failed to create payload license directory");

    let dynamorio = reverie_dbi::bundled_drrun_path()
        .parent()
        .and_then(Path::parent)
        .expect("bundled drrun path has no DynamoRIO root");
    for relative in DYNAMORIO_FILES {
        let destination = payload.join("lib/dynamorio").join(relative);
        copy_file(&dynamorio.join(relative), &destination);
        strip_file(&destination);
    }

    let runtime = payload.join("lib/libdetcore_dbi.so");
    build_detcore_runtime(&out, &runtime);
    strip_file(&runtime);

    let client = payload.join("lib/libreverie_dbi_client.so");
    build_native_client(&out, &payload.join("lib"));
    assert!(
        client.is_file(),
        "native client build did not produce {}",
        client.display()
    );
    strip_file(&client);

    copy_licenses(&payload);

    let identity = PluginIdentity::current(
        &env::var("CARGO_PKG_VERSION").expect("Cargo did not set CARGO_PKG_VERSION"),
        detcore::DETCORE_BUILD_ID,
    );
    let files = payload_hashes(&payload);
    let manifest = PayloadManifest {
        plugin: identity,
        release_dir: PathBuf::new(),
        drrun: PathBuf::from("lib/dynamorio/bin64/drrun"),
        client: PathBuf::from("lib/libreverie_dbi_client.so"),
        detcore_runtime: PathBuf::from("lib/libdetcore_dbi.so"),
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
            "embedded DynamoRIO payload is {archive_bytes} bytes, exceeding the documented {MAX_EMBEDDED_ARCHIVE_BYTES}-byte release ratchet"
        );
    }
    println!("cargo:warning=embedded DynamoRIO payload ({profile} profile): {archive_bytes} bytes");
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
    let cdylib = profile_dir.join("deps/libdetcore_dbi.so");
    assert!(
        cdylib.is_file(),
        "Cargo did not expose the detcore-dbi build-dependency cdylib at {}",
        cdylib.display()
    );
    copy_file(&cdylib, destination);
}

fn build_native_client(out: &Path, payload_lib: &Path) {
    let manifest = PathBuf::from(required_env("CARGO_MANIFEST_DIR"));
    let build = out.join("native-client-build");
    let source = reverie_dbi::native_client_source_dir().join("client.c");
    let mut configure = Command::new("cmake");
    configure
        .arg("-S")
        .arg(manifest.join("native-client"))
        .arg("-B")
        .arg(&build)
        .arg("-DCMAKE_BUILD_TYPE=Release")
        .arg(format!(
            "-DDynamoRIO_DIR={}",
            reverie_dbi::bundled_dynamorio_cmake_dir().display()
        ))
        .arg(format!("-DREVERIE_DBI_NATIVE_SOURCE={}", source.display()))
        .arg(format!(
            "-DHERMIT_PAYLOAD_LIB_DIR={}",
            payload_lib.display()
        ));
    run(&mut configure, "configure the DynamoRIO native client");

    let mut build_command = Command::new("cmake");
    build_command.arg("--build").arg(&build).args([
        "--config",
        "Release",
        "--target",
        "reverie_dbi_client",
        "--parallel",
    ]);
    let jobs = env::var("NUM_JOBS")
        .ok()
        .and_then(|jobs| jobs.parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, MAX_PARALLEL_JOBS);
    build_command.arg(jobs.to_string());
    run(&mut build_command, "build the DynamoRIO native client");
}

fn copy_licenses(payload: &Path) {
    let manifest = PathBuf::from(required_env("CARGO_MANIFEST_DIR"));
    copy_file(
        &manifest.parent().unwrap().join("LICENSE"),
        &payload.join("licenses/hermit-LICENSE"),
    );
    let reverie_dbi = reverie_dbi::native_client_source_dir()
        .parent()
        .expect("reverie-dbi native source has no crate root");
    copy_file(
        &reverie_dbi.join("vendor/dynamorio/License.txt"),
        &payload.join("licenses/dynamorio-License.txt"),
    );
    copy_file(
        &reverie_dbi.join("vendor/dynamorio/ACKNOWLEDGEMENTS"),
        &payload.join("licenses/dynamorio-ACKNOWLEDGEMENTS"),
    );
    copy_file(
        &reverie_dbi.parent().unwrap().join("LICENSE"),
        &payload.join("licenses/reverie-LICENSE"),
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
