/* Copyright (c) Meta Platforms, Inc. and affiliates. */

//! Exact clean Git source measurement shared by the Cargo stamp and its
//! top-level producer. Native/export/registry identity is not inferred.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use sha2::Digest;
use sha2::Sha256;

use super::compatibility_graph::InputFile;
use super::compatibility_graph::Package;

pub fn full_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .output()
        .map_err(|error| format!("cannot measure core Git identity: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "core Git measurement {args:?} failed: {}",
            output.status
        ));
    }
    String::from_utf8(output.stdout)
        .map(|text| text.trim_end_matches('\n').to_owned())
        .map_err(|_| "core Git measurement returned non-UTF8 data".to_owned())
}

fn metadata_identity(meta: &std::fs::Metadata) -> (u64, u64, u64, u32, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mode(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn source_digest(root: &Path, track: bool) -> Result<String, String> {
    let entries = git(root, &["ls-tree", "-r", "--full-tree", "-z", "HEAD"])?;
    let mut hash = Sha256::new();
    hash.update(b"hermit-full-core-source/v1\0");
    for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
        let (header, name) = entry
            .split_once('\t')
            .ok_or_else(|| "malformed core Git tree entry".to_owned())?;
        let mut fields = header.split(' ');
        let mode = fields
            .next()
            .ok_or_else(|| "missing core tree mode".to_owned())?;
        let kind = fields
            .next()
            .ok_or_else(|| "missing core tree kind".to_owned())?;
        let oid = fields
            .next()
            .ok_or_else(|| "missing core tree object".to_owned())?;
        if fields.next().is_some() || !full_oid(oid) || name.contains('\n') || name.contains('\r') {
            return Err(
                "unsupported core tree entry for exact compatibility measurement".to_owned(),
            );
        }
        hash_field(&mut hash, mode.as_bytes());
        hash_field(&mut hash, name.as_bytes());
        let path = root.join(name);
        match (mode, kind) {
            ("160000", "commit") => {
                // Cargo's git dependencies are separately bound by the lock
                // cohort. A gitlink is a declared source identity, not proof
                // that an optional separately produced runtime was qualified.
                hash_field(&mut hash, oid.as_bytes());
            }
            ("120000", "blob") => {
                let before = std::fs::symlink_metadata(&path)
                    .map_err(|error| format!("cannot inspect core symlink {name}: {error}"))?;
                if !before.file_type().is_symlink() {
                    return Err(format!("core symlink {name} has another type"));
                }
                let target = std::fs::read_link(&path)
                    .map_err(|error| format!("cannot read core symlink {name}: {error}"))?;
                let bytes = target.as_os_str().as_bytes();
                let mut blob = sha1_smol::Sha1::new();
                blob.update(format!("blob {}\0", bytes.len()).as_bytes());
                blob.update(bytes);
                if blob.digest().to_string() != oid {
                    return Err(format!(
                        "actual core symlink {name} differs from its canonical Git blob"
                    ));
                }
                hash_field(&mut hash, bytes);
                let after = std::fs::symlink_metadata(&path).map_err(|error| {
                    format!("core symlink changed while measuring {name}: {error}")
                })?;
                if metadata_identity(&before) != metadata_identity(&after) {
                    return Err(format!("core symlink changed while measuring {name}"));
                }
            }
            ("100644" | "100755", "blob") => {
                let before = std::fs::symlink_metadata(&path)
                    .map_err(|error| format!("cannot inspect core source {name}: {error}"))?;
                if !before.is_file() || (before.mode() & 0o111 != 0) != (mode == "100755") {
                    return Err(format!(
                        "core source {name} type/mode differs from its Git tree"
                    ));
                }
                let mut file = std::fs::File::open(&path)
                    .map_err(|error| format!("cannot open core source {name}: {error}"))?;
                if metadata_identity(&before)
                    != metadata_identity(&file.metadata().map_err(|error| error.to_string())?)
                {
                    return Err(format!("core source was replaced while opening {name}"));
                }
                hash.update(before.len().to_le_bytes());
                let mut blob = sha1_smol::Sha1::new();
                blob.update(format!("blob {}\0", before.len()).as_bytes());
                let mut total = 0u64;
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let n = file
                        .read(&mut buffer)
                        .map_err(|error| format!("cannot read core source {name}: {error}"))?;
                    if n == 0 {
                        break;
                    }
                    total += n as u64;
                    hash.update(&buffer[..n]);
                    blob.update(&buffer[..n]);
                }
                let after = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
                if total != before.len() || metadata_identity(&before) != metadata_identity(&after)
                {
                    return Err(format!("core source changed while measuring {name}"));
                }
                if blob.digest().to_string() != oid {
                    return Err(format!(
                        "actual core source {name} differs from its canonical Git blob"
                    ));
                }
            }
            _ => return Err(format!("unsupported core tree mode/type {mode}/{kind}")),
        }
        if track && path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn source(root: &Path, track: bool) -> Result<serde_json::Value, String> {
    // A generated Cargo project beneath some enclosing repository is not the
    // core checkout. In particular, do not stamp the dev-hermit parent here.
    let actual = std::fs::canonicalize(git(root, &["rev-parse", "--show-toplevel"])?)
        .map_err(|error| format!("cannot resolve core Git root: {error}"))?;
    let expected = std::fs::canonicalize(root)
        .map_err(|error| format!("cannot resolve core source directory: {error}"))?;
    if actual != expected {
        return Err(
            "compatibility measurement requires the actual core repository root".to_owned(),
        );
    }
    let commit = git(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let tree = git(root, &["rev-parse", "--verify", "HEAD^{tree}"])?;
    if !full_oid(&commit) || !full_oid(&tree) {
        return Err(
            "compatibility measurement requires full SHA-1 Git commit and tree IDs".to_owned(),
        );
    }
    let status = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    if !status.is_empty() {
        return Err(
            "compatibility measurement requires clean tracked source, index and nonignored inputs"
                .to_owned(),
        );
    }
    let flags = git(root, &["ls-files", "-v", "-z"])?;
    if flags
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .any(|entry| !entry.starts_with("H "))
    {
        return Err(
            "compatibility measurement refuses sparse/assume-unchanged/unmerged index entries"
                .to_owned(),
        );
    }
    // Full actual source bytes/modes are bound as well as the full Git OIDs.
    // This is independent of stat-cache shortcuts in a dirty-state label.
    let sha256 = source_digest(root, track)?;
    if git(root, &["rev-parse", "--verify", "HEAD^{commit}"])? != commit
        || git(
            root,
            &[
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ],
        )? != status
    {
        return Err("core source changed during compatibility measurement".to_owned());
    }
    if track {
        for name in ["HEAD", "index", "packed-refs"] {
            let path = git(
                root,
                &["rev-parse", "--path-format=absolute", "--git-path", name],
            )?;
            if Path::new(&path).exists() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
        if let Ok(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
            let path = git(
                root,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    &reference,
                ],
            )?;
            println!("cargo:rerun-if-changed={path}");
        }
    }
    Ok(serde_json::json!({"kind":"git", "commit":commit, "tree":tree, "sha256":sha256}))
}

/// The producer checks these complete source measurements around each build;
/// the final build script independently checks the same pre-build snapshot.
/// Paths are evidence locators, not cross-executable compatibility dimensions.
pub fn snapshot(metadata: &InputFile, track: bool) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct Metadata {
        version: u64,
        packages: Vec<Package>,
    }
    let metadata: Metadata = serde_json::from_slice(&metadata.read(64 * 1024 * 1024)?)
        .map_err(|error| format!("cannot read source snapshot metadata: {error}"))?;
    if metadata.version != 1 {
        return Err("unsupported source snapshot metadata version".to_owned());
    }
    let cores: Vec<_> = metadata
        .packages
        .iter()
        .filter(|package| package.name == "hermit-run")
        .collect();
    if cores.len() != 1 {
        return Err("source snapshot requires exactly one actual core package".to_owned());
    }
    let core_root = cores[0]
        .manifest_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "source snapshot core has no repository parent".to_owned())?;
    let core = serde_json::json!({"path":core_root,"identity":source(core_root, track)?});
    let mut roots = BTreeMap::new();
    for package in &metadata.packages {
        let Some(value) = package.source.as_deref() else {
            continue;
        };
        let Some(value) = value.strip_prefix("git+https://github.com/rrnewton/reverie.git?rev=")
        else {
            if value.contains("github.com/rrnewton/reverie") {
                return Err("source snapshot has a noncanonical Reverie source".to_owned());
            }
            continue;
        };
        let (declared, resolved) = value
            .split_once('#')
            .ok_or_else(|| "source snapshot Reverie has no resolved commit".to_owned())?;
        if !full_oid(declared) || declared != resolved {
            return Err(
                "source snapshot Reverie pin is incomplete or differs from resolution".to_owned(),
            );
        }
        let directory = package
            .manifest_path
            .parent()
            .ok_or_else(|| "source snapshot package has no directory".to_owned())?;
        let root = std::fs::canonicalize(git(directory, &["rev-parse", "--show-toplevel"])?)
            .map_err(|error| error.to_string())?;
        if roots
            .insert(root, resolved.to_owned())
            .is_some_and(|old| old != resolved)
        {
            return Err("source snapshot root claims different Reverie commits".to_owned());
        }
    }
    if roots.is_empty() {
        return Err("source snapshot has no actual Reverie source".to_owned());
    }
    let mut reverie = Vec::new();
    for (root, expected) in roots {
        let identity = source(&root, track)?;
        if identity["commit"].as_str() != Some(expected.as_str()) {
            return Err(
                "source snapshot actual Reverie checkout differs from its metadata source"
                    .to_owned(),
            );
        }
        reverie.push(serde_json::json!({"path":root,"identity":identity}));
    }
    Ok(
        serde_json::json!({"schema":"hermit-compatibility-source-snapshot/v1","core":core,"reverie":reverie}),
    )
}
