#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! detcore-model = { path = "../detcore-model" }
//! hermit-manifest-plan = { path = "../ci/manifest-plan" }
//! flate2 = "=1.1.10"
//! serde = { version = "1", features = ["derive"] }
//! serde_json = "1"
//! shell-words = "1.1"
//! tar = "=0.4.46"
//! ```
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Build and shadow-validate the feature-complete Buck release binary.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;
#[allow(dead_code)]
#[path = "lib/safe_ci_scope.rs"]
mod safe_ci_scope;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io::Cursor;
use std::io::Read;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use dagrun::model::CmdType;
use dagrun::model::DagConfig;
use dagrun::model::ResourceHint;
use dagrun::model::Step;
use dagrun::model::StepClass;
use dagrun::scheduler::BoxedCgroups;
use dagrun::scheduler::run_dag_boxed_deadline;
use dagrun::scheduler::steps_violating_run_timeout;
use detcore_model::build_info::BuildFeatures;
use detcore_model::build_info::BuildInfo;
use detcore_model::host_capability::HostCapabilitiesReport;
use flate2::Compression;
use flate2::GzBuilder;
use flate2::read::GzDecoder;
use flate2::read::MultiGzDecoder;
use hermit_manifest_plan::canonical_verdict::ComparedLogScope;
use hermit_manifest_plan::canonical_verdict::LogCompareStrictness;
use hermit_manifest_plan::canonical_verdict::RecordEnvelopeReport;
use hermit_manifest_plan::canonical_verdict::Verdict;
use hermit_manifest_plan::canonical_verdict::VerificationReport;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::de::MapAccess;
use serde::de::SeqAccess;
use serde::de::Visitor;
use serde_json::Value;
use tar::Archive;
use tar::Builder;
use tar::EntryType;
use tar::Header;
use tar::HeaderMode;

const TARGET: &str = "//hermit-cli:hermit-release";
const DOTSLASH_VERSION: &str = "DotSlash 0.5.9";
const RUST_SCRIPT_VERSION: &str = "rust-script 0.36.0";
const OUTER_SCOPE_DEADLINE_SECONDS: i64 = 7_200;
const PROBE_DEADLINE_SECONDS: u64 = 120;
const VERIFY_DEADLINE_SECONDS: u64 = 300;
const MATRIX_STEP_DEADLINE_SECONDS: i64 = 900;
const MATRIX_SUPERVISOR_DEADLINE_SECONDS: i64 = 960;
const MATRIX_CPU_TIMEOUT_SECONDS: i64 = 7_200;
const _: () = assert!(MATRIX_STEP_DEADLINE_SECONDS < MATRIX_SUPERVISOR_DEADLINE_SECONDS);
const MATRIX_CANDIDATE_DEADLINE_SECONDS: u64 = 120;
const MATRIX_PROXY_ENV: &str = "HERMIT_BUCK_PHASE1_MATRIX_PROXY";
const MAX_PUBLIC_EVIDENCE_GZIP_DECODED_BYTES: usize = 128 * 1024 * 1024;
static NEXT_ID: AtomicU64 = AtomicU64::new(0);
const GENERATED_MARKERS: [&str; 3] = [
    "name = \"reverie-dbt-0.2-materialized-manifest\"",
    "manifest_dir = \":reverie-dbt-0.2-materialized-manifest\"",
    "load(\"@shim//build_defs:materialized_manifest.bzl\", \"materialized_manifest\")",
];
const REQUIRED_RESOURCES: [(&str, bool); 8] = [
    ("rsrcs/libdetcore_dbt.so", false),
    ("rsrcs/libdetcore_sabre.so", false),
    ("rsrcs/libreverie_dbt_client.so", false),
    ("rsrcs/libreverie_liteinst.so", false),
    ("rsrcs/dynamorio/bin64/drrun", true),
    ("rsrcs/sabre", true),
    ("rsrcs/e9patch", true),
    ("rsrcs/e9tool", true),
];

#[derive(Debug, PartialEq, Eq)]
struct Options {
    dotslash: PathBuf,
    cargo_binary: PathBuf,
    install_bundle: PathBuf,
    safehermit: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
struct Provenance {
    version: String,
    build_date: String,
    hermit_sha: String,
    hermit_full_sha: String,
    reverie_sha: String,
}

#[derive(Debug, PartialEq, Eq)]
struct PublishedBundle {
    root: PathBuf,
    binary: PathBuf,
    install: PathBuf,
    hermit_manifest: PathBuf,
    resources_manifest: PathBuf,
    binary_sha256: String,
    hermit_manifest_sha256: String,
    resources_manifest_sha256: String,
    resource_inventory: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
struct SafehermitBundle {
    root: PathBuf,
    launcher: PathBuf,
    bounded_run_space: PathBuf,
    launcher_sha256: String,
    bounded_run_space_sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
struct InputSnapshot {
    path: PathBuf,
    hash_manifest: PathBuf,
    sha256: String,
    executable: bool,
}

fn usage() -> &'static str {
    "usage: build-buck-release.rs --dotslash ABSOLUTE-PATH \\\n+  --cargo-binary ABSOLUTE-PATH --install-bundle ABSOLUTE-PATH \\\n+  --safehermit ABSOLUTE-PATH\n\n\
Always regenerates the Buck Rust graph, builds the shadow Buck release with a\n\
retained .json-lines.gz event log, verifies that Buck can decode the log, and\n\
hard-fails unless Cargo/Buck provenance, features, backend inventory, resources,\n\
ELF contract, ptrace strict-verify, record/replay, and DBT strict-verify agree.\n\
The Cargo binary/install bundle remain authoritative; no target/ci pointer is changed."
}

fn parse_options<I>(arguments: I) -> Result<Option<Options>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut dotslash = None;
    let mut cargo_binary = None;
    let mut install_bundle = None;
    let mut safehermit = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if matches!(argument.as_str(), "-h" | "--help") {
            return Ok(None);
        }
        let destination = match argument.as_str() {
            "--dotslash" => &mut dotslash,
            "--cargo-binary" => &mut cargo_binary,
            "--install-bundle" => &mut install_bundle,
            "--safehermit" => &mut safehermit,
            _ => {
                return Err(format!(
                    "unknown argument {argument:?}; run with --help for the required shadow inputs"
                ));
            }
        };
        if destination.is_some() {
            return Err(format!(
                "duplicate {argument}; provide each required path once"
            ));
        }
        *destination = Some(PathBuf::from(arguments.next().ok_or_else(|| {
            format!("{argument} requires an absolute path; run with --help for an example")
        })?));
    }
    Ok(Some(Options {
        dotslash: dotslash.ok_or_else(|| {
            "missing --dotslash; provide the absolute public DotSlash 0.5.9 path".to_owned()
        })?,
        cargo_binary: cargo_binary.ok_or_else(|| {
            "missing --cargo-binary; first build Cargo release with third-party-backends".to_owned()
        })?,
        install_bundle: install_bundle.ok_or_else(|| {
            "missing --install-bundle; first build the authoritative hermit-install bundle"
                .to_owned()
        })?,
        safehermit: safehermit.ok_or_else(|| {
            "missing --safehermit; provide dev-hermit/bin/safehermit explicitly".to_owned()
        })?,
    }))
}

fn checked_output(command: &mut Command, description: &str) -> Result<Output, String> {
    let output = command
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{description} failed with {}: {}; correct the input/tool and retry",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

fn output_text(command: &mut Command, description: &str) -> Result<String, String> {
    let output = checked_output(command, description)?;
    String::from_utf8(output.stdout)
        .map(|text| text.trim().to_owned())
        .map_err(|error| format!("{description} emitted non-UTF-8 output: {error}"))
}

fn git(root: &Path, arguments: &[&str]) -> Result<String, String> {
    output_text(
        Command::new("git")
            .current_dir(root)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .args(arguments),
        &format!("git {}", arguments.join(" ")),
    )
}

fn repository_root() -> Result<PathBuf, String> {
    git(Path::new("."), &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

fn repository_dirty(root: &Path) -> Result<bool, String> {
    Ok(!git(root, &["status", "--porcelain", "--untracked-files=all"])?.is_empty())
}

fn unique_identity() -> Result<String, String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("system clock is before the Unix epoch: {error}"))?
        .as_nanos();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    Ok(format!("{}-{nanos}-{sequence}", std::process::id()))
}

fn create_exclusive_directory(path: &Path, description: &str) -> Result<(), String> {
    fs::create_dir(path).map_err(|error| {
        format!(
            "refusing non-exclusive {description} {}: {error}; choose a fresh identity and never reuse stale evidence",
            path.display()
        )
    })
}

fn create_evidence_directory(root: &Path, full_sha: &str) -> Result<PathBuf, String> {
    let parent = root.join("ignored/buck2-phase1");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let path = parent.join(format!("shadow-{full_sha}-{}", unique_identity()?));
    create_exclusive_directory(&path, "shadow evidence directory")?;
    Ok(path)
}

fn require_absolute_file(path: &Path, description: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{description} must be an absolute path, got {}; resolve it with realpath and retry",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "{description} {} must be a nonempty regular file, not a symlink",
            path.display()
        ));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "{description} {} is not executable; install/build it and retry",
            path.display()
        ));
    }
    Ok(path.to_owned())
}

fn require_absolute_directory(path: &Path, description: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "{description} must be an absolute path, got {}; resolve it with realpath and retry",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} {} must be a real directory, not a symlink",
            path.display()
        ));
    }
    Ok(path.to_owned())
}

fn package_version(manifest: &str) -> Result<String, String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if in_package && line.starts_with("version = ") {
            let value = line
                .strip_prefix("version = \"")
                .and_then(|value| value.strip_suffix('"'))
                .ok_or_else(|| "hermit-cli package version is not a quoted string".to_owned())?;
            if value.is_empty()
                || value == "unknown"
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            {
                return Err(format!("invalid hermit-cli package version {value:?}"));
            }
            return Ok(value.to_owned());
        }
    }
    Err("hermit-cli/Cargo.toml has no [package] version".to_owned())
}

struct DuplicateCheckedJson;

impl<'de> Deserialize<'de> for DuplicateCheckedJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(DuplicateCheckedJsonVisitor)?;
        Ok(Self)
    }
}

struct DuplicateCheckedJsonVisitor;

impl<'de> Visitor<'de> for DuplicateCheckedJsonVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("valid JSON without duplicate object keys")
    }

    fn visit_map<A>(self, mut map: A) -> Result<(), A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON field {key:?}"
                )));
            }
            map.next_value::<DuplicateCheckedJson>()?;
        }
        Ok(())
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<(), A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<DuplicateCheckedJson>()?.is_some() {}
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<(), E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
}

fn decode_typed_json<T: DeserializeOwned>(text: &str, description: &str) -> Result<T, String> {
    let mut duplicate_check = serde_json::Deserializer::from_str(text);
    DuplicateCheckedJson::deserialize(&mut duplicate_check)
        .and_then(|_| duplicate_check.end())
        .map_err(|error| format!("{description} is invalid JSON: {error}"))?;
    serde_json::from_str(text)
        .map_err(|error| format!("{description} violates its schema: {error}"))
}

fn canonical_build_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7) && !byte.is_ascii_digit())
    {
        return false;
    }
    let year = value[0..4].parse::<u32>().ok();
    let month = value[5..7].parse::<u32>().ok();
    let day = value[8..10].parse::<u32>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return false;
    };
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    year > 0 && (1..=days).contains(&day)
}

fn validate_build_info(
    info: &BuildInfo,
    expected_version: &str,
    expected_sha: &str,
    description: &str,
) -> Result<(), String> {
    if info.schema != BuildInfo::SCHEMA {
        return Err(format!(
            "{description} schema is {}, expected {}",
            info.schema,
            BuildInfo::SCHEMA
        ));
    }
    let date = info
        .build_date
        .as_deref()
        .ok_or_else(|| format!("{description} omits build_date"))?;
    let required_features = BuildFeatures {
        dbt: true,
        e9patch: true,
        sabre: true,
    };
    if info.version != expected_version
        || info.git_sha != expected_sha
        || info.git_sha.len() != 12
        || !info
            .git_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !canonical_build_date(date)
        || info.features != required_features
    {
        return Err(format!(
            "{description} does not match canonical version/date/12-character SHA/dbt+e9patch+sabre build facts"
        ));
    }
    Ok(())
}

fn decode_build_info(
    text: &str,
    expected_version: &str,
    expected_sha: &str,
    description: &str,
) -> Result<BuildInfo, String> {
    let info: BuildInfo = decode_typed_json(text, description)?;
    validate_build_info(&info, expected_version, expected_sha, description)?;
    Ok(info)
}

fn decode_host_capabilities(
    text: &str,
    description: &str,
) -> Result<HostCapabilitiesReport, String> {
    let report: HostCapabilitiesReport = decode_typed_json(text, description)?;
    report
        .validate()
        .map_err(|error| format!("{description} is invalid: {error}"))?;
    Ok(report)
}

fn exact_provenance(root: &Path, cargo_build_info: &BuildInfo) -> Result<Provenance, String> {
    let version = package_version(
        &fs::read_to_string(root.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let hermit_full_sha = git(root, &["rev-parse", "HEAD"])?;
    let short = git(root, &["rev-parse", "--short=12", "HEAD"])?;
    if hermit_full_sha.len() != 40
        || short.len() != 12
        || !hermit_full_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !short.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !hermit_full_sha.starts_with(&short)
    {
        return Err("Git returned an invalid full/12-character Hermit SHA".to_owned());
    }
    if repository_dirty(root)? {
        return Err(
            "Hermit checkout has tracked or non-ignored untracked changes; provenance requires a clean exact tree"
                .to_owned(),
        );
    }
    let expected_hermit_sha = short;
    validate_build_info(
        cargo_build_info,
        &version,
        &expected_hermit_sha,
        "Cargo version report",
    )?;
    let cargo_date = cargo_build_info
        .build_date
        .clone()
        .expect("validated build date");
    let hermit_sha = cargo_build_info.git_sha.clone();
    let reverie_sha = git(root, &["rev-parse", "HEAD:reverie"])?;
    if reverie_sha.len() != 40 || !reverie_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("reverie gitlink is not an exact 40-character SHA".to_owned());
    }
    let checkout_sha = git(&root.join("reverie"), &["rev-parse", "HEAD"])?;
    let checkout_dirty = git(
        &root.join("reverie"),
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if checkout_sha != reverie_sha || !checkout_dirty.is_empty() {
        return Err(format!(
            "Reverie checkout must be clean and equal gitlink {reverie_sha}; got {checkout_sha}; run git submodule update --init --recursive"
        ));
    }
    let lock = fs::read_to_string(root.join("Cargo.lock"))
        .map_err(|error| format!("failed to read Cargo.lock: {error}"))?;
    let expected_source =
        format!("git+https://github.com/rrnewton/reverie.git?rev={reverie_sha}#{reverie_sha}");
    let reverie_sources = lock
        .lines()
        .filter_map(|line| line.trim().strip_prefix("source = \"")?.strip_suffix('"'))
        .filter(|source| source.contains("github.com/rrnewton/reverie.git"))
        .collect::<Vec<_>>();
    if reverie_sources.is_empty()
        || reverie_sources
            .iter()
            .any(|source| *source != expected_source)
    {
        return Err(format!(
            "Cargo.lock contains a missing/mixed Reverie source; every entry must equal {expected_source}"
        ));
    }
    for manifest in [
        "detcore/Cargo.toml",
        "detcore-dbt/Cargo.toml",
        "hermit-cli/Cargo.toml",
    ] {
        let text = fs::read_to_string(root.join(manifest))
            .map_err(|error| format!("failed to read {manifest}: {error}"))?;
        if !text.contains(&format!("rev = \"{reverie_sha}\"")) {
            return Err(format!(
                "{manifest} does not bind Reverie to gitlink {reverie_sha}; run the pin synchronizer"
            ));
        }
    }
    Ok(Provenance {
        version,
        build_date: cargo_date,
        hermit_sha,
        hermit_full_sha,
        reverie_sha,
    })
}

fn sha256(path: &Path) -> Result<String, String> {
    let hash = output_text(
        Command::new("sha256sum").arg(path),
        &format!("sha256sum {}", path.display()),
    )?
    .split_whitespace()
    .next()
    .unwrap_or_default()
    .to_owned();
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "sha256sum returned an invalid digest for {}",
            path.display()
        ));
    }
    Ok(hash)
}

fn sha256_bytes(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start sha256sum for retained bytes: {error}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "sha256sum stdin was unavailable".to_owned())?
        .write_all(bytes)
        .map_err(|error| format!("failed to feed retained bytes to sha256sum: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("failed to wait for byte sha256sum: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "sha256sum of retained bytes failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let hash = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if !valid_sha256(&hash) {
        return Err("sha256sum returned an invalid digest for retained bytes".to_owned());
    }
    Ok(hash)
}

fn copy_stable_executable(
    source: &Path,
    destination: &Path,
    description: &str,
) -> Result<String, String> {
    let before = sha256(source)?;
    fs::copy(source, destination).map_err(|error| {
        format!(
            "failed to snapshot {description} from {} to {}: {error}",
            source.display(),
            destination.display()
        )
    })?;
    let after = sha256(source)?;
    let copied = require_absolute_file(destination, &format!("snapshotted {description}"))?;
    let copied_hash = sha256(&copied)?;
    if before != after || before != copied_hash {
        return Err(format!(
            "{description} changed while its execution snapshot was copied"
        ));
    }
    Ok(copied_hash)
}

fn snapshot_input(
    source: &Path,
    evidence_dir: &Path,
    directory_name: &str,
    file_name: &str,
    description: &str,
    executable: bool,
) -> Result<InputSnapshot, String> {
    let source = if executable {
        require_absolute_file(source, description)?
    } else {
        if !source.is_absolute() {
            return Err(format!("{description} must use an absolute path"));
        }
        require_nonempty_regular_file(source, description)?
    };
    let root = evidence_dir.join(directory_name);
    create_exclusive_directory(&root, &format!("{description} snapshot"))?;
    let path = root.join(file_name);
    let sha256 = if executable {
        copy_stable_executable(&source, &path, description)?
    } else {
        let before = sha256(&source)?;
        fs::copy(&source, &path).map_err(|error| {
            format!(
                "failed to snapshot {description} from {} to {}: {error}",
                source.display(),
                path.display()
            )
        })?;
        let after = sha256(&source)?;
        let copied = require_nonempty_regular_file(&path, &format!("snapshotted {description}"))?;
        let copied_hash = sha256(&copied)?;
        if before != after || before != copied_hash {
            return Err(format!(
                "{description} changed while its snapshot was copied"
            ));
        }
        copied_hash
    };
    let hash_manifest = root.join("snapshot-sha256.tsv");
    atomic_write_new(
        &hash_manifest,
        &format!(
            "source_before_sha256\t{sha256}\ncopied_sha256\t{sha256}\nsource_after_sha256\t{sha256}\n"
        ),
    )?;
    Ok(InputSnapshot {
        path,
        hash_manifest,
        sha256,
        executable,
    })
}

fn snapshot_dotslash(source: &Path, evidence_dir: &Path) -> Result<InputSnapshot, String> {
    snapshot_input(
        source,
        evidence_dir,
        "dotslash-tool-snapshot",
        "dotslash",
        "public DotSlash launcher",
        true,
    )
}

fn reverify_input_snapshot(snapshot: &InputSnapshot, description: &str) -> Result<(), String> {
    reverify_hashed_input(
        &snapshot.path,
        &snapshot.sha256,
        description,
        snapshot.executable,
    )?;
    let expected = format!(
        "source_before_sha256\t{}\ncopied_sha256\t{}\nsource_after_sha256\t{}\n",
        snapshot.sha256, snapshot.sha256, snapshot.sha256,
    );
    let actual = fs::read_to_string(&snapshot.hash_manifest).map_err(|error| {
        format!(
            "input snapshot hash manifest {} is unreadable: {error}",
            snapshot.hash_manifest.display()
        )
    })?;
    if actual != expected {
        return Err(format!(
            "{description} snapshot before/copy/after hash evidence changed"
        ));
    }
    Ok(())
}

fn snapshot_safehermit_bundle(
    source_launcher: &Path,
    evidence_dir: &Path,
) -> Result<SafehermitBundle, String> {
    let source_launcher = require_absolute_file(source_launcher, "safehermit launcher")?;
    if source_launcher.file_name() != Some(OsStr::new("safehermit"))
        || source_launcher.parent().and_then(Path::file_name) != Some(OsStr::new("bin"))
    {
        return Err(
            "safehermit must use its reviewed ROOT/bin/safehermit layout so ROOT/scripts/bounded-run-space can be snapshotted"
                .to_owned(),
        );
    }
    let source_root = source_launcher
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| "safehermit launcher has no bundle root".to_owned())?;
    let source_bounded_run_space = require_absolute_file(
        &source_root.join("scripts/bounded-run-space"),
        "safehermit bounded-run-space companion",
    )?;

    let root = evidence_dir.join("safehermit-tool-snapshot");
    create_exclusive_directory(&root, "safehermit tool snapshot")?;
    fs::create_dir(root.join("bin"))
        .map_err(|error| format!("failed to create safehermit snapshot bin directory: {error}"))?;
    fs::create_dir(root.join("scripts")).map_err(|error| {
        format!("failed to create safehermit snapshot scripts directory: {error}")
    })?;
    let launcher = root.join("bin/safehermit");
    let bounded_run_space = root.join("scripts/bounded-run-space");
    let launcher_sha256 =
        copy_stable_executable(&source_launcher, &launcher, "safehermit launcher")?;
    let bounded_run_space_sha256 = copy_stable_executable(
        &source_bounded_run_space,
        &bounded_run_space,
        "safehermit bounded-run-space companion",
    )?;
    Ok(SafehermitBundle {
        root,
        launcher,
        bounded_run_space,
        launcher_sha256,
        bounded_run_space_sha256,
    })
}

fn reverify_safehermit_bundle(bundle: &SafehermitBundle) -> Result<(), String> {
    let launcher = require_absolute_file(&bundle.launcher, "snapshotted safehermit launcher")?;
    let bounded_run_space = require_absolute_file(
        &bundle.bounded_run_space,
        "snapshotted safehermit bounded-run-space companion",
    )?;
    if launcher != bundle.root.join("bin/safehermit")
        || bounded_run_space != bundle.root.join("scripts/bounded-run-space")
        || sha256(&launcher)? != bundle.launcher_sha256
        || sha256(&bounded_run_space)? != bundle.bounded_run_space_sha256
    {
        return Err("snapshotted safehermit execution bundle changed after publication".to_owned());
    }
    Ok(())
}

fn reverify_hashed_input(
    path: &Path,
    expected_sha256: &str,
    description: &str,
    executable: bool,
) -> Result<(), String> {
    let path = if executable {
        require_absolute_file(path, description)?
    } else {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
        if !path.is_absolute() || !metadata.is_file() || metadata.len() == 0 {
            return Err(format!(
                "{description} {} must resolve to a nonempty file",
                path.display()
            ));
        }
        path.to_owned()
    };
    if sha256(&path)? != expected_sha256 {
        return Err(format!("{description} changed during shadow validation"));
    }
    Ok(())
}

fn require_lzma_link_input() -> Result<PathBuf, String> {
    let path = PathBuf::from(output_text(
        Command::new("gcc").arg("-print-file-name=liblzma.so"),
        "gcc liblzma link-input lookup",
    )?);
    if !path.is_absolute() || !path.is_file() {
        return Err(
            "release linkage requires -llzma, but gcc cannot resolve liblzma.so; install liblzma-dev (Debian/Ubuntu) or xz-devel (Fedora/CentOS) and retry"
                .to_owned(),
        );
    }
    Ok(path)
}

fn verify_generated_buck(text: &str) -> Result<(), String> {
    for marker in GENERATED_MARKERS {
        let count = text.match_indices(marker).count();
        if count != 1 {
            return Err(format!(
                "generated Buck graph is stale/partial: marker {marker:?} occurs {count} times; rerun bootstrap/regenerate-rust-deps"
            ));
        }
    }
    let mapped_files = text
        .lines()
        .filter(|line| {
            line.trim_start().starts_with('"') && line.contains(": \"vendor/reverie-dbt-0.2.0/")
        })
        .count();
    if mapped_files != 931 {
        return Err(format!(
            "generated reverie-dbt manifest map has {mapped_files} files, expected 931; rerun regeneration and inspect the pin"
        ));
    }
    Ok(())
}

fn validate_log_header(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Buck event log {} is unreadable: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() < 2 {
        return Err(format!(
            "Buck event log {} is empty/truncated",
            path.display()
        ));
    }
    let mut header = [0_u8; 2];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("cannot read Buck event log {}: {error}", path.display()))?;
    if header != [0x1f, 0x8b] {
        return Err(format!(
            "Buck event log {} is not gzip-compressed .json-lines.gz evidence",
            path.display()
        ));
    }
    Ok(())
}

const RELEASE_SHOW_OUTPUT_TARGET: &str = "hermit//hermit-cli:hermit-release";
const RELEASE_OUTPUT_SUFFIX: &str = "hermit-cli/___hermit-release__/hermit";

fn resolve_release_show_output(stdout: &str, workspace: &Path) -> Result<PathBuf, String> {
    let lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.len() != 1 {
        return Err(format!(
            "Buck --show-output must contain exactly one target/output row, got {}",
            lines.len()
        ));
    }
    let fields = lines[0].split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 || fields[0] != RELEASE_SHOW_OUTPUT_TARGET {
        return Err(
            "Buck --show-output row does not name the canonical release target exactly once"
                .to_owned(),
        );
    }
    let output = PathBuf::from(fields[1]);
    let output = if output.is_absolute() {
        output
    } else {
        workspace.join(output)
    };
    let output = require_absolute_file(&output, "Buck release --show-output binary")?;
    let canonical_workspace = workspace
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize workflow workspace: {error}"))?;
    let canonical_buck_out = require_absolute_directory(
        &canonical_workspace.join("buck-out"),
        "workflow Buck output root",
    )?
    .canonicalize()
    .map_err(|error| format!("cannot canonicalize workflow Buck output root: {error}"))?;
    let canonical_output = output
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize reconciled Buck release output: {error}"))?;
    let relative = canonical_output
        .strip_prefix(&canonical_buck_out)
        .map_err(|_| {
            format!(
                "Buck release --show-output binary {} is outside canonical workspace buck-out {}",
                canonical_output.display(),
                canonical_buck_out.display(),
            )
        })?;
    let suffix = Path::new(RELEASE_OUTPUT_SUFFIX);
    if !canonical_buck_out.starts_with(&canonical_workspace)
        || !relative.ends_with(suffix)
        || relative.components().count() <= suffix.components().count()
    {
        return Err(format!(
            "Buck release output {} does not have the measured configured-target layout <config>/{RELEASE_OUTPUT_SUFFIX}",
            canonical_output.display(),
        ));
    }
    let (class, endian, object_type, machine) = elf_identity(&canonical_output)?;
    if class != 2 || endian != 1 || !matches!(object_type, 2 | 3) || machine != 62 {
        return Err(format!(
            "Buck release output {} is not an x86_64 ELF executable",
            canonical_output.display(),
        ));
    }
    Ok(canonical_output)
}

#[derive(Debug, PartialEq, Eq)]
struct ReleaseBuildEvidence {
    command_end_is_success: bool,
    output: PathBuf,
    output_sha256: String,
}

fn inspect_release_build_evidence(
    event_log: &Path,
    show_output: &Path,
    shell_exit: i32,
    workspace: &Path,
) -> Result<ReleaseBuildEvidence, String> {
    if shell_exit != 0 {
        return Err(format!(
            "Buck release shell exit was {shell_exit}, expected zero"
        ));
    }
    validate_log_header(event_log)?;
    let decoded = checked_output(
        Command::new("gzip").args([OsStr::new("-cd"), event_log.as_os_str()]),
        "decode retained Buck release event log",
    )?;
    let text = String::from_utf8(decoded.stdout)
        .map_err(|error| format!("Buck release event log is not UTF-8 JSON-lines: {error}"))?;
    let mut commands = Vec::new();
    let mut results = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let event: Value = serde_json::from_str(line).map_err(|error| {
            format!(
                "Buck release event log line {} is invalid JSON: {error}",
                index + 1
            )
        })?;
        if let Some(command) = event.pointer("/Event/data/SpanEnd/data/Command") {
            commands.push(command.clone());
        }
        if let Some(result) = event.pointer("/Result/result/build_response") {
            results.push(result.clone());
        }
    }
    if commands.len() != 1 || results.len() != 1 {
        return Err(format!(
            "Buck release evidence needs exactly one CommandEnd and one Result, got {} and {}",
            commands.len(),
            results.len()
        ));
    }
    let command_success = commands[0]
        .get("is_success")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Buck release CommandEnd lacks boolean is_success".to_owned())?;
    let build_completed = commands[0]
        .pointer("/build_result/build_completed")
        .and_then(Value::as_bool)
        .ok_or_else(|| "Buck release CommandEnd lacks boolean build_completed".to_owned())?;
    if !build_completed {
        return Err("Buck release CommandEnd does not prove build completion".to_owned());
    }
    let errors = results[0]
        .get("errors")
        .and_then(Value::as_array)
        .ok_or_else(|| "Buck release Result lacks an errors array".to_owned())?;
    if !errors.is_empty() {
        return Err(format!(
            "Buck release Result contains {} build errors",
            errors.len()
        ));
    }
    let targets = results[0]
        .get("build_targets")
        .and_then(Value::as_array)
        .ok_or_else(|| "Buck release Result lacks build_targets".to_owned())?;
    let matching_targets = targets
        .iter()
        .filter(|target| {
            target.get("target").and_then(Value::as_str) == Some(RELEASE_SHOW_OUTPUT_TARGET)
        })
        .count();
    if targets.len() != 1 || matching_targets != 1 {
        return Err(format!(
            "Buck release Result contains {} targets and names {matching_targets} canonical release targets, expected exactly one canonical target",
            targets.len(),
        ));
    }

    let stdout = fs::read_to_string(show_output).map_err(|error| {
        format!(
            "Buck --show-output evidence {} is unreadable: {error}",
            show_output.display()
        )
    })?;
    let canonical_output = resolve_release_show_output(&stdout, workspace)?;
    let output_hash = sha256(&canonical_output)?;
    Ok(ReleaseBuildEvidence {
        command_end_is_success: command_success,
        output: canonical_output,
        output_sha256: output_hash,
    })
}

fn reconcile_release_build_evidence(
    event_log: &Path,
    show_output: &Path,
    shell_exit: i32,
    workspace: &Path,
    receipt: &Path,
) -> Result<(), String> {
    let evidence = inspect_release_build_evidence(event_log, show_output, shell_exit, workspace)?;
    let receipt_text = format!(
        "reconciliation_schema\thermit-buck-release-build/v1\nshell_exit\t0\ncommand_end_is_success\t{}\ncommand_end_status\tadvisory-known-inconsistent\nbuild_completed\ttrue\nresult_errors\t0\nrelease_target\thermit//hermit-cli:hermit-release\nrelease_output\t{}\nrelease_output_sha256\t{}\n",
        evidence.command_end_is_success,
        evidence.output.display(),
        evidence.output_sha256,
    );
    write_new_file(
        receipt,
        receipt_text.as_bytes(),
        "Buck release reconciliation receipt",
    )
}

fn run_release_evidence_reconciler(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 10
        || arguments[0] != "--event-log"
        || arguments[2] != "--show-output"
        || arguments[4] != "--shell-exit"
        || arguments[6] != "--workspace"
        || arguments[8] != "--receipt"
    {
        return Err("usage: build-buck-release.rs --reconcile-release-evidence --event-log PATH --show-output PATH --shell-exit INTEGER --workspace ABSOLUTE-PATH --receipt PATH".to_owned());
    }
    let shell_exit = arguments[5]
        .parse::<i32>()
        .map_err(|_| "--shell-exit must be an integer".to_owned())?;
    let workspace = require_absolute_directory(Path::new(&arguments[7]), "workflow workspace")?;
    reconcile_release_build_evidence(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        shell_exit,
        &workspace,
        Path::new(&arguments[9]),
    )
}

fn parse_release_metadata(path: &Path) -> Result<(String, String, String, String), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("release metadata {} is unreadable: {error}", path.display()))?;
    let mut fields = BTreeMap::new();
    for line in text.lines() {
        let (name, value) = line
            .split_once('\t')
            .ok_or_else(|| "release metadata contains a malformed row".to_owned())?;
        if !matches!(
            name,
            "version" | "build_date" | "hermit_sha" | "reverie_sha"
        ) || value.is_empty()
            || fields.insert(name, value).is_some()
        {
            return Err("release metadata contains unknown, empty, or duplicate facts".to_owned());
        }
    }
    if fields.len() != 4 {
        return Err("release metadata lacks required exact facts".to_owned());
    }
    Ok((
        fields["version"].to_owned(),
        fields["build_date"].to_owned(),
        fields["hermit_sha"].to_owned(),
        fields["reverie_sha"].to_owned(),
    ))
}

fn validate_release_candidate_version(
    show_output: &Path,
    metadata: &Path,
    workspace: &Path,
    version_json: &Path,
    receipt: &Path,
) -> Result<(), String> {
    let stdout = fs::read_to_string(show_output).map_err(|error| {
        format!(
            "release show-output {} is unreadable: {error}",
            show_output.display()
        )
    })?;
    let candidate = resolve_release_show_output(&stdout, workspace)?;
    let (version, build_date, hermit_sha, reverie_sha) = parse_release_metadata(metadata)?;
    let expected_version = package_version(
        &fs::read_to_string(workspace.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let current_hermit_sha = git(workspace, &["rev-parse", "--short=12", "HEAD"])?;
    let current_reverie_sha = git(workspace, &["rev-parse", "HEAD:reverie"])?;
    if version != expected_version
        || hermit_sha != current_hermit_sha
        || reverie_sha != current_reverie_sha
        || reverie_sha.len() != 40
        || !canonical_build_date(&build_date)
    {
        return Err(
            "release metadata differs from current package/Hermit/Reverie provenance".to_owned(),
        );
    }
    // Deliberately omit --foreground: GNU timeout then owns a separate process
    // group and applies TERM/KILL to the bounded CLI-only process tree.
    let output = Command::new("timeout")
        .args(["--kill-after=2s", "10s"])
        .arg(&candidate)
        .args(["version", "--json"])
        .output()
        .map_err(|error| format!("failed to start bounded release version probe: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "bounded release version probe failed with {}",
            output.status
        ));
    }
    if !output.stderr.is_empty() {
        return Err("bounded release version probe emitted unexpected stderr".to_owned());
    }
    write_new_file(
        version_json,
        &output.stdout,
        "release candidate version JSON",
    )?;
    let output_text = String::from_utf8(output.stdout)
        .map_err(|error| format!("release version probe emitted non-UTF-8 JSON: {error}"))?;
    let build_info = decode_build_info(
        output_text.trim(),
        &version,
        &hermit_sha,
        "release candidate version JSON",
    )?;
    if build_info.build_date.as_deref() != Some(build_date.as_str()) {
        return Err("release candidate version JSON build date differs from metadata".to_owned());
    }
    let receipt_text = format!(
        "candidate_validation_schema\thermit-buck-release-candidate/v1\ncandidate\t{}\ncandidate_sha256\t{}\nversion\t{}\nbuild_date\t{}\nhermit_sha\t{}\nreverie_sha\t{}\nfeatures\tdbt,e9patch,sabre\nversion_probe_timeout_seconds\t10\n",
        candidate.display(),
        sha256(&candidate)?,
        version,
        build_date,
        hermit_sha,
        reverie_sha,
    );
    write_new_file(
        receipt,
        receipt_text.as_bytes(),
        "release candidate validation receipt",
    )
}

fn run_release_candidate_validator(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 10
        || arguments[0] != "--show-output"
        || arguments[2] != "--metadata"
        || arguments[4] != "--workspace"
        || arguments[6] != "--version-json"
        || arguments[8] != "--receipt"
    {
        return Err("usage: build-buck-release.rs --validate-release-candidate --show-output PATH --metadata PATH --workspace ABSOLUTE-PATH --version-json PATH --receipt PATH".to_owned());
    }
    let workspace = require_absolute_directory(Path::new(&arguments[5]), "workflow workspace")?;
    validate_release_candidate_version(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        &workspace,
        Path::new(&arguments[7]),
        Path::new(&arguments[9]),
    )
}

fn has_credential_key_assignment(lower: &str, key: &str) -> bool {
    let bytes = lower.as_bytes();
    for (start, _) in lower.match_indices(key) {
        let end = start + key.len();
        let is_key_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
        if start > 0 && is_key_byte(bytes[start - 1]) {
            continue;
        }
        if end < bytes.len() && is_key_byte(bytes[end]) {
            continue;
        }
        let mut cursor = end;
        if cursor + 1 < bytes.len()
            && bytes[cursor] == b'\\'
            && matches!(bytes[cursor + 1], b'\'' | b'"')
        {
            cursor += 2;
        } else if cursor < bytes.len() && matches!(bytes[cursor], b'\'' | b'"') {
            cursor += 1;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor < bytes.len() && matches!(bytes[cursor], b':' | b'=') {
            return true;
        }
    }
    false
}

fn line_has_known_credential_marker(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    let literals = [
        "authorization",
        "proxy-authorization:",
        "x-api-key:",
        "x-auth-token:",
        "cookie:",
        "set-cookie:",
        "access_token",
        "access-token=",
        "actions_runtime_token",
        "actions_id_token_request_token",
        "refresh_token",
        "github_token",
        "gh_token",
        "ghp_",
        "ghs_",
        "gho_",
        "ghu_",
        "ghr_",
        "github_pat_",
        "aws_access_key_id",
        "akia",
        "security-token",
        "aws_secret_access_key",
        "aws_session_token",
        "asia",
        "bearer ",
        "security_token",
        "security token",
        "-----begin private key-----",
        "-----begin rsa private key-----",
        "-----begin ec private key-----",
        "-----begin openssh private key-----",
    ];
    if literals.iter().any(|marker| lower.contains(marker)) {
        return true;
    }
    if [
        "password",
        "passwd",
        "pwd",
        "secret_key",
        "client_secret",
        "private_key",
    ]
    .iter()
    .any(|key| has_credential_key_assignment(&lower, key))
    {
        return true;
    }
    let mut remainder = lower.as_str();
    while let Some((_, after_scheme)) = remainder.split_once("://") {
        let authority = after_scheme
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default();
        if authority.contains('@') {
            return true;
        }
        remainder = after_scheme;
    }
    false
}

#[derive(Default)]
struct PublicEvidenceScan {
    decoded_gzip_files: usize,
    decoded_bytes: u64,
    marker_lines: usize,
}

struct StableScannedFile {
    identity: StableFileIdentity,
    raw_bytes: Vec<u8>,
}

fn decode_gzip_bytes_with_limit(raw: &[u8], max_decoded_bytes: usize) -> Result<Vec<u8>, String> {
    if !raw.starts_with(&[0x1f, 0x8b]) {
        return Err("prospective .json-lines.gz evidence lacks a gzip header".to_owned());
    }
    let read_limit = max_decoded_bytes
        .checked_add(1)
        .ok_or_else(|| "prospective gzip evidence decode limit overflowed".to_owned())?;
    let read_limit = u64::try_from(read_limit)
        .map_err(|_| "prospective gzip evidence decode limit is unsupported".to_owned())?;
    let mut decoded = Vec::new();
    MultiGzDecoder::new(Cursor::new(raw))
        .take(read_limit)
        .read_to_end(&mut decoded)
        .map_err(|error| format!("prospective gzip evidence is truncated or corrupt: {error}"))?;
    if decoded.len() > max_decoded_bytes {
        return Err(format!(
            "prospective gzip evidence expands beyond the {max_decoded_bytes}-byte decode limit"
        ));
    }
    Ok(decoded)
}

fn decode_gzip_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    decode_gzip_bytes_with_limit(raw, MAX_PUBLIC_EVIDENCE_GZIP_DECODED_BYTES)
}

fn scan_public_evidence_tree(
    root: &Path,
    directory: &Path,
    scan: &mut PublicEvidenceScan,
    files: &mut BTreeMap<String, StableScannedFile>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "failed to enumerate public evidence {}: {error}",
                directory.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to enumerate public evidence: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative_path = path
            .strip_prefix(root)
            .map_err(|_| format!("public evidence path escaped scan root: {}", path.display()))?;
        let relative = relative_path
            .to_str()
            .ok_or_else(|| "public evidence path is not valid UTF-8".to_owned())?;
        if relative.contains(['\n', '\r', '\t', '\\']) {
            return Err(format!(
                "public evidence path is manifest-unsafe: {relative:?}"
            ));
        }
        if line_has_known_credential_marker(relative) {
            scan.marker_lines += 1;
        }
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "failed to inspect public evidence {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "public evidence contains symlink {}; refusing upload",
                path.display()
            ));
        }
        if metadata.is_dir() {
            scan_public_evidence_tree(root, &path, scan, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(format!(
                "public evidence contains special file {}; refusing upload",
                path.display()
            ));
        }
        let stable = read_stable_file_bytes(&path)?;
        let bytes = if path
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.ends_with(".json-lines.gz"))
        {
            scan.decoded_gzip_files += 1;
            decode_gzip_bytes(&stable.raw_bytes)?
        } else {
            stable.raw_bytes.clone()
        };
        scan.decoded_bytes = scan
            .decoded_bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "public evidence decoded byte count overflowed".to_owned())?;
        scan.marker_lines += String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| line_has_known_credential_marker(line))
            .count();
        if files.insert(relative.to_owned(), stable).is_some() {
            return Err("public evidence traversal produced a duplicate file identity".to_owned());
        }
    }
    Ok(())
}

fn stable_public_evidence_population(
    root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, StableFileIdentity>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to re-enumerate public evidence: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read public evidence entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to re-inspect public evidence: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "public evidence contains symlink {}; refusing upload",
                path.display()
            ));
        }
        if metadata.is_dir() {
            stable_public_evidence_population(root, &path, files)?;
        } else if metadata.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .map_err(|_| "public evidence re-verification escaped root".to_owned())?;
            let relative = relative_path
                .to_str()
                .ok_or_else(|| "public evidence path became non-UTF-8".to_owned())?
                .to_owned();
            if relative.contains(['\n', '\r', '\t', '\\']) {
                return Err("public evidence path became manifest-unsafe".to_owned());
            }
            if files
                .insert(relative, read_stable_file_bytes(&path)?.identity)
                .is_some()
            {
                return Err("public evidence re-verification found a duplicate file".to_owned());
            }
        } else {
            return Err(format!(
                "public evidence contains special file {}; refusing upload",
                path.display()
            ));
        }
    }
    Ok(())
}

fn capture_public_evidence(
    root: &Path,
) -> Result<
    (
        PathBuf,
        PublicEvidenceScan,
        BTreeMap<String, StableScannedFile>,
    ),
    String,
> {
    let root = require_absolute_directory(root, "public evidence directory")?;
    let canonical_root = fs::canonicalize(&root)
        .map_err(|error| format!("cannot canonicalize public evidence root: {error}"))?;
    if canonical_root != root {
        return Err("public evidence root must use its canonical real path".to_owned());
    }
    for reserved in ["SHA256SUMS", "public-evidence-scan.tsv"] {
        if root.join(reserved).exists() {
            return Err(format!(
                "public evidence source contains reserved generated path {reserved:?}"
            ));
        }
    }
    let mut scan = PublicEvidenceScan::default();
    let mut files = BTreeMap::new();
    scan_public_evidence_tree(&root, &root, &mut scan, &mut files)?;
    if scan.marker_lines != 0 {
        return Err(format!(
            "public evidence contains {} lines with known credential markers; refusing upload",
            scan.marker_lines
        ));
    }
    Ok((root, scan, files))
}

fn render_captured_evidence_metadata(
    scan: &PublicEvidenceScan,
    files: &BTreeMap<String, StableScannedFile>,
    receipt_name: &str,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut receipt = format!(
        "scan_schema\thermit-public-evidence/v1\nregular_files\t{}\ndecoded_gzip_files\t{}\ndecoded_bytes\t{}\nknown_credential_marker_lines\t0\n",
        files.len() + 2,
        scan.decoded_gzip_files,
        scan.decoded_bytes,
    );
    for (relative, file) in files {
        receipt.push_str(&format!(
            "file_sha256\t{}\t{relative}\n",
            file.identity.sha256
        ));
    }
    if line_has_known_credential_marker(&receipt) {
        return Err(
            "captured evidence receipt unexpectedly matches a credential marker".to_owned(),
        );
    }
    let receipt_bytes = receipt.into_bytes();
    let mut checksum_rows = files
        .iter()
        .map(|(relative, file)| (relative.clone(), file.identity.sha256.clone()))
        .collect::<BTreeMap<_, _>>();
    checksum_rows.insert(receipt_name.to_owned(), sha256_bytes(&receipt_bytes)?);
    let checksums = checksum_rows
        .into_iter()
        .map(|(relative, hash)| format!("{hash}  {relative}\n"))
        .collect::<String>()
        .into_bytes();
    Ok((receipt_bytes, checksums))
}

fn captured_archive_population(
    files: &BTreeMap<String, StableScannedFile>,
    receipt_name: &str,
    receipt: &[u8],
    checksums: &[u8],
) -> BTreeMap<String, Vec<u8>> {
    let mut population = files
        .iter()
        .map(|(relative, file)| (relative.clone(), file.raw_bytes.clone()))
        .collect::<BTreeMap<_, _>>();
    population.insert(receipt_name.to_owned(), receipt.to_vec());
    population.insert("SHA256SUMS".to_owned(), checksums.to_vec());
    population
}

fn deterministic_tar_gz(population: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, String> {
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::default());
    let mut builder = Builder::new(encoder);
    builder.mode(HeaderMode::Deterministic);
    for (relative, bytes) in population {
        let path = Path::new(relative);
        if path.is_absolute()
            || path.as_os_str().is_empty()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!("unsafe captured archive path {relative:?}"));
        }
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_mode(0o444);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder
            .append_data(&mut header, path, Cursor::new(bytes))
            .map_err(|error| format!("failed to append captured evidence to tar: {error}"))?;
    }
    let encoder = builder
        .into_inner()
        .map_err(|error| format!("failed to finish captured evidence tar: {error}"))?;
    encoder
        .finish()
        .map_err(|error| format!("failed to finish captured evidence gzip: {error}"))
}

fn parse_checksum_bytes(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| format!("archived SHA256SUMS is not UTF-8: {error}"))?;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (hash, relative) = line
            .split_once("  ")
            .ok_or_else(|| "archived SHA256SUMS contains a malformed row".to_owned())?;
        if !valid_sha256(hash)
            || relative.is_empty()
            || result
                .insert(relative.to_owned(), hash.to_owned())
                .is_some()
        {
            return Err("archived SHA256SUMS contains an invalid or duplicate row".to_owned());
        }
    }
    Ok(result)
}

fn verify_captured_archive(
    archive_bytes: &[u8],
    expected: &BTreeMap<String, Vec<u8>>,
) -> Result<(), String> {
    let decoder = GzDecoder::new(Cursor::new(archive_bytes));
    let mut archive = Archive::new(decoder);
    let mut actual = BTreeMap::new();
    for entry in archive
        .entries()
        .map_err(|error| format!("cannot enumerate captured evidence archive: {error}"))?
    {
        let mut entry = entry.map_err(|error| format!("cannot read archive entry: {error}"))?;
        if !entry.header().entry_type().is_file() {
            return Err("captured evidence archive contains a non-regular member".to_owned());
        }
        let path = entry
            .path()
            .map_err(|error| format!("cannot decode archive member path: {error}"))?
            .into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err("captured evidence archive contains an unsafe member path".to_owned());
        }
        let relative = path.to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read archived evidence bytes: {error}"))?;
        if actual.insert(relative, bytes).is_some() {
            return Err("captured evidence archive contains a duplicate member".to_owned());
        }
    }
    if &actual != expected {
        return Err("captured evidence archive population/bytes differ from capture".to_owned());
    }
    let checksum_bytes = actual
        .get("SHA256SUMS")
        .ok_or_else(|| "captured evidence archive lacks SHA256SUMS".to_owned())?;
    let checksums = parse_checksum_bytes(checksum_bytes)?;
    let observed = actual
        .iter()
        .filter(|(relative, _)| relative.as_str() != "SHA256SUMS")
        .map(|(relative, bytes)| Ok((relative.clone(), sha256_bytes(bytes)?)))
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    if observed != checksums {
        return Err("archived SHA256SUMS does not bind the archived member bytes".to_owned());
    }
    Ok(())
}

fn reverify_captured_sources(
    root: &Path,
    files: &BTreeMap<String, StableScannedFile>,
) -> Result<(), String> {
    let expected = files
        .iter()
        .map(|(relative, file)| (relative.clone(), &file.identity))
        .collect::<BTreeMap<_, _>>();
    let mut actual = BTreeMap::new();
    stable_public_evidence_population(root, root, &mut actual)?;
    if actual.len() != expected.len()
        || actual
            .iter()
            .any(|(relative, identity)| expected.get(relative) != Some(&identity))
    {
        return Err("captured evidence source population/identity changed".to_owned());
    }
    Ok(())
}

fn atomic_write_new_bytes(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "refusing existing {description} {}",
            path.display()
        ));
    }
    let temporary = path.with_extension(format!("tmp.{}", unique_identity()?));
    write_new_file(&temporary, bytes, &format!("temporary {description}"))?;
    let publish = fs::hard_link(&temporary, path).map_err(|error| {
        format!(
            "failed to publish {description} {} without replacement: {error}",
            path.display()
        )
    });
    let cleanup = fs::remove_file(&temporary)
        .map_err(|error| format!("failed to remove temporary {description}: {error}"));
    match (publish, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => match fs::remove_file(path) {
            Ok(()) => Err(error),
            Err(rollback) => Err(format!(
                "{error}; failed to roll back published {description}: {rollback}"
            )),
        },
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup also failed: {cleanup}")),
    }
}

fn publish_read_only_file(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!("{description} path must be absolute"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{description} path has no parent"))?;
    require_absolute_directory(parent, &format!("{description} parent"))?;
    atomic_write_new_bytes(path, bytes, description)?;
    let mut permissions = fs::metadata(path)
        .map_err(|error| format!("cannot inspect {description}: {error}"))?
        .permissions();
    permissions.set_mode(0o444);
    if let Err(error) = fs::set_permissions(path, permissions) {
        return match fs::remove_file(path) {
            Ok(()) => Err(format!("cannot make {description} read-only: {error}")),
            Err(rollback) => Err(format!(
                "cannot make {description} read-only: {error}; rollback failed: {rollback}"
            )),
        };
    }
    Ok(())
}

fn validate_combined_output_path(
    source_root: &Path,
    output: &Path,
    description: &str,
) -> Result<(), String> {
    if !output.is_absolute()
        || output
            .file_name()
            .and_then(OsStr::to_str)
            .is_none_or(|name| name.is_empty() || name.contains(['\n', '\r', '\t', '\\']))
    {
        return Err(format!(
            "{description} must be an absolute manifest-safe file path"
        ));
    }
    let parent = output
        .parent()
        .ok_or_else(|| format!("{description} has no parent directory"))?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("cannot canonicalize {description} parent: {error}"))?;
    if canonical_parent != parent || canonical_parent.starts_with(source_root) {
        return Err(format!(
            "{description} parent must be canonical and outside the captured source root"
        ));
    }
    Ok(())
}

fn scan_and_package_public_evidence_with_hook<F>(
    root: &Path,
    archive: &Path,
    receipt_output: &Path,
    hook: F,
) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    let (root, scan, files) = capture_public_evidence(root)?;
    if archive == receipt_output {
        return Err("combined evidence archive and receipt outputs must be distinct".to_owned());
    }
    validate_combined_output_path(&root, archive, "combined evidence archive")?;
    validate_combined_output_path(&root, receipt_output, "combined evidence receipt")?;
    if archive.exists() || receipt_output.exists() {
        return Err("combined evidence refuses stale archive or receipt output".to_owned());
    }
    let receipt_name = "public-evidence-scan.tsv";
    let (receipt, checksums) = render_captured_evidence_metadata(&scan, &files, receipt_name)?;
    let population = captured_archive_population(&files, receipt_name, &receipt, &checksums);
    hook(&root)?;
    let archive_bytes = deterministic_tar_gz(&population)?;
    verify_captured_archive(&archive_bytes, &population)?;
    reverify_captured_sources(&root, &files)?;
    publish_read_only_file(archive, &archive_bytes, "captured public evidence archive")?;
    if let Err(error) = publish_read_only_file(
        receipt_output,
        &receipt,
        "captured public evidence scan receipt",
    ) {
        let archive_cleanup = fs::remove_file(archive);
        return match archive_cleanup {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; failed to roll back captured archive: {cleanup}"
            )),
        };
    }
    Ok(())
}

fn scan_and_package_public_evidence(
    root: &Path,
    archive: &Path,
    receipt_output: &Path,
) -> Result<(), String> {
    scan_and_package_public_evidence_with_hook(root, archive, receipt_output, |_root| Ok(()))
}

fn scan_single_public_file(
    source: &Path,
    published_name: &str,
) -> Result<(PublicEvidenceScan, BTreeMap<String, StableScannedFile>), String> {
    let mut components = Path::new(published_name).components();
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || published_name.contains(['\n', '\r', '\t', '\\'])
        || matches!(published_name, "SHA256SUMS" | "public-evidence-scan.tsv")
    {
        return Err("combined staging name must be one safe top-level filename".to_owned());
    }
    if line_has_known_credential_marker(published_name) {
        return Err("combined staging filename matches a credential marker".to_owned());
    }
    let captured = read_stable_file_bytes(source)?;
    let marker_lines = String::from_utf8_lossy(&captured.raw_bytes)
        .lines()
        .filter(|line| line_has_known_credential_marker(line))
        .count();
    if marker_lines != 0 {
        return Err(format!(
            "legacy diagnostic contains {marker_lines} lines with known credential markers"
        ));
    }
    let mut files = BTreeMap::new();
    files.insert(published_name.to_owned(), captured);
    Ok((
        PublicEvidenceScan {
            decoded_gzip_files: 0,
            decoded_bytes: files[published_name].raw_bytes.len() as u64,
            marker_lines: 0,
        },
        files,
    ))
}

fn remove_exclusive_staging(staging: &Path) -> Result<(), String> {
    if !staging.exists() {
        return Ok(());
    }
    let mut permissions = fs::metadata(staging)
        .map_err(|error| format!("cannot inspect failed staging directory: {error}"))?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(staging, permissions)
        .map_err(|error| format!("cannot unlock failed staging directory: {error}"))?;
    fs::remove_dir_all(staging)
        .map_err(|error| format!("cannot remove failed staging directory: {error}"))
}

fn scan_and_stage_public_evidence_with_hook<F>(
    source: &Path,
    staging: &Path,
    published_name: &str,
    hook: F,
) -> Result<(), String>
where
    F: FnOnce(&Path) -> Result<(), String>,
{
    if !source.is_absolute() || !staging.is_absolute() {
        return Err("legacy source and staging paths must be absolute".to_owned());
    }
    let source = require_nonempty_regular_file(source, "legacy diagnostic source")?;
    let source = fs::canonicalize(&source)
        .map_err(|error| format!("cannot canonicalize legacy diagnostic source: {error}"))?;
    let staging_parent = staging
        .parent()
        .ok_or_else(|| "legacy staging path has no parent".to_owned())?;
    let canonical_staging_parent = fs::canonicalize(staging_parent)
        .map_err(|error| format!("cannot canonicalize legacy staging parent: {error}"))?;
    if canonical_staging_parent != staging_parent || source.starts_with(staging) {
        return Err(
            "legacy staging parent must be canonical and outside the source path".to_owned(),
        );
    }
    let (_scan, files) = scan_single_public_file(&source, published_name)?;
    let captured_identity = &files[published_name].identity;
    let checksums = format!("{}  {published_name}\n", captured_identity.sha256).into_bytes();
    let mut population = BTreeMap::from([(
        published_name.to_owned(),
        files[published_name].raw_bytes.clone(),
    )]);
    population.insert("SHA256SUMS".to_owned(), checksums);
    hook(&source)?;
    create_exclusive_directory(staging, "combined legacy staging directory")?;
    let materialize = (|| -> Result<(), String> {
        for (relative, bytes) in &population {
            write_new_file(
                &staging.join(relative),
                bytes,
                "captured legacy staging member",
            )?;
        }
        let mut staged = BTreeMap::new();
        stable_public_evidence_population(staging, staging, &mut staged)?;
        if staged.len() != population.len()
            || staged.iter().any(|(relative, identity)| {
                population
                    .get(relative)
                    .is_none_or(|bytes| sha256_bytes(bytes).ok().as_ref() != Some(&identity.sha256))
            })
            || staged
                .get(published_name)
                .is_none_or(|identity| identity.sha256 != captured_identity.sha256)
        {
            return Err(
                "materialized legacy staging differs from captured bytes/digest".to_owned(),
            );
        }
        let archived_checksums = parse_checksum_bytes(&population["SHA256SUMS"])?;
        let observed = population
            .iter()
            .filter(|(relative, _)| relative.as_str() != "SHA256SUMS")
            .map(|(relative, bytes)| Ok((relative.clone(), sha256_bytes(bytes)?)))
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        if observed != archived_checksums {
            return Err("legacy staging SHA256SUMS does not bind captured bytes".to_owned());
        }
        let final_source = read_stable_file_bytes(&source)?;
        if final_source.identity != *captured_identity {
            return Err("legacy diagnostic source changed after stable capture".to_owned());
        }
        for relative in population.keys() {
            let path = staging.join(relative);
            let mut permissions = fs::metadata(&path)
                .map_err(|error| format!("cannot inspect staged member: {error}"))?
                .permissions();
            permissions.set_mode(0o444);
            fs::set_permissions(path, permissions)
                .map_err(|error| format!("cannot make staged member read-only: {error}"))?;
        }
        let mut permissions = fs::metadata(staging)
            .map_err(|error| format!("cannot inspect staging directory: {error}"))?
            .permissions();
        permissions.set_mode(0o555);
        fs::set_permissions(staging, permissions)
            .map_err(|error| format!("cannot make staging directory read-only: {error}"))?;
        Ok(())
    })();
    if let Err(error) = materialize {
        return match remove_exclusive_staging(staging) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!("{error}; staging cleanup also failed: {cleanup}")),
        };
    }
    Ok(())
}

fn scan_and_stage_public_evidence(
    source: &Path,
    staging: &Path,
    published_name: &str,
) -> Result<(), String> {
    scan_and_stage_public_evidence_with_hook(source, staging, published_name, |_source| Ok(()))
}

#[derive(Debug, PartialEq, Eq)]
struct StableFileIdentity {
    sha256: String,
    device: u64,
    inode: u64,
    mode: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

fn file_identity(metadata: &fs::Metadata, sha256: String) -> StableFileIdentity {
    StableFileIdentity {
        sha256,
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        size: metadata.size(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn read_stable_file_bytes(path: &Path) -> Result<StableScannedFile, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect stable evidence {}: {error}", path.display()))?;
    if !before.is_file() || before.file_type().is_symlink() || before.nlink() != 1 {
        return Err(format!(
            "stable evidence {} must be a single-link regular file",
            path.display()
        ));
    }
    let mut raw_bytes = Vec::new();
    fs::File::open(path)
        .and_then(|mut file| file.read_to_end(&mut raw_bytes))
        .map_err(|error| format!("cannot read stable evidence {}: {error}", path.display()))?;
    let digest = sha256_bytes(&raw_bytes)?;
    let after = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot re-inspect stable evidence {}: {error}",
            path.display()
        )
    })?;
    if file_identity(&before, digest.clone()) != file_identity(&after, digest.clone()) {
        return Err(format!(
            "stable evidence {} changed while read and hashed",
            path.display()
        ));
    }
    Ok(StableScannedFile {
        identity: file_identity(&after, digest),
        raw_bytes,
    })
}

fn stable_file_identity(path: &Path) -> Result<StableFileIdentity, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot attest public evidence {}: {error}", path.display()))?;
    if !before.is_file() || before.file_type().is_symlink() || before.nlink() != 1 {
        return Err(format!(
            "public evidence {} must be a single-link regular file",
            path.display()
        ));
    }
    let sha256 = sha256(path)?;
    let after = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot re-attest public evidence {}: {error}",
            path.display()
        )
    })?;
    let before = file_identity(&before, sha256.clone());
    let after = file_identity(&after, sha256);
    if before != after {
        return Err(format!(
            "public evidence {} changed while it was hashed",
            path.display()
        ));
    }
    Ok(after)
}

fn run_scan_and_package_public_evidence(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 6
        || arguments[0] != "--root"
        || arguments[2] != "--archive"
        || arguments[4] != "--receipt-output"
    {
        return Err("usage: build-buck-release.rs --scan-and-package-public-evidence --root ABSOLUTE-PATH --archive ABSOLUTE-PATH --receipt-output ABSOLUTE-PATH".to_owned());
    }
    scan_and_package_public_evidence(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        Path::new(&arguments[5]),
    )
}

fn run_scan_and_stage_public_evidence(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 6
        || arguments[0] != "--source"
        || arguments[2] != "--staging"
        || arguments[4] != "--name"
    {
        return Err("usage: build-buck-release.rs --scan-and-stage-public-evidence --source ABSOLUTE-PATH --staging ABSOLUTE-PATH --name FILENAME".to_owned());
    }
    scan_and_stage_public_evidence(
        Path::new(&arguments[1]),
        Path::new(&arguments[3]),
        &arguments[5],
    )
}

fn release_evidence_verdict(
    summary_exit: i32,
    reconciliation_exit: i32,
    candidate_exit: i32,
) -> Result<(), String> {
    if summary_exit == 0 && reconciliation_exit == 0 && candidate_exit == 0 {
        Ok(())
    } else {
        Err(format!(
            "Buck shell/event/output evidence is non-green: log-summary exit={summary_exit}, typed-reconciliation exit={reconciliation_exit}, typed-candidate exit={candidate_exit}"
        ))
    }
}

fn run_release_evidence_verdict(arguments: &[String]) -> Result<(), String> {
    if arguments.len() != 6
        || arguments[0] != "--summary-exit"
        || arguments[2] != "--reconciliation-exit"
        || arguments[4] != "--candidate-exit"
    {
        return Err(
            "usage: build-buck-release.rs --release-evidence-verdict --summary-exit INTEGER --reconciliation-exit INTEGER --candidate-exit INTEGER"
                .to_owned(),
        );
    }
    let summary_exit = arguments[1]
        .parse::<i32>()
        .map_err(|_| "--summary-exit must be an integer".to_owned())?;
    let reconciliation_exit = arguments[3]
        .parse::<i32>()
        .map_err(|_| "--reconciliation-exit must be an integer".to_owned())?;
    let candidate_exit = arguments[5]
        .parse::<i32>()
        .map_err(|_| "--candidate-exit must be an integer".to_owned())?;
    release_evidence_verdict(summary_exit, reconciliation_exit, candidate_exit)
}

fn verify_install_bundle(
    bundle: &Path,
    reverie_sha: &str,
) -> Result<Vec<(String, String)>, String> {
    let bundle = require_absolute_directory(bundle, "Cargo install bundle")?;
    let mut hashes = Vec::new();
    for (relative, executable) in REQUIRED_RESOURCES {
        let path = bundle.join(relative);
        let metadata = fs::metadata(&path).map_err(|error| {
            format!(
                "Cargo install bundle is incomplete: {} is unavailable ({error}); rebuild hermit-install",
                path.display()
            )
        })?;
        if !metadata.is_file()
            || metadata.len() == 0
            || (executable && metadata.permissions().mode() & 0o111 == 0)
        {
            return Err(format!(
                "Cargo install bundle is incomplete: {} has the wrong type/size/mode; rebuild hermit-install",
                path.display()
            ));
        }
        hashes.push((relative.to_owned(), sha256(&path)?));
    }
    let sabre_revision = fs::read_to_string(bundle.join("rsrcs/sabre.revision"))
        .map_err(|error| format!("Cargo install bundle lacks readable sabre.revision: {error}"))?;
    if sabre_revision.trim() != reverie_sha {
        return Err(format!(
            "Cargo install bundle Reverie mismatch: sabre.revision={} expected={reverie_sha}; rebuild it at this pin",
            sabre_revision.trim()
        ));
    }
    hashes.sort();
    Ok(hashes)
}

fn elf_identity(path: &Path) -> Result<(u8, u8, u16, u16), String> {
    let mut header = [0_u8; 64];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| format!("cannot read ELF header {}: {error}", path.display()))?;
    if &header[..4] != b"\x7fELF" || header[5] != 1 {
        return Err(format!(
            "{} is not a little-endian ELF binary",
            path.display()
        ));
    }
    let object_type = u16::from_le_bytes([header[16], header[17]]);
    let machine = u16::from_le_bytes([header[18], header[19]]);
    Ok((header[4], header[5], object_type, machine))
}

fn needed_libraries(path: &Path) -> Result<BTreeSet<String>, String> {
    let text = output_text(
        Command::new("readelf").args([OsStr::new("-d"), path.as_os_str()]),
        &format!("readelf -d {}", path.display()),
    )?;
    Ok(text
        .lines()
        .filter(|line| line.contains("(NEEDED)"))
        .filter_map(|line| {
            line.split_once('[')?
                .1
                .split_once(']')
                .map(|pair| pair.0.to_owned())
        })
        .collect())
}

fn require_equal(label: &str, cargo: &str, buck: &str) -> Result<(), String> {
    if cargo == buck {
        Ok(())
    } else {
        Err(format!(
            "Cargo/Buck {label} mismatch; rebuild both at the same SHA/configuration and inspect retained evidence"
        ))
    }
}

fn require_nonempty_regular_file(path: &Path, description: &str) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(format!(
            "{description} {} must be a nonempty regular file, not a symlink",
            path.display()
        ));
    }
    Ok(path.to_owned())
}

fn parse_resource_manifest(
    path: &Path,
    description: &str,
) -> Result<BTreeMap<String, String>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("{description} {} is unreadable: {error}", path.display()))?;
    let mut inventory = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let (hash, relative) = line.split_once("  ").ok_or_else(|| {
            format!(
                "{description} {} has malformed row {}",
                path.display(),
                index + 1
            )
        })?;
        let relative_path = Path::new(relative);
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || relative_path.is_absolute()
            || relative_path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "{description} {} has unsafe or invalid row {}",
                path.display(),
                index + 1
            ));
        }
        if inventory
            .insert(relative.to_owned(), hash.to_owned())
            .is_some()
        {
            return Err(format!(
                "{description} {} repeats resource {relative:?}",
                path.display()
            ));
        }
    }
    if inventory.is_empty() {
        return Err(format!(
            "{description} {} contains no resources",
            path.display()
        ));
    }
    Ok(inventory)
}

fn inspect_published_bundle(root: &Path, name: &str) -> Result<PublishedBundle, String> {
    let binary = require_absolute_file(&root.join("hermit"), &format!("verified {name} Hermit"))?;
    let install = require_absolute_directory(
        &root.join("install"),
        &format!("verified {name} install bundle"),
    )?;
    let hermit_manifest = require_nonempty_regular_file(
        &root.join("hermit.sha256"),
        &format!("verified {name} Hermit manifest"),
    )?;
    let resources_manifest = require_nonempty_regular_file(
        &root.join("resources.sha256"),
        &format!("verified {name} resource manifest"),
    )?;
    let binary_sha256 = sha256(&binary)?;
    let claimed_binary_sha256 = fs::read_to_string(&hermit_manifest)
        .map_err(|error| {
            format!(
                "verified {name} Hermit manifest {} is unreadable: {error}",
                hermit_manifest.display()
            )
        })?
        .trim()
        .to_owned();
    if claimed_binary_sha256 != binary_sha256 {
        return Err(format!(
            "verified {name} Hermit manifest does not bind the candidate binary"
        ));
    }
    let resource_inventory = parse_resource_manifest(
        &resources_manifest,
        &format!("verified {name} resource manifest"),
    )?;
    Ok(PublishedBundle {
        root: root.to_owned(),
        binary,
        install,
        hermit_manifest_sha256: sha256(&hermit_manifest)?,
        resources_manifest_sha256: sha256(&resources_manifest)?,
        hermit_manifest,
        resources_manifest,
        binary_sha256,
        resource_inventory,
    })
}

fn reverify_published_bundle(
    repository_root: &Path,
    expected: &PublishedBundle,
    name: &str,
) -> Result<(), String> {
    let resolved = PathBuf::from(output_text(
        Command::new(repository_root.join("ci/verify-hermit-e2e-artifact.sh")).arg(&expected.root),
        &format!("reverify isolated {name} shadow bundle"),
    )?);
    let resolved = resolved
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize reverified {name} bundle: {error}"))?;
    if resolved != expected.root {
        return Err(format!(
            "reverified {name} bundle resolved to {}, expected {}",
            resolved.display(),
            expected.root.display()
        ));
    }
    let actual = inspect_published_bundle(&resolved, name)?;
    if &actual != expected {
        return Err(format!(
            "verified {name} bundle changed after its tested snapshot was created"
        ));
    }
    Ok(())
}

fn require_equal_resource_manifests(
    cargo: &PublishedBundle,
    buck: &PublishedBundle,
) -> Result<(), String> {
    let cargo_bytes = fs::read(&cargo.resources_manifest).map_err(|error| {
        format!(
            "Cargo copied resource manifest {} is unreadable: {error}",
            cargo.resources_manifest.display()
        )
    })?;
    let buck_bytes = fs::read(&buck.resources_manifest).map_err(|error| {
        format!(
            "Buck copied resource manifest {} is unreadable: {error}",
            buck.resources_manifest.display()
        )
    })?;
    if cargo_bytes != buck_bytes
        || cargo.resources_manifest_sha256 != buck.resources_manifest_sha256
        || cargo.resource_inventory != buck.resource_inventory
    {
        return Err(
            "Cargo/Buck copied install manifests differ in inventory or resource hashes; refusing mixed mutable caller inputs"
                .to_owned(),
        );
    }
    Ok(())
}

fn publish_verified_bundle(
    root: &Path,
    evidence_dir: &Path,
    name: &str,
    binary: &Path,
    install_bundle: &Path,
) -> Result<PublishedBundle, String> {
    let artifact_root = evidence_dir.join(format!("{name}-artifacts"));
    let pointer = evidence_dir.join(format!("{name}-artifact.path"));
    checked_output(
        Command::new(root.join("ci/publish-hermit-e2e-artifact.sh")).args([
            binary.as_os_str(),
            artifact_root.as_os_str(),
            pointer.as_os_str(),
            install_bundle.as_os_str(),
        ]),
        &format!("publish isolated {name} shadow bundle"),
    )?;
    let resolved = PathBuf::from(output_text(
        Command::new(root.join("ci/verify-hermit-e2e-artifact.sh")).arg(&pointer),
        &format!("verify isolated {name} shadow bundle"),
    )?);
    let canonical_evidence = evidence_dir
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize {}: {error}", evidence_dir.display()))?;
    let canonical_resolved = resolved
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize verified {name} bundle: {error}"))?;
    if !canonical_resolved.starts_with(&canonical_evidence) {
        return Err(format!(
            "verified {name} bundle escaped ignored evidence root: {}",
            canonical_resolved.display()
        ));
    }
    inspect_published_bundle(&canonical_resolved, name)
}

#[derive(Debug)]
struct CandidateOutput {
    stdout: String,
}

struct CandidateInvocation<'a> {
    safehermit: &'a Path,
    binary: &'a Path,
    install_bundle: &'a Path,
    evidence_dir: &'a Path,
    identity: &'a str,
    data_dir: &'a Path,
    arguments: &'a [&'a str],
    deadline_seconds: u64,
    description: &'a str,
}

fn write_new_file(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            format!(
                "refusing to replace existing {description} {}: {error}",
                path.display()
            )
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("failed to retain {description} {}: {error}", path.display()))
}

fn validate_and_surface_safehermit_report(path: &Path, description: &str) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|error| {
        format!(
            "safehermit report for {description} is unreadable at {}: {error}",
            path.display()
        )
    })?;
    let bound_lines = text
        .lines()
        .filter(|line| line.starts_with("safehermit: bound."))
        .collect::<Vec<_>>();
    if bound_lines.is_empty()
        || bound_lines.iter().any(|line| line.contains("NOT_APPLIED"))
        || !bound_lines
            .iter()
            .any(|line| line.starts_with("safehermit: bound.wall=APPLIED:"))
        || !bound_lines
            .iter()
            .any(|line| line.starts_with("safehermit: bound.cgroup=APPLIED:"))
    {
        return Err(format!(
            "safehermit report for {description} lacks applied wall/cgroup bounds: {}",
            path.display()
        ));
    }
    for line in bound_lines {
        eprintln!("{description}: {line}");
    }
    Ok(())
}

fn run_safehermit(request: CandidateInvocation<'_>) -> Result<CandidateOutput, String> {
    let CandidateInvocation {
        safehermit,
        binary,
        install_bundle,
        evidence_dir,
        identity,
        data_dir,
        arguments,
        deadline_seconds,
        description,
    } = request;
    create_exclusive_directory(data_dir, &format!("{description} data directory"))?;
    let invocations = evidence_dir.join("candidate-invocations");
    fs::create_dir_all(&invocations)
        .map_err(|error| format!("failed to create {}: {error}", invocations.display()))?;
    let invocation = invocations.join(format!("{identity}-{}", unique_identity()?));
    create_exclusive_directory(&invocation, &format!("{description} invocation"))?;
    let report = invocation.join("safehermit.report");
    let output = Command::new(safehermit)
        .arg(format!("--sh-deadline={deadline_seconds}"))
        .arg("--sh-report")
        .arg(&report)
        .arg(binary)
        .args(arguments)
        .env("HERMIT_INSTALL_DIR", install_bundle)
        .env("HERMIT_DATA_DIR", data_dir)
        .output()
        .map_err(|error| format!("failed to start {description}: {error}"))?;
    write_new_file(
        &invocation.join("stdout"),
        &output.stdout,
        "candidate stdout",
    )?;
    write_new_file(
        &invocation.join("stderr"),
        &output.stderr,
        "candidate stderr",
    )?;
    validate_and_surface_safehermit_report(&report, description)?;
    if !output.status.success() {
        return Err(format!(
            "{description} failed with {}: {}; retained invocation={}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
            invocation.display()
        ));
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|error| format!("{description} emitted non-UTF-8 stdout: {error}"))?
        .trim()
        .to_owned();
    Ok(CandidateOutput { stdout })
}

fn require_object_keys(
    value: &Value,
    location: &str,
    required: &[&str],
    allowed: &[&str],
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("strict verify report {location} must be an object"))?;
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "strict verify report {location} contains unknown field {key:?}"
            ));
        }
    }
    for key in required {
        if !object.contains_key(*key) {
            return Err(format!(
                "strict verify report {location} lacks required field {key:?}"
            ));
        }
    }
    Ok(())
}

fn verify_closed_report_shape(value: &Value) -> Result<(), String> {
    const TOP_LEVEL: &[&str] = &[
        "verified",
        "bitwise_parity",
        "verdict",
        "no_result_reason",
        "infrastructure_error",
        "comparison",
        "compared_log_messages",
        "compared_outputs",
        "dbt_counted_branches",
        "runtime",
        "guest_exit_code",
        "guest_signal",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ];
    const CURRENT_REQUIRED: &[&str] = &[
        "verified",
        "bitwise_parity",
        "verdict",
        "no_result_reason",
        "infrastructure_error",
        "comparison",
        "compared_log_messages",
        "compared_outputs",
        "guest_exit_code",
        "guest_signal",
        "first_divergent_scheduler_turn",
        "first_divergent_virtual_nanoseconds",
        "first_divergent_record",
        "first_divergent_syscall",
        "first_divergent_left_message",
        "first_divergent_right_message",
    ];
    require_object_keys(value, "root", CURRENT_REQUIRED, TOP_LEVEL)?;
    let comparison = &value["comparison"];
    const COMPARISON: &[&str] = &[
        "strictness",
        "display_name",
        "compare_logs",
        "compare_io_buffers",
        "log_scope",
        "record_envelope",
        "virtualize_time",
        "strip_lines",
        "canonicalize_addresses",
        "full_trace",
        "exact_remainder",
        "stripped_prefixes",
        "canonicalizations",
        "ignore_lines",
        "skip_commit",
        "skip_detlog",
    ];
    require_object_keys(comparison, "comparison", COMPARISON, COMPARISON)?;
    require_object_keys(
        &value["compared_log_messages"],
        "compared_log_messages",
        &["left", "right"],
        &["left", "right"],
    )?;
    require_object_keys(
        &value["compared_outputs"],
        "compared_outputs",
        &["left", "right"],
        &["left", "right"],
    )?;
    for side in ["left", "right"] {
        require_object_keys(
            &value["compared_outputs"][side],
            &format!("compared_outputs.{side}"),
            &[
                "exit_code",
                "signal",
                "stdout_sha256",
                "stdout_bytes",
                "stderr_sha256",
                "stderr_bytes",
            ],
            &[
                "exit_code",
                "signal",
                "stdout_sha256",
                "stdout_bytes",
                "stderr_sha256",
                "stderr_bytes",
            ],
        )?;
    }
    if let Some(runtime) = value.get("runtime").filter(|value| !value.is_null()) {
        require_object_keys(runtime, "runtime", &[], &["run1", "run2"])?;
        for run in ["run1", "run2"] {
            if let Some(stats) = runtime.get(run).filter(|value| !value.is_null()) {
                require_object_keys(
                    stats,
                    &format!("runtime.{run}"),
                    &["scheduler_turns", "virtual_nanoseconds"],
                    &["scheduler_turns", "virtual_nanoseconds", "syscalls"],
                )?;
            }
        }
    }
    if let Some(branches) = value
        .get("dbt_counted_branches")
        .filter(|value| !value.is_null())
    {
        require_object_keys(
            branches,
            "dbt_counted_branches",
            &["left", "right"],
            &["left", "right"],
        )?;
    }
    Ok(())
}

fn verify_report(
    path: &Path,
    expected_virtualize_time: bool,
) -> Result<VerificationReport, String> {
    let bytes = fs::read(path).map_err(|error| {
        format!(
            "strict verify report {} is unreadable: {error}",
            path.display()
        )
    })?;
    let report = VerificationReport::from_current_json_slice(&bytes)
        .map_err(|error| format!("strict verify report {}: {error}", path.display()))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("strict verify report {}: {error}", path.display()))?;
    verify_closed_report_shape(&value)?;
    report.require_canonical_match().map_err(|error| {
        format!(
            "strict verify report {} is not a canonical match: {error}",
            path.display()
        )
    })?;
    let comparison = report
        .comparison
        .as_ref()
        .ok_or_else(|| "canonical validator admitted a report without comparison".to_owned())?;
    if report.verdict != Verdict::Matched
        || report.guest_exit_code != Some(0)
        || report.guest_signal.is_some()
        || report.no_result_reason.is_some()
        || report.infrastructure_error.is_some()
        || report.first_divergent_scheduler_turn.is_some()
        || report.first_divergent_virtual_nanoseconds.is_some()
        || report.first_divergent_record.is_some()
        || report.first_divergent_syscall.is_some()
        || report.first_divergent_left_message.is_some()
        || report.first_divergent_right_message.is_some()
        || comparison.strictness != LogCompareStrictness::Canonical
        || comparison.display_name.as_deref() != Some("BitwiseInfoV1")
        || !comparison.compare_logs
        || comparison.compare_io_buffers != Some(true)
        || comparison.log_scope != Some(ComparedLogScope::Info)
        || comparison.record_envelope != RecordEnvelopeReport::AllRecordsV1
        || comparison.virtualize_time != Some(expected_virtualize_time)
        || comparison.strip_lines != Some(false)
        || comparison.canonicalize_addresses != Some(true)
        || comparison.full_trace != Some(true)
        || comparison.exact_remainder != Some(true)
        || comparison.stripped_prefixes.as_deref()
            != Some(&["real-wall-clock-prefix/v1".to_owned()][..])
        || comparison.canonicalizations.as_deref()
            != Some(&["host-address-to-first-appearance-ordinal/v1".to_owned()][..])
        || comparison.ignore_lines != Some(false)
        || comparison.skip_commit != Some(false)
        || comparison.skip_detlog != Some(false)
    {
        return Err(format!(
            "strict verify report {} does not carry the exact BitwiseInfoV1 policy, expected virtualize_time={expected_virtualize_time}",
            path.display()
        ));
    }
    let counts = report
        .compared_log_messages
        .as_ref()
        .ok_or_else(|| "canonical report omitted compared INFO counts".to_owned())?;
    if counts.left == 0 || counts.left != counts.right {
        return Err(format!(
            "strict verify report {} compared invalid INFO counts {}/{}",
            path.display(),
            counts.left,
            counts.right
        ));
    }
    Ok(report)
}

fn behavioral_parity(
    safehermit: &Path,
    cargo_binary: &Path,
    buck_binary: &Path,
    cargo_install: &Path,
    buck_install: &Path,
    evidence_dir: &Path,
) -> Result<(), String> {
    {
        let backend = "ptrace";
        let cargo_report = evidence_dir.join(format!("cargo-{backend}-verify.json"));
        let buck_report = evidence_dir.join(format!("buck-{backend}-verify.json"));
        let cargo_report_text = cargo_report.to_string_lossy();
        let buck_report_text = buck_report.to_string_lossy();
        let cargo_identity = format!("cargo-{backend}-verify");
        let cargo_data = evidence_dir.join(format!("cargo-{backend}-verify-data"));
        let cargo_arguments = [
            "--log=info",
            "run",
            "--backend",
            backend,
            "--base-env=minimal",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            &cargo_report_text,
            "--",
            "/bin/true",
        ];
        let cargo_description = format!("Cargo {backend} strict verify through safehermit");
        let cargo = run_safehermit(CandidateInvocation {
            safehermit,
            binary: cargo_binary,
            install_bundle: cargo_install,
            evidence_dir,
            identity: &cargo_identity,
            data_dir: &cargo_data,
            arguments: &cargo_arguments,
            deadline_seconds: VERIFY_DEADLINE_SECONDS,
            description: &cargo_description,
        })?;
        let buck_identity = format!("buck-{backend}-verify");
        let buck_data = evidence_dir.join(format!("buck-{backend}-verify-data"));
        let buck_arguments = [
            "--log=info",
            "run",
            "--backend",
            backend,
            "--base-env=minimal",
            "--strict",
            "--verify",
            "--verify-strict",
            "--verify-json",
            &buck_report_text,
            "--",
            "/bin/true",
        ];
        let buck_description = format!("Buck {backend} strict verify through safehermit");
        let buck = run_safehermit(CandidateInvocation {
            safehermit,
            binary: buck_binary,
            install_bundle: buck_install,
            evidence_dir,
            identity: &buck_identity,
            data_dir: &buck_data,
            arguments: &buck_arguments,
            deadline_seconds: VERIFY_DEADLINE_SECONDS,
            description: &buck_description,
        })?;
        require_equal(
            &format!("{backend} guest stdout"),
            &cargo.stdout,
            &buck.stdout,
        )?;
        let cargo_typed = verify_report(&cargo_report, true)?;
        let buck_typed = verify_report(&buck_report, true)?;
        if cargo_typed != buck_typed {
            return Err(format!(
                "Cargo/Buck {backend} typed strict report mismatch; inspect retained reports"
            ));
        }
    }

    let cargo_record_report = evidence_dir.join("cargo-ptrace-record-verify.json");
    let buck_record_report = evidence_dir.join("buck-ptrace-record-verify.json");
    let cargo_record_data = evidence_dir.join("cargo-record-data");
    let buck_record_data = evidence_dir.join("buck-record-data");
    let cargo_record_report_text = cargo_record_report.to_string_lossy();
    let buck_record_report_text = buck_record_report.to_string_lossy();
    let cargo_record_data_text = cargo_record_data.to_string_lossy();
    let buck_record_data_text = buck_record_data.to_string_lossy();
    let record_arguments = [
        "--log=info",
        "--backend",
        "ptrace",
        "record",
        "start",
        "--strict",
        "--verify",
        "--verify-strict",
        "--record-timeout=30",
        "--verify-json",
        &cargo_record_report_text,
        "--data-dir",
        &cargo_record_data_text,
        "--",
        "/bin/true",
    ];
    let cargo = run_safehermit(CandidateInvocation {
        safehermit,
        binary: cargo_binary,
        install_bundle: cargo_install,
        evidence_dir,
        identity: "cargo-ptrace-record",
        data_dir: &cargo_record_data,
        arguments: &record_arguments,
        deadline_seconds: VERIFY_DEADLINE_SECONDS,
        description: "Cargo ptrace record/replay through safehermit",
    })?;
    let buck_record_arguments = [
        "--log=info",
        "--backend",
        "ptrace",
        "record",
        "start",
        "--strict",
        "--verify",
        "--verify-strict",
        "--record-timeout=30",
        "--verify-json",
        &buck_record_report_text,
        "--data-dir",
        &buck_record_data_text,
        "--",
        "/bin/true",
    ];
    let buck = run_safehermit(CandidateInvocation {
        safehermit,
        binary: buck_binary,
        install_bundle: buck_install,
        evidence_dir,
        identity: "buck-ptrace-record",
        data_dir: &buck_record_data,
        arguments: &buck_record_arguments,
        deadline_seconds: VERIFY_DEADLINE_SECONDS,
        description: "Buck ptrace record/replay through safehermit",
    })?;
    require_equal("ptrace record/replay stdout", &cargo.stdout, &buck.stdout)?;
    let cargo_typed = verify_report(&cargo_record_report, false)?;
    let buck_typed = verify_report(&buck_record_report, false)?;
    if cargo_typed != buck_typed {
        return Err(
            "Cargo/Buck ptrace record/replay typed strict report mismatch; inspect retained reports"
                .to_owned(),
        );
    }
    Ok(())
}

fn normalized_matrix(path: &Path) -> Result<String, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("DBT matrix {} is unreadable: {error}", path.display()))?;
    let mut lines = text.lines();
    let header = lines
        .next()
        .ok_or_else(|| format!("DBT matrix {} is empty", path.display()))?;
    let columns = header.split('\t').collect::<Vec<_>>();
    let seconds = columns
        .iter()
        .position(|column| *column == "seconds")
        .ok_or_else(|| format!("DBT matrix {} has no seconds column", path.display()))?;
    let mut normalized = Vec::new();
    normalized.push(
        columns
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != seconds)
            .map(|(_, value)| *value)
            .collect::<Vec<_>>()
            .join("\t"),
    );
    for line in lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        if fields.len() != columns.len() {
            return Err(format!("DBT matrix {} has a malformed row", path.display()));
        }
        normalized.push(
            fields
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != seconds)
                .map(|(_, value)| *value)
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    if normalized.len() <= 1 {
        return Err(format!(
            "DBT matrix {} contains no result rows",
            path.display()
        ));
    }
    Ok(format!("{}\n", normalized.join("\n")))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixProxyInvocation {
    invocation_schema: String,
    case_identity: String,
    role: String,
    normalized_argv: Vec<String>,
}

fn normalize_matrix_argument(argument: &str) -> String {
    let marker = "/hermit-backend-parity-";
    let Some(start) = argument.find(marker) else {
        return argument.to_owned();
    };
    let unique_start = start + 1;
    let suffix_start = argument[unique_start..]
        .find('/')
        .map(|offset| unique_start + offset)
        .unwrap_or(argument.len());
    format!(
        "{}$MATRIX_TMP{}",
        &argument[..unique_start],
        &argument[suffix_start..]
    )
}

fn matrix_case_from_guest(guest: &[String]) -> Result<&'static str, String> {
    let executable = guest
        .first()
        .ok_or_else(|| "matrix proxy invocation has an empty guest command".to_owned())?;
    let basename = Path::new(executable)
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| "matrix proxy guest executable has no UTF-8 basename".to_owned())?;
    let trailing = guest.iter().skip(1).map(String::as_str).collect::<Vec<_>>();
    let case = match (basename, trailing.as_slice()) {
        ("echo", ["hello world"]) => "hello_stdout",
        ("printf", ["%s|%s\n", "alpha", "two words"]) => "argument_forwarding",
        ("true", []) => "exit_zero",
        ("sh", ["-c", "exit 23"]) => "exit_status",
        ("cat", [_]) => "file_read",
        ("syscall_file_io", []) => "file_mutation",
        ("syscall_file_metadata", []) => "file_metadata",
        ("io_uring_fallback", []) => "io_uring_fallback",
        ("listmount_enosys", []) => "listmount_unavailable",
        ("process_vm_readv_refusal", []) => "process_vm_readv_refusal",
        ("process_vm_writev_refusal", []) => "process_vm_writev_refusal",
        ("mmap_exec", []) => "executable_mmap",
        ("madvise_determinism", []) => "memory_advice",
        ("mmap_determinism", ["heap"]) => "heap_growth",
        ("mmap_determinism", ["multiple"]) => "anonymous_mmap_layout",
        ("mmap_determinism", ["shared"]) => "shared_anonymous_mmap",
        ("pthread_lifecycle", []) => "pthread_lifecycle",
        ("process_wait_lifecycle", ["--accounting-only"]) => "process_wait_accounting",
        ("process_wait_lifecycle", []) => "process_wait_lifecycle",
        ("cpuid_probe", []) => "cpuid_policy",
        ("clock_determinism", []) => "virtual_clock",
        ("random_sources", ["--root-only"]) => "random_sources",
        ("pid_probe", []) => "virtual_pid",
        ("scheduler_policy_queries", []) => "scheduler_policy_queries",
        ("signal_disposition", []) => "signal_disposition",
        ("sigaction_state", []) => "sigaction_state",
        ("sigprocmask_state", []) => "sigprocmask_state",
        ("sigaltstack_state", []) => "sigaltstack_state",
        _ => {
            return Err(format!(
                "matrix proxy cannot map guest command to an official case: {guest:?}"
            ));
        }
    };
    Ok(case)
}

fn classify_matrix_proxy_invocation(arguments: &[String]) -> Result<MatrixProxyInvocation, String> {
    let normalized_argv = arguments
        .iter()
        .map(|argument| normalize_matrix_argument(argument))
        .collect::<Vec<_>>();
    if arguments == ["host-capabilities", "--json"] {
        return Ok(MatrixProxyInvocation {
            invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
            case_identity: "probe/host-capabilities".to_owned(),
            role: "probe".to_owned(),
            normalized_argv,
        });
    }
    let separator = arguments
        .iter()
        .position(|argument| argument == "--")
        .ok_or_else(|| "matrix proxy invocation lacks a guest separator".to_owned())?;
    if arguments.first().map(String::as_str) != Some("run") {
        return Err("matrix proxy invocation is neither a probe nor `hermit run`".to_owned());
    }
    let guest = &arguments[separator + 1..];
    let is_dbt = arguments
        .windows(2)
        .any(|pair| pair == ["--backend", "dbt"]);
    let is_matrix_case = arguments
        .iter()
        .any(|argument| argument == "--max-timeslice=disabled");
    let (case_identity, role) = if is_dbt && !is_matrix_case && guest == ["/bin/true"] {
        ("probe/dbt-smoke".to_owned(), "probe".to_owned())
    } else if is_matrix_case {
        let case = matrix_case_from_guest(guest)?.to_owned();
        let role = if is_dbt {
            "dbt-run"
        } else {
            "ptrace-reference"
        };
        (case, role.to_owned())
    } else {
        return Err(format!(
            "matrix proxy invocation does not match the official smoke/case envelope: {arguments:?}"
        ));
    };
    Ok(MatrixProxyInvocation {
        invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
        case_identity,
        role,
        normalized_argv,
    })
}

fn matrix_candidate_proxy(arguments: impl Iterator<Item = String>) -> Result<ExitCode, String> {
    let required = |name: &str| {
        env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| format!("matrix candidate proxy lacks required {name}"))
    };
    let safehermit = require_absolute_file(
        &required("HERMIT_BUCK_PHASE1_SAFEHERMIT")?,
        "matrix safehermit launcher",
    )?;
    let binary = require_absolute_file(
        &required("HERMIT_BUCK_PHASE1_CANDIDATE")?,
        "matrix candidate binary",
    )?;
    let install = require_absolute_directory(
        &required("HERMIT_BUCK_PHASE1_INSTALL")?,
        "matrix candidate install bundle",
    )?;
    let evidence = require_absolute_directory(
        &required("HERMIT_BUCK_PHASE1_MATRIX_EVIDENCE")?,
        "matrix evidence directory",
    )?;
    let label = env::var("HERMIT_BUCK_PHASE1_CANDIDATE_LABEL")
        .map_err(|_| "matrix candidate proxy lacks candidate label".to_owned())?;
    if !matches!(label.as_str(), "cargo" | "buck") {
        return Err(format!("invalid matrix candidate label {label:?}"));
    }
    let arguments = arguments.collect::<Vec<_>>();
    let invocation_metadata = classify_matrix_proxy_invocation(&arguments)?;
    let identity = unique_identity()?;
    let invocation = evidence.join(format!("{label}-{identity}"));
    create_exclusive_directory(&invocation, "matrix candidate invocation")?;
    let report = invocation.join("safehermit.report");
    let data = invocation.join("data");
    create_exclusive_directory(&data, "matrix candidate data directory")?;
    write_new_file(
        &invocation.join("invocation.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&invocation_metadata)
                .map_err(|error| format!("failed to encode matrix invocation metadata: {error}"))?
        )
        .as_bytes(),
        "matrix candidate invocation metadata",
    )?;
    let output = Command::new(&safehermit)
        .arg(format!("--sh-deadline={MATRIX_CANDIDATE_DEADLINE_SECONDS}"))
        .arg("--sh-report")
        .arg(&report)
        .arg(&binary)
        .args(&arguments)
        .env("HERMIT_INSTALL_DIR", &install)
        .env("HERMIT_DATA_DIR", &data)
        .output()
        .map_err(|error| format!("failed to start matrix candidate through safehermit: {error}"))?;
    write_new_file(
        &invocation.join("stdout"),
        &output.stdout,
        "matrix candidate stdout evidence",
    )?;
    write_new_file(
        &invocation.join("stderr"),
        &output.stderr,
        "matrix candidate stderr evidence",
    )?;
    validate_and_surface_safehermit_report(&report, &format!("{label} DBT matrix candidate"))?;
    std::io::stdout()
        .write_all(&output.stdout)
        .map_err(|error| format!("failed to replay matrix candidate stdout: {error}"))?;
    std::io::stderr()
        .write_all(&output.stderr)
        .map_err(|error| format!("failed to replay matrix candidate stderr: {error}"))?;
    Ok(ExitCode::from(
        output
            .status
            .code()
            .and_then(|code| u8::try_from(code).ok())
            .unwrap_or(2),
    ))
}

fn shell_command(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| shell_words::quote(argument).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

#[allow(clippy::too_many_arguments)]
fn require_clean_matrix_supervision(
    run_ok: bool,
    run_timed_out: bool,
    run_cpu_timed_out: bool,
    cpu_accounting_failed: bool,
    skipped: usize,
    not_launched: usize,
    outcomes: usize,
    outcome_ok: bool,
    outcome_timed_out: bool,
    outcome_cpu_timed_out: bool,
    outcome_aborted: bool,
    outcome_returncode: Option<i64>,
) -> Result<(), String> {
    if run_ok
        && !run_timed_out
        && !run_cpu_timed_out
        && !cpu_accounting_failed
        && skipped == 0
        && not_launched == 0
        && outcomes == 1
        && outcome_ok
        && !outcome_timed_out
        && !outcome_cpu_timed_out
        && !outcome_aborted
        && outcome_returncode == Some(0)
    {
        Ok(())
    } else {
        Err("matrix process tree did not complete and clean up under its cgroup supervisor".into())
    }
}

struct MatrixInvocation<'a> {
    root: &'a Path,
    cgroups: BoxedCgroups,
    safehermit: &'a Path,
    binary: &'a Path,
    install: &'a Path,
    evidence_dir: &'a Path,
    candidate_label: &'a str,
    output: &'a Path,
    description: &'a str,
    envelope: &'a MatrixExecutionEnvelope,
}

#[derive(Clone)]
struct MatrixExecutionEnvelope {
    cmdtype: CmdType,
    hint: ResourceHint,
    networkonly: bool,
    engine_only: bool,
    timeout: i64,
    cpu_timeout: i64,
    jobs_flag: Option<String>,
    jobs_env: Option<String>,
}

fn require_official_dbt_matrix_envelope(step: &Step) -> Result<(), String> {
    let hint = &step.hint;
    if !hint.resources.is_empty()
        || hint.est_duration_s != 180.0
        || hint.rss_baseline_bytes != Some(512 * 1024 * 1024)
        || hint.rss_baseline_inner_jobs.is_some()
        || hint.hard_mem_max_bytes != Some(2_i64 * 1024 * 1024 * 1024)
        || hint.classification != StepClass::LatencyBound
        || hint.preferred_inner_jobs.is_some()
        || hint.measured_effective_cores.is_some()
        || hint.measured_cpu_utilization.is_some()
        || !matches!(step.cmdtype, CmdType::Unknown)
        || step.networkonly
        || step.engine_only
        || step.timeout != MATRIX_STEP_DEADLINE_SECONDS
        || step.cpu_timeout != MATRIX_CPU_TIMEOUT_SECONDS
        || step.jobs_flag.is_some()
        || step.jobs_env.is_some()
    {
        Err(
            "official test.dbt_parity resource envelope drifted from the reviewed 512-MiB baseline/2-GiB hard/latency/900-wall/7200-CPU/no-jobs contract"
                .to_owned(),
        )
    } else {
        Ok(())
    }
}

fn official_dbt_matrix_envelope(root: &Path) -> Result<MatrixExecutionEnvelope, String> {
    let generated = hermit_manifest_plan::validation_dag::generate(root)?;
    let mut matches = generated
        .steps
        .into_iter()
        .filter(|step| step.group == "test" && step.job == "dbt_parity");
    let step = matches
        .next()
        .ok_or_else(|| "generated validation DAG lacks test.dbt_parity".to_owned())?;
    if matches.next().is_some() {
        return Err("generated validation DAG contains duplicate test.dbt_parity nodes".to_owned());
    }
    require_official_dbt_matrix_envelope(&step)?;
    Ok(MatrixExecutionEnvelope {
        cmdtype: step.cmdtype,
        hint: step.hint,
        networkonly: step.networkonly,
        engine_only: step.engine_only,
        timeout: step.timeout,
        cpu_timeout: step.cpu_timeout,
        jobs_flag: step.jobs_flag,
        jobs_env: step.jobs_env,
    })
}

fn run_dbt_matrix(request: MatrixInvocation<'_>) -> Result<String, String> {
    let MatrixInvocation {
        root,
        cgroups,
        safehermit,
        binary,
        install,
        evidence_dir,
        candidate_label,
        output,
        description,
        envelope,
    } = request;
    if !matches!(candidate_label, "cargo" | "buck") {
        return Err(format!(
            "invalid DBT matrix candidate label {candidate_label:?}"
        ));
    }
    let proxy = root.join("scripts/build-buck-release.rs");
    let arguments = vec![
        "python3".to_owned(),
        root.join("tests/backend-parity/run_matrix.py")
            .to_string_lossy()
            .into_owned(),
        "--hermit".to_owned(),
        proxy.to_string_lossy().into_owned(),
        "--backend".to_owned(),
        "dbt".to_owned(),
        "--strict".to_owned(),
        "--require-backend".to_owned(),
        "--no-parent-scorecard".to_owned(),
        "--output".to_owned(),
        output.to_string_lossy().into_owned(),
    ];
    let matrix_invocations = evidence_dir.join("matrix-candidate-invocations");
    fs::create_dir_all(&matrix_invocations).map_err(|error| {
        format!(
            "failed to create matrix invocation evidence {}: {error}",
            matrix_invocations.display()
        )
    })?;
    let mut environment = BTreeMap::new();
    environment.insert(MATRIX_PROXY_ENV.to_owned(), "1".to_owned());
    environment.insert(
        "HERMIT_BUCK_PHASE1_SAFEHERMIT".to_owned(),
        safehermit.to_string_lossy().into_owned(),
    );
    environment.insert(
        "HERMIT_BUCK_PHASE1_CANDIDATE".to_owned(),
        binary.to_string_lossy().into_owned(),
    );
    environment.insert(
        "HERMIT_BUCK_PHASE1_INSTALL".to_owned(),
        install.to_string_lossy().into_owned(),
    );
    environment.insert(
        "HERMIT_BUCK_PHASE1_MATRIX_EVIDENCE".to_owned(),
        matrix_invocations.to_string_lossy().into_owned(),
    );
    environment.insert(
        "HERMIT_BUCK_PHASE1_CANDIDATE_LABEL".to_owned(),
        candidate_label.to_owned(),
    );
    environment.insert(
        "HERMIT_INSTALL_DIR".to_owned(),
        install.to_string_lossy().into_owned(),
    );
    environment.insert("HERMIT_E2E_EMPTY_WORKDIR".to_owned(), "/test".to_owned());
    let step = Step {
        group: "buck_phase1".into(),
        job: format!("{candidate_label}_dbt_matrix"),
        desc: description.into(),
        description: "Official DBT matrix under the repository cgroup process-tree supervisor"
            .into(),
        labels: Vec::new(),
        cmd: shell_command(&arguments),
        cmdtype: envelope.cmdtype,
        manifest: None,
        integration_test_binaries: None,
        result_manifests: None,
        deps: Vec::new(),
        env: environment,
        hint: envelope.hint.clone(),
        networkonly: envelope.networkonly,
        engine_only: envelope.engine_only,
        timeout: envelope.timeout,
        cpu_timeout: envelope.cpu_timeout,
        jobs_flag: envelope.jobs_flag.clone(),
        jobs_env: envelope.jobs_env.clone(),
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    };
    let config = DagConfig {
        description: format!("{candidate_label} Buck phase-1 shadow DBT matrix"),
        steps: vec![step],
        ..Default::default()
    };
    let violating = steps_violating_run_timeout(&config, MATRIX_SUPERVISOR_DEADLINE_SECONDS);
    if !violating.is_empty() {
        return Err(format!(
            "DBT matrix inner/outer deadline ordering is invalid before launch: {violating:?}"
        ));
    }
    let result = run_dag_boxed_deadline(
        &config,
        1,
        false,
        2,
        cgroups,
        None,
        Some(1),
        Some(MATRIX_SUPERVISOR_DEADLINE_SECONDS),
    );
    let outcome = result.outcomes.first();
    let supervisor_record = format!(
        "supervisor\tdagrun-cgroup-v2\nstep_wall_seconds\t{}\nouter_wall_seconds\t{}\ncleanup_evidence_grace_seconds\t{}\nprocess_tree_cleanup\tcomplete\nok\t{}\nrun_timed_out\t{}\nrun_cpu_timed_out\t{}\noutcome_count\t{}\nskipped_count\t{}\nnot_launched_count\t{}\noutcome_ok\t{}\noutcome_timed_out\t{}\noutcome_cpu_timed_out\t{}\noutcome_aborted\t{}\noutcome_returncode\t{}\n",
        envelope.timeout,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS - envelope.timeout,
        result.ok,
        result.run_timed_out,
        result.run_cpu_timed_out,
        result.outcomes.len(),
        result.skipped.len(),
        result.not_launched.len(),
        outcome.is_some_and(|outcome| outcome.ok),
        outcome.is_some_and(|outcome| outcome.timed_out),
        outcome.is_some_and(|outcome| outcome.cpu_timed_out),
        outcome.is_some_and(|outcome| outcome.aborted),
        outcome
            .and_then(|outcome| outcome.returncode)
            .map_or_else(|| "none".to_owned(), |code| code.to_string()),
    );
    write_new_file(
        &evidence_dir.join(format!("{candidate_label}-matrix-supervisor.tsv")),
        supervisor_record.as_bytes(),
        "matrix supervisor evidence",
    )?;
    if require_clean_matrix_supervision(
        result.ok,
        result.run_timed_out,
        result.run_cpu_timed_out,
        result.run_cpu_accounting_failed,
        result.skipped.len(),
        result.not_launched.len(),
        result.outcomes.len(),
        outcome.is_some_and(|outcome| outcome.ok),
        outcome.is_some_and(|outcome| outcome.timed_out),
        outcome.is_some_and(|outcome| outcome.cpu_timed_out),
        outcome.is_some_and(|outcome| outcome.aborted),
        outcome.and_then(|outcome| outcome.returncode),
    )
    .is_err()
    {
        return Err(format!(
            "{description} did not complete cleanly under cgroup supervision; retained supervisor evidence records cancellation/cleanup disposition"
        ));
    }
    normalized_matrix(output)
}

fn atomic_write_new(path: &Path, contents: &str) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "refusing to replace existing final evidence {}",
            path.display()
        ));
    }
    let temporary = path.with_extension(format!("tmp.{}", unique_identity()?));
    write_new_file(&temporary, contents.as_bytes(), "temporary atomic evidence")?;
    let publish = fs::hard_link(&temporary, path).map_err(|error| {
        format!(
            "failed to publish new evidence {} without replacement: {error}",
            path.display()
        )
    });
    let cleanup = fs::remove_file(&temporary).map_err(|error| {
        format!(
            "failed to remove temporary evidence {}: {error}",
            temporary.display()
        )
    });
    match (publish, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}; cleanup also failed: {cleanup}")),
    }
}

fn collect_evidence_hashes(
    root: &Path,
    directory: &Path,
    receipt: &Path,
    hashes: &mut Vec<(String, String)>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to enumerate {}: {error}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to enumerate {}: {error}", directory.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        if path == receipt {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "final evidence contains an unbound symlink {}; publish real files only",
                path.display()
            ));
        }
        if metadata.is_dir() {
            collect_evidence_hashes(root, &path, receipt, hashes)?;
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| format!("evidence path escaped receipt root: {}", path.display()))?;
            hashes.push((relative.to_string_lossy().into_owned(), sha256(&path)?));
        } else {
            return Err(format!(
                "final evidence contains unsupported special file {}",
                path.display()
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct FinalReceiptFacts {
    result: String,
    evidence_identity: String,
    process_tree_supervisor: String,
    matrix_step_wall_seconds: i64,
    matrix_outer_wall_seconds: i64,
    matrix_cleanup_evidence_grace_seconds: i64,
    process_tree_cleanup: String,
    hermit_full_sha: String,
    hermit_build_sha: String,
    reverie_sha: String,
    version: String,
    build_date: String,
    dotslash_version: String,
    dotslash_sha256: String,
    buck_version: String,
    buck_descriptor_sha256: String,
    buck_executable_sha256: String,
    lzma_input: String,
    lzma_input_sha256: String,
    generated_buck_sha256: String,
    safehermit_sha256: String,
    bounded_run_space_sha256: String,
    cargo_candidate_sha256: String,
    cargo_hermit_manifest_sha256: String,
    cargo_resources_manifest_sha256: String,
    buck_candidate_sha256: String,
    buck_hermit_manifest_sha256: String,
    buck_resources_manifest_sha256: String,
    event_log: String,
    ptrace_verify_messages_left: u64,
    ptrace_verify_messages_right: u64,
    ptrace_record_messages_left: u64,
    ptrace_record_messages_right: u64,
    cargo_matrix_rows: usize,
    buck_matrix_rows: usize,
    matrix_candidate_manifest_sha256: String,
    matrix_candidate_manifest: String,
    matrix_expected_ledger: String,
    matrix_expected_ledger_sha256: String,
    matrix_expected_invocation_count: usize,
    cargo_matrix_candidate_invocations: usize,
    buck_matrix_candidate_invocations: usize,
}

struct FinalReceiptContext<'a> {
    root: &'a Path,
    evidence_dir: &'a Path,
    cargo_build_info: &'a BuildInfo,
    cargo_bundle: &'a PublishedBundle,
    buck_bundle: &'a PublishedBundle,
    safehermit_bundle: &'a SafehermitBundle,
    dotslash_snapshot: &'a InputSnapshot,
    dotslash_version: &'a str,
    buck_descriptor_snapshot: &'a InputSnapshot,
    buck_executable_snapshot: &'a InputSnapshot,
    buck_version: &'a str,
    lzma_input: &'a Path,
    lzma_input_sha256: &'a str,
    generated_buck: &'a Path,
    generated_buck_sha256: &'a str,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct FinalReceiptDocument {
    receipt_schema: String,
    facts: FinalReceiptFacts,
    resource_sha256: BTreeMap<String, String>,
    artifact_sha256: BTreeMap<String, String>,
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_receipt_facts(facts: &FinalReceiptFacts) -> Result<(), String> {
    let hashes = [
        &facts.dotslash_sha256,
        &facts.buck_descriptor_sha256,
        &facts.buck_executable_sha256,
        &facts.lzma_input_sha256,
        &facts.generated_buck_sha256,
        &facts.safehermit_sha256,
        &facts.bounded_run_space_sha256,
        &facts.cargo_candidate_sha256,
        &facts.cargo_hermit_manifest_sha256,
        &facts.cargo_resources_manifest_sha256,
        &facts.buck_candidate_sha256,
        &facts.buck_hermit_manifest_sha256,
        &facts.buck_resources_manifest_sha256,
        &facts.matrix_candidate_manifest_sha256,
        &facts.matrix_expected_ledger_sha256,
    ];
    if facts.result != "pass"
        || facts.evidence_identity.is_empty()
        || facts.process_tree_supervisor != "dagrun-cgroup-v2"
        || facts.process_tree_cleanup != "complete"
        || facts.matrix_step_wall_seconds != MATRIX_STEP_DEADLINE_SECONDS
        || facts.matrix_outer_wall_seconds != MATRIX_SUPERVISOR_DEADLINE_SECONDS
        || facts.matrix_cleanup_evidence_grace_seconds
            != MATRIX_SUPERVISOR_DEADLINE_SECONDS - MATRIX_STEP_DEADLINE_SECONDS
        || facts.hermit_full_sha.len() != 40
        || !facts
            .hermit_full_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || facts.hermit_build_sha.len() != 12
        || !facts
            .hermit_build_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !facts.hermit_full_sha.starts_with(&facts.hermit_build_sha)
        || facts.reverie_sha.len() != 40
        || !facts
            .reverie_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || facts.version.is_empty()
        || facts.version == "unknown"
        || !canonical_build_date(&facts.build_date)
        || hashes.into_iter().any(|hash| !valid_sha256(hash))
        || facts.event_log != "buck-build.json-lines.gz"
        || facts.ptrace_verify_messages_left == 0
        || facts.ptrace_verify_messages_left != facts.ptrace_verify_messages_right
        || facts.ptrace_record_messages_left == 0
        || facts.ptrace_record_messages_left != facts.ptrace_record_messages_right
        || facts.cargo_matrix_rows == 0
        || facts.cargo_matrix_rows != facts.buck_matrix_rows
        || facts.cargo_matrix_candidate_invocations == 0
        || facts.buck_matrix_candidate_invocations == 0
        || facts.cargo_matrix_candidate_invocations != facts.buck_matrix_candidate_invocations
        || facts.matrix_candidate_manifest != "matrix-candidate-invocations.json"
        || facts.matrix_expected_ledger != "matrix-expected-ledger.json"
        || facts.matrix_expected_invocation_count == 0
        || facts.cargo_matrix_candidate_invocations != facts.matrix_expected_invocation_count
    {
        return Err("final receipt contains invalid semantic singleton values".to_owned());
    }
    Ok(())
}

fn expected_clean_matrix_supervisor_record() -> String {
    format!(
        "supervisor\tdagrun-cgroup-v2\nstep_wall_seconds\t{}\nouter_wall_seconds\t{}\ncleanup_evidence_grace_seconds\t{}\nprocess_tree_cleanup\tcomplete\nok\ttrue\nrun_timed_out\tfalse\nrun_cpu_timed_out\tfalse\noutcome_count\t1\nskipped_count\t0\nnot_launched_count\t0\noutcome_ok\ttrue\noutcome_timed_out\tfalse\noutcome_cpu_timed_out\tfalse\noutcome_aborted\tfalse\noutcome_returncode\t0\n",
        MATRIX_STEP_DEADLINE_SECONDS,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS,
        MATRIX_SUPERVISOR_DEADLINE_SECONDS - MATRIX_STEP_DEADLINE_SECONDS,
    )
}

fn verify_matrix_supervisor_evidence(evidence_dir: &Path) -> Result<(), String> {
    let expected = expected_clean_matrix_supervisor_record();
    for label in ["cargo", "buck"] {
        let path = evidence_dir.join(format!("{label}-matrix-supervisor.tsv"));
        let actual = fs::read_to_string(&path).map_err(|error| {
            format!(
                "{label} matrix supervisor evidence {} is unreadable: {error}",
                path.display()
            )
        })?;
        if actual != expected {
            return Err(format!(
                "{label} matrix supervisor evidence does not prove the reviewed clean deadline/cleanup contract"
            ));
        }
    }
    Ok(())
}

const EXPECTED_CANDIDATE_INVOCATIONS: [&str; 10] = [
    "cargo-version",
    "buck-version",
    "cargo-help",
    "buck-help",
    "cargo-host-capabilities",
    "buck-host-capabilities",
    "cargo-ptrace-verify",
    "buck-ptrace-verify",
    "cargo-ptrace-record",
    "buck-ptrace-record",
];

fn verify_candidate_invocation_reports(evidence_dir: &Path) -> Result<(), String> {
    let root = require_absolute_directory(
        &evidence_dir.join("candidate-invocations"),
        "candidate invocation evidence directory",
    )?;
    let mut matched = BTreeMap::<String, PathBuf>::new();
    for entry in fs::read_dir(&root)
        .map_err(|error| format!("cannot enumerate candidate invocation evidence: {error}"))?
    {
        let path = entry
            .map_err(|error| format!("cannot read candidate invocation entry: {error}"))?
            .path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect candidate invocation {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!(
                "candidate invocation evidence contains non-directory {}",
                path.display()
            ));
        }
        let name = path
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| "candidate invocation identity is not UTF-8".to_owned())?;
        let identity = EXPECTED_CANDIDATE_INVOCATIONS
            .iter()
            .find(|identity| name.starts_with(&format!("{identity}-")))
            .ok_or_else(|| format!("unexpected candidate invocation identity {name:?}"))?;
        if matched
            .insert((*identity).to_owned(), path.clone())
            .is_some()
        {
            return Err(format!(
                "candidate invocation identity {identity:?} occurs more than once"
            ));
        }
        let mut files = fs::read_dir(&path)
            .map_err(|error| format!("cannot enumerate candidate invocation {name}: {error}"))?
            .map(|entry| {
                entry
                    .map_err(|error| error.to_string())?
                    .file_name()
                    .into_string()
                    .map_err(|_| "candidate invocation filename is not UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        files.sort();
        if files != ["safehermit.report", "stderr", "stdout"] {
            return Err(format!(
                "candidate invocation {name:?} has unexpected file population {files:?}"
            ));
        }
        validate_and_surface_safehermit_report(
            &path.join("safehermit.report"),
            &format!("retained candidate invocation {identity}"),
        )?;
    }
    let actual = matched.keys().map(String::as_str).collect::<BTreeSet<_>>();
    let expected = EXPECTED_CANDIDATE_INVOCATIONS
        .into_iter()
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "candidate invocation identities differ: expected {expected:?}, got {actual:?}"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixCandidateInvocationBinding {
    candidate_label: String,
    identity: String,
    case_identity: String,
    role: String,
    normalized_argv: Vec<String>,
    invocation_metadata_sha256: String,
    safehermit_report_sha256: String,
    stdout_sha256: String,
    stderr_sha256: String,
    data_sha256: BTreeMap<String, String>,
    data_directories: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct MatrixCandidateInvocationManifest {
    manifest_schema: String,
    invocations: Vec<MatrixCandidateInvocationBinding>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatrixCandidateInvocationFacts {
    manifest_sha256: String,
    cargo_count: usize,
    buck_count: usize,
    expected_ledger_sha256: String,
    expected_invocation_count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct OfficialMatrixCase {
    test_name: String,
    expectation: String,
    selected: bool,
    requires_ptrace_reference: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
struct ExpectedMatrixInvocation {
    case_identity: String,
    role: String,
    count: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
struct OfficialMatrixExpectedLedger {
    ledger_schema: String,
    run_matrix_sha256: String,
    cases: Vec<OfficialMatrixCase>,
    expected_invocations: Vec<ExpectedMatrixInvocation>,
}

fn derive_official_matrix_expected_ledger(
    repository_root: &Path,
) -> Result<OfficialMatrixExpectedLedger, String> {
    let source = repository_root.join("tests/backend-parity/run_matrix.py");
    require_nonempty_regular_file(&source, "official DBT matrix runner")?;
    let program = r#"
import json, runpy, sys
m = runpy.run_path(sys.argv[1])
fixtures = m["CatalogFixtures"]()
rows = []
for name in m["validate_catalog"]():
    _guest, _status, expected_stdout = m["case_command"](name, fixtures)
    expectation, _reason = m["expectation"]("dbt", name, False)
    selected = expectation != "gap"
    rows.append({
        "test_name": name,
        "expectation": expectation,
        "selected": selected,
        "requires_ptrace_reference": selected and m["exact_stdout_parity_contract"]("dbt", name, expected_stdout),
    })
print(json.dumps(rows, separators=(",", ":")))
"#;
    let output = output_text(
        Command::new("python3").args(["-c", program]).arg(&source),
        "derive expected DBT invocation ledger from official matrix implementation",
    )?;
    let cases: Vec<OfficialMatrixCase> =
        decode_typed_json(&output, "official DBT matrix selected-case ledger")?;
    if cases.is_empty()
        || cases.iter().any(|case| {
            case.test_name.is_empty()
                || !matches!(case.expectation.as_str(), "pass" | "gap")
                || case.selected != (case.expectation != "gap")
                || (!case.selected && case.requires_ptrace_reference)
        })
        || cases
            .iter()
            .map(|case| case.test_name.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            != cases.len()
    {
        return Err("official DBT matrix produced an invalid selected-case ledger".to_owned());
    }
    let mut expected = BTreeMap::<(String, String), usize>::new();
    expected.insert(
        ("probe/host-capabilities".to_owned(), "probe".to_owned()),
        1,
    );
    expected.insert(("probe/dbt-smoke".to_owned(), "probe".to_owned()), 1);
    for case in cases.iter().filter(|case| case.selected) {
        expected.insert((case.test_name.clone(), "dbt-run".to_owned()), 3);
        if case.requires_ptrace_reference {
            expected.insert((case.test_name.clone(), "ptrace-reference".to_owned()), 1);
        }
    }
    Ok(OfficialMatrixExpectedLedger {
        ledger_schema: "hermit-official-dbt-expected-ledger/v1".to_owned(),
        run_matrix_sha256: sha256(&source)?,
        cases,
        expected_invocations: expected
            .into_iter()
            .map(|((case_identity, role), count)| ExpectedMatrixInvocation {
                case_identity,
                role,
                count,
            })
            .collect(),
    })
}

fn render_official_matrix_expected_ledger(
    ledger: &OfficialMatrixExpectedLedger,
) -> Result<String, String> {
    if ledger.ledger_schema != "hermit-official-dbt-expected-ledger/v1"
        || !valid_sha256(&ledger.run_matrix_sha256)
        || ledger.cases.is_empty()
        || ledger.expected_invocations.is_empty()
        || ledger.expected_invocations.iter().any(|entry| {
            entry.case_identity.is_empty()
                || !matches!(
                    entry.role.as_str(),
                    "probe" | "ptrace-reference" | "dbt-run"
                )
                || entry.count == 0
        })
    {
        return Err("official DBT expected ledger has invalid semantics".to_owned());
    }
    serde_json::to_string_pretty(ledger)
        .map(|text| format!("{text}\n"))
        .map_err(|error| format!("failed to encode official DBT expected ledger: {error}"))
}

fn publish_official_matrix_expected_ledger(
    repository_root: &Path,
    evidence_dir: &Path,
) -> Result<PathBuf, String> {
    let ledger = derive_official_matrix_expected_ledger(repository_root)?;
    let text = render_official_matrix_expected_ledger(&ledger)?;
    let path = evidence_dir.join("matrix-expected-ledger.json");
    atomic_write_new(&path, &text)?;
    Ok(path)
}

fn verify_official_matrix_tsv(
    path: &Path,
    ledger: &OfficialMatrixExpectedLedger,
    candidate_label: &str,
) -> Result<(), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("cannot read {candidate_label} official matrix TSV: {error}"))?;
    let mut lines = text.lines();
    if lines.next() != Some("test_name\tbackend\texpectation\tresult\tseconds\tdetail") {
        return Err(format!(
            "{candidate_label} official matrix TSV has an unexpected header"
        ));
    }
    let mut observed = BTreeMap::new();
    for line in lines {
        let fields = line.splitn(6, '\t').collect::<Vec<_>>();
        if fields.len() != 6
            || fields[1] != "dbt"
            || !matches!(fields[2], "pass" | "gap")
            || observed
                .insert(
                    fields[0].to_owned(),
                    (fields[2].to_owned(), fields[3].to_owned()),
                )
                .is_some()
        {
            return Err(format!(
                "{candidate_label} official matrix TSV contains a malformed or duplicate row"
            ));
        }
    }
    let expected = ledger
        .cases
        .iter()
        .map(|case| {
            (
                case.test_name.clone(),
                (
                    case.expectation.clone(),
                    if case.selected { "PASS" } else { "GAP" }.to_owned(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if observed.len() != expected.len()
        || observed.iter().any(|(name, (expectation, result))| {
            expected
                .get(name)
                .is_none_or(|(expected_expectation, expected_result)| {
                    expectation != expected_expectation || result != expected_result
                })
        })
    {
        return Err(format!(
            "{candidate_label} official matrix TSV case IDs/selections differ from the independent expected ledger"
        ));
    }
    Ok(())
}

fn load_and_verify_official_matrix_expected_ledger(
    repository_root: &Path,
    evidence_dir: &Path,
) -> Result<(OfficialMatrixExpectedLedger, String), String> {
    let path = require_nonempty_regular_file(
        &evidence_dir.join("matrix-expected-ledger.json"),
        "official DBT expected ledger",
    )?;
    let before = stable_file_identity(&path)?;
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read official DBT expected ledger: {error}"))?;
    let after = stable_file_identity(&path)?;
    if before != after {
        return Err("official DBT expected ledger changed while verified".to_owned());
    }
    let retained: OfficialMatrixExpectedLedger =
        decode_typed_json(&text, "official DBT expected ledger")?;
    let derived = derive_official_matrix_expected_ledger(repository_root)?;
    if text != render_official_matrix_expected_ledger(&retained)? || retained != derived {
        return Err(
            "retained official DBT expected ledger differs from the current matrix implementation"
                .to_owned(),
        );
    }
    for (label, path) in [
        ("cargo", evidence_dir.join("cargo-dbt-matrix.tsv")),
        ("buck", evidence_dir.join("buck-dbt-matrix.tsv")),
    ] {
        verify_official_matrix_tsv(&path, &retained, label)?;
    }
    Ok((retained, before.sha256))
}

fn collect_matrix_data_directories(
    root: &Path,
    directory: &Path,
    directories: &mut Vec<String>,
) -> Result<(), String> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot enumerate matrix candidate data: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read matrix candidate data entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect matrix candidate data {}: {error}",
                path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "matrix candidate data contains symlink {}",
                path.display()
            ));
        }
        if metadata.is_dir() {
            directories.push(
                path.strip_prefix(root)
                    .map_err(|_| "matrix candidate data directory escaped root".to_owned())?
                    .to_string_lossy()
                    .into_owned(),
            );
            collect_matrix_data_directories(root, &path, directories)?;
        } else if !metadata.is_file() {
            return Err(format!(
                "matrix candidate data contains special file {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn inspect_matrix_candidate_invocations(
    evidence_dir: &Path,
) -> Result<MatrixCandidateInvocationManifest, String> {
    let root = require_absolute_directory(
        &evidence_dir.join("matrix-candidate-invocations"),
        "matrix candidate invocation evidence directory",
    )?;
    let mut entries = fs::read_dir(&root)
        .map_err(|error| format!("cannot enumerate matrix candidate invocations: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("cannot read matrix candidate invocation entry: {error}"))?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut identities = BTreeSet::new();
    let mut invocations = Vec::new();
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(|error| {
            format!(
                "cannot inspect matrix candidate invocation {}: {error}",
                path.display()
            )
        })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!(
                "matrix candidate invocation evidence contains non-directory {}",
                path.display()
            ));
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "matrix candidate invocation identity is not UTF-8".to_owned())?;
        let (candidate_label, identity) = if let Some(identity) = name.strip_prefix("cargo-") {
            ("cargo", identity)
        } else if let Some(identity) = name.strip_prefix("buck-") {
            ("buck", identity)
        } else {
            return Err(format!(
                "unexpected matrix candidate invocation identity {name:?}"
            ));
        };
        if identity.is_empty()
            || !identity
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
            || !identities.insert(name.clone())
        {
            return Err(format!(
                "invalid or duplicate matrix candidate invocation identity {name:?}"
            ));
        }
        let mut files = fs::read_dir(&path)
            .map_err(|error| {
                format!("cannot enumerate matrix candidate invocation {name}: {error}")
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("cannot read matrix candidate invocation file: {error}"))?;
        files.sort_by_key(|entry| entry.file_name());
        let actual_names = files
            .iter()
            .map(|entry| {
                entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "matrix candidate invocation filename is not UTF-8".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        if actual_names
            != [
                "data",
                "invocation.json",
                "safehermit.report",
                "stderr",
                "stdout",
            ]
        {
            return Err(format!(
                "matrix candidate invocation {name:?} has unexpected file population {actual_names:?}"
            ));
        }
        for file in files.iter().filter(|file| file.file_name() != "data") {
            let file_metadata = fs::symlink_metadata(file.path()).map_err(|error| {
                format!(
                    "cannot inspect matrix candidate invocation file {}: {error}",
                    file.path().display()
                )
            })?;
            if !file_metadata.is_file() || file_metadata.file_type().is_symlink() {
                return Err(format!(
                    "matrix candidate invocation contains link or special file {}",
                    file.path().display()
                ));
            }
        }
        let report = path.join("safehermit.report");
        let invocation_metadata_path = path.join("invocation.json");
        let invocation_metadata_text = fs::read_to_string(&invocation_metadata_path)
            .map_err(|error| format!("cannot read matrix invocation metadata {name:?}: {error}"))?;
        let invocation_metadata: MatrixProxyInvocation = decode_typed_json(
            &invocation_metadata_text,
            "matrix candidate invocation metadata",
        )?;
        if invocation_metadata.invocation_schema != "hermit-matrix-proxy-invocation/v1"
            || invocation_metadata.case_identity.is_empty()
            || !matches!(
                invocation_metadata.role.as_str(),
                "probe" | "ptrace-reference" | "dbt-run"
            )
            || invocation_metadata.normalized_argv.is_empty()
        {
            return Err(format!(
                "matrix candidate invocation {name:?} has invalid typed metadata"
            ));
        }
        let data = require_absolute_directory(
            &path.join("data"),
            "matrix candidate invocation data directory",
        )?;
        let mut data_rows = Vec::new();
        let mut data_directories = Vec::new();
        collect_matrix_data_directories(&data, &data, &mut data_directories)?;
        collect_evidence_hashes(
            &data,
            &data,
            &data.join(".no-receipt-exclusion"),
            &mut data_rows,
        )?;
        data_rows.sort();
        validate_and_surface_safehermit_report(
            &report,
            &format!("retained {candidate_label} DBT matrix invocation {identity}"),
        )?;
        invocations.push(MatrixCandidateInvocationBinding {
            candidate_label: candidate_label.to_owned(),
            identity: identity.to_owned(),
            case_identity: invocation_metadata.case_identity,
            role: invocation_metadata.role,
            normalized_argv: invocation_metadata.normalized_argv,
            invocation_metadata_sha256: sha256(&invocation_metadata_path)?,
            safehermit_report_sha256: sha256(&report)?,
            stdout_sha256: sha256(&path.join("stdout"))?,
            stderr_sha256: sha256(&path.join("stderr"))?,
            data_sha256: data_rows.into_iter().collect(),
            data_directories,
        });
    }
    let cargo_count = invocations
        .iter()
        .filter(|invocation| invocation.candidate_label == "cargo")
        .count();
    let buck_count = invocations
        .iter()
        .filter(|invocation| invocation.candidate_label == "buck")
        .count();
    if cargo_count == 0 || buck_count == 0 || cargo_count != buck_count {
        return Err(format!(
            "matrix candidate evidence must contain equal nonempty Cargo and Buck populations; got cargo={cargo_count}, buck={buck_count}"
        ));
    }
    Ok(MatrixCandidateInvocationManifest {
        manifest_schema: "hermit-matrix-candidate-invocations/v1".to_owned(),
        invocations,
    })
}

fn render_matrix_candidate_invocation_manifest(
    manifest: &MatrixCandidateInvocationManifest,
) -> Result<String, String> {
    if manifest.manifest_schema != "hermit-matrix-candidate-invocations/v1"
        || manifest.invocations.is_empty()
        || manifest.invocations.iter().any(|invocation| {
            !matches!(invocation.candidate_label.as_str(), "cargo" | "buck")
                || invocation.identity.is_empty()
                || invocation.case_identity.is_empty()
                || !matches!(
                    invocation.role.as_str(),
                    "probe" | "ptrace-reference" | "dbt-run"
                )
                || invocation.normalized_argv.is_empty()
                || !valid_sha256(&invocation.invocation_metadata_sha256)
                || !valid_sha256(&invocation.safehermit_report_sha256)
                || !valid_sha256(&invocation.stdout_sha256)
                || !valid_sha256(&invocation.stderr_sha256)
                || invocation.data_sha256.iter().any(|(relative, hash)| {
                    let path = Path::new(relative);
                    !valid_sha256(hash)
                        || path.is_absolute()
                        || path
                            .components()
                            .any(|component| !matches!(component, std::path::Component::Normal(_)))
                })
                || invocation.data_directories.iter().any(|relative| {
                    let path = Path::new(relative);
                    path.as_os_str().is_empty()
                        || path.is_absolute()
                        || path
                            .components()
                            .any(|component| !matches!(component, std::path::Component::Normal(_)))
                })
        })
    {
        return Err("matrix candidate invocation manifest has invalid semantics".to_owned());
    }
    serde_json::to_string_pretty(manifest)
        .map(|text| format!("{text}\n"))
        .map_err(|error| format!("failed to encode matrix candidate invocation manifest: {error}"))
}

fn validate_matrix_candidate_completeness(
    repository_root: &Path,
    evidence_dir: &Path,
    manifest: &MatrixCandidateInvocationManifest,
) -> Result<(String, usize), String> {
    let (ledger, ledger_sha256) =
        load_and_verify_official_matrix_expected_ledger(repository_root, evidence_dir)?;
    let expected = ledger
        .expected_invocations
        .iter()
        .map(|entry| {
            (
                (entry.case_identity.clone(), entry.role.clone()),
                entry.count,
            )
        })
        .collect::<BTreeMap<_, _>>();
    let expected_count = expected.values().sum::<usize>();
    let mut per_candidate = BTreeMap::<String, BTreeMap<(String, String), usize>>::new();
    let mut argv_per_candidate =
        BTreeMap::<String, BTreeMap<(String, String, Vec<String>), usize>>::new();
    for invocation in &manifest.invocations {
        *per_candidate
            .entry(invocation.candidate_label.clone())
            .or_default()
            .entry((invocation.case_identity.clone(), invocation.role.clone()))
            .or_default() += 1;
        *argv_per_candidate
            .entry(invocation.candidate_label.clone())
            .or_default()
            .entry((
                invocation.case_identity.clone(),
                invocation.role.clone(),
                invocation.normalized_argv.clone(),
            ))
            .or_default() += 1;
    }
    for label in ["cargo", "buck"] {
        if per_candidate.get(label) != Some(&expected) {
            return Err(format!(
                "{label} matrix proxy case/role multiset differs from the independently derived official ledger"
            ));
        }
    }
    if argv_per_candidate.get("cargo") != argv_per_candidate.get("buck") {
        return Err("Cargo/Buck matrix proxy normalized argv/case multisets differ".to_owned());
    }
    Ok((ledger_sha256, expected_count))
}

fn publish_matrix_candidate_invocation_manifest(
    repository_root: &Path,
    evidence_dir: &Path,
) -> Result<PathBuf, String> {
    let path = evidence_dir.join("matrix-candidate-invocations.json");
    let manifest = inspect_matrix_candidate_invocations(evidence_dir)?;
    validate_matrix_candidate_completeness(repository_root, evidence_dir, &manifest)?;
    let text = render_matrix_candidate_invocation_manifest(&manifest)?;
    atomic_write_new(&path, &text)?;
    Ok(path)
}

fn verify_retained_matrix_candidate_invocation_manifest(
    evidence_dir: &Path,
) -> Result<(MatrixCandidateInvocationManifest, String), String> {
    let path = require_nonempty_regular_file(
        &evidence_dir.join("matrix-candidate-invocations.json"),
        "matrix candidate invocation manifest",
    )?;
    let before = stable_file_identity(&path)?;
    let retained_text = fs::read_to_string(&path).map_err(|error| {
        format!(
            "matrix candidate invocation manifest {} is unreadable: {error}",
            path.display()
        )
    })?;
    let after = stable_file_identity(&path)?;
    if before != after {
        return Err("matrix candidate invocation manifest changed while verified".to_owned());
    }
    let retained: MatrixCandidateInvocationManifest =
        decode_typed_json(&retained_text, "matrix candidate invocation manifest")?;
    let canonical_retained = render_matrix_candidate_invocation_manifest(&retained)?;
    let recomputed = inspect_matrix_candidate_invocations(evidence_dir)?;
    let canonical_recomputed = render_matrix_candidate_invocation_manifest(&recomputed)?;
    if retained_text != canonical_retained
        || canonical_retained != canonical_recomputed
        || retained != recomputed
    {
        return Err(
            "matrix candidate invocation manifest differs from the exact retained population"
                .to_owned(),
        );
    }
    Ok((retained, before.sha256))
}

fn verify_matrix_candidate_invocation_manifest(
    repository_root: &Path,
    evidence_dir: &Path,
) -> Result<MatrixCandidateInvocationFacts, String> {
    let (retained, manifest_sha256) =
        verify_retained_matrix_candidate_invocation_manifest(evidence_dir)?;
    let (expected_ledger_sha256, expected_invocation_count) =
        validate_matrix_candidate_completeness(repository_root, evidence_dir, &retained)?;
    Ok(MatrixCandidateInvocationFacts {
        manifest_sha256,
        cargo_count: retained
            .invocations
            .iter()
            .filter(|invocation| invocation.candidate_label == "cargo")
            .count(),
        buck_count: retained
            .invocations
            .iter()
            .filter(|invocation| invocation.candidate_label == "buck")
            .count(),
        expected_ledger_sha256,
        expected_invocation_count,
    })
}

fn verify_retained_buck_log_summary(
    buck_executable: &Path,
    repository_root: &Path,
    event_log: &Path,
    retained_summary: &Path,
) -> Result<(), String> {
    let retained = fs::read_to_string(retained_summary).map_err(|error| {
        format!(
            "retained Buck log summary {} is unreadable: {error}",
            retained_summary.display()
        )
    })?;
    let recomputed = output_text(
        Command::new(buck_executable)
            .current_dir(repository_root)
            .args(["log", "summary"])
            .arg(event_log),
        "recompute Buck event-log summary from snapshotted Buck",
    )?;
    if retained != format!("{recomputed}\n") {
        return Err("retained Buck event-log summary differs from recomputed summary".to_owned());
    }
    Ok(())
}

struct RetainedCandidateEvidence {
    run_report: VerificationReport,
    record_report: VerificationReport,
}

fn read_retained_text(evidence_dir: &Path, name: &str) -> Result<String, String> {
    let path = evidence_dir.join(name);
    fs::read_to_string(&path).map_err(|error| {
        format!(
            "retained candidate evidence {} is unreadable: {error}",
            path.display()
        )
    })
}

fn retain_candidate_text(
    evidence_dir: &Path,
    name: &str,
    contents: &str,
) -> Result<PathBuf, String> {
    let path = evidence_dir.join(name);
    atomic_write_new(&path, &format!("{contents}\n"))?;
    Ok(path)
}

fn verify_retained_candidate_evidence(
    evidence_dir: &Path,
    expected_version: &str,
    expected_sha: &str,
    expected_cargo_info: &BuildInfo,
) -> Result<RetainedCandidateEvidence, String> {
    let cargo_version = read_retained_text(evidence_dir, "cargo-version.json")?;
    let buck_version = read_retained_text(evidence_dir, "buck-version.json")?;
    let cargo_info = decode_build_info(
        &cargo_version,
        expected_version,
        expected_sha,
        "retained Cargo version JSON",
    )?;
    let buck_info = decode_build_info(
        &buck_version,
        expected_version,
        expected_sha,
        "retained Buck version JSON",
    )?;
    if cargo_info != *expected_cargo_info || cargo_info != buck_info {
        return Err(
            "retained Cargo/Buck typed version/provenance/features reports differ".to_owned(),
        );
    }

    let cargo_help = read_retained_text(evidence_dir, "cargo-help.txt")?;
    let buck_help = read_retained_text(evidence_dir, "buck-help.txt")?;
    require_equal("retained exact help text", &cargo_help, &buck_help)?;
    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        if !buck_help.contains(backend) {
            return Err(format!(
                "retained Buck CLI capability output omits backend spelling {backend}"
            ));
        }
    }

    let cargo_host = read_retained_text(evidence_dir, "cargo-host-capabilities.json")?;
    let buck_host = read_retained_text(evidence_dir, "buck-host-capabilities.json")?;
    let cargo_host_report =
        decode_host_capabilities(&cargo_host, "retained Cargo host-capabilities JSON")?;
    let buck_host_report =
        decode_host_capabilities(&buck_host, "retained Buck host-capabilities JSON")?;
    if cargo_host_report != buck_host_report {
        return Err(
            "retained Cargo/Buck typed host-capabilities reports differ in exact semantics"
                .to_owned(),
        );
    }

    let cargo_run = verify_report(&evidence_dir.join("cargo-ptrace-verify.json"), true)?;
    let buck_run = verify_report(&evidence_dir.join("buck-ptrace-verify.json"), true)?;
    if cargo_run != buck_run {
        return Err("retained Cargo/Buck ptrace strict reports differ".to_owned());
    }
    let cargo_record = verify_report(&evidence_dir.join("cargo-ptrace-record-verify.json"), false)?;
    let buck_record = verify_report(&evidence_dir.join("buck-ptrace-record-verify.json"), false)?;
    if cargo_record != buck_record {
        return Err("retained Cargo/Buck ptrace record reports differ".to_owned());
    }
    Ok(RetainedCandidateEvidence {
        run_report: cargo_run,
        record_report: cargo_record,
    })
}

fn recompute_final_receipt(
    context: &FinalReceiptContext<'_>,
) -> Result<(FinalReceiptFacts, BTreeMap<String, String>), String> {
    reverify_published_bundle(context.root, context.cargo_bundle, "Cargo")?;
    reverify_published_bundle(context.root, context.buck_bundle, "Buck")?;
    require_equal_resource_manifests(context.cargo_bundle, context.buck_bundle)?;
    reverify_safehermit_bundle(context.safehermit_bundle)?;
    reverify_input_snapshot(context.dotslash_snapshot, "snapshotted DotSlash launcher")?;
    reverify_input_snapshot(
        context.buck_descriptor_snapshot,
        "snapshotted Buck2 DotSlash descriptor",
    )?;
    reverify_input_snapshot(
        context.buck_executable_snapshot,
        "snapshotted Buck2 executable",
    )?;
    reverify_hashed_input(
        context.lzma_input,
        context.lzma_input_sha256,
        "liblzma link input",
        false,
    )?;
    reverify_hashed_input(
        context.generated_buck,
        context.generated_buck_sha256,
        "generated Buck dependency graph",
        false,
    )?;
    verify_matrix_supervisor_evidence(context.evidence_dir)?;
    let provenance = exact_provenance(context.root, context.cargo_build_info)?;
    let retained = verify_retained_candidate_evidence(
        context.evidence_dir,
        &provenance.version,
        &provenance.hermit_sha,
        context.cargo_build_info,
    )?;
    let run_counts = retained
        .run_report
        .compared_log_messages
        .ok_or_else(|| "Cargo strict verify report lost compared message counts".to_owned())?;
    let record_counts = retained
        .record_report
        .compared_log_messages
        .ok_or_else(|| "Cargo record report lost compared message counts".to_owned())?;
    let cargo_matrix = normalized_matrix(&context.evidence_dir.join("cargo-dbt-matrix.tsv"))?;
    let buck_matrix = normalized_matrix(&context.evidence_dir.join("buck-dbt-matrix.tsv"))?;
    require_equal(
        "official DBT matrix IDs/outcomes (duration excluded)",
        &cargo_matrix,
        &buck_matrix,
    )?;
    verify_candidate_invocation_reports(context.evidence_dir)?;
    let matrix_candidate_invocations =
        verify_matrix_candidate_invocation_manifest(context.root, context.evidence_dir)?;
    let event_log = context.evidence_dir.join("buck-build.json-lines.gz");
    validate_log_header(&event_log)?;
    verify_retained_buck_log_summary(
        &context.buck_executable_snapshot.path,
        context.root,
        &event_log,
        &context.evidence_dir.join("buck-log-summary.txt"),
    )?;
    let shell_exit_text = read_retained_text(context.evidence_dir, "buck-build-shell-exit.tsv")?;
    let shell_exit = shell_exit_text
        .strip_prefix("shell_exit\t")
        .and_then(|value| value.strip_suffix('\n'))
        .filter(|value| !value.contains(char::is_whitespace))
        .and_then(|value| value.parse::<i32>().ok())
        .ok_or_else(|| "retained Buck build shell-exit receipt is malformed".to_owned())?;
    let build_evidence = inspect_release_build_evidence(
        &event_log,
        &context.evidence_dir.join("buck-build.stdout"),
        shell_exit,
        context.root,
    )?;
    if build_evidence.output_sha256 != context.buck_bundle.binary_sha256 {
        return Err(
            "retained Buck build event/stdout does not bind the tested Buck candidate bytes"
                .to_owned(),
        );
    }
    let final_dotslash_version = output_text(
        Command::new(&context.dotslash_snapshot.path).arg("--version"),
        "final DotSlash --version verification",
    )?;
    let final_buck_version = output_text(
        Command::new(&context.buck_executable_snapshot.path).arg("--version"),
        "final pinned Buck2 --version verification",
    )?;
    if final_dotslash_version != context.dotslash_version
        || final_buck_version != context.buck_version
    {
        return Err("DotSlash or Buck2 version output changed during shadow validation".to_owned());
    }
    let resources = verify_install_bundle(&context.cargo_bundle.install, &provenance.reverie_sha)?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let facts = FinalReceiptFacts {
        result: "pass".to_owned(),
        evidence_identity: context
            .evidence_dir
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| "evidence directory has no UTF-8 identity".to_owned())?
            .to_owned(),
        process_tree_supervisor: "dagrun-cgroup-v2".to_owned(),
        matrix_step_wall_seconds: MATRIX_STEP_DEADLINE_SECONDS,
        matrix_outer_wall_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS,
        matrix_cleanup_evidence_grace_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS
            - MATRIX_STEP_DEADLINE_SECONDS,
        process_tree_cleanup: "complete".to_owned(),
        hermit_full_sha: provenance.hermit_full_sha,
        hermit_build_sha: provenance.hermit_sha,
        reverie_sha: provenance.reverie_sha,
        version: provenance.version,
        build_date: provenance.build_date,
        dotslash_version: context.dotslash_version.to_owned(),
        dotslash_sha256: context.dotslash_snapshot.sha256.clone(),
        buck_version: context.buck_version.to_owned(),
        buck_descriptor_sha256: context.buck_descriptor_snapshot.sha256.clone(),
        buck_executable_sha256: context.buck_executable_snapshot.sha256.clone(),
        lzma_input: context.lzma_input.to_string_lossy().into_owned(),
        lzma_input_sha256: context.lzma_input_sha256.to_owned(),
        generated_buck_sha256: context.generated_buck_sha256.to_owned(),
        safehermit_sha256: sha256(&context.safehermit_bundle.launcher)?,
        bounded_run_space_sha256: sha256(&context.safehermit_bundle.bounded_run_space)?,
        cargo_candidate_sha256: sha256(&context.cargo_bundle.binary)?,
        cargo_hermit_manifest_sha256: sha256(&context.cargo_bundle.hermit_manifest)?,
        cargo_resources_manifest_sha256: sha256(&context.cargo_bundle.resources_manifest)?,
        buck_candidate_sha256: sha256(&context.buck_bundle.binary)?,
        buck_hermit_manifest_sha256: sha256(&context.buck_bundle.hermit_manifest)?,
        buck_resources_manifest_sha256: sha256(&context.buck_bundle.resources_manifest)?,
        event_log: "buck-build.json-lines.gz".to_owned(),
        ptrace_verify_messages_left: run_counts.left,
        ptrace_verify_messages_right: run_counts.right,
        ptrace_record_messages_left: record_counts.left,
        ptrace_record_messages_right: record_counts.right,
        cargo_matrix_rows: cargo_matrix.lines().count().saturating_sub(1),
        buck_matrix_rows: buck_matrix.lines().count().saturating_sub(1),
        matrix_candidate_manifest_sha256: matrix_candidate_invocations.manifest_sha256,
        matrix_candidate_manifest: "matrix-candidate-invocations.json".to_owned(),
        matrix_expected_ledger: "matrix-expected-ledger.json".to_owned(),
        matrix_expected_ledger_sha256: matrix_candidate_invocations.expected_ledger_sha256,
        matrix_expected_invocation_count: matrix_candidate_invocations.expected_invocation_count,
        cargo_matrix_candidate_invocations: matrix_candidate_invocations.cargo_count,
        buck_matrix_candidate_invocations: matrix_candidate_invocations.buck_count,
    };
    validate_receipt_facts(&facts)?;
    Ok((facts, resources))
}

fn validate_bound_hashes(
    description: &str,
    bindings: &BTreeMap<String, String>,
) -> Result<(), String> {
    if bindings.is_empty() {
        return Err(format!(
            "final receipt must bind a nonempty {description} population"
        ));
    }
    for (relative, hash) in bindings {
        let path = Path::new(relative);
        if !valid_sha256(hash)
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(format!(
                "final receipt contains an invalid {description} binding: {relative:?}"
            ));
        }
    }
    Ok(())
}

fn parse_final_receipt(text: &str) -> Result<FinalReceiptDocument, String> {
    let document: FinalReceiptDocument = decode_typed_json(text, "final shadow parity receipt")?;
    if document.receipt_schema != "hermit-buck-shadow-parity/v1" {
        return Err(format!(
            "unknown final receipt schema {:?}",
            document.receipt_schema
        ));
    }
    validate_receipt_facts(&document.facts)?;
    validate_bound_hashes("resource", &document.resource_sha256)?;
    validate_bound_hashes("artifact", &document.artifact_sha256)?;
    Ok(document)
}

fn render_final_receipt(
    facts: &FinalReceiptFacts,
    resources: &BTreeMap<String, String>,
    artifacts: &BTreeMap<String, String>,
) -> Result<String, String> {
    let document = FinalReceiptDocument {
        receipt_schema: "hermit-buck-shadow-parity/v1".to_owned(),
        facts: facts.clone(),
        resource_sha256: resources.clone(),
        artifact_sha256: artifacts.clone(),
    };
    validate_receipt_facts(&document.facts)?;
    validate_bound_hashes("resource", &document.resource_sha256)?;
    validate_bound_hashes("artifact", &document.artifact_sha256)?;
    serde_json::to_string_pretty(&document)
        .map(|contents| format!("{contents}\n"))
        .map_err(|error| format!("failed to encode final shadow parity receipt: {error}"))
}

fn verify_receipt_semantics(
    parsed: &FinalReceiptDocument,
    expected_facts: &FinalReceiptFacts,
    expected_resources: &BTreeMap<String, String>,
    expected_artifacts: &BTreeMap<String, String>,
) -> Result<(), String> {
    if &parsed.facts != expected_facts {
        return Err("final receipt semantic singleton mismatch".to_owned());
    }
    if &parsed.resource_sha256 != expected_resources {
        return Err("final receipt resource inventory/hash mismatch".to_owned());
    }
    if &parsed.artifact_sha256 != expected_artifacts {
        return Err("final receipt artifact population/hash mismatch".to_owned());
    }
    Ok(())
}

fn publish_final_receipt(context: &FinalReceiptContext<'_>) -> Result<PathBuf, String> {
    let receipt = context.evidence_dir.join("shadow-parity.json");
    let (facts, resources) = recompute_final_receipt(context)?;
    let mut artifact_rows = Vec::new();
    collect_evidence_hashes(
        context.evidence_dir,
        context.evidence_dir,
        &receipt,
        &mut artifact_rows,
    )?;
    let artifacts = artifact_rows.into_iter().collect::<BTreeMap<_, _>>();
    let contents = render_final_receipt(&facts, &resources, &artifacts)?;
    atomic_write_new(&receipt, &contents)?;
    verify_final_receipt(context, &receipt)?;
    Ok(receipt)
}

fn verify_final_receipt(context: &FinalReceiptContext<'_>, receipt: &Path) -> Result<(), String> {
    let text = fs::read_to_string(receipt)
        .map_err(|error| format!("final receipt {} is unreadable: {error}", receipt.display()))?;
    let parsed = parse_final_receipt(&text)?;
    let (expected_facts, expected_resources) = recompute_final_receipt(context)?;
    let mut artifact_rows = Vec::new();
    collect_evidence_hashes(
        context.evidence_dir,
        context.evidence_dir,
        receipt,
        &mut artifact_rows,
    )?;
    let expected_artifacts = artifact_rows.into_iter().collect::<BTreeMap<_, _>>();
    verify_receipt_semantics(
        &parsed,
        &expected_facts,
        &expected_resources,
        &expected_artifacts,
    )
}

fn run(options: Options, cgroups: BoxedCgroups) -> Result<(), String> {
    if cgroups.is_none() {
        return Err(
            "Buck shadow validation requires live per-step cgroup supervision; unboxed execution is forbidden"
                .to_owned(),
        );
    }
    let root = repository_root()?;
    let caller_dotslash = require_absolute_file(&options.dotslash, "public DotSlash launcher")?;
    let caller_cargo_binary = require_absolute_file(&options.cargo_binary, "Cargo release binary")?;
    let caller_install_bundle =
        require_absolute_directory(&options.install_bundle, "Cargo install bundle")?;
    let caller_safehermit = require_absolute_file(&options.safehermit, "safehermit launcher")?;

    let rust_script_version = output_text(
        Command::new("rust-script").arg("--version"),
        "rust-script --version",
    )?;
    if rust_script_version != RUST_SCRIPT_VERSION {
        return Err(format!(
            "regeneration requires documented {RUST_SCRIPT_VERSION}, got {rust_script_version:?}; install that version and retry"
        ));
    }

    if repository_dirty(&root)? {
        return Err(
            "Hermit checkout has tracked or non-ignored untracked changes; shadow evidence requires a clean exact tree"
                .to_owned(),
        );
    }
    let preflight_full_sha = git(&root, &["rev-parse", "HEAD"])?;
    let evidence_dir = create_evidence_directory(&root, &preflight_full_sha)?;
    let dotslash_snapshot = snapshot_dotslash(&caller_dotslash, &evidence_dir)?;
    drop(caller_dotslash);
    let dotslash = dotslash_snapshot.path.clone();
    let dotslash_version = output_text(
        Command::new(&dotslash).arg("--version"),
        "snapshotted DotSlash --version",
    )?;
    if dotslash_version != DOTSLASH_VERSION {
        return Err(format!(
            "--dotslash must be the documented public {DOTSLASH_VERSION}, got {dotslash_version:?}; install the pinned launcher from docs/BUCK2_OSS.md"
        ));
    }
    let cargo_bundle = publish_verified_bundle(
        &root,
        &evidence_dir,
        "cargo",
        &caller_cargo_binary,
        &caller_install_bundle,
    )?;
    drop(caller_cargo_binary);
    drop(caller_install_bundle);
    let safehermit_bundle = snapshot_safehermit_bundle(&caller_safehermit, &evidence_dir)?;
    drop(caller_safehermit);
    let safehermit = safehermit_bundle.launcher.clone();
    let cargo_version_data = evidence_dir.join("cargo-version-data");
    let cargo_version_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install_bundle: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "cargo-version",
        data_dir: &cargo_version_data,
        arguments: &["version", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Cargo version JSON through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "cargo-version.json", &cargo_version_output)?;
    let cargo_version_json = read_retained_text(&evidence_dir, "cargo-version.json")?;
    let expected_version = package_version(
        &fs::read_to_string(root.join("hermit-cli/Cargo.toml"))
            .map_err(|error| format!("failed to read hermit-cli/Cargo.toml: {error}"))?,
    )?;
    let expected_short_sha = git(&root, &["rev-parse", "--short=12", "HEAD"])?;
    let cargo_build_info = decode_build_info(
        &cargo_version_json,
        &expected_version,
        &expected_short_sha,
        "Cargo version JSON",
    )?;
    let provenance = exact_provenance(&root, &cargo_build_info)?;
    if provenance.hermit_full_sha != preflight_full_sha {
        return Err("Hermit HEAD changed while shadow validation was starting".to_owned());
    }
    verify_install_bundle(&cargo_bundle.install, &provenance.reverie_sha)?;
    let lzma_input = require_lzma_link_input()?;
    let lzma_input_hash = sha256(&lzma_input)?;
    let buck_descriptor_source = root.join("bootstrap/buck2");
    let buck_descriptor_snapshot = snapshot_input(
        &buck_descriptor_source,
        &evidence_dir,
        "buck2-descriptor-snapshot",
        "buck2.descriptor",
        "Buck2 DotSlash descriptor",
        false,
    )?;
    let fetched_buck_executable = PathBuf::from(output_text(
        Command::new(&dotslash).args([
            OsStr::new("--"),
            OsStr::new("fetch"),
            buck_descriptor_snapshot.path.as_os_str(),
        ]),
        "DotSlash fetch of pinned Buck2",
    )?);
    let buck_executable_snapshot = snapshot_input(
        &fetched_buck_executable,
        &evidence_dir,
        "buck2-tool-snapshot",
        "buck2",
        "resolved Buck2 executable",
        true,
    )?;
    let buck_executable = buck_executable_snapshot.path.clone();
    let buck_version = output_text(
        Command::new(&buck_executable).arg("--version"),
        "snapshotted pinned Buck2 --version",
    )?;

    let regenerate = root.join("bootstrap/regenerate-rust-deps");
    checked_output(
        Command::new(&regenerate).current_dir(&root),
        "deterministic Rust dependency regeneration",
    )?;
    let generated_buck = root.join("shim/third-party/rust/BUCK");
    verify_generated_buck(
        &fs::read_to_string(&generated_buck)
            .map_err(|error| format!("generated BUCK is unreadable: {error}"))?,
    )?;
    let generated_buck_hash = sha256(&generated_buck)?;

    let event_log = evidence_dir.join("buck-build.json-lines.gz");
    let build = Command::new(&buck_executable)
        .current_dir(&root)
        .args(["build", "--show-output", "--event-log"])
        .arg(&event_log)
        .args([
            "-c",
            &format!("hermit_release.version={}", provenance.version),
            "-c",
            &format!("hermit_release.build_date={}", provenance.build_date),
            "-c",
            &format!("hermit_release.hermit_sha={}", provenance.hermit_sha),
            "-c",
            &format!("hermit_release.reverie_sha={}", provenance.reverie_sha),
            TARGET,
        ])
        .output()
        .map_err(|error| format!("failed to start snapshotted Buck2 executable: {error}"))?;
    write_new_file(
        &evidence_dir.join("buck-build.stdout"),
        &build.stdout,
        "Buck build stdout",
    )?;
    write_new_file(
        &evidence_dir.join("buck-build.stderr"),
        &build.stderr,
        "Buck build stderr",
    )?;
    let build_shell_exit = build.status.code().unwrap_or(255);
    atomic_write_new(
        &evidence_dir.join("buck-build-shell-exit.tsv"),
        &format!("shell_exit\t{build_shell_exit}\n"),
    )?;
    reverify_hashed_input(
        &generated_buck,
        &generated_buck_hash,
        "generated Buck dependency graph immediately after build",
        false,
    )?;
    reverify_hashed_input(
        &lzma_input,
        &lzma_input_hash,
        "liblzma link input immediately after build",
        false,
    )?;
    validate_log_header(&event_log)?;
    let summary = output_text(
        Command::new(&buck_executable)
            .current_dir(&root)
            .args(["log", "summary"])
            .arg(&event_log),
        "Buck2 event-log decode/summary",
    )?;
    atomic_write_new(
        &evidence_dir.join("buck-log-summary.txt"),
        &format!("{summary}\n"),
    )?;
    if !build.status.success() {
        return Err(format!(
            "Buck release build failed with {} after retaining and decoding {}; inspect it and retry",
            build.status,
            event_log.display()
        ));
    }
    let build_stdout = String::from_utf8(build.stdout)
        .map_err(|error| format!("Buck build output was not UTF-8: {error}"))?;
    let buck_binary = resolve_release_show_output(&build_stdout, &root)?;

    let buck_bundle = publish_verified_bundle(
        &root,
        &evidence_dir,
        "buck",
        &buck_binary,
        &cargo_bundle.install,
    )?;
    require_equal_resource_manifests(&cargo_bundle, &buck_bundle)?;

    let buck_version_data = evidence_dir.join("buck-version-data");
    let buck_version_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install_bundle: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "buck-version",
        data_dir: &buck_version_data,
        arguments: &["version", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Buck version JSON through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "buck-version.json", &buck_version_output)?;
    let buck_version_json = read_retained_text(&evidence_dir, "buck-version.json")?;
    let buck_build_info = decode_build_info(
        &buck_version_json,
        &provenance.version,
        &provenance.hermit_sha,
        "Buck version JSON",
    )?;
    if cargo_build_info != buck_build_info {
        return Err(
            "Cargo/Buck typed version/provenance/features mismatch; rebuild both at the same SHA/configuration"
                .to_owned(),
        );
    }
    let cargo_help_data = evidence_dir.join("cargo-help-data");
    let cargo_help_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install_bundle: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "cargo-help",
        data_dir: &cargo_help_data,
        arguments: &["--help"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Cargo Hermit help through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "cargo-help.txt", &cargo_help_output)?;
    let cargo_help = read_retained_text(&evidence_dir, "cargo-help.txt")?;
    let buck_help_data = evidence_dir.join("buck-help-data");
    let buck_help_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install_bundle: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "buck-help",
        data_dir: &buck_help_data,
        arguments: &["--help"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Buck Hermit help through safehermit",
    })?
    .stdout;
    retain_candidate_text(&evidence_dir, "buck-help.txt", &buck_help_output)?;
    let buck_help = read_retained_text(&evidence_dir, "buck-help.txt")?;
    require_equal("backend capability/help inventory", &cargo_help, &buck_help)?;
    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        if !buck_help.contains(backend) {
            return Err(format!(
                "Buck CLI capability output omits backend spelling {backend}"
            ));
        }
    }
    let cargo_host_data = evidence_dir.join("cargo-host-capabilities-data");
    let cargo_host_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install_bundle: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "cargo-host-capabilities",
        data_dir: &cargo_host_data,
        arguments: &["host-capabilities", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Cargo host capabilities through safehermit",
    })?
    .stdout;
    retain_candidate_text(
        &evidence_dir,
        "cargo-host-capabilities.json",
        &cargo_host_output,
    )?;
    let cargo_host = read_retained_text(&evidence_dir, "cargo-host-capabilities.json")?;
    let buck_host_data = evidence_dir.join("buck-host-capabilities-data");
    let buck_host_output = run_safehermit(CandidateInvocation {
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install_bundle: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        identity: "buck-host-capabilities",
        data_dir: &buck_host_data,
        arguments: &["host-capabilities", "--json"],
        deadline_seconds: PROBE_DEADLINE_SECONDS,
        description: "Buck host capabilities through safehermit",
    })?
    .stdout;
    retain_candidate_text(
        &evidence_dir,
        "buck-host-capabilities.json",
        &buck_host_output,
    )?;
    let buck_host = read_retained_text(&evidence_dir, "buck-host-capabilities.json")?;
    let cargo_host_report = decode_host_capabilities(&cargo_host, "Cargo host-capabilities JSON")?;
    let buck_host_report = decode_host_capabilities(&buck_host, "Buck host-capabilities JSON")?;
    if cargo_host_report != buck_host_report {
        return Err(
            "Cargo/Buck typed host-capabilities mismatch; reports differ in capability or evidence bytes"
                .to_owned(),
        );
    }
    if elf_identity(&cargo_bundle.binary)? != elf_identity(&buck_bundle.binary)? {
        return Err("Cargo/Buck ELF class/endian/type/machine differ".to_owned());
    }
    if needed_libraries(&cargo_bundle.binary)? != needed_libraries(&buck_bundle.binary)? {
        return Err("Cargo/Buck ELF NEEDED library sets differ".to_owned());
    }
    behavioral_parity(
        &safehermit,
        &cargo_bundle.binary,
        &buck_bundle.binary,
        &cargo_bundle.install,
        &buck_bundle.install,
        &evidence_dir,
    )?;
    let matrix_supervisor_logs = evidence_dir.join("matrix-supervisor-logs");
    create_exclusive_directory(&matrix_supervisor_logs, "matrix supervisor log directory")?;
    // This process is single-threaded and owns both sequential scheduler calls.
    // The official runner reads this parent setting when it opens durable logs.
    unsafe {
        env::set_var("DAGRUN_LOG_DIR", &matrix_supervisor_logs);
    }
    let matrix_envelope = official_dbt_matrix_envelope(&root)?;
    publish_official_matrix_expected_ledger(&root, &evidence_dir)?;
    let cargo_matrix_output = evidence_dir.join("cargo-dbt-matrix.tsv");
    let cargo_matrix = run_dbt_matrix(MatrixInvocation {
        root: &root,
        cgroups: cgroups.clone(),
        safehermit: &safehermit,
        binary: &cargo_bundle.binary,
        install: &cargo_bundle.install,
        evidence_dir: &evidence_dir,
        candidate_label: "cargo",
        output: &cargo_matrix_output,
        description: "official Cargo DBT parity matrix",
        envelope: &matrix_envelope,
    })?;
    let buck_matrix_output = evidence_dir.join("buck-dbt-matrix.tsv");
    let buck_matrix = run_dbt_matrix(MatrixInvocation {
        root: &root,
        cgroups,
        safehermit: &safehermit,
        binary: &buck_bundle.binary,
        install: &buck_bundle.install,
        evidence_dir: &evidence_dir,
        candidate_label: "buck",
        output: &buck_matrix_output,
        description: "official Buck DBT parity matrix",
        envelope: &matrix_envelope,
    })?;
    require_equal(
        "official DBT matrix IDs/outcomes (duration excluded)",
        &cargo_matrix,
        &buck_matrix,
    )?;
    publish_matrix_candidate_invocation_manifest(&root, &evidence_dir)?;
    let final_provenance = exact_provenance(&root, &cargo_build_info)?;
    if final_provenance != provenance {
        return Err(
            "Hermit/Reverie provenance changed during shadow validation; refusing a mixed-tree receipt"
                .to_owned(),
        );
    }
    let receipt_context = FinalReceiptContext {
        root: &root,
        evidence_dir: &evidence_dir,
        cargo_build_info: &cargo_build_info,
        cargo_bundle: &cargo_bundle,
        buck_bundle: &buck_bundle,
        safehermit_bundle: &safehermit_bundle,
        dotslash_snapshot: &dotslash_snapshot,
        dotslash_version: &dotslash_version,
        buck_descriptor_snapshot: &buck_descriptor_snapshot,
        buck_executable_snapshot: &buck_executable_snapshot,
        buck_version: &buck_version,
        lzma_input: &lzma_input,
        lzma_input_sha256: &lzma_input_hash,
        generated_buck: &generated_buck,
        generated_buck_sha256: &generated_buck_hash,
    };
    let receipt = publish_final_receipt(&receipt_context)?;
    println!("Buck shadow parity PASS; receipt={}", receipt.display());
    Ok(())
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("--reconcile-release-evidence") {
        return match run_release_evidence_reconciler(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs reconciliation: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--validate-release-candidate") {
        return match run_release_candidate_validator(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs release candidate validation: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--scan-and-package-public-evidence") {
        return match run_scan_and_package_public_evidence(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs combined public evidence package: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--scan-and-stage-public-evidence") {
        return match run_scan_and_stage_public_evidence(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs combined public evidence staging: {error}");
                ExitCode::from(2)
            }
        };
    }
    if arguments.first().map(String::as_str) == Some("--release-evidence-verdict") {
        return match run_release_evidence_verdict(&arguments[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("build-buck-release.rs release evidence verdict: {error}");
                ExitCode::from(2)
            }
        };
    }
    if env::var_os(MATRIX_PROXY_ENV).as_deref() == Some(OsStr::new("1")) {
        return match matrix_candidate_proxy(arguments.into_iter()) {
            Ok(status) => status,
            Err(error) => {
                eprintln!("build-buck-release.rs matrix proxy: {error}");
                ExitCode::from(2)
            }
        };
    }
    match parse_options(arguments) {
        Ok(None) => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Ok(Some(options)) => {
            let cgroups = match safe_ci_scope::resolve_cgroups(
                "Buck phase-1 shadow validation",
                false,
                Some(OUTER_SCOPE_DEADLINE_SECONDS),
                true,
            ) {
                Ok(cgroups) => cgroups,
                Err(code) => return ExitCode::from(code),
            };
            match run(options, cgroups) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("build-buck-release.rs: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Err(error) => {
            eprintln!("build-buck-release.rs: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use std::os::unix::net::UnixListener;
    use std::process::Stdio;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    fn fixture_root(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "build-buck-release-{label}-{}",
            unique_identity().unwrap()
        ))
    }

    fn gzip_member(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn source_root() -> PathBuf {
        Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf()
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn write_complete_install(root: &Path, reverie_sha: &str) {
        fs::create_dir_all(root.join("rsrcs/dynamorio/bin64")).unwrap();
        for (relative, executable) in REQUIRED_RESOURCES {
            let path = root.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            if executable {
                write_executable(&path, b"#!/bin/sh\nexit 0\n");
            } else {
                fs::write(&path, format!("fixture:{relative}\n")).unwrap();
            }
        }
        fs::write(
            root.join("rsrcs/sabre.revision"),
            format!("{reverie_sha}\n"),
        )
        .unwrap();
        fs::write(root.join("rsrcs/unprobed-resource"), b"generation-one\n").unwrap();
    }

    fn canonical_report(virtualize_time: bool) -> Value {
        let output = serde_json::json!({
            "exit_code": 0,
            "signal": null,
            "stdout_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stdout_bytes": 0,
            "stderr_sha256": "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "stderr_bytes": 0
        });
        serde_json::json!({
            "verified": true,
            "bitwise_parity": true,
            "verdict": "matched",
            "no_result_reason": null,
            "infrastructure_error": null,
            "comparison": {
                "strictness": "canonical",
                "display_name": "BitwiseInfoV1",
                "compare_logs": true,
                "compare_io_buffers": true,
                "log_scope": "info",
                "record_envelope": "all_records_v1",
                "virtualize_time": virtualize_time,
                "strip_lines": false,
                "canonicalize_addresses": true,
                "full_trace": true,
                "exact_remainder": true,
                "stripped_prefixes": ["real-wall-clock-prefix/v1"],
                "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines": false,
                "skip_commit": false,
                "skip_detlog": false
            },
            "compared_log_messages": {"left": 12, "right": 12},
            "compared_outputs": {"left": output, "right": output},
            "guest_exit_code": 0,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        })
    }

    fn write_report(root: &Path, name: &str, value: &Value) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
        path
    }

    fn build_info_json() -> String {
        serde_json::json!({
            "schema": BuildInfo::SCHEMA,
            "version": "0.2.0",
            "build_date": "2026-09-22",
            "git_sha": "0123456789ab",
            "features": {"dbt": true, "e9patch": true, "sabre": true}
        })
        .to_string()
    }

    fn host_capabilities_json(cpuid_evidence: &str) -> String {
        serde_json::json!({
            "schema": HostCapabilitiesReport::SCHEMA,
            "host_capabilities": {
                "cpuid-faulting": {"present": true, "evidence": cpuid_evidence},
                "kvm": {"present": false, "evidence": "open /dev/kvm = ENOENT"}
            }
        })
        .to_string()
    }

    fn fixture_receipt_facts(identity: &str) -> FinalReceiptFacts {
        let hash = "a".repeat(64);
        FinalReceiptFacts {
            result: "pass".to_owned(),
            evidence_identity: identity.to_owned(),
            process_tree_supervisor: "dagrun-cgroup-v2".to_owned(),
            matrix_step_wall_seconds: MATRIX_STEP_DEADLINE_SECONDS,
            matrix_outer_wall_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS,
            matrix_cleanup_evidence_grace_seconds: MATRIX_SUPERVISOR_DEADLINE_SECONDS
                - MATRIX_STEP_DEADLINE_SECONDS,
            process_tree_cleanup: "complete".to_owned(),
            hermit_full_sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            hermit_build_sha: "0123456789ab".to_owned(),
            reverie_sha: "123456789abcdef0123456789abcdef012345678".to_owned(),
            version: "0.2.0".to_owned(),
            build_date: "2026-09-22".to_owned(),
            dotslash_version: DOTSLASH_VERSION.to_owned(),
            dotslash_sha256: hash.clone(),
            buck_version: "buck2 fixture".to_owned(),
            buck_descriptor_sha256: hash.clone(),
            buck_executable_sha256: hash.clone(),
            lzma_input: "/fixture/liblzma.so".to_owned(),
            lzma_input_sha256: hash.clone(),
            generated_buck_sha256: hash.clone(),
            safehermit_sha256: hash.clone(),
            bounded_run_space_sha256: hash.clone(),
            cargo_candidate_sha256: hash.clone(),
            cargo_hermit_manifest_sha256: hash.clone(),
            cargo_resources_manifest_sha256: hash.clone(),
            buck_candidate_sha256: hash.clone(),
            buck_hermit_manifest_sha256: hash.clone(),
            buck_resources_manifest_sha256: hash,
            event_log: "buck-build.json-lines.gz".to_owned(),
            ptrace_verify_messages_left: 12,
            ptrace_verify_messages_right: 12,
            ptrace_record_messages_left: 9,
            ptrace_record_messages_right: 9,
            cargo_matrix_rows: 3,
            buck_matrix_rows: 3,
            matrix_candidate_manifest_sha256: "c".repeat(64),
            matrix_candidate_manifest: "matrix-candidate-invocations.json".to_owned(),
            matrix_expected_ledger: "matrix-expected-ledger.json".to_owned(),
            matrix_expected_ledger_sha256: "d".repeat(64),
            matrix_expected_invocation_count: 2,
            cargo_matrix_candidate_invocations: 2,
            buck_matrix_candidate_invocations: 2,
        }
    }

    fn fixture_resources() -> BTreeMap<String, String> {
        BTreeMap::from([("rsrcs/fixture".to_owned(), "b".repeat(64))])
    }

    fn write_retained_candidate_fixture(root: &Path) -> BuildInfo {
        let version = build_info_json();
        let help = "ptrace dbt liteinst sabre kvm e9patch\n";
        let host = host_capabilities_json("arch prctl = ok");
        for (name, contents) in [
            ("cargo-version.json", version.as_str()),
            ("buck-version.json", version.as_str()),
            ("cargo-help.txt", help),
            ("buck-help.txt", help),
            ("cargo-host-capabilities.json", host.as_str()),
            ("buck-host-capabilities.json", host.as_str()),
        ] {
            fs::write(root.join(name), contents).unwrap();
        }
        let run = canonical_report(true);
        let record = canonical_report(false);
        for name in ["cargo-ptrace-verify.json", "buck-ptrace-verify.json"] {
            write_report(root, name, &run);
        }
        for name in [
            "cargo-ptrace-record-verify.json",
            "buck-ptrace-record-verify.json",
        ] {
            write_report(root, name, &record);
        }
        decode_build_info(&version, "0.2.0", "0123456789ab", "fixture build info").unwrap()
    }

    fn write_fixture_receipt(
        root: &Path,
        facts: &FinalReceiptFacts,
        resources: &BTreeMap<String, String>,
    ) -> PathBuf {
        let receipt = root.join("shadow-parity.json");
        let mut rows = Vec::new();
        collect_evidence_hashes(root, root, &receipt, &mut rows).unwrap();
        let artifacts = rows.into_iter().collect::<BTreeMap<_, _>>();
        atomic_write_new(
            &receipt,
            &render_final_receipt(facts, resources, &artifacts).unwrap(),
        )
        .unwrap();
        receipt
    }

    fn verify_fixture_receipt(
        root: &Path,
        receipt: &Path,
        facts: &FinalReceiptFacts,
        resources: &BTreeMap<String, String>,
    ) -> Result<(), String> {
        let parsed = parse_final_receipt(
            &fs::read_to_string(receipt)
                .map_err(|error| format!("fixture receipt is unreadable: {error}"))?,
        )?;
        let mut rows = Vec::new();
        collect_evidence_hashes(root, root, receipt, &mut rows)?;
        let artifacts = rows.into_iter().collect::<BTreeMap<_, _>>();
        verify_receipt_semantics(&parsed, facts, resources, &artifacts)
    }

    #[test]
    fn both_help_forms_are_accepted() {
        assert_eq!(parse_options(["-h".to_owned()]).unwrap(), None);
        assert_eq!(parse_options(["--help".to_owned()]).unwrap(), None);
    }

    #[test]
    fn missing_required_inputs_are_refused() {
        let error = parse_options(Vec::<String>::new()).unwrap_err();
        assert!(error.contains("--dotslash"));
    }

    #[test]
    fn authoritative_output_override_is_not_a_supported_surface() {
        let error =
            parse_options(["--output-root".to_owned(), "target/ci".to_owned()]).unwrap_err();
        assert!(error.contains("unknown argument"));
    }

    #[test]
    fn parses_only_the_package_version() {
        let manifest = "[workspace.package]\nversion = \"9\"\n[package]\nname = \"hermit\"\nversion = \"0.2.0-test+1\"\n";
        assert_eq!(package_version(manifest).unwrap(), "0.2.0-test+1");
        assert!(package_version("[package]\nversion = \"unknown\"\n").is_err());
    }

    #[test]
    fn build_info_uses_closed_producer_schema_and_exact_semantics() {
        let good = build_info_json();
        decode_build_info(&good, "0.2.0", "0123456789ab", "fixture").unwrap();
        for (name, value) in [
            ("malformed", "{".to_owned()),
            ("unknown-root", good.replacen("{", r#"{"future":true,"#, 1)),
            (
                "unknown-feature",
                good.replacen(r#""dbt":true"#, r#""dbt":true,"future":true"#, 1),
            ),
            (
                "duplicate-root",
                good.replacen(r#""schema":1"#, r#""schema":1,"schema":1"#, 1),
            ),
            (
                "duplicate-feature",
                good.replacen(r#""dbt":true"#, r#""dbt":true,"dbt":true"#, 1),
            ),
            (
                "wrong-schema",
                good.replacen(r#""schema":1"#, r#""schema":2"#, 1),
            ),
            ("internal-whitespace", good.replacen("0.2.0", "0.2. 0", 1)),
            ("bad-date", good.replacen("2026-09-22", "2026-02-30", 1)),
            ("bad-sha", good.replacen("0123456789ab", "0123456789AB", 1)),
            (
                "feature-disabled",
                good.replacen(r#""sabre":true"#, r#""sabre":false"#, 1),
            ),
        ] {
            assert!(
                decode_build_info(&value, "0.2.0", "0123456789ab", "fixture").is_err(),
                "accepted {name}: {value}"
            );
        }
    }

    #[test]
    fn host_capabilities_use_closed_typed_schema_and_preserve_string_whitespace() {
        let spaced = host_capabilities_json("arch prctl = ok");
        let compact = host_capabilities_json("archprctl=ok");
        let spaced_report = decode_host_capabilities(&spaced, "spaced").unwrap();
        let compact_report = decode_host_capabilities(&compact, "compact").unwrap();
        assert_ne!(spaced_report, compact_report);

        for (name, value) in [
            ("malformed", "{".to_owned()),
            (
                "unknown-root",
                spaced.replacen("{", r#"{"future":true,"#, 1),
            ),
            (
                "unknown-verdict",
                spaced.replacen(
                    r#""present":true"#,
                    r#""present":true,"future":true"#,
                    1,
                ),
            ),
            (
                "duplicate-capability",
                spaced.replacen(
                    r#""kvm":{"evidence":"open /dev/kvm = ENOENT","present":false}"#,
                    r#""kvm":{"evidence":"open /dev/kvm = ENOENT","present":false},"kvm":{"evidence":"duplicate","present":true}"#,
                    1,
                ),
            ),
            (
                "empty-evidence",
                host_capabilities_json("   "),
            ),
            (
                "wrong-schema",
                spaced.replacen(r#""schema":1"#, r#""schema":2"#, 1),
            ),
        ] {
            assert!(
                decode_host_capabilities(&value, "fixture").is_err(),
                "accepted {name}: {value}"
            );
        }
    }

    #[test]
    fn shadow_dbt_envelope_is_derived_from_exact_official_node() {
        let root = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let generated = hermit_manifest_plan::validation_dag::generate(&root).unwrap();
        let official = generated
            .steps
            .into_iter()
            .find(|step| step.group == "test" && step.job == "dbt_parity")
            .unwrap();
        require_official_dbt_matrix_envelope(&official).unwrap();
        let derived = official_dbt_matrix_envelope(&root).unwrap();
        assert_eq!(derived.hint.rss_baseline_bytes, Some(512 * 1024 * 1024));
        assert_eq!(
            derived.hint.hard_mem_max_bytes,
            Some(2_i64 * 1024 * 1024 * 1024)
        );
        assert_eq!(derived.hint.classification, StepClass::LatencyBound);
        assert_eq!(derived.hint.preferred_inner_jobs, None);
        assert!(matches!(derived.cmdtype, CmdType::Unknown));
        assert_eq!(derived.timeout, 900);
        assert_eq!(derived.cpu_timeout, 7_200);
        assert_eq!(derived.jobs_flag, None);
        assert_eq!(derived.jobs_env, None);
        let predicate_config = DagConfig {
            steps: vec![official.clone()],
            ..Default::default()
        };
        assert!(
            steps_violating_run_timeout(&predicate_config, MATRIX_SUPERVISOR_DEADLINE_SECONDS)
                .is_empty()
        );
        assert_eq!(
            steps_violating_run_timeout(&predicate_config, MATRIX_STEP_DEADLINE_SECONDS),
            vec![("test.dbt_parity".to_owned(), MATRIX_STEP_DEADLINE_SECONDS)]
        );
        assert_eq!(
            steps_violating_run_timeout(&predicate_config, MATRIX_STEP_DEADLINE_SECONDS - 1),
            vec![("test.dbt_parity".to_owned(), MATRIX_STEP_DEADLINE_SECONDS)]
        );

        let marker_root = fixture_root("matrix-deadline-refusal");
        fs::create_dir(&marker_root).unwrap();
        let marker = marker_root.join("launched");
        let mut refused_step = official.clone();
        refused_step.deps.clear();
        refused_step.cmd = format!("touch {}", shell_words::quote(&marker.to_string_lossy()));
        let refused_config = DagConfig {
            steps: vec![refused_step],
            ..Default::default()
        };
        let refused = run_dag_boxed_deadline(
            &refused_config,
            1,
            false,
            0,
            None,
            None,
            Some(1),
            Some(MATRIX_STEP_DEADLINE_SECONDS),
        );
        assert!(!refused.ok);
        assert!(!refused.run_timed_out);
        assert!(refused.outcomes.is_empty());
        assert!(
            !marker.exists(),
            "equal outer deadline launched the matrix step"
        );
        fs::remove_dir_all(marker_root).unwrap();

        let mut widened = official;
        widened.hint.hard_mem_max_bytes = Some(16_i64 * 1024 * 1024 * 1024);
        assert!(require_official_dbt_matrix_envelope(&widened).is_err());
    }

    #[test]
    fn stale_generated_graph_is_refused() {
        assert!(verify_generated_buck("# stale\n").is_err());
    }

    #[test]
    fn unreadable_event_log_shape_is_refused() {
        let path = env::temp_dir().join(format!("buck-log-refusal-{}", std::process::id()));
        fs::write(&path, b"not gzip").unwrap();
        assert!(validate_log_header(&path).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn release_evidence_reconciler_uses_stdout_output_and_treats_command_flag_as_advisory() {
        let root = fixture_root("release-reconciliation");
        fs::create_dir(&root).unwrap();
        let binary = root
            .join("buck-out/v2/gen/root/0123456789abcdef/hermit-cli/___hermit-release__/hermit");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::copy("/bin/true", &binary).unwrap();
        let stdout = root.join("show-output.txt");
        fs::write(
            &stdout,
            format!("hermit//hermit-cli:hermit-release {}\n", binary.display()),
        )
        .unwrap();
        let write_event_log = |path: &Path, events: &[Value]| {
            let text = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
            let mut gzip = Command::new("gzip")
                .arg("-c")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            gzip.stdin
                .as_mut()
                .unwrap()
                .write_all(text.as_bytes())
                .unwrap();
            let gzip_output = gzip.wait_with_output().unwrap();
            assert!(gzip_output.status.success());
            fs::write(path, gzip_output.stdout).unwrap();
        };
        let events = [
            serde_json::json!({"Event":{"data":{"SpanEnd":{"data":{"Command":{"is_success":false,"build_result":{"build_completed":true}}}}}}}),
            serde_json::json!({"Result":{"result":{"build_response":{"errors":[],"build_targets":[{"target":"hermit//hermit-cli:hermit-release","outputs":[]}]}}}}),
        ];
        let event_log = root.join("event.json-lines.gz");
        write_event_log(&event_log, &events);
        let receipt = root.join("reconciliation.tsv");
        reconcile_release_build_evidence(&event_log, &stdout, 0, &root, &receipt).unwrap();
        let text = fs::read_to_string(&receipt).unwrap();
        assert!(text.contains("command_end_is_success\tfalse\n"));
        assert!(text.contains("command_end_status\tadvisory-known-inconsistent\n"));
        assert!(text.contains(&sha256(&binary).unwrap()));

        let elf_bytes = fs::read(&binary).unwrap();
        fs::write(&binary, b"not an ELF binary\n").unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &stdout,
                0,
                &root,
                &root.join("non-elf.tsv"),
            )
            .is_err()
        );
        fs::write(&binary, elf_bytes).unwrap();

        let duplicate_stdout = root.join("duplicate-output.txt");
        fs::write(
            &duplicate_stdout,
            format!(
                "hermit//hermit-cli:hermit-release {}\nhermit//hermit-cli:hermit-release {}\n",
                binary.display(),
                binary.display()
            ),
        )
        .unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &duplicate_stdout,
                0,
                &root,
                &root.join("duplicate.tsv"),
            )
            .is_err()
        );
        let decoy = root.join("buck-out/decoy/hermit");
        fs::create_dir_all(decoy.parent().unwrap()).unwrap();
        fs::copy("/bin/true", &decoy).unwrap();
        let decoy_stdout = root.join("decoy-output.txt");
        fs::write(
            &decoy_stdout,
            format!("{RELEASE_SHOW_OUTPUT_TARGET} {}\n", decoy.display()),
        )
        .unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &decoy_stdout,
                0,
                &root,
                &root.join("decoy.tsv"),
            )
            .is_err()
        );
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &stdout,
                3,
                &root,
                &root.join("shell-failure.tsv"),
            )
            .is_err()
        );
        let outside_stdout = root.join("outside-output.txt");
        fs::write(
            &outside_stdout,
            "hermit//hermit-cli:hermit-release /bin/true\n",
        )
        .unwrap();
        assert!(
            reconcile_release_build_evidence(
                &event_log,
                &outside_stdout,
                0,
                &root,
                &root.join("outside.tsv"),
            )
            .is_err()
        );
        let extra_target_log = root.join("extra-target.json-lines.gz");
        let extra_target_events = [
            events[0].clone(),
            serde_json::json!({"Result":{"result":{"build_response":{"errors":[],"build_targets":[{"target":"hermit//hermit-cli:hermit-release","outputs":[]},{"target":"hermit//other:unexpected","outputs":[]}]}}}}),
        ];
        write_event_log(&extra_target_log, &extra_target_events);
        assert!(
            reconcile_release_build_evidence(
                &extra_target_log,
                &stdout,
                0,
                &root,
                &root.join("extra-target.tsv"),
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_candidate_version_probe_refuses_expected_layout_elf_decoy() {
        let root = fixture_root("release-candidate-version");
        fs::create_dir_all(root.join("hermit-cli")).unwrap();
        fs::create_dir_all(root.join("reverie")).unwrap();
        fs::write(
            root.join("hermit-cli/Cargo.toml"),
            "[package]\nname = \"hermit\"\nversion = \"0.2.0\"\n",
        )
        .unwrap();
        fs::write(root.join("reverie/pin"), b"fixture\n").unwrap();
        checked_output(
            Command::new("git").current_dir(&root).args(["init", "-q"]),
            "git init",
        )
        .unwrap();
        checked_output(
            Command::new("git").current_dir(&root).args(["add", "."]),
            "git add fixture",
        )
        .unwrap();
        checked_output(
            Command::new("git").current_dir(&root).args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ]),
            "git commit fixture",
        )
        .unwrap();
        let hermit_sha = git(&root, &["rev-parse", "--short=12", "HEAD"]).unwrap();
        let reverie_sha = git(&root, &["rev-parse", "HEAD:reverie"]).unwrap();
        let candidate =
            root.join("buck-out/v2/gen/root/config-hash/hermit-cli/___hermit-release__/hermit");
        fs::create_dir_all(candidate.parent().unwrap()).unwrap();
        let source = root.join("candidate.c");
        fs::write(
            &source,
            format!(
                "#include <stdio.h>\nint main(void) {{ puts(\"{{\\\"schema\\\":1,\\\"version\\\":\\\"0.2.0\\\",\\\"build_date\\\":\\\"2026-09-23\\\",\\\"git_sha\\\":\\\"{hermit_sha}\\\",\\\"features\\\":{{\\\"dbt\\\":true,\\\"e9patch\\\":true,\\\"sabre\\\":true}}}}\"); return 0; }}\n"
            ),
        )
        .unwrap();
        checked_output(
            Command::new("gcc").args([source.as_os_str(), OsStr::new("-o"), candidate.as_os_str()]),
            "compile release candidate fixture",
        )
        .unwrap();
        let show_output = root.join("build.stdout");
        fs::write(
            &show_output,
            format!("{RELEASE_SHOW_OUTPUT_TARGET} {}\n", candidate.display()),
        )
        .unwrap();
        let metadata = root.join("metadata.tsv");
        fs::write(
            &metadata,
            format!(
                "version\t0.2.0\nbuild_date\t2026-09-23\nhermit_sha\t{hermit_sha}\nreverie_sha\t{reverie_sha}\n"
            ),
        )
        .unwrap();
        validate_release_candidate_version(
            &show_output,
            &metadata,
            &root,
            &root.join("candidate-version.json"),
            &root.join("candidate-validation.tsv"),
        )
        .unwrap();
        fs::copy("/bin/true", &candidate).unwrap();
        assert!(
            validate_release_candidate_version(
                &show_output,
                &metadata,
                &root,
                &root.join("decoy-version.json"),
                &root.join("decoy-validation.tsv"),
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn combined_scan_package_is_deterministic_and_refuses_coherent_source_swap() {
        let fixture = fixture_root("combined-scan-package");
        fs::create_dir(&fixture).unwrap();
        let gzip_bytes = {
            let mut gzip = Command::new("gzip")
                .args(["-n", "-c"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            gzip.stdin
                .take()
                .unwrap()
                .write_all(b"{\"public\":true}\n")
                .unwrap();
            let output = gzip.wait_with_output().unwrap();
            assert!(output.status.success());
            output.stdout
        };
        let mut archives = Vec::new();
        for suffix in ["one", "two"] {
            let root = fixture.join(format!("evidence-{suffix}"));
            fs::create_dir(&root).unwrap();
            fs::write(root.join("plain.txt"), b"benign public evidence\n").unwrap();
            fs::write(root.join("event.json-lines.gz"), &gzip_bytes).unwrap();
            let archive = fixture.join(format!("evidence-{suffix}.tar.gz"));
            let receipt = fixture.join(format!("evidence-{suffix}-scan.tsv"));
            scan_and_package_public_evidence(&root, &archive, &receipt).unwrap();
            assert_eq!(
                fs::metadata(&archive).unwrap().permissions().mode() & 0o222,
                0
            );
            assert!(
                fs::read_to_string(&receipt)
                    .unwrap()
                    .contains("known_credential_marker_lines\t0\n")
            );
            archives.push(fs::read(&archive).unwrap());
        }
        assert_eq!(archives[0], archives[1]);
        let mut archived_names = Archive::new(GzDecoder::new(Cursor::new(&archives[0])))
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        archived_names.sort();
        assert_eq!(
            archived_names,
            [
                "SHA256SUMS",
                "event.json-lines.gz",
                "plain.txt",
                "public-evidence-scan.tsv",
            ]
        );

        let root = fixture.join("mutated-evidence");
        fs::create_dir(&root).unwrap();
        let source = root.join("plain.txt");
        fs::write(&source, b"benign public evidence\n").unwrap();
        let archive = fixture.join("mutated.tar.gz");
        let receipt = fixture.join("mutated-scan.tsv");
        let result = scan_and_package_public_evidence_with_hook(
            &root,
            &archive,
            &receipt,
            |captured_root| {
                let original =
                    fs::read(captured_root.join("plain.txt")).map_err(|error| error.to_string())?;
                fs::write(
                    captured_root.join("plain.txt"),
                    b"Authorization: Bearer swapped-secret\n",
                )
                .map_err(|error| error.to_string())?;
                fs::write(
                    captured_root.join("public-evidence-scan.tsv"),
                    b"known_credential_marker_lines\t0\n",
                )
                .map_err(|error| error.to_string())?;
                fs::write(
                    captured_root.join("SHA256SUMS"),
                    b"coherently-rewritten-old-manifest\n",
                )
                .map_err(|error| error.to_string())?;
                fs::write(captured_root.join("plain.txt"), original)
                    .map_err(|error| error.to_string())?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!archive.exists());
        assert!(!receipt.exists());

        let poisoned = fixture.join("poisoned-evidence");
        fs::create_dir(&poisoned).unwrap();
        fs::write(
            poisoned.join("plain.txt"),
            b"Authorization: Bearer original-secret\n",
        )
        .unwrap();
        assert!(
            scan_and_package_public_evidence(
                &poisoned,
                &fixture.join("poisoned.tar.gz"),
                &fixture.join("poisoned-scan.tsv"),
            )
            .is_err()
        );

        let unsafe_names = fixture.join("unsafe-names");
        fs::create_dir(&unsafe_names).unwrap();
        fs::write(
            unsafe_names.join(std::ffi::OsString::from_vec(vec![b'b', 0xff])),
            b"benign bytes\n",
        )
        .unwrap();
        assert!(
            scan_and_package_public_evidence(
                &unsafe_names,
                &fixture.join("unsafe.tar.gz"),
                &fixture.join("unsafe-scan.tsv"),
            )
            .is_err()
        );

        let partial_root = fixture.join("partial-publication-evidence");
        fs::create_dir(&partial_root).unwrap();
        fs::write(partial_root.join("plain.txt"), b"benign\n").unwrap();
        let partial_archive = fixture.join("partial.tar.gz");
        let stale_receipt = fixture.join("stale-receipt.tsv");
        fs::write(&stale_receipt, b"stale\n").unwrap();
        assert!(
            scan_and_package_public_evidence(&partial_root, &partial_archive, &stale_receipt)
                .is_err()
        );

        let aliased_parent = fixture.join("output-alias");
        symlink(&fixture, &aliased_parent).unwrap();
        assert!(
            scan_and_package_public_evidence(
                &partial_root,
                &aliased_parent.join("aliased.tar.gz"),
                &aliased_parent.join("aliased-scan.tsv"),
            )
            .is_err()
        );
        assert!(!partial_archive.exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn gzip_evidence_decode_is_bounded_and_scans_large_multimember_payloads() {
        let fixture = fixture_root("bounded-gzip-evidence");
        fs::create_dir(&fixture).unwrap();

        let large = vec![b'x'; 4 * 1024 * 1024 + 17];
        let split = large.len() / 2;
        let mut multimember = gzip_member(&large[..split]);
        multimember.extend(gzip_member(&large[split..]));
        assert_eq!(decode_gzip_bytes(&multimember).unwrap(), large);

        let safe_root = fixture.join("safe-large");
        fs::create_dir(&safe_root).unwrap();
        fs::write(safe_root.join("event.json-lines.gz"), &multimember).unwrap();
        scan_and_package_public_evidence(
            &safe_root,
            &fixture.join("safe-large.tar.gz"),
            &fixture.join("safe-large-scan.tsv"),
        )
        .unwrap();

        let mut late_marker = vec![b'x'; 4 * 1024 * 1024];
        late_marker.extend(b"\nAuthorization: Bearer late-secret\n");
        let poisoned_root = fixture.join("late-marker");
        fs::create_dir(&poisoned_root).unwrap();
        fs::write(
            poisoned_root.join("event.json-lines.gz"),
            gzip_member(&late_marker),
        )
        .unwrap();
        let poisoned_archive = fixture.join("late-marker.tar.gz");
        let poisoned_receipt = fixture.join("late-marker-scan.tsv");
        assert!(
            scan_and_package_public_evidence(&poisoned_root, &poisoned_archive, &poisoned_receipt,)
                .is_err()
        );
        assert!(!poisoned_archive.exists());
        assert!(!poisoned_receipt.exists());

        let valid = gzip_member(b"public evidence\n");
        let mut corrupted = valid.clone();
        *corrupted.last_mut().unwrap() ^= 0xff;
        assert!(decode_gzip_bytes(&corrupted).is_err());
        let mut truncated = valid;
        truncated.truncate(truncated.len() - 3);
        assert!(decode_gzip_bytes(&truncated).is_err());

        let compressed_bomb = gzip_member(&vec![0; 256 * 1024]);
        let error = decode_gzip_bytes_with_limit(&compressed_bomb, 64 * 1024).unwrap_err();
        assert!(error.contains("decode limit"), "unexpected error: {error}");

        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn combined_scan_stage_preserves_legacy_payload_and_refuses_source_swap() {
        let fixture = fixture_root("combined-scan-stage");
        let evidence = fixture.join("evidence");
        fs::create_dir_all(&evidence).unwrap();
        let source = evidence.join("buck2-third-party.log");
        fs::write(&source, b"legacy diagnostic log\n").unwrap();
        let staging = fixture.join("staging");
        scan_and_stage_public_evidence(&source, &staging, "buck2-third-party.log").unwrap();
        let mut population = fs::read_dir(&staging)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        population.sort();
        assert_eq!(population, ["SHA256SUMS", "buck2-third-party.log"]);
        assert_eq!(
            fs::read(staging.join("buck2-third-party.log")).unwrap(),
            b"legacy diagnostic log\n"
        );
        assert_eq!(
            fs::read_to_string(staging.join("SHA256SUMS")).unwrap(),
            format!("{}  buck2-third-party.log\n", sha256(&source).unwrap())
        );
        assert_eq!(
            fs::metadata(&staging).unwrap().permissions().mode() & 0o222,
            0
        );

        let mut staging_permissions = fs::metadata(&staging).unwrap().permissions();
        staging_permissions.set_mode(0o755);
        fs::set_permissions(&staging, staging_permissions).unwrap();
        fs::remove_dir_all(&staging).unwrap();
        let result = scan_and_stage_public_evidence_with_hook(
            &source,
            &staging,
            "buck2-third-party.log",
            |source| {
                let original = fs::read(source).map_err(|error| error.to_string())?;
                fs::write(source, b"client_secret=swapped-secret\n")
                    .map_err(|error| error.to_string())?;
                let parent = source.parent().unwrap();
                fs::write(parent.join("public-evidence-scan.tsv"), b"old receipt\n")
                    .map_err(|error| error.to_string())?;
                fs::write(parent.join("SHA256SUMS"), b"old coherent manifest\n")
                    .map_err(|error| error.to_string())?;
                fs::write(source, original).map_err(|error| error.to_string())?;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!staging.exists());
        assert!(
            scan_and_stage_public_evidence(&source, &fixture.join("relative-reject"), "../log")
                .is_err()
        );
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn combined_package_retains_non_green_evidence_before_verdict() {
        let fixture = fixture_root("combined-non-green-release");
        let evidence = fixture.join("evidence");
        fs::create_dir_all(&evidence).unwrap();
        fs::write(evidence.join("shell-exit.tsv"), b"shell_exit\t124\n").unwrap();
        let archive = fixture.join("release.tar.gz");
        let receipt = fixture.join("release-scan.tsv");
        scan_and_package_public_evidence(&evidence, &archive, &receipt).unwrap();
        assert!(archive.is_file());
        assert!(release_evidence_verdict(1, 2, 124).is_err());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn mutation_during_real_publisher_copy_is_refused() {
        let fixture = fixture_root("publisher-race");
        let install = fixture.join("caller-install");
        let binary = fixture.join("candidate");
        let artifacts = fixture.join("artifacts");
        let pointer = fixture.join("artifact.path");
        let fake_bin = fixture.join("fake-bin");
        let cp_entered = fixture.join("cp-entered");
        let cp_release = fixture.join("cp-release");
        fs::create_dir_all(&fixture).unwrap();
        fs::create_dir(&fake_bin).unwrap();
        write_complete_install(&install, &"1".repeat(40));
        write_executable(&binary, b"#!/bin/sh\nexit 0\n");
        write_executable(
            &fake_bin.join("cp"),
            b"#!/usr/bin/env bash\nset -euo pipefail\n: >\"${CP_ENTERED:?}\"\nwhile [[ ! -e ${CP_RELEASE:?} ]]; do sleep 0.01; done\nexec /usr/bin/cp \"$@\"\n",
        );

        let mut child = Command::new(source_root().join("ci/publish-hermit-e2e-artifact.sh"))
            .args([&binary, &artifacts, &pointer, &install])
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    fake_bin.display(),
                    env::var("PATH").unwrap_or_default()
                ),
            )
            .env("CP_ENTERED", &cp_entered)
            .env("CP_RELEASE", &cp_release)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if cp_entered.is_file() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "publisher never reached cp barrier"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "publisher exited before cp barrier"
            );
            thread::sleep(Duration::from_millis(1));
        }
        fs::write(
            install.join("rsrcs/unprobed-resource"),
            b"mutated-during-copy\n",
        )
        .unwrap();
        fs::write(&cp_release, b"release\n").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            !output.status.success(),
            "publisher accepted a source mutation during its copy window"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("changed during publication"), "{stderr}");
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn symlink_special_and_stale_inputs_are_refused() {
        let fixture = fixture_root("unsafe-inputs");
        fs::create_dir(&fixture).unwrap();
        let regular = fixture.join("regular");
        write_executable(&regular, b"#!/bin/sh\nexit 0\n");
        let linked = fixture.join("linked");
        symlink(&regular, &linked).unwrap();
        assert!(require_absolute_file(&linked, "fixture symlink").is_err());
        let socket = fixture.join("socket");
        let listener = UnixListener::bind(&socket).unwrap();
        assert!(require_absolute_file(&socket, "fixture socket").is_err());
        drop(listener);
        assert!(create_exclusive_directory(&fixture, "stale fixture").is_err());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn safehermit_bundle_snapshot_survives_poisoned_caller_path_absence() {
        let fixture = fixture_root("safehermit-snapshot");
        let caller_root = fixture.join("caller-tools");
        let evidence = fixture.join("evidence");
        fs::create_dir_all(caller_root.join("bin")).unwrap();
        fs::create_dir_all(caller_root.join("scripts")).unwrap();
        fs::create_dir(&evidence).unwrap();
        write_executable(
            &caller_root.join("bin/safehermit"),
            b"#!/usr/bin/env bash\nroot=$(cd \"$(dirname \"${BASH_SOURCE[0]}\")/..\" && pwd)\ntest -x \"$root/scripts/bounded-run-space\"\nprintf 'snapshot-ok\\n'\n",
        );
        write_executable(
            &caller_root.join("scripts/bounded-run-space"),
            b"#!/usr/bin/env bash\nexit 0\n",
        );
        let snapshot =
            snapshot_safehermit_bundle(&caller_root.join("bin/safehermit"), &evidence).unwrap();
        fs::remove_dir_all(&caller_root).unwrap();
        assert_eq!(
            output_text(
                &mut Command::new(&snapshot.launcher),
                "snapshotted safehermit fixture"
            )
            .unwrap(),
            "snapshot-ok"
        );
        reverify_safehermit_bundle(&snapshot).unwrap();

        let mut facts = fixture_receipt_facts(evidence.file_name().unwrap().to_str().unwrap());
        facts.safehermit_sha256 = snapshot.launcher_sha256.clone();
        facts.bounded_run_space_sha256 = snapshot.bounded_run_space_sha256.clone();
        let resources = fixture_resources();
        let receipt = write_fixture_receipt(&evidence, &facts, &resources);
        verify_fixture_receipt(&evidence, &receipt, &facts, &resources).unwrap();
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn dotslash_snapshot_survives_poisoned_caller_path_absence() {
        let fixture = fixture_root("dotslash-snapshot");
        let evidence = fixture.join("evidence");
        let caller = fixture.join("caller-dotslash");
        fs::create_dir_all(&evidence).unwrap();
        write_executable(&caller, b"#!/bin/sh\nprintf 'DotSlash 0.5.9\\n'\n");
        let snapshot = snapshot_dotslash(&caller, &evidence).unwrap();
        fs::remove_file(&caller).unwrap();
        assert_eq!(
            output_text(
                Command::new(&snapshot.path).arg("--version"),
                "snapshotted DotSlash fixture",
            )
            .unwrap(),
            DOTSLASH_VERSION,
        );
        reverify_input_snapshot(&snapshot, "DotSlash fixture").unwrap();
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn retained_buck_report_version_help_and_host_mutations_are_refused() {
        let fixture = fixture_root("retained-candidate-mutation");
        fs::create_dir_all(&fixture).unwrap();
        let cargo_info = write_retained_candidate_fixture(&fixture);
        verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info).unwrap();

        let run_path = fixture.join("buck-ptrace-verify.json");
        let original_run = fs::read(&run_path).unwrap();
        let mut changed_run = canonical_report(true);
        changed_run["compared_log_messages"]["left"] = serde_json::json!(13);
        changed_run["compared_log_messages"]["right"] = serde_json::json!(13);
        fs::write(&run_path, serde_json::to_vec(&changed_run).unwrap()).unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&run_path, original_run).unwrap();

        let record_path = fixture.join("buck-ptrace-record-verify.json");
        let original_record = fs::read(&record_path).unwrap();
        let mut changed_record = canonical_report(false);
        changed_record["compared_log_messages"]["left"] = serde_json::json!(14);
        changed_record["compared_log_messages"]["right"] = serde_json::json!(14);
        fs::write(&record_path, serde_json::to_vec(&changed_record).unwrap()).unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&record_path, original_record).unwrap();

        let version_path = fixture.join("buck-version.json");
        let original_version = fs::read(&version_path).unwrap();
        let changed_version = build_info_json().replacen("\"dbt\":true", "\"dbt\":false", 1);
        assert_ne!(changed_version, build_info_json());
        fs::write(&version_path, changed_version).unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&version_path, original_version).unwrap();

        let help_path = fixture.join("buck-help.txt");
        let original_help = fs::read(&help_path).unwrap();
        fs::write(
            &help_path,
            b"ptrace dbt liteinst sabre kvm e9patch changed\n",
        )
        .unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::write(&help_path, original_help).unwrap();

        let host_path = fixture.join("buck-host-capabilities.json");
        fs::write(
            &host_path,
            host_capabilities_json("changed exact evidence bytes"),
        )
        .unwrap();
        assert!(
            verify_retained_candidate_evidence(&fixture, "0.2.0", "0123456789ab", &cargo_info)
                .is_err()
        );
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn incomplete_install_bundle_is_refused() {
        let root = fixture_root("incomplete-install-bundle");
        fs::create_dir(&root).unwrap();
        let reverie_sha = "1".repeat(40);

        // Preserve the original regression: an empty authoritative install
        // bundle must never be accepted as a complete backend resource set.
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());

        write_complete_install(&root, &reverie_sha);
        verify_install_bundle(&root, &reverie_sha).unwrap();

        let missing = root.join("rsrcs/libdetcore_dbt.so");
        fs::remove_file(&missing).unwrap();
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());
        write_complete_install(&root, &reverie_sha);

        let executable = root.join("rsrcs/sabre");
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&executable, permissions).unwrap();
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());
        write_complete_install(&root, &reverie_sha);

        fs::write(
            root.join("rsrcs/sabre.revision"),
            format!("{}\n", "2".repeat(40)),
        )
        .unwrap();
        assert!(verify_install_bundle(&root, &reverie_sha).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parity_mismatch_is_refused() {
        assert!(require_equal("fixture", "cargo", "buck").is_err());
    }

    #[test]
    fn malformed_stripped_or_no_result_reports_are_refused() {
        let root = env::temp_dir().join(format!("buck-report-refusal-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (name, report) in [
            ("malformed", "not-json"),
            (
                "no-result",
                r#"{"verified":false,"bitwise_parity":false,"verdict":"no_result"}"#,
            ),
            (
                "stripped",
                r#"{"verified":true,"bitwise_parity":true,"verdict":"matched","comparison":{"strictness":"stripped"}}"#,
            ),
        ] {
            let path = root.join(name);
            fs::write(&path, report).unwrap();
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn typed_report_accepts_only_exact_current_policy() {
        let root = fixture_root("typed-report");
        fs::create_dir(&root).unwrap();
        let good = write_report(&root, "good", &canonical_report(true));
        verify_report(&good, true).unwrap();
        for (name, mutate) in [
            ("unknown-root", ("unknown", serde_json::json!(true))),
            (
                "misplaced-token",
                ("comparison", serde_json::json!("BitwiseInfoV1")),
            ),
        ] {
            let mut report = canonical_report(true);
            report[mutate.0] = mutate.1;
            let path = write_report(&root, name, &report);
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        for (name, field, value) in [
            ("wrong-mode", "strictness", serde_json::json!("stripped")),
            (
                "wrong-policy",
                "display_name",
                serde_json::json!("Stripped"),
            ),
            (
                "wrong-envelope",
                "record_envelope",
                serde_json::json!("caller_defined"),
            ),
        ] {
            let mut report = canonical_report(true);
            report["comparison"][field] = value;
            let path = write_report(&root, name, &report);
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        for (name, left, right) in [("zero", 0, 0), ("unequal", 12, 13)] {
            let mut report = canonical_report(true);
            report["compared_log_messages"] = serde_json::json!({"left":left,"right":right});
            let path = write_report(&root, name, &report);
            assert!(verify_report(&path, true).is_err(), "accepted {name}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn magic_tokens_in_nested_strings_and_duplicates_cannot_forge_a_report() {
        let root = fixture_root("magic-token-report");
        fs::create_dir(&root).unwrap();
        let magic = r#"{\"verified\":true,\"bitwise_parity\":true,\"verdict\":\"matched\",\"strictness\":\"canonical\",\"display_name\":\"BitwiseInfoV1\",\"compare_logs\":true,\"compare_io_buffers\":true,\"log_scope\":\"info\",\"record_envelope\":\"all_records_v1\",\"full_trace\":true,\"exact_remainder\":true,\"canonicalize_addresses\":true,\"strip_lines\":false,\"ignore_lines\":false,\"skip_commit\":false,\"skip_detlog\":false,\"guest_exit_code\":0,\"compared_log_messages\":{\"left\":9,\"right\":9}}"#;
        let nested = serde_json::json!({"message": magic, "nested": {"all_tokens": magic}});
        let nested_path = write_report(&root, "nested", &nested);
        assert!(verify_report(&nested_path, true).is_err());

        let raw = serde_json::to_string(&canonical_report(true)).unwrap();
        let duplicate = raw.replacen(
            r#""verified":true"#,
            r#""verified":true,"verified":true"#,
            1,
        );
        assert_ne!(raw, duplicate);
        let duplicate_path = root.join("duplicate");
        fs::write(&duplicate_path, duplicate).unwrap();
        assert!(verify_report(&duplicate_path, true).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nonignored_untracked_files_dirty_provenance() {
        let root = fixture_root("git-untracked");
        fs::create_dir(&root).unwrap();
        checked_output(
            Command::new("git").current_dir(&root).args(["init", "-q"]),
            "git init",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "ignored\n").unwrap();
        checked_output(
            Command::new("git")
                .current_dir(&root)
                .args(["add", ".gitignore"]),
            "git add",
        )
        .unwrap();
        checked_output(
            Command::new("git").current_dir(&root).args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ]),
            "git commit",
        )
        .unwrap();
        fs::write(root.join("ignored"), "ignored").unwrap();
        assert!(!repository_dirty(&root).unwrap());
        fs::write(root.join("untracked"), "must refuse").unwrap();
        assert!(repository_dirty(&root).unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn evidence_paths_and_pass_markers_are_never_reused() {
        let root = fixture_root("collision");
        fs::create_dir(&root).unwrap();
        assert!(create_exclusive_directory(&root, "fixture").is_err());
        let marker = root.join("shadow-parity.json");
        fs::write(&marker, "stale-pass\n").unwrap();
        assert!(atomic_write_new(&marker, "result\tpass\n").is_err());
        assert_eq!(fs::read_to_string(marker).unwrap(), "stale-pass\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_receipt_refuses_mutation_and_added_evidence() {
        let root = fixture_root("receipt-mutation");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("strict-report.json"), "original").unwrap();
        let facts = fixture_receipt_facts(root.file_name().unwrap().to_str().unwrap());
        let resources = fixture_resources();
        let receipt = write_fixture_receipt(&root, &facts, &resources);
        verify_fixture_receipt(&root, &receipt, &facts, &resources).unwrap();
        fs::write(root.join("strict-report.json"), "mutated").unwrap();
        assert!(verify_fixture_receipt(&root, &receipt, &facts, &resources).is_err());
        fs::write(root.join("strict-report.json"), "original").unwrap();
        fs::write(root.join("stale-pass-marker"), "pass").unwrap();
        assert!(verify_fixture_receipt(&root, &receipt, &facts, &resources).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn final_receipt_schema_refuses_semantic_mutation_duplicate_unknown_and_deletion() {
        let facts = fixture_receipt_facts("fixture-evidence");
        let resources = fixture_resources();
        let artifacts = BTreeMap::from([("strict-report.json".to_owned(), "c".repeat(64))]);
        let good = render_final_receipt(&facts, &resources, &artifacts).unwrap();
        let parsed = parse_final_receipt(&good).unwrap();
        verify_receipt_semantics(&parsed, &facts, &resources, &artifacts).unwrap();

        let mut mutated_value = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        mutated_value["facts"]["cargo_candidate_sha256"] =
            serde_json::Value::String("d".repeat(64));
        let mutated_hash = serde_json::to_string(&mutated_value).unwrap();
        let parsed_mutation = parse_final_receipt(&mutated_hash).unwrap();
        assert!(
            verify_receipt_semantics(&parsed_mutation, &facts, &resources, &artifacts).is_err()
        );

        for (field, value) in [
            (
                "matrix_candidate_manifest",
                serde_json::json!("renamed-manifest.json"),
            ),
            (
                "matrix_candidate_manifest_sha256",
                serde_json::json!("not-a-sha256"),
            ),
            ("cargo_matrix_candidate_invocations", serde_json::json!(0)),
            ("buck_matrix_candidate_invocations", serde_json::json!(3)),
            (
                "matrix_expected_ledger",
                serde_json::json!("renamed-ledger.json"),
            ),
            (
                "matrix_expected_ledger_sha256",
                serde_json::json!("not-a-sha256"),
            ),
            ("matrix_expected_invocation_count", serde_json::json!(0)),
        ] {
            let mut invalid = serde_json::from_str::<serde_json::Value>(&good).unwrap();
            invalid["facts"][field] = value;
            assert!(parse_final_receipt(&serde_json::to_string(&invalid).unwrap()).is_err());
        }

        let duplicate = good.replacen(
            "\"result\": \"pass\"",
            "\"result\": \"pass\", \"result\": \"pass\"",
            1,
        );
        assert_ne!(good, duplicate);
        assert!(parse_final_receipt(&duplicate).is_err());
        let unknown = good.replacen('{', "{\"future_semantic\":true,", 1);
        assert!(parse_final_receipt(&unknown).is_err());
        let mut deleted_value = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        deleted_value["facts"]
            .as_object_mut()
            .unwrap()
            .remove("safehermit_sha256");
        assert!(parse_final_receipt(&serde_json::to_string(&deleted_value).unwrap()).is_err());
        let mut deleted_manifest = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        deleted_manifest["facts"]
            .as_object_mut()
            .unwrap()
            .remove("matrix_candidate_manifest");
        assert!(parse_final_receipt(&serde_json::to_string(&deleted_manifest).unwrap()).is_err());
        let mut deleted_ledger = serde_json::from_str::<serde_json::Value>(&good).unwrap();
        deleted_ledger["facts"]
            .as_object_mut()
            .unwrap()
            .remove("matrix_expected_ledger");
        assert!(parse_final_receipt(&serde_json::to_string(&deleted_ledger).unwrap()).is_err());
        let duplicate_resource = good.replacen(
            &format!("\"rsrcs/fixture\": \"{}\"", "b".repeat(64)),
            &format!(
                "\"rsrcs/fixture\": \"{}\", \"rsrcs/fixture\": \"{}\"",
                "b".repeat(64),
                "b".repeat(64)
            ),
            1,
        );
        assert_ne!(good, duplicate_resource);
        assert!(parse_final_receipt(&duplicate_resource).is_err());
    }

    #[test]
    fn safehermit_report_requires_applied_bounds() {
        let root = fixture_root("safehermit-report");
        fs::create_dir(&root).unwrap();
        let report = root.join("report");
        fs::write(
            &report,
            "safehermit: bound.wall=APPLIED:120s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\n",
        )
        .unwrap();
        validate_and_surface_safehermit_report(&report, "fixture").unwrap();
        fs::write(&report, "safehermit: bound.wall=NOT_APPLIED:test\n").unwrap();
        assert!(validate_and_surface_safehermit_report(&report, "fixture").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn candidate_invocation_reports_require_exact_identities_population_and_applied_bounds() {
        let root = fixture_root("candidate-invocation-receipts");
        let invocations = root.join("candidate-invocations");
        fs::create_dir_all(&invocations).unwrap();
        for identity in EXPECTED_CANDIDATE_INVOCATIONS {
            let invocation = invocations.join(format!("{identity}-fixture"));
            fs::create_dir(&invocation).unwrap();
            fs::write(invocation.join("stdout"), b"stdout\n").unwrap();
            fs::write(invocation.join("stderr"), b"stderr\n").unwrap();
            fs::write(
                invocation.join("safehermit.report"),
                b"safehermit: bound.wall=APPLIED:10s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\n",
            )
            .unwrap();
        }
        verify_candidate_invocation_reports(&root).unwrap();

        let report = invocations.join("buck-help-fixture/safehermit.report");
        let original = fs::read(&report).unwrap();
        fs::write(
            &report,
            b"safehermit: bound.wall=NOT_APPLIED:test\nsafehermit: bound.cgroup=APPLIED:MemoryMax=1G\n",
        )
        .unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::write(&report, &original).unwrap();

        fs::remove_file(&report).unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::write(&report, &original).unwrap();

        let extra = invocations.join("unexpected-probe-fixture");
        fs::create_dir(&extra).unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::remove_dir(&extra).unwrap();

        fs::write(invocations.join("cargo-help-fixture/extra"), b"extra\n").unwrap();
        assert!(verify_candidate_invocation_reports(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn official_matrix_expected_ledger_and_proxy_classification_are_exact() {
        let repository = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let ledger = derive_official_matrix_expected_ledger(&repository).unwrap();
        assert_eq!(ledger.cases.len(), 28);
        assert_eq!(ledger.cases.iter().filter(|case| case.selected).count(), 27);
        assert_eq!(
            ledger
                .cases
                .iter()
                .filter(|case| case.requires_ptrace_reference)
                .count(),
            23
        );
        assert_eq!(
            ledger
                .expected_invocations
                .iter()
                .map(|entry| entry.count)
                .sum::<usize>(),
            106
        );

        let host = classify_matrix_proxy_invocation(&[
            "host-capabilities".to_owned(),
            "--json".to_owned(),
        ])
        .unwrap();
        assert_eq!(host.case_identity, "probe/host-capabilities");
        let smoke = classify_matrix_proxy_invocation(&[
            "run".to_owned(),
            "--backend".to_owned(),
            "dbt".to_owned(),
            "--strict".to_owned(),
            "--".to_owned(),
            "/bin/true".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            (smoke.case_identity.as_str(), smoke.role.as_str()),
            ("probe/dbt-smoke", "probe")
        );
        let exit_zero = classify_matrix_proxy_invocation(&[
            "run".to_owned(),
            "--backend".to_owned(),
            "dbt".to_owned(),
            "--strict".to_owned(),
            "--max-timeslice=disabled".to_owned(),
            "--".to_owned(),
            "/bin/true".to_owned(),
        ])
        .unwrap();
        assert_eq!(
            (exit_zero.case_identity.as_str(), exit_zero.role.as_str()),
            ("exit_zero", "dbt-run")
        );
        assert_eq!(
            normalize_matrix_argument(
                "/tmp/hermit-backend-parity-one/host-tmp/ptrace-exit_zero-reference-1"
            ),
            normalize_matrix_argument(
                "/tmp/hermit-backend-parity-two/host-tmp/ptrace-exit_zero-reference-1"
            )
        );
        assert!(
            classify_matrix_proxy_invocation(&[
                "run".to_owned(),
                "--backend".to_owned(),
                "dbt".to_owned(),
                "--max-timeslice=disabled".to_owned(),
                "--".to_owned(),
                "/unknown/fixture".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn matrix_completeness_refuses_symmetric_deletion_argv_mismatch_and_duplicate() {
        let repository = Path::new(file!())
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        let evidence = fixture_root("matrix-completeness-ledger");
        fs::create_dir(&evidence).unwrap();
        let ledger = derive_official_matrix_expected_ledger(&repository).unwrap();
        atomic_write_new(
            &evidence.join("matrix-expected-ledger.json"),
            &render_official_matrix_expected_ledger(&ledger).unwrap(),
        )
        .unwrap();
        let header = "test_name\tbackend\texpectation\tresult\tseconds\tdetail\n";
        let rows = ledger
            .cases
            .iter()
            .map(|case| {
                format!(
                    "{}\tdbt\t{}\t{}\t0.001\tfixture\n",
                    case.test_name,
                    case.expectation,
                    if case.selected { "PASS" } else { "GAP" }
                )
            })
            .collect::<String>();
        for label in ["cargo", "buck"] {
            fs::write(
                evidence.join(format!("{label}-dbt-matrix.tsv")),
                format!("{header}{rows}"),
            )
            .unwrap();
        }
        let hash = "a".repeat(64);
        let mut invocations = Vec::new();
        for label in ["cargo", "buck"] {
            let mut serial = 0;
            for expected in &ledger.expected_invocations {
                for _ in 0..expected.count {
                    serial += 1;
                    invocations.push(MatrixCandidateInvocationBinding {
                        candidate_label: label.to_owned(),
                        identity: format!("fixture-{serial}"),
                        case_identity: expected.case_identity.clone(),
                        role: expected.role.clone(),
                        normalized_argv: vec![
                            expected.case_identity.clone(),
                            expected.role.clone(),
                        ],
                        invocation_metadata_sha256: hash.clone(),
                        safehermit_report_sha256: hash.clone(),
                        stdout_sha256: hash.clone(),
                        stderr_sha256: hash.clone(),
                        data_sha256: BTreeMap::new(),
                        data_directories: Vec::new(),
                    });
                }
            }
        }
        let manifest = MatrixCandidateInvocationManifest {
            manifest_schema: "hermit-matrix-candidate-invocations/v1".to_owned(),
            invocations,
        };
        validate_matrix_candidate_completeness(&repository, &evidence, &manifest).unwrap();

        let mut symmetric_missing = manifest.clone();
        let missing_case = symmetric_missing.invocations[0].case_identity.clone();
        let missing_role = symmetric_missing.invocations[0].role.clone();
        for label in ["cargo", "buck"] {
            let index = symmetric_missing
                .invocations
                .iter()
                .position(|invocation| {
                    invocation.candidate_label == label
                        && invocation.case_identity == missing_case
                        && invocation.role == missing_role
                })
                .unwrap();
            symmetric_missing.invocations.remove(index);
        }
        assert!(
            validate_matrix_candidate_completeness(&repository, &evidence, &symmetric_missing)
                .is_err()
        );

        let mut argv_mismatch = manifest.clone();
        argv_mismatch
            .invocations
            .iter_mut()
            .find(|invocation| invocation.candidate_label == "buck")
            .unwrap()
            .normalized_argv
            .push("--mismatch".to_owned());
        assert!(
            validate_matrix_candidate_completeness(&repository, &evidence, &argv_mismatch).is_err()
        );

        let mut symmetric_case_mismatch = manifest.clone();
        for label in ["cargo", "buck"] {
            symmetric_case_mismatch
                .invocations
                .iter_mut()
                .find(|invocation| invocation.candidate_label == label)
                .unwrap()
                .case_identity = "correlated-wrong-case".to_owned();
        }
        assert!(
            validate_matrix_candidate_completeness(
                &repository,
                &evidence,
                &symmetric_case_mismatch,
            )
            .is_err()
        );

        let mut duplicate = manifest.clone();
        for label in ["cargo", "buck"] {
            let mut extra = duplicate
                .invocations
                .iter()
                .find(|invocation| invocation.candidate_label == label)
                .unwrap()
                .clone();
            extra.identity.push_str("-duplicate");
            duplicate.invocations.push(extra);
        }
        assert!(
            validate_matrix_candidate_completeness(&repository, &evidence, &duplicate).is_err()
        );

        let first_row = rows.lines().next().unwrap();
        let missing_rows = rows
            .lines()
            .filter(|line| *line != first_row)
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        for label in ["cargo", "buck"] {
            fs::write(
                evidence.join(format!("{label}-dbt-matrix.tsv")),
                format!("{header}{missing_rows}"),
            )
            .unwrap();
        }
        assert!(validate_matrix_candidate_completeness(&repository, &evidence, &manifest).is_err());
        for label in ["cargo", "buck"] {
            fs::write(
                evidence.join(format!("{label}-dbt-matrix.tsv")),
                format!("{header}{rows}{first_row}\n"),
            )
            .unwrap();
        }
        assert!(validate_matrix_candidate_completeness(&repository, &evidence, &manifest).is_err());
        fs::remove_dir_all(evidence).unwrap();
    }

    #[test]
    fn matrix_candidate_manifest_binds_observed_population_and_refuses_drift() {
        let root = fixture_root("matrix-candidate-invocation-manifest");
        let invocations = root.join("matrix-candidate-invocations");
        fs::create_dir_all(&invocations).unwrap();
        for name in ["cargo-observed-1", "buck-observed-2"] {
            let invocation = invocations.join(name);
            fs::create_dir(&invocation).unwrap();
            fs::create_dir(invocation.join("data")).unwrap();
            fs::write(
                invocation.join("data/runtime.log"),
                format!("{name} data\n"),
            )
            .unwrap();
            fs::write(
                invocation.join("invocation.json"),
                serde_json::to_vec_pretty(&MatrixProxyInvocation {
                    invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
                    case_identity: "exit_zero".to_owned(),
                    role: "dbt-run".to_owned(),
                    normalized_argv: vec![
                        "run".to_owned(),
                        "--".to_owned(),
                        "/bin/true".to_owned(),
                    ],
                })
                .unwrap(),
            )
            .unwrap();
            fs::write(invocation.join("stdout"), format!("{name} stdout\n")).unwrap();
            fs::write(invocation.join("stderr"), format!("{name} stderr\n")).unwrap();
            fs::write(
                invocation.join("safehermit.report"),
                b"safehermit: bound.wall=APPLIED:120s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=2G\n",
            )
            .unwrap();
        }
        let manifest = root.join("matrix-candidate-invocations.json");
        let observed = inspect_matrix_candidate_invocations(&root).unwrap();
        atomic_write_new(
            &manifest,
            &render_matrix_candidate_invocation_manifest(&observed).unwrap(),
        )
        .unwrap();
        let (retained, retained_hash) =
            verify_retained_matrix_candidate_invocation_manifest(&root).unwrap();
        assert_eq!(retained.invocations.len(), 2);
        assert_eq!(retained_hash, sha256(&manifest).unwrap());

        let original_manifest = fs::read_to_string(&manifest).unwrap();
        fs::write(
            &manifest,
            original_manifest.replace("observed-1", "mutated-1"),
        )
        .unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&manifest, &original_manifest).unwrap();

        fs::remove_file(&manifest).unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&manifest, &original_manifest).unwrap();

        let stdout = invocations.join("cargo-observed-1/stdout");
        let original_stdout = fs::read(&stdout).unwrap();
        fs::write(&stdout, b"mutated stdout\n").unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&stdout, original_stdout).unwrap();

        let data = invocations.join("cargo-observed-1/data/runtime.log");
        let original_data = fs::read(&data).unwrap();
        fs::write(&data, b"mutated data\n").unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&data, original_data).unwrap();

        let empty_directory = invocations.join("cargo-observed-1/data/late-empty-directory");
        fs::create_dir(&empty_directory).unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::remove_dir(&empty_directory).unwrap();

        let stderr = invocations.join("cargo-observed-1/stderr");
        let original_stderr = fs::read(&stderr).unwrap();
        fs::write(&stderr, b"mutated stderr\n").unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&stderr, original_stderr).unwrap();

        let report = invocations.join("buck-observed-2/safehermit.report");
        let original_report = fs::read(&report).unwrap();
        fs::write(
            &report,
            b"safehermit: bound.wall=APPLIED:119s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=2G\n",
        )
        .unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&report, &original_report).unwrap();
        fs::remove_file(&report).unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::write(&report, &original_report).unwrap();

        let extra = invocations.join("cargo-observed-1/extra");
        fs::write(&extra, b"unexpected\n").unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::remove_file(&extra).unwrap();

        let link = invocations.join("cargo-observed-1/linked");
        std::os::unix::fs::symlink("stdout", &link).unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::remove_file(&link).unwrap();

        for name in ["cargo-extra-observed", "buck-extra-observed"] {
            let invocation = invocations.join(name);
            fs::create_dir(&invocation).unwrap();
            fs::create_dir(invocation.join("data")).unwrap();
            fs::write(
                invocation.join("invocation.json"),
                serde_json::to_vec_pretty(&MatrixProxyInvocation {
                    invocation_schema: "hermit-matrix-proxy-invocation/v1".to_owned(),
                    case_identity: "exit_zero".to_owned(),
                    role: "dbt-run".to_owned(),
                    normalized_argv: vec![
                        "run".to_owned(),
                        "--".to_owned(),
                        "/bin/true".to_owned(),
                    ],
                })
                .unwrap(),
            )
            .unwrap();
            fs::write(invocation.join("stdout"), b"extra stdout\n").unwrap();
            fs::write(invocation.join("stderr"), b"extra stderr\n").unwrap();
            fs::write(
                invocation.join("safehermit.report"),
                b"safehermit: bound.wall=APPLIED:120s\nsafehermit: bound.cgroup=APPLIED:MemoryMax=2G\n",
            )
            .unwrap();
        }
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::remove_dir_all(invocations.join("cargo-extra-observed")).unwrap();
        fs::remove_dir_all(invocations.join("buck-extra-observed")).unwrap();

        fs::remove_dir_all(invocations.join("buck-observed-2")).unwrap();
        assert!(verify_retained_matrix_candidate_invocation_manifest(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_buck_log_summary_is_recomputed_and_mutation_or_deletion_refuses() {
        let root = fixture_root("buck-log-summary-recompute");
        fs::create_dir(&root).unwrap();
        let buck = root.join("buck2");
        write_executable(
            &buck,
            b"#!/bin/sh\nset -eu\ntest \"$1\" = log\ntest \"$2\" = summary\ntest -f \"$3\"\nprintf 'summary-fixture\\n'\n",
        );
        let event = root.join("event.json-lines.gz");
        fs::write(&event, b"fixture event bytes\n").unwrap();
        let summary = root.join("buck-log-summary.txt");
        fs::write(&summary, b"summary-fixture\n").unwrap();
        verify_retained_buck_log_summary(&buck, &root, &event, &summary).unwrap();
        fs::write(&summary, b"mutated-summary\n").unwrap();
        assert!(verify_retained_buck_log_summary(&buck, &root, &event, &summary).is_err());
        fs::remove_file(&summary).unwrap();
        assert!(verify_retained_buck_log_summary(&buck, &root, &event, &summary).is_err());
        fs::remove_file(&event).unwrap();
        fs::write(&summary, b"summary-fixture\n").unwrap();
        assert!(verify_retained_buck_log_summary(&buck, &root, &event, &summary).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matrix_supervisor_refuses_timeout_abort_and_incomplete_cleanup() {
        let clean = || {
            require_clean_matrix_supervision(
                true,
                false,
                false,
                false,
                0,
                0,
                1,
                true,
                false,
                false,
                false,
                Some(0),
            )
        };
        clean().unwrap();
        for (run_timed_out, outcome_aborted, not_launched) in
            [(true, false, 0), (false, true, 0), (false, false, 1)]
        {
            assert!(
                require_clean_matrix_supervision(
                    !run_timed_out && !outcome_aborted && not_launched == 0,
                    run_timed_out,
                    false,
                    false,
                    0,
                    not_launched,
                    1,
                    false,
                    run_timed_out,
                    false,
                    outcome_aborted,
                    None,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn dbt_selected_row_mismatch_is_refused() {
        assert!(
            require_equal(
                "DBT rows",
                "test_name\tresult\na\tPASS\n",
                "test_name\tresult\nb\tPASS\n",
            )
            .is_err()
        );
    }
}
