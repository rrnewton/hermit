use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use sha2::Digest as _;
use sha2::Sha256;

fn source_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) {
    let mut entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", directory.display()))
        .map(|entry| entry.expect("failed to read Detcore source entry").path())
        .collect::<Vec<_>>();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            source_files(root, &path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

fn main() {
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=src");

    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut files = vec![PathBuf::from("Cargo.toml"), PathBuf::from("build.rs")];
    source_files(&root, &root.join("src"), &mut files);
    files.sort();

    let mut hash = Sha256::new();
    for relative in files {
        let contents = fs::read(root.join(&relative))
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", relative.display()));
        hash.update(relative.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update((contents.len() as u64).to_le_bytes());
        hash.update(contents);
    }
    for name in ["CARGO_PKG_VERSION", "PROFILE", "TARGET"] {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(env::var(name).unwrap_or_default().as_bytes());
        hash.update([0]);
    }
    let mut features = env::vars()
        .filter(|(name, _)| name.starts_with("CARGO_FEATURE_"))
        .collect::<Vec<_>>();
    features.sort();
    for (name, value) in features {
        hash.update(name.as_bytes());
        hash.update([0]);
        hash.update(value.as_bytes());
        hash.update([0]);
    }
    let rustc = Command::new(env::var_os("RUSTC").unwrap())
        .arg("-vV")
        .output()
        .expect("failed to query rustc identity");
    assert!(rustc.status.success(), "rustc -vV failed");
    hash.update(&rustc.stdout);

    let identity = hex::encode(hash.finalize());
    let generated = format!(
        "/// Exact identity of the Detcore source, feature, target, profile, and compiler inputs.\n\
         pub const DETCORE_BUILD_ID: &str = \"{identity}\";\n"
    );
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("detcore-build-id.rs"),
        generated,
    )
    .expect("failed to write Detcore build identity");
}
