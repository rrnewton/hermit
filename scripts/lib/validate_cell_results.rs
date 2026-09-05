// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the per-cell result population carried by one full validate run.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use hermit_manifest_plan::canonical_verdict::InfrastructureError;
use hermit_manifest_plan::canonical_verdict::Verdict as VerificationVerdict;
use hermit_manifest_plan::canonical_verdict::VerificationReport;
use hermit_manifest_plan::ledger::CellIdentity;
use hermit_manifest_plan::ledger::CellResult as LedgerCellResult;
use hermit_manifest_plan::ledger::CellResultsArtifact;
use hermit_manifest_plan::ledger::CellResultsEvidence;
use hermit_manifest_plan::ledger::CellResultsEvidenceV10;
use hermit_manifest_plan::ledger::CellVerdict;
use hermit_manifest_plan::ledger::ComparedLogCounts;
use hermit_manifest_plan::ledger::ComparisonSpec;
use hermit_manifest_plan::ledger::ComparisonTier;
use hermit_manifest_plan::ledger::RequiredNullable;
use hermit_manifest_plan::ledger::RetainedVerifyLogArtifact;
use hermit_manifest_plan::ledger::RetainedVerifyLogIndexRow;
use hermit_manifest_plan::ledger::RetainedVerifyLogsArtifact;
use hermit_manifest_plan::ledger::ValidatePath;
use hermit_manifest_plan::ledger::is_canonical_hermetic_image_digest;
use hermit_manifest_plan::runner::CELL_RESULT_SCHEMA;
use hermit_manifest_plan::runner::RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA;
use hermit_manifest_plan::runner::CellId;
use hermit_manifest_plan::runner::RetainedVerifyLog;
use hermit_manifest_plan::runner::VerifyLogRetentionPolicy;
use hermit_manifest_plan::runner::copy_verified_retained_verify_log;
use hermit_manifest_plan::runner::outcome_after_retries;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

/// Outer ledger schema for rows carrying [`RetainedCellResults`].
///
/// Schema 6 contains two historical rows written before the current comparison
/// fields existed. Schema 7 guarantees that every compared verdict carries the
/// complete current comparison object; a missing or additional field is kept
/// out of the compared-verdict projection instead of changing this shape under
/// the same version.
pub const CELL_RESULTS_LEDGER_SCHEMA_MIN: i64 = 6;
pub const CELL_RESULTS_LEDGER_SCHEMA_VERSION: i64 = 7;
pub const RETAINED_VERIFY_LOGS_LEDGER_SCHEMA_VERSION: i64 = 10;

#[derive(Debug)]
pub struct RetainedCellResults {
    pub schema_version: i64,
    pub run_id: String,
    /// The surrounding validation row is assembled as JSON, but this value is
    /// always serialized from the producer-owned [`CellResultsEvidence`]
    /// contract rather than constructed as a second untyped definition.
    pub evidence: Value,
}

#[derive(Debug)]
pub struct RetainedCoverageEvidence {
    pub evidence: Value,
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn collect_results_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read per-cell result root {}: {error}",
            path.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read per-cell result entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot classify {}: {error}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_results_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == "results.jsonl" {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn read_result_rows(path: &Path) -> Result<Vec<(PathBuf, usize, Value)>, String> {
    let mut files = Vec::new();
    collect_results_files(path, &mut files)?;
    files.sort();
    let mut rows = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row = serde_json::from_str(line).map_err(|error| {
                format!(
                    "{}:{} malformed result row: {error}",
                    file.display(),
                    line_number + 1
                )
            })?;
            rows.push((file.clone(), line_number + 1, row));
        }
    }
    Ok(rows)
}

/// Read every retained cell attempt from the harness's appended result files.
///
/// This is the same population `retain` validates. Keeping one reader prevents
/// the history writer from silently omitting retries that the terminal-verdict
/// projection deliberately reduces to the latest attempt.
pub fn all_result_rows(path: &Path) -> Result<Vec<Value>, String> {
    read_result_rows(path).map(|rows| rows.into_iter().map(|(_, _, row)| row).collect())
}

#[derive(Clone)]
struct RetainedVerifyLogSource {
    cell: CellIdentity,
    attempt: u64,
    artifact_dir: PathBuf,
    retained: RetainedVerifyLog,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetentionFailurePoint {
    ArtifactAncestorSync,
    BeforeVerifyLogRename,
    AfterVerifyLogRename,
    BeforeCellResultPublication,
}

fn require_normal_component(value: &str, description: &str) -> Result<(), String> {
    let mut components = Path::new(value).components();
    if !matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(component)), None) if component == value
    ) {
        return Err(format!("{description} is not one normal path component"));
    }
    Ok(())
}

fn checked_relative_path<'a>(
    root: &'a Path,
    path: &'a Path,
    description: &str,
) -> Result<&'a Path, String> {
    let relative = path.strip_prefix(root).map_err(|_| {
        format!(
            "{description} {} is outside {}",
            path.display(),
            root.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{description} {} is not a normal relative path below {}",
            path.display(),
            root.display()
        ));
    }
    Ok(relative)
}

fn require_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} {} is not a non-symlink directory",
            path.display()
        ));
    }
    Ok(())
}

fn require_plain_directory_path_below(
    root: &Path,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    let relative = checked_relative_path(root, path, description)?;
    require_plain_directory(root, "retained result root")?;
    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            unreachable!("checked_relative_path accepted only normal components")
        };
        current.push(component);
        require_plain_directory(&current, description)?;
    }
    Ok(())
}

fn create_plain_directory_path_below(
    root: &Path,
    relative: &Path,
    description: &str,
    failure: Option<RetentionFailurePoint>,
) -> Result<PathBuf, String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("{description} is not a normal relative path"));
    }
    require_plain_directory(root, "retained validation state root")?;
    let mut current = root.to_owned();
    let mut chain = vec![root.to_owned()];
    let mut created = Vec::new();
    let create_and_sync = (|| -> Result<PathBuf, String> {
        for component in relative.components() {
            let Component::Normal(component) = component else {
                unreachable!("relative path was checked above")
            };
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(format!(
                        "{description} {} is not a non-symlink directory",
                        current.display()
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).map_err(|error| {
                        format!("cannot create {description} {}: {error}", current.display())
                    })?;
                    created.push(current.clone());
                }
                Err(error) => {
                    return Err(format!(
                        "cannot inspect {description} {}: {error}",
                        current.display()
                    ));
                }
            }
            chain.push(current.clone());
        }
        if !created.is_empty() {
            for directory in &chain {
                if failure == Some(RetentionFailurePoint::ArtifactAncestorSync)
                    && directory == root
                {
                    return Err(
                        "injected failure syncing retained validation artifact ancestor".into(),
                    );
                }
                sync_directory(directory, description)?;
            }
        }
        Ok(current.clone())
    })();
    match create_and_sync {
        Ok(path) => Ok(path),
        Err(error) => match remove_created_directory_chain(&created, description) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; cannot clean newly created directory chain: {cleanup_error}"
            )),
        },
    }
}

fn remove_created_directory_chain(created: &[PathBuf], description: &str) -> Result<(), String> {
    for directory in created.iter().rev() {
        fs::remove_dir(directory).map_err(|error| {
            format!(
                "cannot remove newly created {description} {}: {error}",
                directory.display()
            )
        })?;
        let parent = directory
            .parent()
            .ok_or_else(|| format!("newly created directory {} has no parent", directory.display()))?;
        sync_directory(parent, description)?;
    }
    Ok(())
}

fn sync_directory(path: &Path, description: &str) -> Result<(), String> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    directory
        .sync_all()
        .map_err(|error| format!("cannot sync {description} {}: {error}", path.display()))
}

fn sync_directory_tree(path: &Path) -> Result<(), String> {
    require_plain_directory(path, "retained verify-log staging directory")?;
    for entry in fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read retained verify-log staging directory {}: {error}",
            path.display()
        )
    })? {
        let entry = entry.map_err(|error| format!("cannot read staging entry: {error}"))?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            format!("cannot inspect staging entry {}: {error}", entry.path().display())
        })?;
        if metadata.is_dir() {
            sync_directory_tree(&entry.path())?;
        } else if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "retained verify-log staging entry {} is not a regular file",
                entry.path().display()
            ));
        }
    }
    sync_directory(path, "retained verify-log staging directory")
}

fn rename_directory_noreplace(source: &Path, destination: &Path) -> Result<(), String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| "retained verify-log staging path contains NUL".to_string())?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| "retained verify-log destination path contains NUL".to_string())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(format!(
            "cannot atomically publish retained verify-log directory {}: {}",
            destination.to_string_lossy(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn collect_verify_log_sources(
    result_root: &Path,
    run_id: &str,
    rows: &[(PathBuf, usize, Value)],
) -> Result<Vec<RetainedVerifyLogSource>, String> {
    require_normal_component(run_id, "per-cell result run_id")?;
    let run_root = result_root.join("runs").join(run_id);
    require_plain_directory(result_root, "per-cell result root")?;
    match fs::symlink_metadata(&run_root) {
        Ok(_) => require_plain_directory_path_below(
            result_root,
            &run_root,
            "retained verify-log run directory",
        )?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot inspect retained verify-log run directory {}: {error}",
                run_root.display()
            ));
        }
    }
    let mut sources = BTreeMap::<(CellIdentity, u64), RetainedVerifyLogSource>::new();
    for (file, line_number, row) in rows {
        let schema = row.get("schema").and_then(Value::as_u64).unwrap_or(0);
        let mode = string(row, "mode")?;
        let attempts = row
            .get("attempts")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let compared = attempts.iter().any(|attempt| {
            attempt
                .get("verification_report")
                .and_then(Value::as_str)
                .is_some()
        });
        let descriptors = attempts
            .iter()
            .filter_map(|attempt| {
                attempt
                    .get("retained_verify_log")
                    .filter(|value| !value.is_null())
            })
            .collect::<Vec<_>>();

        if mode != "verify" && !descriptors.is_empty() {
            return Err(format!(
                "{}:{line_number} non-verify result carries retained_verify_log",
                file.display()
            ));
        }
        if descriptors.len() > 1 {
            return Err(format!(
                "{}:{line_number} carries more than one retained_verify_log",
                file.display()
            ));
        }
        let Some(descriptor) = descriptors.first() else {
            continue;
        };
        if schema != RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA {
            return Err(format!(
                "{}:{line_number} schema {schema} cannot claim current retained_verify_log evidence",
                file.display()
            ));
        }
        if attempts.len() != 1 || !compared {
            return Err(format!(
                "{}:{line_number} retained_verify_log is not bound to one compared attempt",
                file.display()
            ));
        }
        let report_bytes = attempts[0]
            .get("verification_report")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "{}:{line_number} retained_verify_log has no typed verification report",
                    file.display()
                )
            })?;
        let report_sha256 = attempts[0]
            .get("verification_report_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "{}:{line_number} retained_verify_log has no verification_report_sha256",
                    file.display()
                )
            })?;
        if hex_digest(report_bytes.as_bytes()) != report_sha256 {
            return Err(format!(
                "{}:{line_number} retained verify-log report digest mismatch",
                file.display()
            ));
        }
        let report = VerificationReport::from_json_slice(report_bytes.as_bytes()).map_err(|error| {
            format!(
                "{}:{line_number} retained verify-log report is malformed: {error}",
                file.display()
            )
        })?;
        let retained: RetainedVerifyLog = serde_json::from_value((*descriptor).clone()).map_err(
            |error| {
                format!(
                    "{}:{line_number} retained_verify_log is malformed: {error}",
                    file.display()
                )
            },
        )?;
        let attempt = row.get("attempt").and_then(Value::as_u64).unwrap_or(1);
        if attempt == 0 || retained.attempt != attempt {
            return Err(format!(
                "{}:{line_number} retained_verify_log attempt differs from its result row",
                file.display()
            ));
        }
        let cell = identity(row)?;
        let expected_id = CellId {
            test: cell.test.clone(),
            mode: cell.mode.clone(),
            backend: Some(cell.backend.clone()),
        };
        if retained.cell_id != expected_id {
            return Err(format!(
                "{}:{line_number} retained_verify_log cell_id differs from its result row",
                file.display()
            ));
        }
        let report_count = report
            .compared_log_messages
            .as_ref()
            .map(|counts| counts.left)
            .unwrap_or(0);
        if retained.compared_info_messages != report_count {
            return Err(format!(
                "{}:{line_number} retained verify-log compared count differs from its verification report",
                file.display()
            ));
        }
        let artifact_dir = PathBuf::from(string(row, "artifact_dir")?);
        let relative_artifact = checked_relative_path(
            &run_root,
            &artifact_dir,
            "retained verify-log cell artifact directory",
        )?;
        if relative_artifact.components().count() != 1 {
            return Err(format!(
                "{}:{line_number} retained verify-log artifact directory is not one cell below {}",
                file.display(),
                run_root.display()
            ));
        }
        require_plain_directory_path_below(
            &run_root,
            &artifact_dir,
            "retained verify-log cell artifact directory",
        )?;
        let key = (cell.clone(), attempt);
        let source = RetainedVerifyLogSource {
            cell,
            attempt,
            artifact_dir,
            retained,
        };
        if sources.insert(key, source).is_some() {
            return Err(format!(
                "{}:{line_number} duplicates a retained verify-log cell and attempt",
                file.display()
            ));
        }
    }

    let mut referenced = BTreeSet::new();
    let mut expected_by_namespace = BTreeMap::<PathBuf, BTreeSet<PathBuf>>::new();
    for source in sources.values() {
        let path = source.artifact_dir.join(&source.retained.relative_path);
        if !referenced.insert(path.clone()) {
            return Err(
                "multiple retained_verify_log descriptors reference the same gzip".into(),
            );
        }
        let namespace = source.artifact_dir.join("retained/verify");
        let relative = checked_relative_path(
            &namespace,
            &path,
            "retained verify-log descriptor path",
        )?
        .to_owned();
        let expected_relative = PathBuf::from(source.attempt.to_string()).join("run-1.log.gz");
        if relative != expected_relative {
            return Err(format!(
                "retained verify-log descriptor path must be {}, got {}",
                expected_relative.display(),
                relative.display()
            ));
        }
        let entries = expected_by_namespace.entry(namespace).or_default();
        entries.insert(
            relative
                .parent()
                .expect("canonical retained log has an attempt directory")
                .to_owned(),
        );
        entries.insert(relative);
    }
    if run_root.is_dir() {
        for entry in fs::read_dir(&run_root).map_err(|error| {
            format!(
                "cannot read retained verify-log run directory {}: {error}",
                run_root.display()
            )
        })? {
            let entry = entry.map_err(|error| {
                format!("cannot read retained verify-log cell directory entry: {error}")
            })?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
                format!(
                    "cannot inspect retained verify-log cell directory {}: {error}",
                    entry.path().display()
                )
            })?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                let retained_root = entry.path().join("retained");
                match fs::symlink_metadata(&retained_root) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(format!(
                            "cannot inspect retained verify-log root {}: {error}",
                            retained_root.display()
                        ));
                    }
                    Ok(retained_metadata)
                        if retained_metadata.file_type().is_symlink()
                            || !retained_metadata.is_dir() =>
                    {
                        return Err(format!(
                            "retained verify-log root {} is not a non-symlink directory",
                            retained_root.display()
                        ));
                    }
                    Ok(_) => {}
                }
                let namespace = retained_root.join("verify");
                match fs::symlink_metadata(&namespace) {
                    Ok(_) => {
                        if !expected_by_namespace.contains_key(&namespace) {
                            return Err(format!(
                                "unreferenced retained verify-log namespace {}",
                                namespace.display()
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "cannot inspect retained verify-log namespace {}: {error}",
                            namespace.display()
                        ));
                    }
                }
            }
        }
    }
    for (namespace, expected_entries) in expected_by_namespace {
        let mut present_entries = BTreeSet::new();
        collect_retained_namespace_entries(&namespace, &namespace, &mut present_entries)?;
        if present_entries != expected_entries {
            let unexpected = present_entries.difference(&expected_entries).count();
            let missing = expected_entries.difference(&present_entries).count();
            return Err(format!(
                "retained verify-log namespace {} differs from result descriptors: {missing} missing, {unexpected} unreferenced or unexpected",
                namespace.display()
            ));
        }
    }
    Ok(sources.into_values().collect())
}

fn publish_retained_verify_logs(
    parent: &Path,
    artifact_dir: &Path,
    sources: &[RetainedVerifyLogSource],
    policy: VerifyLogRetentionPolicy,
    failure: Option<RetentionFailurePoint>,
) -> Result<RetainedVerifyLogsArtifact, String> {
    if sources.is_empty() {
        return Err("cannot publish an empty retained verify-log index".into());
    }
    let total_compressed_bytes = sources.iter().try_fold(0u64, |total, source| {
        total
            .checked_add(source.retained.compressed_bytes)
            .ok_or_else(|| "retained verify-log compressed byte total overflowed u64".to_string())
    })?;
    if total_compressed_bytes > policy.maximum_total_compressed_bytes {
        return Err(format!(
            "retained verify logs require {total_compressed_bytes} compressed bytes, exceeding the configured aggregate limit of {} bytes",
            policy.maximum_total_compressed_bytes
        ));
    }

    let final_directory = artifact_dir.join("verify-logs");
    match fs::symlink_metadata(&final_directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot inspect retained verify-log destination {}: {error}",
                final_directory.display()
            ));
        }
        Ok(_) => {
            return Err(format!(
                "retained verify-log destination already exists: {}",
                final_directory.display()
            ));
        }
    }

    let staging = tempfile::Builder::new()
        .prefix(".verify-logs.")
        .tempdir_in(artifact_dir)
        .map_err(|error| {
            format!(
                "cannot create retained verify-log staging directory in {}: {error}",
                artifact_dir.display()
            )
        })?;
    let mut index_rows = Vec::with_capacity(sources.len());
    let mut destination_checks = Vec::with_capacity(sources.len());
    for (offset, source) in sources.iter().enumerate() {
        let ordinal = offset + 1;
        let entry_relative = PathBuf::from(format!("{ordinal:06}"));
        let entry_root = staging.path().join(&entry_relative);
        let retained_parent = entry_root
            .join(&source.retained.relative_path)
            .parent()
            .ok_or("retained verify-log destination has no parent")?
            .to_owned();
        fs::create_dir_all(&retained_parent).map_err(|error| {
            format!(
                "cannot create retained verify-log destination {}: {error}",
                retained_parent.display()
            )
        })?;
        let destination = entry_root.join(&source.retained.relative_path);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(&destination)
            .map_err(|error| {
                format!(
                    "cannot create retained verify-log destination {}: {error}",
                    destination.display()
                )
            })?;
        let copied = copy_verified_retained_verify_log(
            &source.artifact_dir,
            &source.retained,
            &source.retained.cell_id,
            source.attempt,
            &mut output,
            policy.maximum_total_compressed_bytes,
        )?;
        output
            .flush()
            .and_then(|()| output.sync_all())
            .map_err(|error| {
                format!(
                    "cannot sync retained verify-log destination {}: {error}",
                    destination.display()
                )
            })?;
        if copied.compressed_sha256 != source.retained.compressed_sha256
            || copied.compressed_bytes != source.retained.compressed_bytes
            || copied.uncompressed_sha256 != source.retained.uncompressed_sha256
            || copied.uncompressed_bytes != source.retained.uncompressed_bytes
        {
            return Err("retained verify-log copy differs from its descriptor".into());
        }
        let final_path = final_directory
            .join(&entry_relative)
            .join(&source.retained.relative_path);
        let relative = final_path
            .strip_prefix(parent)
            .map_err(|_| "retained verify-log destination is outside parent root")?
            .to_string_lossy()
            .into_owned();
        index_rows.push(RetainedVerifyLogIndexRow {
            cell: source.cell.clone(),
            attempt: source.attempt,
            retained_verify_log: source.retained.clone(),
            artifact: RetainedVerifyLogArtifact {
                path: relative,
                sha256: copied.compressed_sha256,
                bytes: copied.compressed_bytes,
            },
        });
        destination_checks.push((
            entry_relative,
            source.retained.clone(),
            source.retained.cell_id.clone(),
            source.attempt,
        ));
    }

    let mut index_bytes = Vec::new();
    for row in &index_rows {
        serde_json::to_writer(&mut index_bytes, row)
            .map_err(|error| format!("cannot encode retained verify-log index row: {error}"))?;
        index_bytes.push(b'\n');
    }
    let staging_index = staging.path().join("index.jsonl");
    let mut index_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&staging_index)
        .map_err(|error| {
            format!(
                "cannot create retained verify-log index {}: {error}",
                staging_index.display()
            )
        })?;
    index_file
        .write_all(&index_bytes)
        .and_then(|()| index_file.flush())
        .and_then(|()| index_file.sync_all())
        .map_err(|error| format!("cannot write retained verify-log index: {error}"))?;
    sync_directory_tree(staging.path())?;
    if failure == Some(RetentionFailurePoint::BeforeVerifyLogRename) {
        return Err("injected failure before retained verify-log directory rename".into());
    }
    rename_directory_noreplace(staging.path(), &final_directory)?;
    let finish_publication = || -> Result<RetainedVerifyLogsArtifact, String> {
        if failure == Some(RetentionFailurePoint::AfterVerifyLogRename) {
            return Err("injected failure after retained verify-log directory rename".into());
        }
        sync_directory(artifact_dir, "retained validation artifact directory")?;
        for (entry_relative, retained, cell_id, attempt) in &destination_checks {
            hermit_manifest_plan::runner::verify_retained_verify_log(
                &final_directory.join(entry_relative),
                retained,
                cell_id,
                *attempt,
            )?;
        }
        let published_index = final_directory.join("index.jsonl");
        let metadata = fs::symlink_metadata(&published_index).map_err(|error| {
            format!(
                "cannot inspect published retained verify-log index {}: {error}",
                published_index.display()
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
            return Err("published retained verify-log index is not one regular file".into());
        }
        let observed = fs::read(&published_index).map_err(|error| {
            format!(
                "cannot read published retained verify-log index {}: {error}",
                published_index.display()
            )
        })?;
        if observed != index_bytes {
            return Err("published retained verify-log index differs from staged bytes".into());
        }
        let index_path = final_directory.join("index.jsonl");
        let relative = index_path
            .strip_prefix(parent)
            .map_err(|_| "retained verify-log index is outside parent root")?
            .to_string_lossy()
            .into_owned();
        Ok(RetainedVerifyLogsArtifact {
            path: relative,
            sha256: hex_digest(&index_bytes),
            row_count: u64::try_from(index_rows.len())
                .map_err(|_| "retained verify-log index row count does not fit u64")?,
            compressed_bytes: total_compressed_bytes,
        })
    };
    match finish_publication() {
        Ok(artifact) => Ok(artifact),
        Err(error) => match remove_published_verify_logs(artifact_dir) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(format!(
                "{error}; cannot clean failed retained verify-log publication {}: {cleanup_error}",
                final_directory.display()
            )),
        },
    }
}

fn publish_file_noclobber(path: &Path, bytes: &[u8], description: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{description} {} has no parent", path.display()))?;
    require_plain_directory(parent, "retained validation artifact directory")?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".artifact.")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "cannot create temporary {description} beside {}: {error}",
                path.display()
            )
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.flush())
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|error| format!("cannot write temporary {description}: {error}"))?;
    temporary.persist_noclobber(path).map_err(|error| {
        format!(
            "cannot publish {description} to {} without replacement: {}",
            path.display(),
            error.error
        )
    })?;
    if let Err(error) = sync_directory(parent, "retained validation artifact directory") {
        return match fs::remove_file(path) {
            Ok(()) => match sync_directory(parent, "retained validation artifact directory") {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; cannot sync cleanup of failed {description}: {cleanup_error}"
                )),
            },
            Err(cleanup_error) => Err(format!(
                "{error}; cannot remove failed {description} {}: {cleanup_error}",
                path.display()
            )),
        };
    }
    Ok(())
}

fn remove_published_verify_logs(artifact_dir: &Path) -> Result<(), String> {
    let path = artifact_dir.join("verify-logs");
    match fs::remove_dir_all(&path) {
        Ok(()) => sync_directory(artifact_dir, "retained validation artifact directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot remove retained verify-log publication {}: {error}",
            path.display()
        )),
    }
}

fn collect_retained_namespace_entries(
    namespace: &Path,
    path: &Path,
    output: &mut BTreeSet<PathBuf>,
) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect retained verify-log source {}: {error}",
                path.display()
            ));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(format!(
                "retained verify-log namespace entry {} is a symlink",
                path.display()
            ));
        }
        Ok(metadata) if metadata.is_file() => {
            let relative = checked_relative_path(
                namespace,
                path,
                "retained verify-log namespace entry",
            )?;
            output.insert(relative.to_owned());
            return Ok(());
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(format!(
                "retained verify-log namespace entry {} is not a regular file or directory",
                path.display()
            ));
        }
        Ok(_) => {}
    }
    for entry in fs::read_dir(path)
        .map_err(|error| format!("cannot read retained verify-log source {}: {error}", path.display()))?
    {
        let entry = entry.map_err(|error| format!("cannot read retained source entry: {error}"))?;
        let entry_path = entry.path();
        let relative = checked_relative_path(
            namespace,
            &entry_path,
            "retained verify-log namespace entry",
        )?;
        output.insert(relative.to_owned());
        collect_retained_namespace_entries(namespace, &entry_path, output)?;
    }
    Ok(())
}

#[cfg(test)]
fn collect_retained_gzip_paths(path: &Path, output: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect retained verify-log artifact {}: {error}",
                path.display()
            ));
        }
        Ok(metadata) if metadata.is_file() => {
            if path.extension().is_some_and(|extension| extension == "gz") {
                output.insert(path.to_owned());
            }
            return Ok(());
        }
        Ok(metadata) if !metadata.is_dir() => return Ok(()),
        Ok(_) => {}
    }
    for entry in fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read retained verify-log artifact {}: {error}",
            path.display()
        )
    })? {
        let entry = entry.map_err(|error| format!("cannot read retained artifact entry: {error}"))?;
        collect_retained_gzip_paths(&entry.path(), output)?;
    }
    Ok(())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("per-cell result has no nonempty {key}"))
}

fn identity(value: &Value) -> Result<CellIdentity, String> {
    Ok(CellIdentity {
        lane: string(value, "lane")?.into(),
        category: string(value, "category")?.into(),
        test: string(value, "test")?.into(),
        mode: string(value, "mode")?.into(),
        backend: string(value, "backend")?.into(),
    })
}

fn identity_value(identity: &CellIdentity) -> Result<Value, String> {
    serde_json::to_value(identity)
        .map_err(|error| format!("cannot encode selected cell identity: {error}"))
}

fn require_current_timeout_policy(row: &Value) -> Result<(), String> {
    let timeout = row
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .ok_or("current cell result has no timeout_seconds")?;
    let cpu = row
        .get("execution_cpu_timeout_seconds")
        .and_then(Value::as_u64)
        .ok_or("current cell result omitted execution_cpu_timeout_seconds")?;
    let wall = row
        .get("execution_wall_timeout_seconds")
        .and_then(Value::as_u64)
        .ok_or("current cell result omitted execution_wall_timeout_seconds")?;
    if cpu == 0 || wall != timeout || wall <= cpu {
        return Err(format!(
            "current cell result timeout policy disagrees: timeout_seconds={timeout} execution_cpu_timeout_seconds={cpu} execution_wall_timeout_seconds={wall}"
        ));
    }
    Ok(())
}

fn canonical_report(
    value: Value,
) -> Result<Option<(VerificationReport, ComparisonSpec, ComparedLogCounts)>, String> {
    // `VerificationReport` owns the complete current top-level report. The
    // ledger types additionally deny unknown comparison/count fields, which
    // preserves schema 7's exact shape without a second hard-coded key list.
    let report = VerificationReport::from_current_json_value(value.clone())?;
    if report.verdict == VerificationVerdict::InfrastructureError {
        return Err(match report.infrastructure_error.as_ref() {
            Some(InfrastructureError::SkidOvershoot { count }) => format!(
                "recorded infrastructure_error: {count} HERMIT_SKID_OVERSHOOT report(s)"
            ),
            None => unreachable!("typed report parser requires an infrastructure error"),
        });
    }
    let comparison = value
        .get("comparison")
        .cloned()
        .ok_or("incomplete cell comparison: missing `comparison`")?;
    let comparison = serde_json::from_value::<ComparisonSpec>(comparison)
        .map_err(|error| format!("incomplete cell comparison: {error}"))?;
    let compared_log_messages = serde_json::from_value::<RequiredNullable<ComparedLogCounts>>(
        value
            .get("compared_log_messages")
            .cloned()
            .ok_or("incomplete cell comparison: missing `compared_log_messages`")?,
    )
    .map_err(|error| format!("incomplete cell comparison counts: {error}"))?;
    if !comparison.is_canonical_bitwise_info_v1(&compared_log_messages) {
        return Ok(None);
    }
    let RequiredNullable::Value(compared_log_messages) = compared_log_messages else {
        return Ok(None);
    };
    Ok(Some((report, comparison, compared_log_messages)))
}

fn cell_verdict(row: &Value) -> Result<CellVerdict, String> {
    let mode = string(row, "mode")?;
    if mode == "naked" || mode == "custom" {
        return Ok(CellVerdict::PerformsNoComparisonByDesign {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: format!("{mode} mode does not perform canonical two-run comparison"),
        });
    }
    let Some(attempts) = row.get("attempts").and_then(Value::as_array) else {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: "cell emitted no typed attempts".into(),
        });
    };
    let mut reports = Vec::new();
    let mut unavailable_reason = None;
    for (index, attempt) in attempts.iter().enumerate() {
        let Some(raw) = attempt.get("verification_report").and_then(Value::as_str) else {
            unavailable_reason = Some(format!(
                "attempt {} emitted no typed verification report",
                index + 1
            ));
            continue;
        };
        let expected_sha = attempt
            .get("verification_report_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("attempt {} omitted verification_report_sha256", index + 1))?;
        if hex_digest(raw.as_bytes()) != expected_sha {
            return Err(format!(
                "attempt {} verification_report_sha256 mismatch",
                index + 1
            ));
        }
        let value = serde_json::from_str::<Value>(raw).map_err(|error| {
            format!(
                "attempt {} verification report is malformed: {error}",
                index + 1
            )
        })?;
        match canonical_report(value) {
            Ok(Some(report)) => reports.push(report),
            Ok(None) => {
                unavailable_reason = Some(format!(
                    "attempt {} did not compare canonical nonzero INFO evidence",
                    index + 1
                ));
            }
            Err(error) => unavailable_reason = Some(format!("attempt {} {error}", index + 1)),
        }
    }
    let classify = |(report, _, _): &(VerificationReport, ComparisonSpec, ComparedLogCounts)| {
        let matched = report.verified
            && report.verdict == VerificationVerdict::Matched
            && report.bitwise_parity;
        let diverged = report.verdict == VerificationVerdict::Diverged && !report.bitwise_parity;
        (matched, diverged)
    };
    // A genuine canonical divergence is sticky across sibling attempts. Missing
    // or weaker evidence may prevent a clean leg, but it must never erase a red
    // leg merely because it was observed before or after that divergence.
    if let Some((_, comparison, compared_log_messages)) =
        reports.iter().find(|report| classify(report).1)
    {
        return Ok(CellVerdict::ComparedAndDiverged {
            comparison_tier: ComparisonTier::CanonicalBitwise,
            comparison: comparison.clone(),
            bitwise_parity: false,
            compared_log_messages: RequiredNullable::Value(compared_log_messages.clone()),
        });
    }
    if reports.is_empty() || unavailable_reason.is_some() {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: unavailable_reason
                .unwrap_or_else(|| "cell emitted no typed verification report".into()),
        });
    }
    if reports.iter().any(|report| !classify(report).0) {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: "typed canonical report was neither a match nor a divergence".into(),
        });
    }
    if string(row, "outcome")? != "PASS" {
        return Ok(CellVerdict::UnavailableWithReason {
            comparison_tier: ComparisonTier::DeclaredButUnverifiable,
            reason: row
                .get("reason")
                .and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("cell outcome was not PASS despite matched comparison evidence")
                .into(),
        });
    }
    let (_, comparison, compared_log_messages) = reports.last().expect("nonempty reports");
    Ok(CellVerdict::ComparedAndMatched {
        comparison_tier: ComparisonTier::CanonicalBitwise,
        comparison: comparison.clone(),
        bitwise_parity: true,
        compared_log_messages: RequiredNullable::Value(compared_log_messages.clone()),
    })
}

fn sort_key(value: &Value) -> Result<CellIdentity, String> {
    identity(value)
}

pub fn expected_plan(repo_root: &Path) -> Result<Vec<Value>, String> {
    let path = repo_root.join("ci/expected-e2e-plan.json");
    let document: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read expected plan {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("expected plan {} is malformed: {error}", path.display()))?;
    document
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("expected plan {} has no cells array", path.display()))?
        .iter()
        .map(|cell| identity(cell).and_then(|identity| identity_value(&identity)))
        .collect()
}

fn string_set(value: &Value, key: &str) -> Result<BTreeSet<String>, String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("test-binary registration has no {key} array"))?
        .iter()
        .map(|item| {
            item.as_str()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("test-binary registration {key} contains a non-name"))
        })
        .collect()
}

fn enabled_cell_scope(cell: &Value) -> Result<Value, String> {
    let mut scoped = identity_value(&identity(cell)?)?
        .as_object()
        .cloned()
        .ok_or("cell identity was not an object")?;
    for key in ["status", "measurement", "reason", "last_tested", "observations"] {
        if let Some(value) = cell.get(key) {
            scoped.insert(key.into(), value.clone());
        }
    }
    let mut passes = 0u64;
    let mut failures = 0u64;
    let mut other = BTreeMap::<String, u64>::new();
    for result in cell
        .get("observations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|observation| observation.get("results").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
    {
        match result {
            "pass" => passes += 1,
            "fail" => failures += 1,
            value => *other.entry(value.to_string()).or_default() += 1,
        }
    }
    scoped.insert("observed_pass_count".into(), serde_json::json!(passes));
    scoped.insert("observed_fail_count".into(), serde_json::json!(failures));
    scoped.insert("observed_other_results".into(), serde_json::json!(other));
    Ok(Value::Object(scoped))
}

fn coverage_document(
    plan_name: &str,
    selection_mode: &str,
    planned_nodes: &BTreeSet<String>,
    planned_test_nodes: &BTreeSet<String>,
    test_node_coverage: &Value,
    selected: &[Value],
    cells_document: &Value,
    registration: &Value,
) -> Result<Value, String> {
    let selected: BTreeMap<_, _> = selected
        .iter()
        .map(|cell| Ok((sort_key(cell)?, cell.clone())))
        .collect::<Result<_, String>>()?;
    let enabled: BTreeMap<_, _> = cells_document
        .get("cells")
        .and_then(Value::as_array)
        .ok_or("ci/compat-envelope/cells.json has no cells array")?
        .iter()
        .filter(|cell| cell.get("enabled").and_then(Value::as_bool) == Some(true))
        .map(|cell| {
            let id = identity(cell)?;
            Ok((id, enabled_cell_scope(cell)?))
        })
        .collect::<Result<_, String>>()?;
    let selected_and_enabled = selected.keys().filter(|key| enabled.contains_key(*key)).count();
    let enabled_not_selected: Vec<Value> = enabled
        .iter()
        .filter(|(key, _)| !selected.contains_key(*key))
        .map(|(_, value)| value.clone())
        .collect();
    let selected_not_enabled: Vec<Value> = selected
        .iter()
        .filter(|(key, _)| !enabled.contains_key(*key))
        .map(|(_, value)| value.clone())
        .collect();

    if registration.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("test-binary registration has unsupported schema".into());
    }
    let present = string_set(registration, "present")?;
    let ci_registered = string_set(registration, "ci_registered")?;
    let none_recorded = string_set(registration, "none_recorded")?;
    let undeclared = string_set(registration, "undeclared")?;
    let reason_rows = registration
        .get("reason_recorded")
        .and_then(Value::as_array)
        .ok_or("test-binary registration has no reason_recorded array")?;
    let reason_recorded: BTreeSet<String> = reason_rows
        .iter()
        .map(|row| string(row, "binary").map(str::to_string))
        .collect::<Result<_, _>>()?;
    let accounted: BTreeSet<String> = ci_registered
        .iter()
        .chain(reason_recorded.iter())
        .chain(none_recorded.iter())
        .chain(undeclared.iter())
        .cloned()
        .collect();
    if accounted != present
        || !ci_registered.is_disjoint(&reason_recorded)
        || !ci_registered.is_disjoint(&none_recorded)
        || !ci_registered.is_disjoint(&undeclared)
        || !reason_recorded.is_disjoint(&none_recorded)
        || !reason_recorded.is_disjoint(&undeclared)
        || !none_recorded.is_disjoint(&undeclared)
    {
        return Err("test-binary registration does not form an exact partition".into());
    }

    Ok(serde_json::json!({
        "schema": 1,
        "plan": {
            "name": plan_name,
            "selection_mode": selection_mode,
            "outer_node_count": planned_nodes.len(),
            "outer_nodes": planned_nodes,
        },
        "test_nodes": {
            "planned": planned_test_nodes,
            "coverage": test_node_coverage,
        },
        "e2e": {
            "selected_count": selected.len(),
            "enabled_count": enabled.len(),
            "selected_and_enabled_count": selected_and_enabled,
            "enabled_not_selected_count": enabled_not_selected.len(),
            "selected_not_enabled_count": selected_not_enabled.len(),
            "selected": selected.into_values().collect::<Vec<_>>(),
            "enabled_not_selected": enabled_not_selected,
            "selected_not_enabled": selected_not_enabled,
        },
        "integration_test_binaries": registration,
    }))
}

/// Retain the exact test population around a full run, including work outside
/// the selected set. This is reporting, not an exemption: a reader can tell a
/// selected cell from an enabled cell that ordinary validation never selected,
/// and can see every integration-test binary outside the CI DAG.
pub fn retain_coverage_evidence(
    parent: &Path,
    repo_root: &Path,
    run_id: &str,
    commit: &str,
    plan_name: &str,
    selection_mode: &str,
    planned_nodes: &BTreeSet<String>,
    planned_test_nodes: &BTreeSet<String>,
    test_node_coverage: &Value,
    selected: &[Value],
) -> Result<RetainedCoverageEvidence, String> {
    let cells_path = repo_root.join("ci/compat-envelope/cells.json");
    let cells_document: Value = serde_json::from_slice(
        &fs::read(&cells_path)
            .map_err(|error| format!("cannot read {}: {error}", cells_path.display()))?,
    )
    .map_err(|error| format!("{} is malformed: {error}", cells_path.display()))?;
    let audit = repo_root.join("ci/audit-test-binary-registration.py");
    let output = std::process::Command::new("python3")
        .arg(&audit)
        .arg("--root")
        .arg(repo_root)
        .arg("--json")
        .output()
        .map_err(|error| format!("cannot execute {}: {error}", audit.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} --json exited {}; {}",
            audit.display(),
            output.status.code().map_or_else(|| "by signal".into(), |code| code.to_string()),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let registration: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("test-binary registration JSON is malformed: {error}"))?;
    let document = coverage_document(
        plan_name,
        selection_mode,
        planned_nodes,
        planned_test_nodes,
        test_node_coverage,
        selected,
        &cells_document,
        &registration,
    )?;
    let artifact_dir = parent.join("ignored").join("validate").join("artifacts").join(run_id);
    fs::create_dir_all(&artifact_dir).map_err(|error| {
        format!("cannot create retained coverage directory {}: {error}", artifact_dir.display())
    })?;
    let artifact = artifact_dir.join("coverage.json");
    let mut bytes = serde_json::to_vec_pretty(&document)
        .map_err(|error| format!("cannot encode coverage evidence: {error}"))?;
    bytes.push(b'\n');
    fs::write(&artifact, &bytes)
        .map_err(|error| format!("cannot publish {}: {error}", artifact.display()))?;
    let relative = artifact
        .strip_prefix(parent)
        .map_err(|_| "retained coverage artifact is outside parent root")?
        .to_string_lossy()
        .into_owned();
    let e2e = document.get("e2e").expect("constructed e2e scope");
    let binaries = document
        .get("integration_test_binaries")
        .expect("constructed integration-test scope");
    let mut evidence = test_node_coverage
        .as_object()
        .cloned()
        .ok_or("test-node coverage was not an object")?;
    evidence.insert("schema".into(), serde_json::json!(1));
    evidence.insert("run_id".into(), serde_json::json!(run_id));
    evidence.insert("hermit_sha".into(), serde_json::json!(commit));
    evidence.insert("plan".into(), document["plan"].clone());
    evidence.insert("test_nodes".into(), document["test_nodes"].clone());
    evidence.insert(
        "e2e".into(),
        serde_json::json!({
            "selected_count": e2e["selected_count"],
            "enabled_count": e2e["enabled_count"],
            "selected_and_enabled_count": e2e["selected_and_enabled_count"],
            "enabled_not_selected_count": e2e["enabled_not_selected_count"],
            "selected_not_enabled_count": e2e["selected_not_enabled_count"],
        }),
    );
    evidence.insert(
        "integration_test_binaries".into(),
        serde_json::json!({
            "present_count": binaries["present"].as_array().map_or(0, Vec::len),
            "ci_registered_count": binaries["ci_registered"].as_array().map_or(0, Vec::len),
            "reason_recorded_count": binaries["reason_recorded"].as_array().map_or(0, Vec::len),
            "none_recorded_count": binaries["none_recorded"].as_array().map_or(0, Vec::len),
            "undeclared_count": binaries["undeclared"].as_array().map_or(0, Vec::len),
        }),
    );
    evidence.insert(
        "artifact".into(),
        serde_json::json!({
            "path": relative,
            "sha256": hex_digest(&bytes),
        }),
    );
    Ok(RetainedCoverageEvidence { evidence: Value::Object(evidence) })
}

/// Transform historical result rows for one validate invocation into the
/// closed schema-7 cell-verdict artifact and summary used by ci-hub.
///
/// Current rows that carry retained verify logs require
/// [`retain_with_policy`]. Keeping this entrypoint unable to guess a storage
/// limit prevents a new descriptor from disappearing into a schema-7 receipt.
pub fn retain(
    parent: &Path,
    result_root: &Path,
    commit: &str,
    expected: &[Value],
) -> Result<RetainedCellResults, String> {
    retain_inner(parent, result_root, commit, expected, None, None)
}

/// Retain cell results plus every harness-managed verify gzip under one shared
/// aggregate compressed-byte policy.
///
/// `scripts/validate.rs` must pass the same policy used by the harness-wide
/// [`VerifyLogRetentionPolicy`] and the image digest captured for the command
/// passed to `run-in-pinned-root.sh` when the split-run scheduler is activated.
/// This function deliberately does not reread `ci/hermetic/image.digest` after
/// execution. No independent default belongs at this boundary.
#[allow(dead_code)] // Activated with the harness-managed verify scheduler and its shared policy.
pub fn retain_with_policy(
    parent: &Path,
    result_root: &Path,
    commit: &str,
    expected: &[Value],
    validate_path: ValidatePath,
    hermetic_image_digest: &str,
    policy: VerifyLogRetentionPolicy,
) -> Result<RetainedCellResults, String> {
    if !is_canonical_hermetic_image_digest(hermetic_image_digest) {
        return Err("retained verify logs require a canonical hermetic_image_digest".into());
    }
    retain_inner(
        parent,
        result_root,
        commit,
        expected,
        Some((validate_path, hermetic_image_digest, policy)),
        None,
    )
}

fn retain_inner(
    parent: &Path,
    result_root: &Path,
    commit: &str,
    expected: &[Value],
    verify_log_policy: Option<(ValidatePath, &str, VerifyLogRetentionPolicy)>,
    failure: Option<RetentionFailurePoint>,
) -> Result<RetainedCellResults, String> {
    let mut run_id: Option<String> = None;
    let mut has_schema5 = false;
    let mut schema4_verify_row: Option<(PathBuf, usize)> = None;
    let mut selected = Vec::new();
    let mut identities = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut attempt_rows: BTreeMap<CellIdentity, Vec<(u64, Value)>> = BTreeMap::new();
    let mut has_retained_verify_log = false;
    let source_rows = read_result_rows(result_root)?;
    for (file, line_number, row) in &source_rows {
        let schema = row.get("schema").and_then(Value::as_u64).unwrap_or(0);
        if !matches!(
            schema,
            CELL_RESULT_SCHEMA | RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA
        )
            || string(row, "hermit_sha")? != commit
            || row.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
        {
            return Err(format!(
                "{}:{line_number} is not an exact clean supported cell result for {commit}",
                file.display()
            ));
        }
        let retained_descriptor_count = row
            .get("attempts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|attempt| {
                attempt
                    .get("retained_verify_log")
                    .is_some_and(|value| !value.is_null())
            })
            .count();
        let mode = string(row, "mode")?;
        match schema {
            CELL_RESULT_SCHEMA => {
                if retained_descriptor_count != 0 {
                    return Err(format!(
                        "{}:{line_number} schema-4 result cannot carry retained_verify_log",
                        file.display()
                    ));
                }
                if mode == "verify" {
                    schema4_verify_row.get_or_insert_with(|| (file.clone(), *line_number));
                }
            }
            RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA => {
                has_schema5 = true;
                if mode != "verify" {
                    return Err(format!(
                        "{}:{line_number} schema-5 result must use verify mode",
                        file.display()
                    ));
                }
                if retained_descriptor_count != 1 {
                    return Err(format!(
                        "{}:{line_number} schema-5 result requires exactly one retained_verify_log, got {retained_descriptor_count}",
                        file.display()
                    ));
                }
            }
            _ => unreachable!("the supported-schema check above admitted an unknown schema"),
        }
        has_retained_verify_log |= retained_descriptor_count != 0;
        if retained_descriptor_count != 0 && verify_log_policy.is_none() {
            return Err(
                "retained verify-log descriptors require the shared explicit retention policy; refusing to emit schema 7 and lose their binding"
                    .into(),
            );
        }
        require_current_timeout_policy(row)
            .map_err(|error| format!("{}:{line_number} {error}", file.display()))?;
        let row_run_id = string(row, "run_id")?;
        match run_id.as_deref() {
            None => run_id = Some(row_run_id.into()),
            Some(existing) if existing == row_run_id => {}
            Some(existing) => {
                return Err(format!(
                    "per-cell results mix run_id {existing} with {row_run_id}"
                ));
            }
        }
        let id = identity(row)?;
        let key = id.clone();
        let attempt = row.get("attempt").and_then(Value::as_u64).unwrap_or(1);
        if attempt == 0 {
            return Err("per-cell result attempt must be positive".into());
        }
        if !observations.insert((key.clone(), attempt)) {
            return Err("per-cell results contain a duplicate identity and attempt".into());
        }
        if identities.insert(key.clone()) {
            selected.push(id);
        }
        attempt_rows
            .entry(key)
            .or_default()
            .push((attempt, row.clone()));
    }
    // Dormant cutover rule: an all-schema-4 population retains its historical
    // meaning. Once any harness-managed verify result uses schema 5, every
    // verify row in that same population must use schema 5 and every schema-4
    // row is therefore a non-verify cell. This prevents a partial cutover from
    // silently mixing retained and hidden verify evidence.
    if has_schema5 {
        if let Some((file, line_number)) = schema4_verify_row {
            return Err(format!(
                "{}:{line_number} schema-4 verify result is not allowed in a population containing schema-5 retained verify logs",
                file.display()
            ));
        }
    }
    let mut cells = attempt_rows
        .into_iter()
        .map(|(identity, mut rows)| {
            rows.sort_by_key(|(attempt, _)| *attempt);
            let outcome = outcome_after_retries(rows.iter().map(|(attempt, row)| {
                Ok((*attempt, string(row, "outcome")?))
            }).collect::<Result<Vec<_>, String>>()?)?;
            let row = rows
                .iter()
                .rev()
                .find(|(_, row)| row.get("outcome").and_then(Value::as_str) == Some(outcome))
                .map(|(_, row)| row)
                .ok_or_else(|| {
                    format!(
                        "cell result history selected {outcome} without a matching row for {identity:?}"
                    )
                })?;
            Ok(LedgerCellResult {
                lane: identity.lane,
                category: identity.category,
                test: identity.test,
                mode: identity.mode,
                backend: identity.backend,
                cell_verdict: cell_verdict(row)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let run_id = run_id.ok_or("full validation retained zero per-cell result rows")?;
    let verify_logs = if has_retained_verify_log || verify_log_policy.is_some() {
        collect_verify_log_sources(result_root, &run_id, &source_rows)?
    } else {
        Vec::new()
    };
    selected.sort();
    cells.sort_by_key(|cell| CellIdentity {
        lane: cell.lane.clone(),
        category: cell.category.clone(),
        test: cell.test.clone(),
        mode: cell.mode.clone(),
        backend: cell.backend.clone(),
    });
    let mut expected = expected
        .iter()
        .map(identity)
        .collect::<Result<Vec<_>, String>>()?;
    expected.sort();
    if selected != expected {
        let observed_keys = selected.iter().collect::<BTreeSet<_>>();
        let expected_keys = expected.iter().collect::<BTreeSet<_>>();
        let missing = expected_keys.difference(&observed_keys).count();
        let extra = observed_keys.difference(&expected_keys).count();
        return Err(format!(
            "per-cell results differ from the exact planned population: {missing} missing, {extra} extra"
        ));
    }
    let selected_values = selected
        .iter()
        .map(identity_value)
        .collect::<Result<Vec<_>, String>>()?;
    let population_bytes = serde_json::to_vec(&selected_values)
        .map_err(|error| format!("cannot encode selected cell population: {error}"))?;
    let artifact_relative = PathBuf::from("ignored")
        .join("validate")
        .join("artifacts")
        .join(&run_id);
    let artifact_dir = create_plain_directory_path_below(
        parent,
        &artifact_relative,
        "retained validation artifact directory",
        failure,
    )?;
    let artifact = artifact_dir.join("cell-results.jsonl");
    if fs::symlink_metadata(&artifact).is_ok() {
        return Err(format!(
            "retained cell artifact already exists: {}",
            artifact.display()
        ));
    }
    let mut artifact_bytes = Vec::new();
    for cell in &cells {
        let mut record = serde_json::to_value(cell)
            .map_err(|error| format!("cannot encode retained cell row: {error}"))?
            .as_object()
            .cloned()
            .ok_or("shared cell result did not serialize as an object")?;
        record.insert("run_id".into(), Value::String(run_id.clone()));
        record.insert("hermit_sha".into(), Value::String(commit.into()));
        record.insert("source_tree_dirty".into(), Value::Bool(false));
        serde_json::to_writer(&mut artifact_bytes, &record)
            .map_err(|error| format!("cannot encode retained cell row: {error}"))?;
        artifact_bytes.push(b'\n');
    }
    let relative = artifact
        .strip_prefix(parent)
        .map_err(|_| "retained cell artifact is outside parent root")?
        .to_string_lossy()
        .into_owned();
    let recorded_count = u64::try_from(cells.len())
        .map_err(|_| "retained cell count does not fit the ledger type")?;
    let selected_count = u64::try_from(selected.len())
        .map_err(|_| "selected cell count does not fit the ledger type")?;
    let cell_artifact = CellResultsArtifact {
        path: relative,
        sha256: hex_digest(&artifact_bytes),
        row_count: recorded_count,
    };
    let historical_evidence = CellResultsEvidence {
        run_id: run_id.clone(),
        hermit_sha: commit.into(),
        source_tree_dirty: false,
        selected_count,
        recorded_count,
        population_sha256: hex_digest(&population_bytes),
        artifact: cell_artifact.clone(),
        selected: selected.clone(),
        cells: cells.clone(),
    };
    let (schema_version, evidence, published_verify_logs) = match verify_log_policy {
        Some((validate_path, hermetic_image_digest, policy)) if !verify_logs.is_empty() => {
            let schema10_population_bytes = serde_json::to_vec(&selected).map_err(|error| {
                format!("cannot encode schema-10 selected cell population: {error}")
            })?;
            let retained_verify_logs =
                publish_retained_verify_logs(parent, &artifact_dir, &verify_logs, policy, failure)?;
            let evidence = serde_json::to_value(CellResultsEvidenceV10 {
                path: validate_path,
                run_id: run_id.clone(),
                hermit_sha: commit.into(),
                source_tree_dirty: false,
                hermetic_image_digest: hermetic_image_digest.into(),
                selected_count,
                recorded_count,
                population_sha256: hex_digest(&schema10_population_bytes),
                artifact: cell_artifact,
                retained_verify_logs,
                selected,
                cells,
            });
            let evidence = match evidence {
                Ok(evidence) => evidence,
                Err(error) => {
                    let error = format!("cannot encode schema-10 cell_results evidence: {error}");
                    return match remove_published_verify_logs(&artifact_dir) {
                        Ok(()) => Err(error),
                        Err(cleanup_error) => Err(format!(
                            "{error}; retained verify-log cleanup also failed: {cleanup_error}"
                        )),
                    };
                }
            };
            (
                RETAINED_VERIFY_LOGS_LEDGER_SCHEMA_VERSION,
                evidence,
                true,
            )
        }
        _ => (
            CELL_RESULTS_LEDGER_SCHEMA_VERSION,
            serde_json::to_value(historical_evidence)
                .map_err(|error| format!("cannot encode cell_results evidence: {error}"))?,
            false,
        ),
    };
    if failure == Some(RetentionFailurePoint::BeforeCellResultPublication) {
        let error = "injected failure before retained cell-result publication".to_string();
        if published_verify_logs {
            return match remove_published_verify_logs(&artifact_dir) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; retained verify-log cleanup also failed: {cleanup_error}"
                )),
            };
        }
        return Err(error);
    }
    if let Err(error) = publish_file_noclobber(&artifact, &artifact_bytes, "retained cell artifact") {
        if published_verify_logs {
            return match remove_published_verify_logs(&artifact_dir) {
                Ok(()) => Err(error),
                Err(cleanup_error) => Err(format!(
                    "{error}; retained verify-log cleanup also failed: {cleanup_error}"
                )),
            };
        }
        return Err(error);
    }
    Ok(RetainedCellResults {
        schema_version,
        run_id,
        evidence,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture_root() -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("validate-cell-results-{}-{id}", std::process::id()))
    }

    fn report(verdict: &str, log_scope: &str) -> String {
        let matched = verdict == "matched";
        serde_json::json!({
            "verified": matched,
            "verdict": verdict,
            "bitwise_parity": matched,
            "infrastructure_error": null,
            "comparison": {
                "strictness": "canonical",
                "display_name": "BitwiseInfoV1",
                "compare_logs": true,
                "compare_io_buffers": true,
                "log_scope": log_scope,
                "record_envelope": "all_records_v1",
                "virtualize_time": true,
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
            "compared_log_messages": {"left": 123, "right": if matched { 123 } else { 124 }},
            "guest_exit_code": 0,
            "guest_signal": null,
            "first_divergent_scheduler_turn": null,
            "first_divergent_virtual_nanoseconds": null,
            "first_divergent_record": null,
            "first_divergent_syscall": null,
            "first_divergent_left_message": null,
            "first_divergent_right_message": null
        })
        .to_string()
    }

    fn replace_report(row: &mut Value, report: &Value) {
        let raw = serde_json::to_string(report).unwrap();
        row["attempts"][0] = attempt(&raw);
    }

    fn attempt(raw: &str) -> Value {
        serde_json::json!({
            "verification_report": raw,
            "verification_report_sha256": hex_digest(raw.as_bytes())
        })
    }

    fn result_row(run_id: &str, commit: &str) -> Value {
        let matched = report("matched", "info");
        serde_json::json!({
            "schema": CELL_RESULT_SCHEMA,
            "run_id": run_id,
            "hermit_sha": commit,
            "source_tree_dirty": false,
            "lane": "portable",
            "category": "c-programs",
            "test": "uname",
            "mode": "verify",
            "backend": "ptrace",
            "outcome": "PASS",
            "reason": null,
            "timeout_seconds": 57,
            "execution_cpu_timeout_seconds": 22,
            "execution_wall_timeout_seconds": 57,
            "attempts": [attempt(&matched)]
        })
    }

    const RETAINED_LOG_BODY: &[u8] =
        b"Apr 09 06:08:01.100  INFO detcore: DETLOG retained\n";
    const HERMETIC_IMAGE_DIGEST: &str = concat!(
        "localhost/hermit-hermetic-validate@sha256:",
        "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
    );
    const RETAINED_LOG_GZIP: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x73, 0x2c, 0x28, 0x52,
        0x30, 0xb0, 0x54, 0x30, 0x30, 0xb3, 0x32, 0xb0, 0xb0, 0x32, 0x30, 0xd4, 0x33, 0x34,
        0x30, 0x50, 0x50, 0xf0, 0xf4, 0x73, 0xf3, 0x57, 0x48, 0x49, 0x2d, 0x49, 0xce, 0x2f,
        0x4a, 0xb5, 0x52, 0x70, 0x71, 0x0d, 0xf1, 0xf1, 0x77, 0x57, 0x28, 0x4a, 0x2d, 0x49,
        0xcc, 0xcc, 0x4b, 0x4d, 0xe1, 0x02, 0x00, 0x28, 0x0b, 0x31, 0xf5, 0x33, 0x00, 0x00,
        0x00,
    ];

    fn add_retained_verify_log(result_root: &Path, row: &mut Value) -> PathBuf {
        row["schema"] = Value::from(RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA);
        row["attempt"] = Value::from(1);
        let run_id = row["run_id"].as_str().unwrap();
        let slug = format!(
            "{}-{}-{}",
            row["test"].as_str().unwrap().replace('/', "-"),
            row["mode"].as_str().unwrap(),
            row["backend"].as_str().unwrap()
        );
        let artifact_dir = result_root.join("runs").join(run_id).join(slug);
        let relative = PathBuf::from("retained/verify/1/run-1.log.gz");
        let source = artifact_dir.join(&relative);
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, RETAINED_LOG_GZIP).unwrap();
        row["artifact_dir"] = Value::String(artifact_dir.to_string_lossy().into_owned());
        row["attempts"][0]["retained_verify_log"] =
            serde_json::to_value(RetainedVerifyLog {
                relative_path: relative.to_string_lossy().into_owned(),
                role: hermit_manifest_plan::runner::RetainedVerifyLogRole::Run1,
                cell_id: CellId {
                    test: row["test"].as_str().unwrap().into(),
                    mode: "verify".into(),
                    backend: Some(row["backend"].as_str().unwrap().into()),
                },
                attempt: 1,
                uncompressed_sha256:
                    "88c093a87fc7c41d677e0fd785fe0c0c1417f5d0af1c2721a397d8ba9970c054"
                        .into(),
                uncompressed_bytes: 51,
                compressed_sha256:
                    "57163e2d716668da05d566bb6a489310f4cf79e286937ef2f8308b91df83de35"
                        .into(),
                compressed_bytes: 71,
                peer_uncompressed_sha256:
                    "88c093a87fc7c41d677e0fd785fe0c0c1417f5d0af1c2721a397d8ba9970c054"
                        .into(),
                peer_uncompressed_bytes: 51,
                compared_info_messages: 123,
            })
            .unwrap();
        source
    }

    fn write_harness_verify_run(spec: &hermit_manifest_plan::runner::VerifyRunSpec) {
        use hermit_manifest_plan::canonical_verdict::CapturedGuestStream;
        use hermit_manifest_plan::canonical_verdict::GuestDisposition;
        use hermit_manifest_plan::canonical_verdict::GuestRunResult;

        let empty_sha256 = hex_digest(&[]);
        let result = GuestRunResult {
            schema: GuestRunResult::SCHEMA,
            disposition: GuestDisposition::Exited { code: 0 },
            determinism: spec.expected_determinism(),
            stdout: CapturedGuestStream {
                bytes: 0,
                sha256: empty_sha256.clone(),
            },
            stderr: CapturedGuestStream {
                bytes: 0,
                sha256: empty_sha256,
            },
        };
        fs::write(&spec.paths.result, serde_json::to_vec(&result).unwrap()).unwrap();
        fs::write(&spec.paths.stdout, []).unwrap();
        fs::write(&spec.paths.stderr, []).unwrap();
        let structured_log = format!(
            "{} DETLOG_RECORD={{\"schema\":1,\"event\":{{\"kind\":\"other\"}}}}\n",
            std::str::from_utf8(RETAINED_LOG_BODY).unwrap().trim_end()
        );
        fs::write(&spec.paths.log, structured_log).unwrap();
        fs::write(
            &spec.paths.summary,
            serde_json::to_vec(&serde_json::json!({
                "sched_turns": 1,
                "schedevent_replayed": 0,
                "schedevent_recorded": 0,
                "schedevent_desynced": 0,
                "desync_descrip": null,
                "reprio_descrip": null,
                "threads_descrip": "[1]",
                "num_processes": 1,
                "num_threads": 1,
                "syscalls": 1,
                "virttime_elapsed": 1,
                "virttime_final": 1,
                "realtime_elapsed": null,
                "timeslice_stats": {"count": 0, "sum_ns": 0, "min_ns": 0, "max_ns": 0},
                "per_thread_timeslice": []
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn current_timeout_policy_is_required_without_breaking_legacy_raw_reads() {
        let commit = "1515151515151515151515151515151515151515";
        for (label, remove, replacement, expected_error) in [
            (
                "missing",
                Some("execution_cpu_timeout_seconds"),
                None,
                "omitted execution_cpu_timeout_seconds",
            ),
            (
                "half-present",
                Some("execution_wall_timeout_seconds"),
                None,
                "omitted execution_wall_timeout_seconds",
            ),
            (
                "wrong-value",
                None,
                Some(("execution_wall_timeout_seconds", 56_u64)),
                "timeout policy disagrees",
            ),
        ] {
            let root = fixture_root();
            let results = root.join("results");
            let mut row = result_row(&format!("validate-{label}"), commit);
            let object = row.as_object_mut().unwrap();
            if let Some(field) = remove {
                object.remove(field);
            }
            if let Some((field, value)) = replacement {
                object.insert(field.into(), Value::from(value));
            }
            write_result(&results, &row);
            assert_eq!(
                all_result_rows(&results).unwrap().len(),
                1,
                "legacy raw-row reading must retain additive-field compatibility"
            );
            let error = retain(&root, &results, commit, &expected(&row))
                .expect_err("a malformed current timeout policy reached the ledger");
            assert!(error.contains(expected_error), "{label}: {error}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    fn expected(row: &Value) -> Vec<Value> {
        vec![identity_value(&identity(row).unwrap()).unwrap()]
    }

    fn write_result(root: &Path, row: &Value) {
        let directory = root.join("bucket");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("results.jsonl"),
            format!("{}\n", serde_json::to_string(row).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn coverage_separates_selected_from_enabled_but_not_selected() {
        let selected = vec![
            serde_json::json!({
                "lane":"portable", "category":"c-programs", "test":"c-programs/a",
                "mode":"verify", "backend":"ptrace"
            }),
            serde_json::json!({
                "lane":"portable", "category":"c-programs", "test":"c-programs/custom",
                "mode":"custom", "backend":"ptrace"
            }),
        ];
        let cells = serde_json::json!({"cells":[
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/a",
                "mode":"verify", "backend":"ptrace", "enabled":true
            },
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/b",
                "mode":"verify", "backend":"ptrace", "enabled":true,
                "status":"red", "measurement":"measured-and-failed",
                "reason":"excluded after the recorded observations",
                "observations":[{"results":["pass", "fail", "fail"]}]
            },
            {
                "lane":"portable", "category":"c-programs", "test":"c-programs/custom",
                "mode":"custom", "backend":"ptrace", "enabled":false
            }
        ]});
        let registration = serde_json::json!({
            "schema":1,
            "present":["covered", "unknown"],
            "ci_registered":["covered"],
            "reason_recorded":[],
            "none_recorded":["unknown"],
            "undeclared":[]
        });
        let planned_nodes = BTreeSet::from([
            "check.example".to_string(),
            "test.example".to_string(),
        ]);
        let planned_test_nodes = BTreeSet::from(["test.example".to_string()]);
        let test_node_coverage = serde_json::json!({
            "planned_test_nodes": 1,
            "executed_test_nodes": 1,
            "zero_executed_nodes": [],
            "absent_nodes": [],
        });
        let scope = coverage_document(
            "full",
            "full",
            &planned_nodes,
            &planned_test_nodes,
            &test_node_coverage,
            &selected,
            &cells,
            &registration,
        )
        .unwrap();
        assert_eq!(scope["plan"]["name"], "full");
        assert_eq!(scope["plan"]["selection_mode"], "full");
        assert_eq!(scope["plan"]["outer_node_count"], 2);
        assert_eq!(scope["plan"]["outer_nodes"][0], "check.example");
        assert_eq!(scope["plan"]["outer_nodes"][1], "test.example");
        assert_eq!(scope["test_nodes"]["planned"][0], "test.example");
        assert_eq!(scope["test_nodes"]["coverage"], test_node_coverage);
        assert_eq!(scope["e2e"]["selected_count"], 2);
        assert_eq!(scope["e2e"]["enabled_count"], 2);
        assert_eq!(scope["e2e"]["selected_and_enabled_count"], 1);
        assert_eq!(scope["e2e"]["enabled_not_selected_count"], 1);
        assert_eq!(scope["e2e"]["selected_not_enabled_count"], 1);
        assert_eq!(scope["e2e"]["enabled_not_selected"][0]["observed_pass_count"], 1);
        assert_eq!(scope["e2e"]["enabled_not_selected"][0]["observed_fail_count"], 2);
        assert_eq!(
            scope["e2e"]["enabled_not_selected"][0]["reason"],
            "excluded after the recorded observations"
        );
        assert_eq!(scope["integration_test_binaries"]["ci_registered"][0], "covered");
        assert_eq!(scope["integration_test_binaries"]["none_recorded"][0], "unknown");
    }

    fn append_result_row(root: &Path, row: &Value) {
        use std::io::Write;

        let directory = root.join("bucket");
        fs::create_dir_all(&directory).unwrap();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("results.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(row).unwrap()).unwrap();
    }

    #[test]
    fn retains_one_closed_schema7_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1111111111111111111111111111111111111111";
        let row = result_row("validate-one", commit);
        write_result(&results, &row);
        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        assert_eq!(retained.schema_version, 7);
        assert_eq!(retained.run_id, "validate-one");
        assert_eq!(retained.evidence["selected_count"], 1);
        assert_eq!(retained.evidence["recorded_count"], 1);
        assert_eq!(
            retained.evidence["population_sha256"],
            "6134692d215e0bd6a4da5514206e3670a377cf32dae1b507453560e62cb41557"
        );
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        let expected_comparison: Value =
            serde_json::from_str::<Value>(&report("matched", "info")).unwrap()["comparison"]
                .clone();
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["comparison"],
            expected_comparison
        );
        let artifact = root.join(retained.evidence["artifact"]["path"].as_str().unwrap());
        let bytes = fs::read(&artifact).unwrap();
        assert_eq!(hex_digest(&bytes), retained.evidence["artifact"]["sha256"]);
        assert!(bytes.ends_with(b"\n"));
        let artifact_row = serde_json::json!({
            "run_id": "validate-one",
            "hermit_sha": commit,
            "source_tree_dirty": false,
            "lane": "portable",
            "category": "c-programs",
            "test": "uname",
            "mode": "verify",
            "backend": "ptrace",
            "cell_verdict": {
                "state": "compared-and-matched",
                "comparison_tier": "canonical-bitwise",
                "comparison": expected_comparison,
                "bitwise_parity": true,
                "compared_log_messages": {"left": 123, "right": 123}
            }
        });
        let mut expected_bytes = serde_json::to_vec(&artifact_row).unwrap();
        expected_bytes.push(b'\n');
        assert_eq!(bytes, expected_bytes, "the shared type must preserve artifact bytes");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_refuses_a_missing_comparison_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1212121212121212121212121212121212121212";
        let mut row = result_row("validate-missing-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report["comparison"]
            .as_object_mut()
            .unwrap()
            .remove("virtualize_time");
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(retained.schema_version, 7);
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict.get("comparison").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_names_a_missing_current_verification_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1515151515151515151515151515151515151515";
        let mut row = result_row("validate-missing-report-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report.as_object_mut().unwrap().remove("first_divergent_record");
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict["reason"]
            .as_str()
            .unwrap()
            .contains("missing current producer field `first_divergent_record`"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_refuses_an_unknown_comparison_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1313131313131313131313131313131313131313";
        let mut row = result_row("validate-unknown-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report["comparison"]["future_comparison_field"] = Value::Bool(true);
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(retained.schema_version, 7);
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict.get("comparison").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema7_refuses_an_unknown_compared_log_messages_field() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1414141414141414141414141414141414141414";
        let mut row = result_row("validate-unknown-count-field", commit);
        let mut report: Value = serde_json::from_str(&report("matched", "info")).unwrap();
        report["compared_log_messages"]["future_count_field"] = Value::from(123);
        replace_report(&mut row, &report);
        write_result(&results, &row);

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        let verdict = &retained.evidence["cells"][0]["cell_verdict"];
        assert_eq!(retained.schema_version, 7);
        assert_eq!(verdict["state"], "unavailable-with-reason");
        assert!(verdict.get("compared_log_messages").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retains_recovered_retry_without_rejecting_preserved_history() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "abababababababababababababababababababab";
        let mut first = result_row("validate-retry", commit);
        first["attempt"] = Value::from(1);
        first["outcome"] = Value::String("FAIL".into());
        first["reason"] = Value::String("forced first failure".into());
        first["duration_ms"] = Value::from(111);
        first["timeout_seconds"] = Value::from(15);
        first["execution_cpu_timeout_seconds"] = Value::from(10);
        first["execution_wall_timeout_seconds"] = Value::from(15);
        let mut second = result_row("validate-retry", commit);
        second["attempt"] = Value::from(2);
        second["duration_ms"] = Value::from(222);
        second["timeout_seconds"] = Value::from(15);
        second["execution_cpu_timeout_seconds"] = Value::from(10);
        second["execution_wall_timeout_seconds"] = Value::from(15);
        append_result_row(&results, &first);
        append_result_row(&results, &second);

        let retained = retain(&root, &results, commit, &expected(&second)).unwrap();
        assert_eq!(retained.evidence["recorded_count"], 1);
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        let raw = fs::read_to_string(results.join("bucket/results.jsonl")).unwrap();
        let observations = raw
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0]["attempt"], 1);
        assert_eq!(observations[0]["duration_ms"], 111);
        assert_eq!(observations[0]["timeout_seconds"], 15);
        assert_eq!(observations[1]["attempt"], 2);
        assert_eq!(observations[1]["duration_ms"], 222);
        assert_eq!(observations[1]["timeout_seconds"], 15);
        let history_rows = all_result_rows(&results).unwrap();
        assert_eq!(history_rows, observations);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn product_failure_remains_red_when_the_retry_has_an_infrastructure_error() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
        let mut first = result_row("validate-product-then-infrastructure", commit);
        first["attempt"] = Value::from(1);
        first["outcome"] = Value::String("FAIL".into());
        let diverged: Value = serde_json::from_str(&report("diverged", "info")).unwrap();
        replace_report(&mut first, &diverged);
        let mut second = result_row("validate-product-then-infrastructure", commit);
        second["attempt"] = Value::from(2);
        second["outcome"] = Value::String("ERROR".into());
        second["reason"] = Value::String("runner timed out before producing evidence".into());
        append_result_row(&results, &first);
        append_result_row(&results, &second);

        let retained = retain(&root, &results, commit, &expected(&first)).unwrap();
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-diverged"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_retry_history_refuses_a_missing_first_attempt_by_name() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "dededededededededededededededededededede";
        let mut row = result_row("validate-missing-first-attempt", commit);
        row["attempt"] = Value::from(2);
        write_result(&results, &row);

        let error = retain(&root, &results, commit, &expected(&row)).unwrap_err();
        assert!(error.contains("attempt 2"), "{error}");
        assert!(error.contains("expected 1"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_mixed_bucket_run_ids() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "2222222222222222222222222222222222222222";
        let first = result_row("validate-one", commit);
        write_result(&results, &first);
        let second = results.join("other");
        write_result(&second, &result_row("validate-two", commit));
        let error = retain(&root, &results, commit, &expected(&first)).unwrap_err();
        assert!(error.contains("mix run_id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn top_level_failure_cannot_project_a_matched_attempt_as_clean() {
        let mut row = result_row("failed-row", "3333333333333333333333333333333333333333");
        row["outcome"] = Value::String("FAIL".into());
        row["reason"] = Value::String("SaBRe interception path was incomplete".into());
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::UnavailableWithReason { .. }));
    }

    #[test]
    fn infrastructure_error_preserves_its_cause_with_or_without_a_comparison() {
        for retain_comparison in [true, false] {
            let mut row = result_row(
                "infrastructure-row",
                "3434343434343434343434343434343434343434",
            );
            row["outcome"] = Value::String("ERROR".into());
            row["reason"] = Value::String(
                "verification recorded 2 HERMIT_SKID_OVERSHOOT report(s)".into(),
            );
            let mut infrastructure: Value =
                serde_json::from_str(&report("matched", "info")).unwrap();
            infrastructure["verified"] = Value::Bool(false);
            infrastructure["bitwise_parity"] = Value::Bool(false);
            infrastructure["verdict"] = Value::String("infrastructure_error".into());
            infrastructure["infrastructure_error"] =
                serde_json::json!({"kind": "skid_overshoot", "count": 2});
            if !retain_comparison {
                infrastructure["comparison"] = Value::Null;
                infrastructure["compared_log_messages"] = Value::Null;
            }
            replace_report(&mut row, &infrastructure);

            let CellVerdict::UnavailableWithReason { reason, .. } = cell_verdict(&row).unwrap()
            else {
                panic!("infrastructure error became product evidence")
            };
            assert!(reason.contains("2 HERMIT_SKID_OVERSHOOT"), "{reason}");
        }
    }

    #[test]
    fn earlier_divergence_cannot_be_hidden_by_a_final_match() {
        let mut row = result_row("retry-row", "4444444444444444444444444444444444444444");
        let diverged = report("diverged", "info");
        let matched = report("matched", "info");
        row["attempts"] = Value::Array(vec![attempt(&diverged), attempt(&matched)]);
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::ComparedAndDiverged { .. }));
    }

    #[test]
    fn missing_sibling_attempt_cannot_erase_a_divergence_in_either_order() {
        let diverged = report("diverged", "info");
        let missing = serde_json::json!({"outcome": "ERROR"});
        for attempts in [
            vec![attempt(&diverged), missing.clone()],
            vec![missing.clone(), attempt(&diverged)],
        ] {
            let mut row = result_row(
                "missing-sibling",
                "8888888888888888888888888888888888888888",
            );
            row["attempts"] = Value::Array(attempts);
            let verdict = cell_verdict(&row).unwrap();
            assert!(matches!(verdict, CellVerdict::ComparedAndDiverged { .. }));
        }
    }

    #[test]
    fn non_info_sibling_attempt_cannot_erase_a_divergence() {
        let diverged = report("diverged", "info");
        let weaker = report("matched", "deterministic");
        let mut row = result_row("weaker-sibling", "9999999999999999999999999999999999999999");
        row["attempts"] = Value::Array(vec![attempt(&weaker), attempt(&diverged)]);
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::ComparedAndDiverged { .. }));
    }

    #[test]
    fn non_info_scope_never_becomes_a_clean_leg() {
        let mut row = result_row("scope-row", "5555555555555555555555555555555555555555");
        let deterministic = report("matched", "deterministic");
        row["attempts"] = Value::Array(vec![attempt(&deterministic)]);
        let verdict = cell_verdict(&row).unwrap();
        assert!(matches!(verdict, CellVerdict::UnavailableWithReason { .. }));
    }

    #[test]
    fn mismatched_report_hash_refuses_the_receipt() {
        let mut row = result_row("hash-row", "7777777777777777777777777777777777777777");
        row["attempts"][0]["verification_report_sha256"] = Value::String("0".repeat(64));
        let error = cell_verdict(&row).unwrap_err();
        assert!(error.contains("verification_report_sha256 mismatch"));
    }

    #[test]
    fn missing_planned_cell_refuses_the_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "6666666666666666666666666666666666666666";
        let row = result_row("missing-row", commit);
        write_result(&results, &row);
        let mut plan = expected(&row);
        let mut missing = plan[0].clone();
        missing["test"] = Value::String("c-programs/missing".into());
        plan.push(missing);
        let error = retain(&root, &results, commit, &plan).unwrap_err();
        assert!(error.contains("1 missing, 0 extra"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema10_retains_sorted_verify_gzips_after_the_source_tree_is_deleted() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("disposable-results");
        let commit = "1010101010101010101010101010101010101010";
        let mut second = result_row("validate-retained", commit);
        second["test"] = Value::String("z-last".into());
        second["backend"] = Value::String("kvm".into());
        add_retained_verify_log(&results, &mut second);
        let mut first = result_row("validate-retained", commit);
        first["test"] = Value::String("a-first".into());
        add_retained_verify_log(&results, &mut first);
        append_result_row(&results, &second);
        append_result_row(&results, &first);

        let retained = retain_with_policy(
            &root,
            &results,
            commit,
            &[identity_value(&identity(&second).unwrap()).unwrap(), identity_value(&identity(&first).unwrap()).unwrap()],
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            VerifyLogRetentionPolicy::new(142),
        )
        .unwrap();
        assert_eq!(retained.schema_version, 10);
        assert_eq!(
            retained.evidence["hermetic_image_digest"],
            HERMETIC_IMAGE_DIGEST
        );
        assert_eq!(retained.evidence["retained_verify_logs"]["row_count"], 2);
        assert_eq!(
            retained.evidence["retained_verify_logs"]["compressed_bytes"],
            142
        );
        assert_eq!(retained.evidence["selected_count"], 2);
        let index = root.join(
            retained.evidence["retained_verify_logs"]["path"]
                .as_str()
                .unwrap(),
        );
        let index_bytes = fs::read(&index).unwrap();
        assert_eq!(
            hex_digest(&index_bytes),
            retained.evidence["retained_verify_logs"]["sha256"]
        );
        let rows = index_bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<RetainedVerifyLogIndexRow>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows[0].cell.test, "a-first");
        assert_eq!(rows[1].cell.test, "z-last");
        assert!(rows[0].artifact.path.contains("verify-logs/000001/"));
        assert!(rows[1].artifact.path.contains("verify-logs/000002/"));

        fs::remove_dir_all(&results).unwrap();
        for (ordinal, row) in rows.iter().enumerate() {
            let entry_root = index
                .parent()
                .unwrap()
                .join(format!("{:06}", ordinal + 1));
            assert_eq!(
                hermit_manifest_plan::runner::read_verified_retained_verify_log(
                    &entry_root,
                    &row.retained_verify_log,
                    &row.retained_verify_log.cell_id,
                    row.attempt,
                )
                .unwrap(),
                RETAINED_LOG_BODY
            );
            let bytes = fs::read(root.join(&row.artifact.path)).unwrap();
            assert_eq!(hex_digest(&bytes), row.artifact.sha256);
            assert_eq!(u64::try_from(bytes.len()).unwrap(), row.artifact.bytes);
        }
        let mut gzips = BTreeSet::new();
        collect_retained_gzip_paths(index.parent().unwrap(), &mut gzips).unwrap();
        assert_eq!(gzips.len(), 2, "one gzip per compared pair is retained");
        assert!(!root
            .join("ignored/validate/artifacts/validate-retained/verify-logs")
            .join("run-1.log")
            .exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn harness_publication_is_accepted_by_schema10_retention_without_rewriting_the_row() {
        use hermit_manifest_plan::ci_selection::CiSelectionSpec;
        use hermit_manifest_plan::runner::AttemptResult;
        use hermit_manifest_plan::runner::CellResult;
        use hermit_manifest_plan::runner::DirectCommand;
        use hermit_manifest_plan::runner::ModeRecipe;
        use hermit_manifest_plan::runner::Observation;
        use hermit_manifest_plan::runner::ObservedResult;
        use hermit_manifest_plan::runner::RunContext;
        use hermit_manifest_plan::runner::ScheduledWorkerCapacity;
        use hermit_manifest_plan::runner::SelectedCell;
        use hermit_manifest_plan::runner::TestRecipe;
        use hermit_manifest_plan::runner::VerifyLogRetentionBudget;
        use hermit_manifest_plan::runner::VerifyRun;
        use hermit_manifest_plan::runner::build_verify_run_spec;
        use hermit_manifest_plan::runner::compare_verify_runs;
        use hermit_manifest_plan::runner::load_verify_run;
        use hermit_manifest_plan::runner::prepare_result_path_from_root;
        use hermit_manifest_plan::runner::publish_retained_verify_log;
        use hermit_manifest_plan::timeouts::TimeoutMultipliers;

        let root = fixture_root();
        let results = root.join("results");
        let run_id = "validate-writer-consumer";
        let run_root = results.join("runs").join(run_id);
        let artifact_dir = run_root.join("fixture-test-verify-ptrace");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mode = ModeRecipe {
            ci: CiSelectionSpec::Uniform(true),
            backends_enabled: vec!["ptrace".into()],
            ..ModeRecipe::default()
        };
        let test = TestRecipe {
            id: "fixture/test".into(),
            description: "fixture".into(),
            lane: "portable".into(),
            requires: Vec::new(),
            occasional: false,
            program: None,
            direct: Some(DirectCommand::Argv(vec!["/bin/true".into()])),
            observation: Observation {
                status: true,
                stdout: true,
                stderr: true,
                artifacts: Vec::new(),
            },
            build: None,
            modes: BTreeMap::from([("verify".into(), mode)]),
            preprocessors: Vec::new(),
        };
        let cell = SelectedCell {
            category: "fixture".into(),
            id: CellId {
                test: test.id.clone(),
                mode: "verify".into(),
                backend: Some("ptrace".into()),
            },
            test,
            enabled: true,
            timeout_seconds: 57,
            cpu_timeout_seconds: 22,
        };
        let commit = "1818181818181818181818181818181818181818";
        let context = RunContext {
            root: root.clone(),
            hermit_bin: root.join("hermit"),
            result_root: results.clone(),
            build_root: root.join("build"),
            run_id: run_id.into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: BTreeMap::new(),
            attempt: 1,
            run_index: None,
            source_sha: commit.into(),
            source_dirty: false,
            binary_build_sha: None,
            prebuilt: true,
            keep_logs: false,
            run_verify_strict: false,
            record_verify_strict: false,
            timeout_multipliers: TimeoutMultipliers::default(),
            scheduled_worker_capacity: ScheduledWorkerCapacity::new(1),
            isolated_workdir: None,
        };
        let run1 = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.clone(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run1,
            57,
        )
        .unwrap();
        let run2 = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.clone(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run2,
            57,
        )
        .unwrap();
        write_harness_verify_run(&run1);
        write_harness_verify_run(&run2);
        let pair = compare_verify_runs(
            &run1,
            load_verify_run(&run1).unwrap(),
            &run2,
            load_verify_run(&run2).unwrap(),
        )
        .unwrap();

        let attempt = AttemptResult {
            index: "1".into(),
            outcome: "PASS".into(),
            error_kind: None,
            status: Some(0),
            signal: None,
            timed_out: false,
            duration_ms: 1,
            cpu_usage_usec: Some(1),
            observation_sha256: None,
            argv: run1.execution.argv.clone(),
            guest_argv: run1.execution.guest_argv.clone(),
            env: run1.execution.env.clone(),
            cwd: run1.execution.cwd.to_string_lossy().into_owned(),
            shell_command: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            verification_report: None,
            verification_report_sha256: None,
            retained_verify_log: None,
            runtime: None,
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            sabre_path_evidence: None,
            sabre_path_evidence_sha256: None,
            reason: None,
        };
        let mut result = CellResult {
            schema: CELL_RESULT_SCHEMA,
            run_id: run_id.into(),
            machine_shortname: "fixture-host".into(),
            kernel_version: "7.1.3-fixture".into(),
            host_capabilities: BTreeMap::new(),
            attempt: 1,
            run_index: None,
            hermit_sha: commit.into(),
            source_tree_dirty: false,
            binary_sha256: None,
            binary_build_sha: None,
            test_sha256: "fixture-test-digest".into(),
            test: cell.id.test.clone(),
            category: cell.category.clone(),
            lane: cell.test.lane.clone(),
            mode: cell.id.mode.clone(),
            backend: cell.id.backend.clone(),
            classification: "required".into(),
            outcome: "PASS".into(),
            result: Some(ObservedResult::Pass),
            failure_class: None,
            error_kind: None,
            timeout_seconds: 57,
            execution_cpu_timeout_seconds: Some(22),
            execution_wall_timeout_seconds: Some(57),
            duration_ms: Some(1),
            cpu_usage_usec: Some(1),
            runtime: None,
            log_level: Some("info".into()),
            effective_args: Vec::new(),
            argv: run1.execution.argv.clone(),
            guest_argv: run1.execution.guest_argv.clone(),
            env: run1.execution.env.clone(),
            cwd: run1.execution.cwd.to_string_lossy().into_owned(),
            shell_command: String::new(),
            relaxations: Vec::new(),
            execution_path: None,
            diversity: None,
            attempts: vec![attempt],
            first_divergent_scheduler_turn: None,
            first_divergent_virtual_nanoseconds: None,
            first_divergent_record: None,
            first_divergent_syscall: None,
            first_divergent_left_message: None,
            first_divergent_right_message: None,
            reason: None,
            artifact_dir: artifact_dir.to_string_lossy().into_owned(),
        };
        let results_path = run_root.join("results.jsonl");
        prepare_result_path_from_root(&run_root, &results_path).unwrap();
        let policy = VerifyLogRetentionPolicy::new(u64::MAX);
        let budget = VerifyLogRetentionBudget::open(&run_root, &results_path, policy).unwrap();
        publish_retained_verify_log(pair, &budget, &results_path, &mut result).unwrap();

        let written: Value = serde_json::from_str(
            fs::read_to_string(&results_path)
                .unwrap()
                .trim_end(),
        )
        .unwrap();
        assert_eq!(
            written["schema"],
            RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA
        );
        assert_eq!(written["outcome"], "PASS");
        assert!(written["attempts"][0]["verification_report"].is_string());
        assert!(written["attempts"][0]["retained_verify_log"].is_object());

        let retained = retain_with_policy(
            &root,
            &results,
            commit,
            &expected(&written),
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            policy,
        )
        .unwrap();
        assert_eq!(
            retained.schema_version,
            RETAINED_VERIFY_LOGS_LEDGER_SCHEMA_VERSION
        );
        assert_eq!(retained.evidence["retained_verify_logs"]["row_count"], 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema5_transition_accepts_verify_rows_only_when_every_verify_row_is_retained() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let commit = "1717171717171717171717171717171717171717";
        let mut verify = result_row("validate-mixed-schema", commit);
        add_retained_verify_log(&results, &mut verify);
        let mut nonverify = result_row("validate-mixed-schema", commit);
        nonverify["test"] = Value::String("c-programs/nonverify".into());
        nonverify["mode"] = Value::String("naked".into());
        nonverify["attempts"] = Value::Array(Vec::new());
        append_result_row(&results, &verify);
        append_result_row(&results, &nonverify);

        let retained = retain_with_policy(
            &root,
            &results,
            commit,
            &[
                identity_value(&identity(&verify).unwrap()).unwrap(),
                identity_value(&identity(&nonverify).unwrap()).unwrap(),
            ],
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            VerifyLogRetentionPolicy::new(71),
        )
        .unwrap();
        assert_eq!(
            retained.schema_version,
            RETAINED_VERIFY_LOGS_LEDGER_SCHEMA_VERSION
        );
        assert_eq!(retained.evidence["selected_count"], 2);
        assert_eq!(retained.evidence["retained_verify_logs"]["row_count"], 1);
        fs::remove_dir_all(root).unwrap();

        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let mut retained_verify = result_row("validate-partial-schema5", commit);
        add_retained_verify_log(&results, &mut retained_verify);
        let mut hidden_verify = result_row("validate-partial-schema5", commit);
        hidden_verify["test"] = Value::String("c-programs/hidden-verify".into());
        append_result_row(&results, &retained_verify);
        append_result_row(&results, &hidden_verify);
        let error = retain_with_policy(
            &root,
            &results,
            commit,
            &[
                identity_value(&identity(&retained_verify).unwrap()).unwrap(),
                identity_value(&identity(&hidden_verify).unwrap()).unwrap(),
            ],
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            VerifyLogRetentionPolicy::new(71),
        )
        .unwrap_err();
        assert!(error.contains("schema-4 verify result is not allowed"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_verify_descriptor_requires_schema5_verify_mode_exactly() {
        for (label, mutate, expected_error) in [
            (
                "schema4-descriptor",
                0_u8,
                "schema-4 result cannot carry retained_verify_log",
            ),
            (
                "schema5-nonverify",
                1_u8,
                "schema-5 result must use verify mode",
            ),
        ] {
            let root = fixture_root();
            fs::create_dir_all(&root).unwrap();
            let results = root.join("results");
            let commit = "1919191919191919191919191919191919191919";
            let mut row = result_row(&format!("validate-{label}"), commit);
            add_retained_verify_log(&results, &mut row);
            if mutate == 0 {
                row["schema"] = Value::from(CELL_RESULT_SCHEMA);
            } else {
                row["mode"] = Value::String("naked".into());
            }
            write_result(&results, &row);
            let error = retain_with_policy(
                &root,
                &results,
                commit,
                &expected(&row),
                ValidatePath::Full,
                HERMETIC_IMAGE_DIGEST,
                VerifyLogRetentionPolicy::new(71),
            )
            .unwrap_err();
            assert!(error.contains(expected_error), "{label}: {error}");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn descriptor_input_without_the_shared_policy_cannot_fall_back_to_schema7() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let commit = "2020202020202020202020202020202020202020";
        let mut row = result_row("validate-no-policy", commit);
        add_retained_verify_log(&results, &mut row);
        write_result(&results, &row);

        let error = retain(&root, &results, commit, &expected(&row)).unwrap_err();
        assert!(error.contains("shared explicit retention policy"), "{error}");
        assert!(!root
            .join("ignored/validate/artifacts/validate-no-policy/cell-results.jsonl")
            .exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema4_retention_does_not_inventory_unrelated_run_artifacts() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "2323232323232323232323232323232323232323";
        let row = result_row("validate-schema4-no-inventory", commit);
        write_result(&results, &row);
        let unrelated = results
            .join("runs/validate-schema4-no-inventory/unrelated-cell/captures");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("ordinary.log.gz"), b"not a retained log").unwrap();
        std::os::unix::fs::symlink(
            root.join("missing-workdir"),
            unrelated.join("workdir-link"),
        )
        .unwrap();

        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
        assert_eq!(retained.schema_version, CELL_RESULTS_LEDGER_SCHEMA_VERSION);
        assert_eq!(retained.evidence["run_id"], "validate-schema4-no-inventory");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema10_ignores_unrelated_cell_artifacts_outside_retained_verify() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let commit = "2424242424242424242424242424242424242424";
        let mut row = result_row("validate-schema10-scope", commit);
        add_retained_verify_log(&results, &mut row);
        let artifact_dir = PathBuf::from(row["artifact_dir"].as_str().unwrap());
        let captures = artifact_dir.join("captures/verify/1/run-1");
        fs::create_dir_all(&captures).unwrap();
        fs::write(captures.join("ordinary.log.gz"), b"not retained evidence").unwrap();
        std::os::unix::fs::symlink(
            root.join("missing-workdir"),
            artifact_dir.join("workdir-link"),
        )
        .unwrap();
        write_result(&results, &row);

        let retained = retain_with_policy(
            &root,
            &results,
            commit,
            &expected(&row),
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            VerifyLogRetentionPolicy::new(71),
        )
        .unwrap();
        assert_eq!(
            retained.schema_version,
            RETAINED_VERIFY_LOGS_LEDGER_SCHEMA_VERSION
        );
        assert_eq!(retained.evidence["retained_verify_logs"]["row_count"], 1);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema10_refuses_an_orphan_retained_verify_namespace_in_a_sibling_cell() {
        enum OrphanNamespace {
            File,
            Directory,
            Symlink,
        }

        for mutation in [
            OrphanNamespace::File,
            OrphanNamespace::Directory,
            OrphanNamespace::Symlink,
        ] {
            let root = fixture_root();
            fs::create_dir_all(&root).unwrap();
            let results = root.join("results");
            let commit = "2525252525252525252525252525252525252525";
            let mut row = result_row("validate-orphan-namespace", commit);
            add_retained_verify_log(&results, &mut row);
            write_result(&results, &row);

            let orphan = results
                .join("runs/validate-orphan-namespace/orphan-cell/retained");
            fs::create_dir_all(&orphan).unwrap();
            let namespace = orphan.join("verify");
            match mutation {
                OrphanNamespace::File => fs::write(&namespace, b"orphan").unwrap(),
                OrphanNamespace::Directory => fs::create_dir(&namespace).unwrap(),
                OrphanNamespace::Symlink => {
                    std::os::unix::fs::symlink(root.join("missing"), &namespace).unwrap()
                }
            }

            let error = retain_with_policy(
                &root,
                &results,
                commit,
                &expected(&row),
                ValidatePath::Full,
                HERMETIC_IMAGE_DIGEST,
                VerifyLogRetentionPolicy::new(71),
            )
            .unwrap_err();
            assert!(
                error.contains("unreferenced retained verify-log namespace"),
                "{error}"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn retained_verify_logs_require_the_captured_canonical_image_digest() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let commit = "2121212121212121212121212121212121212121";
        let mut row = result_row("validate-image-digest", commit);
        add_retained_verify_log(&results, &mut row);
        write_result(&results, &row);

        let error = retain_with_policy(
            &root,
            &results,
            commit,
            &expected(&row),
            ValidatePath::Full,
            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            VerifyLogRetentionPolicy::new(71),
        )
        .unwrap_err();
        assert!(error.contains("canonical hermetic_image_digest"), "{error}");
        assert!(!root.join("ignored/validate/artifacts").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_descriptors_cannot_publish_the_same_gzip_twice() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let commit = "2222222222222222222222222222222222222222";
        let mut first = result_row("validate-duplicate-log", commit);
        add_retained_verify_log(&results, &mut first);
        let mut second = first.clone();
        second["test"] = Value::String("different-test".into());
        second["attempts"][0]["retained_verify_log"]["cell_id"]["test"] =
            Value::String("different-test".into());
        append_result_row(&results, &first);
        append_result_row(&results, &second);

        let error = retain_with_policy(
            &root,
            &results,
            commit,
            &[
                identity_value(&identity(&first).unwrap()).unwrap(),
                identity_value(&identity(&second).unwrap()).unwrap(),
            ],
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            VerifyLogRetentionPolicy::new(142),
        )
        .unwrap_err();
        assert!(error.contains("same gzip"), "{error}");
        assert!(!root
            .join("ignored/validate/artifacts/validate-duplicate-log/verify-logs")
            .exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_verify_log_copy_refuses_cap_corruption_symlink_hardlink_and_unreferenced_file() {
        enum Mutation {
            Cap,
            Corrupt,
            Symlink,
            Hardlink,
            Traversal,
            Unreferenced,
        }

        for mutation in [
            Mutation::Cap,
            Mutation::Corrupt,
            Mutation::Symlink,
            Mutation::Hardlink,
            Mutation::Traversal,
            Mutation::Unreferenced,
        ] {
            let root = fixture_root();
            fs::create_dir_all(&root).unwrap();
            let results = root.join("results");
            let commit = "3030303030303030303030303030303030303030";
            let mut row = result_row("validate-refusal", commit);
            let source = add_retained_verify_log(&results, &mut row);
            let limit = match mutation {
                Mutation::Cap => 70,
                Mutation::Corrupt => {
                    fs::write(&source, b"not gzip").unwrap();
                    71
                }
                Mutation::Symlink => {
                    let target = root.join("outside.log.gz");
                    fs::write(&target, RETAINED_LOG_GZIP).unwrap();
                    fs::remove_file(&source).unwrap();
                    std::os::unix::fs::symlink(&target, &source).unwrap();
                    71
                }
                Mutation::Hardlink => {
                    let artifact_dir = PathBuf::from(row["artifact_dir"].as_str().unwrap());
                    fs::hard_link(&source, artifact_dir.join("retained-log-hardlink")).unwrap();
                    71
                }
                Mutation::Traversal => {
                    row["artifact_dir"] =
                        Value::String(root.join("outside").to_string_lossy().into_owned());
                    71
                }
                Mutation::Unreferenced => {
                    fs::write(source.with_file_name("unreferenced.log.gz"), RETAINED_LOG_GZIP)
                        .unwrap();
                    142
                }
            };
            write_result(&results, &row);
            let error = retain_with_policy(
                &root,
                &results,
                commit,
                &expected(&row),
                ValidatePath::Full,
                HERMETIC_IMAGE_DIGEST,
                VerifyLogRetentionPolicy::new(limit),
            )
            .unwrap_err();
            let expected = match mutation {
                Mutation::Cap => "aggregate limit",
                Mutation::Corrupt => "deterministic gzip header",
                Mutation::Symlink => "symlink",
                Mutation::Hardlink => "hard links",
                Mutation::Traversal => "outside",
                Mutation::Unreferenced => "unreferenced",
            };
            assert!(error.contains(expected), "{expected}: {error}");
            let retained = root.join("ignored/validate/artifacts/validate-refusal");
            assert!(!retained.join("verify-logs").exists());
            assert!(!retained.join("cell-results.jsonl").exists());
            assert!(!fs::read_dir(&retained)
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".verify-logs.")));
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn schema5_compared_verify_row_without_a_descriptor_refuses() {
        let root = fixture_root();
        fs::create_dir_all(&root).unwrap();
        let results = root.join("results");
        let commit = "4040404040404040404040404040404040404040";
        let mut row = result_row("validate-missing-retained", commit);
        row["schema"] = Value::from(RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA);
        write_result(&results, &row);
        let error = retain_with_policy(
            &root,
            &results,
            commit,
            &expected(&row),
            ValidatePath::Full,
            HERMETIC_IMAGE_DIGEST,
            VerifyLogRetentionPolicy::new(1024),
        )
        .unwrap_err();
        assert!(
            error.contains("requires exactly one retained_verify_log"),
            "{error}"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn artifact_ancestor_sync_failure_removes_the_new_directory_chain() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "4141414141414141414141414141414141414141";
        let row = result_row("validate-ancestor-sync", commit);
        write_result(&results, &row);

        let error = retain_inner(
            &root,
            &results,
            commit,
            &expected(&row),
            None,
            Some(RetentionFailurePoint::ArtifactAncestorSync),
        )
        .unwrap_err();
        assert!(error.contains("artifact ancestor"), "{error}");
        assert!(
            !root.join("ignored").exists(),
            "an ancestor sync failure left the newly created artifact directory chain"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_retained_verify_log_publication_leaves_no_visible_bundle_or_index() {
        for failure in [
            RetentionFailurePoint::BeforeVerifyLogRename,
            RetentionFailurePoint::AfterVerifyLogRename,
            RetentionFailurePoint::BeforeCellResultPublication,
        ] {
            let root = fixture_root();
            fs::create_dir_all(&root).unwrap();
            let results = root.join("results");
            let commit = "5050505050505050505050505050505050505050";
            let mut row = result_row("validate-interrupted", commit);
            add_retained_verify_log(&results, &mut row);
            write_result(&results, &row);

            let error = retain_inner(
                &root,
                &results,
                commit,
                &expected(&row),
                Some((
                    ValidatePath::Full,
                    HERMETIC_IMAGE_DIGEST,
                    VerifyLogRetentionPolicy::new(71),
                )),
                Some(failure),
            )
            .unwrap_err();
            assert!(error.contains("injected failure"), "{error}");
            let retained = root.join("ignored/validate/artifacts/validate-interrupted");
            assert!(!retained.join("verify-logs").exists());
            assert!(!retained.join("cell-results.jsonl").exists());
            assert!(!fs::read_dir(&retained)
                .into_iter()
                .flatten()
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(".verify-logs.")));
            fs::remove_dir_all(root).unwrap();
        }
    }
}
