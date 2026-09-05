use super::*;

/// One side of a harness-managed verify pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyRun {
    Run1,
    Run2,
}

impl VerifyRun {
    fn directory_name(self) -> &'static str {
        match self {
            Self::Run1 => "run-1",
            Self::Run2 => "run-2",
        }
    }

    fn comparison_label(self) -> &'static str {
        match self {
            Self::Run1 => "run 1",
            Self::Run2 => "run 2",
        }
    }
}

/// Harness-owned paths for one ordinary execution in a verify pair.
///
/// Every path is below the cell artifact directory. The work directory is
/// separate for each side and is mounted at the same guest-visible `/test`
/// path when the pair is eventually enabled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifyRunPaths {
    pub workdir: PathBuf,
    pub log: PathBuf,
    pub result: PathBuf,
    pub stdout: PathBuf,
    pub stderr: PathBuf,
    pub summary: PathBuf,
}

/// One ordinary Hermit invocation that supplies one side of a verify pair.
#[derive(Clone, Debug)]
pub struct VerifyRunSpec {
    pub run: VerifyRun,
    pub execution: CellRunSpec,
    pub paths: VerifyRunPaths,
    attempt: u64,
    expected_determinism: GuestRunDeterminism,
}

impl VerifyRunSpec {
    pub fn expected_determinism(&self) -> GuestRunDeterminism {
        self.expected_determinism
    }

    fn validate_policy_binding(&self) -> Result<(), String> {
        let flag_count = |flag: &str| {
            self.execution
                .argv
                .iter()
                .filter(|argument| argument.as_str() == flag)
                .count()
        };
        let expected_no_io = usize::from(!self.expected_determinism.detlog_io_buffers);
        if flag_count("--no-detlog-io-buffers") != expected_no_io {
            return Err(format!(
                "verify run command does not match its detlog_io_buffers setting: expected {expected_no_io} --no-detlog-io-buffers flag(s)"
            ));
        }
        let expected_no_virtual_time = usize::from(!self.expected_determinism.virtualize_time);
        if flag_count("--no-virtualize-time") != expected_no_virtual_time {
            return Err(format!(
                "verify run command does not match its virtualize_time setting: expected {expected_no_virtual_time} --no-virtualize-time flag(s)"
            ));
        }
        if self.execution.argv.iter().any(|argument| {
            matches!(
                argument.as_str(),
                "--verify" | "--verify-json" | "--verify-strict" | "--keep-logs"
            ) || argument.starts_with("--verify-log-dir")
        }) {
            return Err(
                "harness-managed verify run command contains an internal verify flag".into(),
            );
        }
        Ok(())
    }
}

/// The single member of a verify pair retained after comparison.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetainedVerifyLogRole {
    #[serde(rename = "run-1")]
    Run1,
}

/// Durable evidence for the one compressed verify log retained by an attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedVerifyLog {
    /// Path relative to the cell's `artifact_dir`.
    pub relative_path: String,
    pub role: RetainedVerifyLogRole,
    pub cell_id: CellId,
    pub attempt: u64,
    pub uncompressed_sha256: String,
    pub uncompressed_bytes: u64,
    pub compressed_sha256: String,
    pub compressed_bytes: u64,
    pub peer_uncompressed_sha256: String,
    pub peer_uncompressed_bytes: u64,
    pub compared_info_messages: u64,
}

/// Digests and sizes checked while copying one retained gzip from a single
/// held source descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedRetainedLogCopy {
    pub compressed_sha256: String,
    pub compressed_bytes: u64,
    pub uncompressed_sha256: String,
    pub uncompressed_bytes: u64,
}

/// A verified compressed log plus the two raw inputs that may be removed only
/// after its descriptor has been durably published with the cell result.
#[derive(Clone, Debug)]
pub struct RetainedVerifyLogPublication {
    pub retained: RetainedVerifyLog,
    run1_raw: PathBuf,
    run2_raw: PathBuf,
    run1_digest: ContentDigest,
    run2_digest: ContentDigest,
}

/// Storage policy shared by every retained verify log in one validation run.
///
/// The aggregate limit is explicit rather than inferred from free space: a
/// caller must choose it once at the scheduling boundary and share the
/// resulting [`VerifyLogRetentionBudget`] across all pair publications.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyLogRetentionPolicy {
    pub maximum_total_compressed_bytes: u64,
}

impl VerifyLogRetentionPolicy {
    pub const fn new(maximum_total_compressed_bytes: u64) -> Self {
        Self {
            maximum_total_compressed_bytes,
        }
    }
}

#[derive(Debug)]
struct VerifyLogRetentionState {
    accounted_compressed_bytes: u64,
}

/// Synchronized aggregate accounting for retained verify logs.
///
/// Clones share one counter. Each compressed write is provisionally charged
/// before touching the staging file and releases any unwritten bytes, so
/// staging may proceed concurrently without allowing physical bytes to run
/// ahead of the aggregate limit.
#[derive(Clone, Debug)]
pub struct VerifyLogRetentionBudget {
    policy: VerifyLogRetentionPolicy,
    retention_root: PathBuf,
    results_path: PathBuf,
    state: Arc<Mutex<VerifyLogRetentionState>>,
    result_publication: Arc<Mutex<()>>,
}

impl VerifyLogRetentionBudget {
    /// Reconstruct aggregate accounting from the canonical finals referenced
    /// by one prepared result file. Unreferenced finals and interrupted
    /// staging files make restart state inconsistent and are refused without
    /// deleting them.
    pub fn open(
        retention_root: impl Into<PathBuf>,
        results_path: impl Into<PathBuf>,
        policy: VerifyLogRetentionPolicy,
    ) -> Result<Self, String> {
        let retention_root = retention_root.into();
        let results_path = results_path.into();
        require_plain_directory(&retention_root, "verify-log retention root")?;
        let results = check_file_path_below(
            &retention_root,
            &results_path,
            "retained verify-log result-row destination",
        )?;
        if results.identity.is_none() {
            return Err(format!(
                "retained verify-log result-row destination {} must be prepared before opening the budget",
                results_path.display()
            ));
        }
        let accounted_compressed_bytes =
            scan_existing_verify_log_bytes(&retention_root, &results_path, policy)?;
        Ok(Self {
            policy,
            retention_root,
            results_path,
            state: Arc::new(Mutex::new(VerifyLogRetentionState {
                accounted_compressed_bytes,
            })),
            result_publication: Arc::new(Mutex::new(())),
        })
    }

    pub fn policy(&self) -> VerifyLogRetentionPolicy {
        self.policy
    }

    pub fn accounted_compressed_bytes(&self) -> Result<u64, String> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "retained verify-log byte accounting lock is poisoned".to_string())?
            .accounted_compressed_bytes)
    }

    fn reserve_additional(&self, compressed_bytes: u64) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "retained verify-log byte accounting lock is poisoned".to_string())?;
        let next = state
            .accounted_compressed_bytes
            .checked_add(compressed_bytes)
            .ok_or_else(|| {
                format!(
                    "retained verify-log aggregate compressed-byte accounting overflow: {} + {compressed_bytes}",
                    state.accounted_compressed_bytes
                )
            })?;
        if next > self.policy.maximum_total_compressed_bytes {
            return Err(format!(
                "retained verify logs require {next} compressed bytes, exceeding the {}-byte aggregate limit",
                self.policy.maximum_total_compressed_bytes
            ));
        }
        state.accounted_compressed_bytes = next;
        Ok(())
    }

    fn release(&self, compressed_bytes: u64) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "retained verify-log byte accounting lock is poisoned".to_string())?;
        state.accounted_compressed_bytes = state
            .accounted_compressed_bytes
            .checked_sub(compressed_bytes)
            .ok_or_else(|| {
                "retained verify-log provisional accounting underflowed during rollback".to_string()
            })?;
        Ok(())
    }

    fn require_artifact_dir(&self, artifact_dir: &Path) -> Result<(), String> {
        let relative = checked_relative_path(
            &self.retention_root,
            artifact_dir,
            "verify cell artifact directory",
        )?;
        if relative.components().count() != 1 {
            return Err(format!(
                "verify cell artifact directory {} is not an immediate child of retention root {}",
                artifact_dir.display(),
                self.retention_root.display()
            ));
        }
        require_plain_directory(artifact_dir, "verify cell artifact directory")
    }

    fn require_results_path(&self, results_path: &Path) -> Result<(), String> {
        let expected = check_file_path_below(
            &self.retention_root,
            &self.results_path,
            "configured result-row destination",
        )?;
        let actual =
            check_file_path_below(&self.retention_root, results_path, "result-row destination")?;
        if expected.normalized != actual.normalized {
            return Err(format!(
                "result-row destination {} does not match configured path {}",
                results_path.display(),
                self.results_path.display()
            ));
        }
        Ok(())
    }
}

struct VerifyLogRetentionReservation {
    budget: VerifyLogRetentionBudget,
    compressed_bytes: u64,
    resolved: bool,
}

impl VerifyLogRetentionReservation {
    fn commit(mut self) {
        self.resolved = true;
    }

    fn rollback(mut self) -> Result<(), String> {
        self.budget.release(self.compressed_bytes)?;
        self.resolved = true;
        Ok(())
    }
}

impl Drop for VerifyLogRetentionReservation {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        let _ = self.budget.release(self.compressed_bytes);
    }
}

const VERIFY_LOG_MAX_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_LOG_MAX_COMPRESSED_BYTES: u64 = VERIFY_LOG_MAX_UNCOMPRESSED_BYTES + 16 * 1024 * 1024;
const VERIFY_LOG_STAGING_PREFIX: &str = ".run-1.log.gz.";
const VERIFY_LOG_STAGING_SUFFIX: &str = ".staging";
const VERIFY_RUN_RESULT_MAX_BYTES: u64 = 1024 * 1024;
const VERIFY_RUN_STDOUT_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_RUN_STDERR_MAX_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_RUN_SUMMARY_MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug)]
struct VerifyRunReadLimits {
    result: u64,
    stdout: u64,
    stderr: u64,
    summary: u64,
}

const VERIFY_RUN_READ_LIMITS: VerifyRunReadLimits = VerifyRunReadLimits {
    result: VERIFY_RUN_RESULT_MAX_BYTES,
    stdout: VERIFY_RUN_STDOUT_MAX_BYTES,
    stderr: VERIFY_RUN_STDERR_MAX_BYTES,
    summary: VERIFY_RUN_SUMMARY_MAX_BYTES,
};

pub fn verify_run_paths(
    cell_dir: &Path,
    attempt: &str,
    run: VerifyRun,
) -> Result<VerifyRunPaths, String> {
    let workdir = fixed_workdir_source_for_attempt(cell_dir, attempt)?.join(run.directory_name());
    let capture_dir = cell_dir
        .join("captures")
        .join("verify")
        .join(attempt)
        .join(run.directory_name());
    Ok(VerifyRunPaths {
        workdir,
        log: capture_dir.join("detlog.log"),
        result: capture_dir.join("result.json"),
        stdout: capture_dir.join("stdout"),
        stderr: capture_dir.join("stderr"),
        summary: capture_dir.join("summary.json"),
    })
}

/// Fully checked inputs for one side of a harness-managed comparison.
#[derive(Clone, Debug)]
pub struct VerifyRunObservation {
    pub result: GuestRunResult,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub runtime: RuntimeStats,
    log_bytes: Vec<u8>,
    log_digest: ContentDigest,
    log_identity: FileIdentity,
}

impl VerifyRunObservation {
    fn as_logdiff_input(&self) -> BitwiseInfoV1RunObservation<'_> {
        BitwiseInfoV1RunObservation {
            disposition: self.result.disposition,
            stdout: &self.stdout,
            stderr: &self.stderr,
            runtime: Some(&self.runtime),
        }
    }
}

fn validate_captured_guest_stream(
    name: &str,
    expected: &crate::canonical_verdict::CapturedGuestStream,
    actual: &ContentDigest,
) -> Result<(), String> {
    if expected.bytes != actual.bytes || expected.sha256 != actual.sha256 {
        return Err(format!(
            "captured guest {name} does not match its typed result: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            expected.bytes, expected.sha256, actual.bytes, actual.sha256
        ));
    }
    Ok(())
}

/// Read one ordinary-run sidecar and bind it to the exact captured bytes.
pub fn load_verify_run(spec: &VerifyRunSpec) -> Result<VerifyRunObservation, String> {
    load_verify_run_with_limits(spec, VERIFY_RUN_READ_LIMITS)
}

fn read_verify_run_artifact(
    spec: &VerifyRunSpec,
    path: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<(Vec<u8>, ContentDigest), String> {
    let mut file = open_plain_file_below(&spec.execution.cell_dir, path, description)?;
    let mut bytes = Vec::new();
    let digest = copy_and_hash_bounded(&mut file, &mut bytes, maximum_bytes, description)?;
    Ok((bytes, digest))
}

fn load_verify_run_with_limits(
    spec: &VerifyRunSpec,
    limits: VerifyRunReadLimits,
) -> Result<VerifyRunObservation, String> {
    spec.validate_policy_binding()?;
    let paths = &spec.paths;
    let (result_bytes, _) = read_verify_run_artifact(
        spec,
        &paths.result,
        limits.result,
        "verify ordinary-run result",
    )?;
    let result = GuestRunResult::from_current_json_slice(&result_bytes)?;
    if result.determinism != spec.expected_determinism {
        return Err(format!(
            "guest run result determinism settings do not match the command: expected {:?}, got {:?}",
            spec.expected_determinism, result.determinism
        ));
    }
    let (stdout, stdout_digest) = read_verify_run_artifact(
        spec,
        &paths.stdout,
        limits.stdout,
        "verify ordinary-run stdout",
    )?;
    let (stderr, stderr_digest) = read_verify_run_artifact(
        spec,
        &paths.stderr,
        limits.stderr,
        "verify ordinary-run stderr",
    )?;
    validate_captured_guest_stream("stdout", &result.stdout, &stdout_digest)?;
    validate_captured_guest_stream("stderr", &result.stderr, &stderr_digest)?;
    let (summary_bytes, _) = read_verify_run_artifact(
        spec,
        &paths.summary,
        limits.summary,
        "verify ordinary-run summary",
    )?;
    let summary = serde_json::from_slice::<RunSummary>(&summary_bytes)
        .map_err(|error| format!("cannot parse {}: {error}", paths.summary.display()))?;
    let runtime = RuntimeStats::from(&summary);
    let mut log_file = open_plain_file_below(
        &spec.execution.cell_dir,
        &paths.log,
        "verify ordinary-run log",
    )?;
    let log_metadata = log_file
        .metadata()
        .map_err(|error| format!("cannot inspect {}: {error}", paths.log.display()))?;
    let mut log_bytes = Vec::new();
    let log_digest = copy_and_hash_bounded(
        &mut log_file,
        &mut log_bytes,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        "verify ordinary-run log",
    )?;
    Ok(VerifyRunObservation {
        result,
        stdout,
        stderr,
        runtime,
        log_bytes,
        log_digest,
        log_identity: FileIdentity::from_metadata(&log_metadata),
    })
}

/// One comparison whose exact input bytes and policy remain available for
/// retention. Callers cannot supply record counts or digests independently.
#[derive(Clone, Debug)]
pub struct ComparedVerifyPair {
    comparison: BitwiseInfoV1Comparison,
    artifact_dir: PathBuf,
    cell_id: CellId,
    attempt: u64,
    run1_log: PathBuf,
    run2_log: PathBuf,
    run1_log_bytes: Vec<u8>,
    run1_log_digest: ContentDigest,
    run1_log_identity: FileIdentity,
    run2_log_digest: ContentDigest,
    run2_log_identity: FileIdentity,
    fixed_result_destinations: Vec<(String, PathBuf)>,
}

impl ComparedVerifyPair {
    pub fn comparison(&self) -> &BitwiseInfoV1Comparison {
        &self.comparison
    }
}

/// Compare two checked ordinary runs using Detcore's fixed canonical policy.
pub fn compare_verify_runs(
    run1_spec: &VerifyRunSpec,
    run1: VerifyRunObservation,
    run2_spec: &VerifyRunSpec,
    run2: VerifyRunObservation,
) -> Result<ComparedVerifyPair, String> {
    run1_spec.validate_policy_binding()?;
    run2_spec.validate_policy_binding()?;
    if run1_spec.run != VerifyRun::Run1 || run2_spec.run != VerifyRun::Run2 {
        return Err("verify comparison requires run 1 on the left and run 2 on the right".into());
    }
    if run1_spec.execution.id != run2_spec.execution.id
        || run1_spec.execution.cell_dir != run2_spec.execution.cell_dir
        || run1_spec.attempt != run2_spec.attempt
    {
        return Err("verify comparison inputs do not belong to the same cell attempt".into());
    }
    if run1.result.determinism != run2.result.determinism
        || run1.result.determinism != run1_spec.expected_determinism
        || run2.result.determinism != run2_spec.expected_determinism
    {
        return Err("verify comparison inputs do not carry one matching run policy".into());
    }
    if run1.log_identity == run2.log_identity {
        return Err("verify run-1 and run-2 observations came from the same file".into());
    }
    let comparison = detcore::logdiff::try_compare_bitwise_info_v1_run_bytes(
        &run1.log_bytes,
        &run2.log_bytes,
        ComparisonSideLabels::new(
            VerifyRun::Run1.comparison_label(),
            VerifyRun::Run2.comparison_label(),
        ),
        run1.as_logdiff_input(),
        run2.as_logdiff_input(),
        run1.result.determinism.detlog_io_buffers,
        run1.result.determinism.virtualize_time,
    )
    .map_err(|error| format!("canonical verify log comparison failed: {error}"))?;
    Ok(ComparedVerifyPair {
        comparison,
        artifact_dir: run1_spec.execution.cell_dir.clone(),
        cell_id: run1_spec.execution.id.clone(),
        attempt: run1_spec.attempt,
        run1_log: run1_spec.paths.log.clone(),
        run2_log: run2_spec.paths.log.clone(),
        run1_log_bytes: run1.log_bytes,
        run1_log_digest: run1.log_digest,
        run1_log_identity: run1.log_identity,
        run2_log_digest: run2.log_digest,
        run2_log_identity: run2.log_identity,
        fixed_result_destinations: [
            ("run-1 result", &run1_spec.paths.result),
            ("run-1 stdout", &run1_spec.paths.stdout),
            ("run-1 stderr", &run1_spec.paths.stderr),
            ("run-1 summary", &run1_spec.paths.summary),
            ("run-2 result", &run2_spec.paths.result),
            ("run-2 stdout", &run2_spec.paths.stdout),
            ("run-2 stderr", &run2_spec.paths.stderr),
            ("run-2 summary", &run2_spec.paths.summary),
        ]
        .into_iter()
        .map(|(description, path)| (description.to_string(), path.clone()))
        .collect(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ContentDigest {
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InspectedGzip {
    identity: FileIdentity,
    compressed: ContentDigest,
    uncompressed: ContentDigest,
}

struct OpenedGzipEvidence {
    file: File,
    inspection: InspectedGzip,
}

fn checked_relative_path<'a>(
    artifact_dir: &'a Path,
    path: &'a Path,
    description: &str,
) -> Result<&'a Path, String> {
    let relative = path.strip_prefix(artifact_dir).map_err(|_| {
        format!(
            "{description} {} is outside cell artifact directory {}",
            path.display(),
            artifact_dir.display()
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
            artifact_dir.display()
        ));
    }
    Ok(relative)
}

#[derive(Clone, Debug)]
struct CheckedFilePath {
    description: String,
    path: PathBuf,
    normalized: PathBuf,
    identity: Option<FileIdentity>,
}

fn check_file_path_below(
    stable_root: &Path,
    path: &Path,
    description: impl Into<String>,
) -> Result<CheckedFilePath, String> {
    let description = description.into();
    let relative = checked_relative_path(stable_root, path, &description)?;
    let canonical_root = fs::canonicalize(stable_root).map_err(|error| {
        format!(
            "cannot canonicalize stable root {} for {description}: {error}",
            stable_root.display()
        )
    })?;
    let components = relative.components().collect::<Vec<_>>();
    let mut current = stable_root.to_owned();
    let mut identity = None;
    let mut missing_parent = false;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            unreachable!("checked_relative_path accepted only normal components")
        };
        current.push(component);
        if missing_parent {
            continue;
        }
        let is_file = index + 1 == components.len();
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "{description} {} or one of its parents is a symlink",
                    current.display()
                ));
            }
            Ok(metadata) if is_file && !metadata.is_file() => {
                return Err(format!(
                    "{description} {} is not a regular file",
                    current.display()
                ));
            }
            Ok(metadata) if is_file => {
                identity = Some(FileIdentity::from_metadata(&metadata));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!(
                    "parent of {description} {} is not a directory",
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing_parent = true;
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect {description} path {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(CheckedFilePath {
        description,
        path: path.to_owned(),
        normalized: canonical_root.join(relative),
        identity,
    })
}

fn reject_file_path_aliases(paths: &[CheckedFilePath]) -> Result<(), String> {
    for (index, left) in paths.iter().enumerate() {
        for right in &paths[index + 1..] {
            if left.normalized == right.normalized
                || left
                    .identity
                    .zip(right.identity)
                    .is_some_and(|(left, right)| left == right)
            {
                return Err(format!(
                    "{} {} aliases {} {}",
                    left.description,
                    left.path.display(),
                    right.description,
                    right.path.display()
                ));
            }
        }
    }
    Ok(())
}

fn require_path_identity(
    path: &Path,
    expected: FileIdentity,
    description: &str,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot recheck {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{description} {} is no longer a regular non-symlink file",
            path.display()
        ));
    }
    let actual = FileIdentity::from_metadata(&metadata);
    if actual != expected {
        return Err(format!(
            "{description} {} changed identity before publication",
            path.display()
        ));
    }
    Ok(())
}

pub(super) fn require_plain_directory(path: &Path, description: &str) -> Result<(), String> {
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

fn require_plain_parent_chain(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    let relative = checked_relative_path(artifact_dir, path, description)?;
    require_plain_directory(artifact_dir, "cell artifact directory")?;
    let mut current = artifact_dir.to_owned();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(component) = component else {
                unreachable!("checked_relative_path accepted only normal components")
            };
            current.push(component);
            require_plain_directory(&current, description)?;
        }
    }
    Ok(())
}

fn create_plain_relative_directory(
    artifact_dir: &Path,
    relative: &Path,
    description: &str,
) -> Result<PathBuf, String> {
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("{description} is not a normal relative path"));
    }
    require_plain_directory(artifact_dir, "cell artifact directory")?;
    let mut current = artifact_dir.to_owned();
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
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect {description} {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(current)
}

pub(super) fn sync_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    directory
        .sync_all()
        .map_err(|error| format!("cannot sync {description} {}: {error}", path.display()))
}

pub(super) fn sync_relative_directory_chain_with_failure(
    root: &Path,
    final_directory: &Path,
    description: &str,
    fail_at: Option<usize>,
) -> Result<(), String> {
    let relative = final_directory.strip_prefix(root).map_err(|_| {
        format!(
            "{description} {} is outside stable root {}",
            final_directory.display(),
            root.display()
        )
    })?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{description} {} is not a normal path below stable root {}",
            final_directory.display(),
            root.display()
        ));
    }
    if fail_at == Some(0) {
        return Err(format!(
            "injected failure syncing {description} ancestor {}",
            root.display()
        ));
    }
    sync_plain_directory(root, description)?;
    let mut current = root.to_owned();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            unreachable!("checked_relative_path accepted only normal components")
        };
        current.push(component);
        require_plain_directory(&current, description)?;
        if fail_at == Some(index + 1) {
            return Err(format!(
                "injected failure syncing {description} ancestor {}",
                current.display()
            ));
        }
        sync_plain_directory(&current, description)?;
    }
    Ok(())
}

fn remove_empty_retained_verify_directories(
    retention_root: &Path,
    retained_directory: &Path,
) -> Result<(), String> {
    let relative = checked_relative_path(
        retention_root,
        retained_directory,
        "retained verify-log rollback directory",
    )?;
    let components = relative.components().collect::<Vec<_>>();
    if components.len() != 4
        || components[1].as_os_str() != OsStr::new("retained")
        || components[2].as_os_str() != OsStr::new("verify")
    {
        return Err(format!(
            "retained verify-log rollback directory {} does not have the expected artifact/retained/verify/attempt layout below {}",
            retained_directory.display(),
            retention_root.display()
        ));
    }
    let artifact_dir = retention_root.join(components[0].as_os_str());
    let verify_dir = retained_directory
        .parent()
        .expect("validated retained verify-log attempt directory has a parent");
    let retained_dir = verify_dir
        .parent()
        .expect("validated retained verify-log verify directory has a parent");
    for (index, directory) in [retained_directory, verify_dir, retained_dir]
        .into_iter()
        .enumerate()
    {
        match fs::remove_dir(directory) {
            Ok(()) => {
                let parent = directory
                    .parent()
                    .expect("a retained verify-log rollback directory has a parent");
                sync_plain_directory(parent, "retained verify-log rollback parent")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty && index == 1 => {
                break;
            }
            Err(error) => {
                return Err(format!(
                    "cannot remove empty retained verify-log rollback directory {}: {error}",
                    directory.display()
                ));
            }
        }
    }
    sync_plain_directory(retention_root, "retained verify-log rollback stable root")?;
    for directory in [&artifact_dir, retained_dir, verify_dir] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => {
                sync_plain_directory(directory, "retained verify-log rollback ancestor")?;
            }
            Ok(_) => {
                return Err(format!(
                    "retained verify-log rollback ancestor {} is not a non-symlink directory",
                    directory.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(format!(
                    "cannot inspect retained verify-log rollback ancestor {}: {error}",
                    directory.display()
                ));
            }
        }
    }
    Ok(())
}

fn open_plain_file_below(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<File, String> {
    require_plain_parent_chain(artifact_dir, path, description)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} {} is not a regular file",
            path.display()
        ));
    }
    Ok(file)
}

fn copy_and_hash_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum_bytes: u64,
    description: &str,
) -> Result<ContentDigest, String> {
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {description}: {error}"))?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(read).expect("read buffer length fits u64"))
            .ok_or_else(|| format!("{description} size overflowed u64"))?;
        if bytes > maximum_bytes {
            return Err(format!(
                "{description} exceeds the {maximum_bytes}-byte limit"
            ));
        }
        digest.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|error| format!("cannot write {description}: {error}"))?;
    }
    Ok(ContentDigest {
        sha256: format!("{:x}", digest.finalize()),
        bytes,
    })
}

struct OpenedPlainFileEvidence {
    _file: File,
    identity: FileIdentity,
    digest: ContentDigest,
}

fn require_single_link(file: &File, path: &Path, description: &str) -> Result<(), String> {
    let links = file
        .metadata()
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?
        .nlink();
    if links != 1 {
        return Err(format!(
            "{description} {} has {links} hard links; expected exactly one",
            path.display()
        ));
    }
    Ok(())
}

fn open_and_inspect_plain_file_bounded(
    root: &Path,
    path: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<OpenedPlainFileEvidence, String> {
    let mut file = open_plain_file_below(root, path, description)?;
    let identity =
        FileIdentity::from_metadata(&file.metadata().map_err(|error| {
            format!("cannot inspect {description} {}: {error}", path.display())
        })?);
    let digest =
        copy_and_hash_bounded(&mut file, &mut std::io::sink(), maximum_bytes, description)?;
    require_path_identity(path, identity, description)?;
    Ok(OpenedPlainFileEvidence {
        _file: file,
        identity,
        digest,
    })
}

fn inspect_plain_file_bounded(
    root: &Path,
    path: &Path,
    maximum_bytes: u64,
    description: &str,
) -> Result<(FileIdentity, ContentDigest), String> {
    let evidence = open_and_inspect_plain_file_bounded(root, path, maximum_bytes, description)?;
    Ok((evidence.identity, evidence.digest))
}

fn inspect_gzip_file(
    root: &Path,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<InspectedGzip, String> {
    Ok(open_and_inspect_gzip_file(
        root,
        path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        description,
    )?
    .inspection)
}

fn open_and_inspect_gzip_file(
    root: &Path,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<OpenedGzipEvidence, String> {
    let mut file = open_plain_file_below(root, path, description)?;
    let inspection = inspect_open_gzip_file(
        &mut file,
        path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        description,
    )?;
    Ok(OpenedGzipEvidence { file, inspection })
}

fn inspect_open_gzip_file(
    file: &mut File,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<InspectedGzip, String> {
    let identity =
        FileIdentity::from_metadata(&file.metadata().map_err(|error| {
            format!("cannot inspect {description} {}: {error}", path.display())
        })?);
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let compressed = copy_and_hash_bounded(
        file,
        &mut std::io::sink(),
        maximum_compressed_bytes,
        description,
    )?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).map_err(|error| {
        format!(
            "cannot read deterministic gzip header from {description} {}: {error}",
            path.display()
        )
    })?;
    if header[..3] != [0x1f, 0x8b, 8] || header[3] != 0 || header[4..8] != [0, 0, 0, 0] {
        return Err(format!(
            "{description} {} does not have the canonical deterministic gzip header",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let uncompressed = {
        let mut decoder = MultiGzDecoder::new(file);
        copy_and_hash_bounded(
            &mut decoder,
            &mut std::io::sink(),
            maximum_uncompressed_bytes,
            description,
        )?
    };
    Ok(InspectedGzip {
        identity,
        compressed,
        uncompressed,
    })
}

pub(super) fn retained_verify_log_relative_path(attempt: u64) -> Result<PathBuf, String> {
    if attempt == 0 {
        return Err("retained verify-log attempt must be positive".into());
    }
    Ok(PathBuf::from("retained")
        .join("verify")
        .join(attempt.to_string())
        .join("run-1.log.gz"))
}

fn is_verify_log_staging_name(name: &str) -> bool {
    name.starts_with(VERIFY_LOG_STAGING_PREFIX) && name.ends_with(VERIFY_LOG_STAGING_SUFFIX)
}

fn scan_existing_verify_log_bytes(
    retention_root: &Path,
    results_path: &Path,
    policy: VerifyLogRetentionPolicy,
) -> Result<u64, String> {
    scan_existing_verify_log_bytes_with_hook(retention_root, results_path, policy, None)
}

#[derive(Debug)]
struct ScannedRetainedVerifyLog {
    path: PathBuf,
    retained: RetainedVerifyLog,
    inspection: InspectedGzip,
}

fn scan_existing_verify_log_bytes_with_hook(
    retention_root: &Path,
    results_path: &Path,
    policy: VerifyLogRetentionPolicy,
    before_final_recheck: Option<&dyn Fn(&Path)>,
) -> Result<u64, String> {
    let results_bytes = read_existing_result(results_path)?;
    let mut descriptors = BTreeMap::<(PathBuf, u64), RetainedVerifyLog>::new();
    if let Some(results_bytes) = results_bytes {
        for (line_index, line) in results_bytes.split(|byte| *byte == b'\n').enumerate() {
            if line.is_empty() {
                continue;
            }
            let result = serde_json::from_slice::<CellResult>(line).map_err(|error| {
                format!(
                    "cannot parse result row {} in {} while reconciling retained verify logs: {error}",
                    line_index + 1,
                    results_path.display()
                )
            })?;
            let Some(retained) = result.validate_retained_verify_log_binding()? else {
                continue;
            };
            let artifact_dir = PathBuf::from(&result.artifact_dir);
            checked_relative_path(
                retention_root,
                &artifact_dir,
                "retained verify-log artifact directory from result row",
            )?;
            let key = (artifact_dir, retained.attempt);
            if descriptors.insert(key.clone(), retained.clone()).is_some() {
                return Err(format!(
                    "duplicate retained verify-log descriptors for {} attempt {}",
                    key.0.display(),
                    key.1
                ));
            }
        }
    }

    let configured_result = check_file_path_below(
        retention_root,
        results_path,
        "configured result-row destination",
    )?;
    let mut artifact_dirs = Vec::new();
    for entry in fs::read_dir(retention_root).map_err(|error| {
        format!(
            "cannot read verify-log retention root {}: {error}",
            retention_root.display()
        )
    })? {
        let entry = entry.map_err(|error| {
            format!(
                "cannot read entry below verify-log retention root {}: {error}",
                retention_root.display()
            )
        })?;
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            format!(
                "cannot inspect verify-log retention-root entry {}: {error}",
                entry.path().display()
            )
        })?;
        let entry_path = entry.path();
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "verify-log retention-root entry {} is a symlink",
                entry_path.display()
            ));
        }
        if metadata.is_dir() {
            if entry.file_name() == OsStr::new("retained") {
                return Err(format!(
                    "unexpected root-level retained verify-log layout: {}",
                    entry_path.display()
                ));
            }
            artifact_dirs.push(entry_path);
            continue;
        }
        let root_file = check_file_path_below(
            retention_root,
            &entry_path,
            "verify-log retention-root file",
        )?;
        if root_file.normalized != configured_result.normalized {
            return Err(format!(
                "unexpected root-level file in verify-log retention layout: {}",
                entry_path.display()
            ));
        }
    }

    let mut accounted = 0u64;
    let mut final_inodes = BTreeSet::new();
    let mut matched_descriptors = BTreeSet::new();
    let mut scanned_finals = Vec::new();
    for artifact_dir in artifact_dirs {
        let retained_dir = artifact_dir.join("retained");
        match fs::symlink_metadata(&retained_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot inspect retained verify-log directory {}: {error}",
                    retained_dir.display()
                ));
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(format!(
                    "retained verify-log path {} is not a non-symlink directory",
                    retained_dir.display()
                ));
            }
            Ok(_) => {}
        }
        let mut found_verify_dir = false;
        for retained_entry in fs::read_dir(&retained_dir).map_err(|error| {
            format!(
                "cannot read retained verify-log directory {}: {error}",
                retained_dir.display()
            )
        })? {
            let retained_entry = retained_entry.map_err(|error| {
                format!(
                    "cannot read entry below retained verify-log directory {}: {error}",
                    retained_dir.display()
                )
            })?;
            let retained_path = retained_entry.path();
            if retained_entry.file_name() != OsStr::new("verify") {
                return Err(format!(
                    "unreferenced bytes in retained verify-log layout: {}",
                    retained_path.display()
                ));
            }
            require_plain_directory(&retained_path, "retained verify-log directory")?;
            found_verify_dir = true;
        }
        if !found_verify_dir {
            continue;
        }
        let verify_dir = retained_dir.join("verify");
        require_plain_parent_chain(
            retention_root,
            &verify_dir.join("entry"),
            "retained verify-log directory",
        )?;
        for attempt_entry in fs::read_dir(&verify_dir).map_err(|error| {
            format!(
                "cannot read retained verify-log directory {}: {error}",
                verify_dir.display()
            )
        })? {
            let attempt_entry = attempt_entry.map_err(|error| {
                format!(
                    "cannot read entry below retained verify-log directory {}: {error}",
                    verify_dir.display()
                )
            })?;
            let attempt_path = attempt_entry.path();
            let attempt_name = attempt_entry.file_name();
            let attempt_name = attempt_name.to_str().ok_or_else(|| {
                format!(
                    "retained verify-log attempt directory {} is not UTF-8",
                    attempt_path.display()
                )
            })?;
            let attempt = attempt_name.parse::<u64>().ok().filter(|value| *value > 0);
            let Some(attempt) = attempt else {
                return Err(format!(
                    "retained verify-log attempt directory {} is not a canonical positive integer",
                    attempt_path.display()
                ));
            };
            if attempt.to_string() != attempt_name {
                return Err(format!(
                    "retained verify-log attempt directory {} is not canonical (expected {})",
                    attempt_path.display(),
                    attempt
                ));
            }
            require_plain_directory(&attempt_path, "retained verify-log attempt directory")?;
            let mut final_path = None;
            let mut staging_paths = Vec::new();
            for file_entry in fs::read_dir(&attempt_path).map_err(|error| {
                format!(
                    "cannot read retained verify-log attempt directory {}: {error}",
                    attempt_path.display()
                )
            })? {
                let file_entry = file_entry.map_err(|error| {
                    format!(
                        "cannot read entry below retained verify-log attempt directory {}: {error}",
                        attempt_path.display()
                    )
                })?;
                let path = file_entry.path();
                let name = file_entry.file_name();
                let name = name.to_str().ok_or_else(|| {
                    format!("retained verify-log file {} is not UTF-8", path.display())
                })?;
                if name == "run-1.log.gz" {
                    if final_path.replace(path.clone()).is_some() {
                        return Err(format!(
                            "duplicate retained verify-log finals in {}",
                            attempt_path.display()
                        ));
                    }
                } else if is_verify_log_staging_name(name) {
                    staging_paths.push(path);
                } else {
                    return Err(format!(
                        "unexpected file in retained verify-log directory: {}",
                        path.display()
                    ));
                }
            }
            if !staging_paths.is_empty() {
                for staging_path in &staging_paths {
                    inspect_gzip_file(
                        retention_root,
                        staging_path,
                        VERIFY_LOG_MAX_COMPRESSED_BYTES,
                        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                        "pre-existing retained verify-log staging file",
                    )?;
                }
                let detail = if final_path.is_some() {
                    "a final and staging file coexist"
                } else if staging_paths.len() > 1 {
                    "multiple staging files coexist"
                } else {
                    "an unreferenced staging file remains"
                };
                return Err(format!(
                    "restart-inconsistent retained verify-log layout in {}: {detail}; refusing to discard or silently charge {}",
                    attempt_path.display(),
                    staging_paths[0].display()
                ));
            }
            let final_path = final_path.ok_or_else(|| {
                format!(
                    "retained verify-log attempt directory {} contains no canonical final",
                    attempt_path.display()
                )
            })?;
            let key = (artifact_dir.clone(), attempt);
            let retained = descriptors.get(&key).ok_or_else(|| {
                format!(
                    "retained verify log {} has no result-row descriptor",
                    final_path.display()
                )
            })?;
            let inspected = inspect_gzip_file(
                retention_root,
                &final_path,
                VERIFY_LOG_MAX_COMPRESSED_BYTES,
                VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                "pre-existing retained verify log",
            )?;
            validate_retained_verify_log_inspection(retained, &inspected)?;
            let attempt_label = attempt.to_string();
            let run1_paths = verify_run_paths(&artifact_dir, &attempt_label, VerifyRun::Run1)?;
            let run2_paths = verify_run_paths(&artifact_dir, &attempt_label, VerifyRun::Run2)?;
            let mut bound_paths = vec![
                configured_result.clone(),
                check_file_path_below(
                    retention_root,
                    &final_path,
                    "pre-existing retained verify log",
                )?,
                check_file_path_below(retention_root, &run1_paths.log, "verify run-1 raw log")?,
                check_file_path_below(retention_root, &run2_paths.log, "verify run-2 raw log")?,
            ];
            for (description, path) in [
                ("run-1 result", run1_paths.result),
                ("run-1 stdout", run1_paths.stdout),
                ("run-1 stderr", run1_paths.stderr),
                ("run-1 summary", run1_paths.summary),
                ("run-2 result", run2_paths.result),
                ("run-2 stdout", run2_paths.stdout),
                ("run-2 stderr", run2_paths.stderr),
                ("run-2 summary", run2_paths.summary),
            ] {
                bound_paths.push(check_file_path_below(retention_root, &path, description)?);
            }
            reject_file_path_aliases(&bound_paths)?;
            for (description, path, expected) in [
                (
                    "pre-existing verify run-1 raw log",
                    &run1_paths.log,
                    ContentDigest {
                        sha256: retained.uncompressed_sha256.clone(),
                        bytes: retained.uncompressed_bytes,
                    },
                ),
                (
                    "pre-existing verify run-2 raw log",
                    &run2_paths.log,
                    ContentDigest {
                        sha256: retained.peer_uncompressed_sha256.clone(),
                        bytes: retained.peer_uncompressed_bytes,
                    },
                ),
            ] {
                match fs::symlink_metadata(path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "cannot inspect {description} {}: {error}",
                            path.display()
                        ));
                    }
                    Ok(_) => {
                        let (_, actual) = inspect_plain_file_bounded(
                            retention_root,
                            path,
                            VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                            description,
                        )?;
                        if actual != expected {
                            return Err(format!(
                                "{description} {} disagrees with its result-row descriptor",
                                path.display()
                            ));
                        }
                    }
                }
            }
            if !final_inodes.insert(inspected.identity) {
                return Err(format!(
                    "retained verify-log finals reuse one inode at {}",
                    final_path.display()
                ));
            }
            scanned_finals.push(ScannedRetainedVerifyLog {
                path: final_path,
                retained: retained.clone(),
                inspection: inspected.clone(),
            });
            matched_descriptors.insert(key);
            accounted = accounted
                .checked_add(inspected.compressed.bytes)
                .ok_or_else(|| {
                    "pre-existing retained verify-log byte accounting overflowed u64".to_string()
                })?;
            if accounted > policy.maximum_total_compressed_bytes {
                return Err(format!(
                    "pre-existing retained verify logs require {accounted} compressed bytes, exceeding the {}-byte aggregate limit",
                    policy.maximum_total_compressed_bytes
                ));
            }
        }
    }
    if let Some(((artifact_dir, attempt), _)) = descriptors
        .iter()
        .find(|(key, _)| !matched_descriptors.contains(*key))
    {
        return Err(format!(
            "result-row descriptor for {} attempt {} has no retained verify-log final",
            artifact_dir.display(),
            attempt
        ));
    }
    for scanned in scanned_finals {
        if let Some(before_final_recheck) = before_final_recheck {
            before_final_recheck(&scanned.path);
        }
        let rechecked = inspect_gzip_file(
            retention_root,
            &scanned.path,
            VERIFY_LOG_MAX_COMPRESSED_BYTES,
            VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
            "pre-existing retained verify log at end of restart scan",
        )?;
        validate_retained_verify_log_inspection(&scanned.retained, &rechecked)?;
        if rechecked != scanned.inspection {
            return Err(format!(
                "pre-existing retained verify log {} changed during restart scan",
                scanned.path.display()
            ));
        }
        require_path_identity(
            &scanned.path,
            rechecked.identity,
            "pre-existing retained verify log at end of restart scan",
        )?;
    }
    Ok(accounted)
}

// The variants are production fault boundaries constructed only by tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StagingWriteFault {
    ZeroFirst,
    ShortFirst,
    ErrorAfterFirstSuccessfulWrite,
}

struct VerifyLogStagingWriter<'a> {
    file: &'a mut File,
    budget: VerifyLogRetentionBudget,
    /// Bytes still charged to `budget`, including a pessimistic whole write
    /// when the underlying writer reports an error after it may have changed
    /// the staging file.
    charged: &'a mut u64,
    on_first_write: Option<&'a (dyn Fn() + Sync)>,
    first_write_observed: bool,
    fault: Option<StagingWriteFault>,
    write_calls: usize,
}

impl Write for VerifyLogStagingWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let requested = u64::try_from(buffer.len()).map_err(|_| {
            std::io::Error::other("retained verify-log write length does not fit u64")
        })?;
        let staged = self.charged.checked_add(requested).ok_or_else(|| {
            std::io::Error::other("retained verify-log staging byte count overflowed u64")
        })?;
        if staged > VERIFY_LOG_MAX_COMPRESSED_BYTES {
            return Err(std::io::Error::other(format!(
                "retained compressed verify log exceeds the {VERIFY_LOG_MAX_COMPRESSED_BYTES}-byte per-log limit"
            )));
        }
        self.budget
            .reserve_additional(requested)
            .map_err(std::io::Error::other)?;
        if !self.first_write_observed {
            self.first_write_observed = true;
            if let Some(on_first_write) = self.on_first_write {
                // The reservation lock has been released, but the requested
                // bytes remain charged while the callback and actual write
                // overlap other staging writers.
                on_first_write();
            }
        }
        let call = self.write_calls;
        self.write_calls += 1;
        let write_result = match self.fault {
            Some(StagingWriteFault::ZeroFirst) if call == 0 => Ok(0),
            Some(StagingWriteFault::ShortFirst) if call == 0 => {
                self.file.write(&buffer[..buffer.len().min(1)])
            }
            Some(StagingWriteFault::ErrorAfterFirstSuccessfulWrite) if call > 0 => Err(
                std::io::Error::other("injected retained verify-log staging write failure"),
            ),
            _ => self.file.write(buffer),
        };
        let written = match write_result {
            Ok(written) => written,
            Err(error) => {
                // `Write::write` may have changed the file before returning an
                // error. Keep the whole request charged until the staging file
                // is removed and its parent directory is synced.
                *self.charged = staged;
                return Err(error);
            }
        };
        if written == 0 && !buffer.is_empty() {
            if let Err(error) = self.budget.release(requested) {
                *self.charged = staged;
                return Err(std::io::Error::other(error));
            }
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        let unused = requested
            .checked_sub(u64::try_from(written).expect("write result fits input length"))
            .expect("write cannot report more bytes than requested");
        if unused == 0 {
            *self.charged = staged;
        } else if let Err(error) = self.budget.release(unused) {
            // The full request remains charged if releasing the unused tail
            // fails. This deliberately overcounts instead of allowing bytes
            // on disk to escape the aggregate limit.
            *self.charged = staged;
            return Err(std::io::Error::other(error));
        } else {
            *self.charged = self
                .charged
                .checked_add(u64::try_from(written).expect("write result fits input length"))
                .expect("the requested staging-byte addition was already checked");
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn open_verified_retained_verify_log_with_limits(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
) -> Result<OpenedGzipEvidence, String> {
    if retained.role != RetainedVerifyLogRole::Run1 {
        return Err("retained verify log has an unsupported role".into());
    }
    if &retained.cell_id != expected_cell_id || retained.attempt != expected_attempt {
        return Err("retained verify log does not match its cell id and attempt".into());
    }
    let expected_relative = retained_verify_log_relative_path(expected_attempt)?;
    if Path::new(&retained.relative_path) != expected_relative {
        return Err(format!(
            "retained verify log path must be exactly {}, got {}",
            expected_relative.display(),
            retained.relative_path
        ));
    }
    let path = artifact_dir.join(&expected_relative);
    let opened = open_and_inspect_gzip_file(
        artifact_dir,
        &path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        "retained compressed verify log",
    )?;
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    validate_retained_verify_log_inspection(retained, &opened.inspection)?;
    require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(opened)
}

fn verify_retained_verify_log_with_limit(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    maximum_uncompressed_bytes: u64,
) -> Result<(), String> {
    open_verified_retained_verify_log_with_limits(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
    )?;
    Ok(())
}

fn validate_retained_verify_log_inspection(
    retained: &RetainedVerifyLog,
    inspected: &InspectedGzip,
) -> Result<(), String> {
    if inspected.compressed.sha256 != retained.compressed_sha256
        || inspected.compressed.bytes != retained.compressed_bytes
    {
        return Err(format!(
            "retained compressed verify log digest/size mismatch: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            retained.compressed_bytes,
            retained.compressed_sha256,
            inspected.compressed.bytes,
            inspected.compressed.sha256
        ));
    }
    if inspected.uncompressed.sha256 != retained.uncompressed_sha256
        || inspected.uncompressed.bytes != retained.uncompressed_bytes
    {
        return Err(format!(
            "retained uncompressed verify log digest/size mismatch: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            retained.uncompressed_bytes,
            retained.uncompressed_sha256,
            inspected.uncompressed.bytes,
            inspected.uncompressed.sha256
        ));
    }
    Ok(())
}

/// Re-read and authenticate one retained verify log against its typed binding.
pub fn verify_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
) -> Result<(), String> {
    verify_retained_verify_log_with_limit(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )
}

/// Read the exact uncompressed bytes named by a retained-log descriptor.
///
/// Descriptor validation, gzip decoding, and path-identity checks all use one
/// held `O_NOFOLLOW` file descriptor. This is the scoring/read path; callers
/// retaining the gzip itself should use [`copy_verified_retained_verify_log`].
pub fn read_verified_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
) -> Result<Vec<u8>, String> {
    let mut opened = open_verified_retained_verify_log_with_limits(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )?;
    opened
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek retained compressed verify log: {error}"))?;
    let mut bytes = Vec::new();
    let digest = {
        let mut decoder = MultiGzDecoder::new(&mut opened.file);
        copy_and_hash_bounded(
            &mut decoder,
            &mut bytes,
            VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
            "retained uncompressed verify log",
        )?
    };
    if digest != opened.inspection.uncompressed {
        return Err("retained verify log changed while reading its uncompressed bytes".into());
    }
    let path = artifact_dir.join(&retained.relative_path);
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(bytes)
}

/// Copy the exact gzip bytes named by a retained-log descriptor from one held
/// source descriptor into a caller-owned destination.
pub fn copy_verified_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    destination: &mut impl Write,
    maximum_compressed_bytes: u64,
) -> Result<VerifiedRetainedLogCopy, String> {
    let maximum_compressed_bytes = maximum_compressed_bytes.min(VERIFY_LOG_MAX_COMPRESSED_BYTES);
    let mut opened = open_verified_retained_verify_log_with_limits(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        maximum_compressed_bytes,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )?;
    opened
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek retained compressed verify log: {error}"))?;
    let copied = copy_and_hash_bounded(
        &mut opened.file,
        destination,
        maximum_compressed_bytes,
        "retained compressed verify log copy",
    )?;
    if copied != opened.inspection.compressed {
        return Err("retained verify log changed while copying its compressed bytes".into());
    }
    let path = artifact_dir.join(&retained.relative_path);
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(VerifiedRetainedLogCopy {
        compressed_sha256: copied.sha256,
        compressed_bytes: copied.bytes,
        uncompressed_sha256: opened.inspection.uncompressed.sha256,
        uncompressed_bytes: opened.inspection.uncompressed.bytes,
    })
}

/// Reconstruct cleanup authority from one durable result row after restart.
///
/// Raw paths are derived from the fixed verify layout; the serialized
/// descriptor supplies both expected digests and remains bound to the outer
/// cell id and attempt by [`CellResult::validate_retained_verify_log_binding`].
pub fn reopen_retained_verify_log_publication(
    result: &CellResult,
) -> Result<RetainedVerifyLogPublication, String> {
    let retained = result
        .validate_retained_verify_log_binding()?
        .ok_or_else(|| "result row has no retained verify-log descriptor".to_string())?;
    let artifact_dir = PathBuf::from(&result.artifact_dir);
    let expected_id = CellId {
        test: result.test.clone(),
        mode: result.mode.clone(),
        backend: result.backend.clone(),
    };
    verify_retained_verify_log(&artifact_dir, retained, &expected_id, result.attempt)?;
    let attempt = result.attempt.to_string();
    let run1_raw = verify_run_paths(&artifact_dir, &attempt, VerifyRun::Run1)?.log;
    let run2_raw = verify_run_paths(&artifact_dir, &attempt, VerifyRun::Run2)?.log;
    Ok(RetainedVerifyLogPublication {
        retained: retained.clone(),
        run1_raw,
        run2_raw,
        run1_digest: ContentDigest {
            sha256: retained.uncompressed_sha256.clone(),
            bytes: retained.uncompressed_bytes,
        },
        run2_digest: ContentDigest {
            sha256: retained.peer_uncompressed_sha256.clone(),
            bytes: retained.peer_uncompressed_bytes,
        },
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerifyLogTransactionFailurePoint {
    BeforeStagingFileCreation,
    AfterFinalRename,
    AfterDirectorySync,
    AfterFinalVerification,
    BeforeResultPublication,
}

#[derive(Clone, Copy, Default)]
struct VerifyLogTransactionHooks<'a> {
    failure: Option<VerifyLogTransactionFailurePoint>,
    result_publication_failure: Option<ResultPublicationFailurePoint>,
    fail_final_removal: bool,
    on_first_staging_write: Option<&'a (dyn Fn() + Sync)>,
    before_descriptor_publication: Option<&'a (dyn Fn(&Path) + Sync)>,
    staging_write_fault: Option<StagingWriteFault>,
    directory_sync_failure_at: Option<usize>,
}

fn retained_verify_log_attempt_index(
    result: &CellResult,
    cell_id: &CellId,
    attempt: u64,
    artifact_dir: &Path,
) -> Result<usize, String> {
    result.require_current_classification()?;
    result.require_current_timeout_policy()?;
    if result.test != cell_id.test
        || result.mode != cell_id.mode
        || result.backend != cell_id.backend
        || result.attempt != attempt
    {
        return Err("retained verify log does not match its result-row identity".into());
    }
    if Path::new(&result.artifact_dir) != artifact_dir {
        return Err("retained verify log does not match its result-row artifact directory".into());
    }
    if result.attempts.len() != 1 {
        return Err("a harness-managed verify result must contain exactly one attempt row".into());
    }
    if result.attempts[0].retained_verify_log.is_some() {
        return Err("verify attempt already carries a retained-log descriptor".into());
    }
    if result.outcome == "HOST-INAPPLICABLE" {
        return Err("a host-inapplicable cell cannot publish a retained verify log".into());
    }
    Ok(0)
}

fn verify_retention_paths_do_not_alias(
    pair: &ComparedVerifyPair,
    retention_budget: &VerifyLogRetentionBudget,
    results_path: &Path,
    retained_path: &Path,
) -> Result<(), String> {
    let mut paths = vec![
        check_file_path_below(
            &retention_budget.retention_root,
            results_path,
            "result-row destination",
        )?,
        check_file_path_below(
            &retention_budget.retention_root,
            &pair.run1_log,
            "verify run-1 raw log",
        )?,
        check_file_path_below(
            &retention_budget.retention_root,
            &pair.run2_log,
            "verify run-2 raw log",
        )?,
        check_file_path_below(
            &retention_budget.retention_root,
            retained_path,
            "retained verify-log destination",
        )?,
    ];
    for (description, path) in &pair.fixed_result_destinations {
        paths.push(check_file_path_below(
            &retention_budget.retention_root,
            path,
            description.clone(),
        )?);
    }
    let run1_identity = paths[1]
        .identity
        .ok_or_else(|| "verify run-1 raw log is missing before retention".to_string())?;
    let run2_identity = paths[2]
        .identity
        .ok_or_else(|| "verify run-2 raw log is missing before retention".to_string())?;
    if run1_identity != pair.run1_log_identity || run2_identity != pair.run2_log_identity {
        return Err("verify raw-log identity changed after comparison".into());
    }
    reject_file_path_aliases(&paths)
}

fn abort_staged_verify_log(
    temporary: tempfile::NamedTempFile,
    retained_directory: &Path,
    reservation: VerifyLogRetentionReservation,
    primary_error: String,
) -> Result<RetainedVerifyLogPublication, String> {
    let staging_path = temporary.path().to_owned();
    let retention_root = reservation.budget.retention_root.clone();
    match temporary.close() {
        Ok(()) => {
            match remove_empty_retained_verify_directories(&retention_root, retained_directory) {
                Ok(()) => match reservation.rollback() {
                    Ok(()) => Err(primary_error),
                    Err(accounting_error) => Err(format!(
                        "{primary_error}; removed the retained verify-log staging file but could not roll back its accounting: {accounting_error}"
                    )),
                },
                Err(sync_error) => {
                    reservation.commit();
                    Err(format!(
                        "{primary_error}; removed the retained verify-log staging file but could not durably clean up its directories, so its bytes remain charged: {sync_error}"
                    ))
                }
            }
        }
        Err(removal_error) => {
            reservation.commit();
            Err(format!(
                "{primary_error}; could not remove retained verify-log staging file {}, so its bytes remain charged: {removal_error}",
                staging_path.display()
            ))
        }
    }
}

fn abort_empty_retained_verify_log_layout(
    retention_root: &Path,
    retained_directory: &Path,
    primary_error: String,
) -> Result<RetainedVerifyLogPublication, String> {
    match remove_empty_retained_verify_directories(retention_root, retained_directory) {
        Ok(()) => Err(primary_error),
        Err(cleanup_error) => Err(format!(
            "{primary_error}; could not durably clean up the empty retained verify-log directories: {cleanup_error}"
        )),
    }
}

fn abort_retained_verify_log(
    retained_path: &Path,
    retained_directory: &Path,
    reservation: VerifyLogRetentionReservation,
    primary_error: String,
    fail_final_removal: bool,
) -> Result<RetainedVerifyLogPublication, String> {
    let retention_root = reservation.budget.retention_root.clone();
    let removal = if fail_final_removal {
        Err(std::io::Error::other(
            "injected retained verify-log final-removal failure",
        ))
    } else {
        fs::remove_file(retained_path)
    };
    match removal {
        Ok(()) => {
            match remove_empty_retained_verify_directories(&retention_root, retained_directory) {
                Ok(()) => match reservation.rollback() {
                    Ok(()) => Err(primary_error),
                    Err(accounting_error) => Err(format!(
                        "{primary_error}; removed the unreferenced retained verify log but could not roll back its accounting: {accounting_error}"
                    )),
                },
                Err(sync_error) => {
                    reservation.commit();
                    Err(format!(
                        "{primary_error}; removed the unreferenced retained verify log but could not durably clean up its directories, so its bytes remain charged: {sync_error}"
                    ))
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match remove_empty_retained_verify_directories(&retention_root, retained_directory) {
                Ok(()) => match reservation.rollback() {
                    Ok(()) => Err(primary_error),
                    Err(accounting_error) => Err(format!(
                        "{primary_error}; retained verify log was absent but its accounting could not be rolled back: {accounting_error}"
                    )),
                },
                Err(sync_error) => {
                    reservation.commit();
                    Err(format!(
                        "{primary_error}; retained verify log was absent but its directories could not be durably cleaned up, so its bytes remain charged: {sync_error}"
                    ))
                }
            }
        }
        Err(removal_error) => {
            reservation.commit();
            Err(format!(
                "{primary_error}; could not remove unreferenced retained verify log {}, so its bytes remain charged: {removal_error}",
                retained_path.display()
            ))
        }
    }
}

fn retain_verify_log_with_limit(
    pair: ComparedVerifyPair,
    maximum_uncompressed_bytes: u64,
    retention_budget: &VerifyLogRetentionBudget,
    results_path: &Path,
    result: &mut CellResult,
    hooks: VerifyLogTransactionHooks<'_>,
) -> Result<RetainedVerifyLogPublication, String> {
    retention_budget.require_artifact_dir(&pair.artifact_dir)?;
    let retained_relative = retained_verify_log_relative_path(pair.attempt)?;
    let retained_path = pair.artifact_dir.join(&retained_relative);
    retained_verify_log_attempt_index(result, &pair.cell_id, pair.attempt, &pair.artifact_dir)?;
    verify_retention_paths_do_not_alias(&pair, retention_budget, results_path, &retained_path)?;
    retention_budget.require_results_path(results_path)?;
    let (current_run1_identity, current_run1) = inspect_plain_file_bounded(
        &pair.artifact_dir,
        &pair.run1_log,
        maximum_uncompressed_bytes,
        "verify run-1 log",
    )?;
    let (current_run2_identity, current_run2) = inspect_plain_file_bounded(
        &pair.artifact_dir,
        &pair.run2_log,
        maximum_uncompressed_bytes,
        "verify run-2 log",
    )?;
    if current_run1_identity != pair.run1_log_identity
        || current_run2_identity != pair.run2_log_identity
    {
        return Err("verify raw-log identity changed after comparison".into());
    }
    if current_run1 != pair.run1_log_digest || current_run2 != pair.run2_log_digest {
        return Err(
            "verify logs changed after comparison; refusing to retain bytes that were not compared"
                .into(),
        );
    }

    let ComparedVerifyPair {
        comparison,
        artifact_dir,
        cell_id,
        attempt,
        run1_log,
        run2_log,
        run1_log_bytes,
        run1_log_digest,
        run1_log_identity: _,
        run2_log_digest,
        run2_log_identity: _,
        fixed_result_destinations: _,
    } = pair;
    let compared_info_messages = comparison
        .report
        .compared_log_messages
        .as_ref()
        .ok_or_else(|| "canonical verify comparison omitted compared log counts".to_string())?
        .left;
    let retained_directory = create_plain_relative_directory(
        &artifact_dir,
        retained_relative
            .parent()
            .expect("retained verify log has a parent"),
        "retained verify log directory",
    )?;
    match fs::symlink_metadata(&retained_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(format!(
                "retained verify log {} already exists",
                retained_path.display()
            ));
        }
        Err(error) => {
            return Err(format!(
                "cannot inspect retained verify log {}: {error}",
                retained_path.display()
            ));
        }
    }

    if hooks.failure == Some(VerifyLogTransactionFailurePoint::BeforeStagingFileCreation) {
        return abort_empty_retained_verify_log_layout(
            &retention_budget.retention_root,
            &retained_directory,
            "injected failure before retained verify-log staging-file creation".into(),
        );
    }

    let mut temporary = match tempfile::Builder::new()
        .prefix(VERIFY_LOG_STAGING_PREFIX)
        .suffix(VERIFY_LOG_STAGING_SUFFIX)
        .tempfile_in(&retained_directory)
    {
        Ok(temporary) => temporary,
        Err(error) => {
            return abort_empty_retained_verify_log_layout(
                &retention_budget.retention_root,
                &retained_directory,
                format!(
                    "cannot create temporary retained verify log in {}: {error}",
                    retained_directory.display()
                ),
            );
        }
    };
    let created_staging_identity =
        FileIdentity::from_metadata(&temporary.as_file().metadata().map_err(|error| {
            format!(
                "cannot inspect temporary retained verify log {}: {error}",
                temporary.path().display()
            )
        })?);
    let mut charged = 0u64;
    let compression = {
        let mut run1_bytes = run1_log_bytes.as_slice();
        let staging_writer = VerifyLogStagingWriter {
            file: temporary.as_file_mut(),
            budget: retention_budget.clone(),
            charged: &mut charged,
            on_first_write: hooks.on_first_staging_write,
            first_write_observed: false,
            fault: hooks.staging_write_fault,
            write_calls: 0,
        };
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(staging_writer, Compression::new(6));
        let digest = copy_and_hash_bounded(
            &mut run1_bytes,
            &mut encoder,
            maximum_uncompressed_bytes,
            "verify run-1 log",
        );
        match digest {
            Ok(digest) => encoder
                .finish()
                .map(|_| digest)
                .map_err(|error| format!("cannot finish retained verify gzip stream: {error}")),
            Err(error) => {
                // Dropping the encoder can attempt final writes. `charged` is
                // borrowed by its writer, so create the reservation only after
                // the encoder has finished dropping.
                drop(encoder);
                Err(error)
            }
        }
    };
    let reservation = VerifyLogRetentionReservation {
        budget: retention_budget.clone(),
        compressed_bytes: charged,
        resolved: false,
    };
    let uncompressed = match compression {
        Ok(uncompressed) => uncompressed,
        Err(error) => {
            return abort_staged_verify_log(temporary, &retained_directory, reservation, error);
        }
    };
    if uncompressed != run1_log_digest {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            "captured verify run-1 bytes changed after comparison".into(),
        );
    }
    if let Err(error) = temporary
        .as_file_mut()
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
    {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            format!("cannot flush retained verify gzip stream: {error}"),
        );
    }
    let staging = match inspect_gzip_file(
        &artifact_dir,
        temporary.path(),
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "temporary compressed verify log",
    ) {
        Ok(staging) => staging,
        Err(error) => {
            return abort_staged_verify_log(temporary, &retained_directory, reservation, error);
        }
    };
    if staging.identity != created_staging_identity {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            "temporary retained verify log changed identity before inspection".into(),
        );
    }
    if staging.uncompressed != run1_log_digest {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            "temporary compressed verify log does not decode to the compared run-1 bytes".into(),
        );
    }
    if staging.compressed.bytes != reservation.compressed_bytes {
        return abort_staged_verify_log(
            temporary,
            &retained_directory,
            reservation,
            format!(
                "retained verify-log staging count changed: wrote {} bytes but read back {}",
                charged, staging.compressed.bytes
            ),
        );
    }
    if let Err(error) = require_path_identity(
        temporary.path(),
        staging.identity,
        "temporary compressed verify log",
    ) {
        return abort_staged_verify_log(temporary, &retained_directory, reservation, error);
    }
    match temporary.persist_noclobber(&retained_path) {
        Ok(file) => drop(file),
        Err(error) => {
            let primary_error = format!(
                "cannot atomically publish retained verify log {}: {}",
                retained_path.display(),
                error.error
            );
            return abort_staged_verify_log(
                error.file,
                &retained_directory,
                reservation,
                primary_error,
            );
        }
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::AfterFinalRename) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure after retained verify-log rename".into(),
            hooks.fail_final_removal,
        );
    }
    if let Err(error) = sync_relative_directory_chain_with_failure(
        &retention_budget.retention_root,
        &retained_directory,
        "retained verify-log directory",
        hooks.directory_sync_failure_at,
    ) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::AfterDirectorySync) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure after retained verify-log directory sync".into(),
            hooks.fail_final_removal,
        );
    }

    let mut final_evidence = match open_and_inspect_gzip_file(
        &artifact_dir,
        &retained_path,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "retained compressed verify log",
    ) {
        Ok(inspection) => inspection,
        Err(error) => {
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                error,
                hooks.fail_final_removal,
            );
        }
    };
    let final_inspection = final_evidence.inspection.clone();
    if final_inspection.compressed != staging.compressed
        || final_inspection.uncompressed != staging.uncompressed
    {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "retained verify log changed during atomic rename".into(),
            hooks.fail_final_removal,
        );
    }
    let retained = RetainedVerifyLog {
        relative_path: retained_relative.to_string_lossy().into_owned(),
        role: RetainedVerifyLogRole::Run1,
        cell_id: cell_id.clone(),
        attempt,
        uncompressed_sha256: final_inspection.uncompressed.sha256.clone(),
        uncompressed_bytes: final_inspection.uncompressed.bytes,
        compressed_sha256: final_inspection.compressed.sha256.clone(),
        compressed_bytes: final_inspection.compressed.bytes,
        peer_uncompressed_sha256: run2_log_digest.sha256.clone(),
        peer_uncompressed_bytes: run2_log_digest.bytes,
        compared_info_messages,
    };
    if let Err(error) = validate_retained_verify_log_inspection(&retained, &final_inspection) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::AfterFinalVerification) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure after retained verify-log final verification".into(),
            hooks.fail_final_removal,
        );
    }
    if let Some(before_descriptor_publication) = hooks.before_descriptor_publication {
        before_descriptor_publication(&retained_path);
    }
    let publication_inspection = match inspect_open_gzip_file(
        &mut final_evidence.file,
        &retained_path,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
        "retained compressed verify log immediately before descriptor publication",
    ) {
        Ok(inspection) => inspection,
        Err(error) => {
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                error,
                hooks.fail_final_removal,
            );
        }
    };
    if publication_inspection.identity != final_inspection.identity {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "retained compressed verify log changed identity before publication".into(),
            hooks.fail_final_removal,
        );
    }
    if publication_inspection.compressed != final_inspection.compressed
        || publication_inspection.uncompressed != final_inspection.uncompressed
    {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "retained verify log changed before descriptor publication".into(),
            hooks.fail_final_removal,
        );
    }
    if let Err(error) = validate_retained_verify_log_inspection(&retained, &publication_inspection)
    {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if let Err(error) = require_path_identity(
        &retained_path,
        publication_inspection.identity,
        "retained compressed verify log immediately before descriptor publication",
    ) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if hooks.failure == Some(VerifyLogTransactionFailurePoint::BeforeResultPublication) {
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            "injected failure at retained verify-log descriptor-publication boundary".into(),
            hooks.fail_final_removal,
        );
    }
    let result_publication = match retention_budget.result_publication.lock() {
        Ok(publication) => publication,
        Err(_) => {
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                "retained verify-log result-publication lock is poisoned".into(),
                hooks.fail_final_removal,
            );
        }
    };
    let previous_result = result.clone();
    if let Err(error) =
        result.prepare_retained_verify_log_publication(&comparison.report, retained.clone())
    {
        drop(result_publication);
        *result = previous_result;
        return abort_retained_verify_log(
            &retained_path,
            &retained_directory,
            reservation,
            error,
            hooks.fail_final_removal,
        );
    }
    if let Err(error) =
        append_result_with_failure(results_path, result, hooks.result_publication_failure, true)
    {
        drop(result_publication);
        if !error.descriptor_visible {
            *result = previous_result;
            return abort_retained_verify_log(
                &retained_path,
                &retained_directory,
                reservation,
                error.message,
                hooks.fail_final_removal,
            );
        }
        reservation.commit();
        return Err(format!(
            "{}; retained verify log remains published and charged because its result row may be visible",
            error.message
        ));
    }
    drop(result_publication);
    reservation.commit();

    Ok(RetainedVerifyLogPublication {
        retained,
        run1_raw: run1_log,
        run2_raw: run2_log,
        run1_digest: run1_log_digest,
        run2_digest: run2_log_digest,
    })
}

/// Durably publish run 1 and the result row that retains its descriptor.
///
/// The compressed file is staged, provisionally accounted, renamed, synced,
/// and re-read before the descriptor is added to the attempt and the complete
/// result row is atomically replaced and synced. Accounting becomes permanent
/// only after that row is durable.
pub fn publish_retained_verify_log(
    pair: ComparedVerifyPair,
    retention_budget: &VerifyLogRetentionBudget,
    results_path: &Path,
    result: &mut CellResult,
) -> Result<RetainedVerifyLogPublication, String> {
    retain_verify_log_with_limit(
        pair,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        retention_budget,
        results_path,
        result,
        VerifyLogTransactionHooks::default(),
    )
}

fn cleanup_verify_log_sources_with(
    artifact_dir: &Path,
    publication: &RetainedVerifyLogPublication,
    maximum_uncompressed_bytes: u64,
    remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    cleanup_verify_log_sources_with_hook(
        artifact_dir,
        publication,
        maximum_uncompressed_bytes,
        None,
        remove,
    )
}

fn cleanup_verify_log_sources_with_hook(
    artifact_dir: &Path,
    publication: &RetainedVerifyLogPublication,
    maximum_uncompressed_bytes: u64,
    before_raw_removal: Option<&dyn Fn()>,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let retained_path = artifact_dir.join(&publication.retained.relative_path);
    let cleanup_paths = [
        check_file_path_below(
            artifact_dir,
            &retained_path,
            "retained verify log during raw cleanup",
        )?,
        check_file_path_below(
            artifact_dir,
            &publication.run1_raw,
            "verify run-1 raw log during cleanup",
        )?,
        check_file_path_below(
            artifact_dir,
            &publication.run2_raw,
            "verify run-2 raw log during cleanup",
        )?,
    ];
    reject_file_path_aliases(&cleanup_paths)?;
    verify_retained_verify_log_with_limit(
        artifact_dir,
        &publication.retained,
        &publication.retained.cell_id,
        publication.retained.attempt,
        maximum_uncompressed_bytes,
    )?;
    let raw_inputs = [
        (
            "verify run-1 log before cleanup",
            &publication.run1_raw,
            &publication.run1_digest,
        ),
        (
            "verify run-2 log before cleanup",
            &publication.run2_raw,
            &publication.run2_digest,
        ),
    ];
    let mut raw_evidence = Vec::with_capacity(raw_inputs.len());
    for (description, path, expected) in raw_inputs {
        let evidence = match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "cannot inspect {description} {}: {error}",
                    path.display()
                ));
            }
            Ok(_) => {
                let evidence = open_and_inspect_plain_file_bounded(
                    artifact_dir,
                    path,
                    maximum_uncompressed_bytes,
                    description,
                )?;
                if &evidence.digest != expected {
                    return Err(
                        "verify logs changed after retention; refusing to remove the recovery inputs"
                            .into(),
                    );
                }
                Some(evidence)
            }
        };
        raw_evidence.push((description, path, evidence));
    }
    if let Some(before_raw_removal) = before_raw_removal {
        before_raw_removal();
    }
    for (description, path, evidence) in &raw_evidence {
        match evidence {
            Some(evidence) => require_path_identity(path, evidence.identity, description)?,
            None => match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "cannot recheck absent {description} {} before cleanup: {error}",
                        path.display()
                    ));
                }
                Ok(_) => {
                    return Err(format!(
                        "{description} {} appeared after cleanup evidence was collected",
                        path.display()
                    ));
                }
            },
        }
    }

    let mut errors = Vec::new();
    let mut parents_to_sync = BTreeSet::new();
    for index in [1usize, 0] {
        let (description, path, evidence) = &raw_evidence[index];
        let Some(evidence) = evidence else {
            continue;
        };
        if let Err(error) = require_path_identity(path, evidence.identity, description) {
            errors.push(error);
            break;
        }
        match remove(path) {
            Ok(()) => match fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(parent) = path.parent() {
                        parents_to_sync.insert(parent.to_owned());
                    }
                }
                Err(error) => errors.push(format!(
                    "cannot inspect {description} {} after removal: {error}",
                    path.display()
                )),
                Ok(_) => errors.push(format!(
                    "removal of {description} {} reported success but the path still exists",
                    path.display()
                )),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(path) {
                    Err(check) if check.kind() == std::io::ErrorKind::NotFound => {}
                    Err(check) => errors.push(format!(
                        "cannot inspect {description} {} after a not-found removal: {check}",
                        path.display()
                    )),
                    Ok(_) => errors.push(format!(
                        "removal of {description} {} reported not found but the path still exists",
                        path.display()
                    )),
                }
            }
            Err(error) => errors.push(format!(
                "cannot remove {description} {}: {error}",
                path.display()
            )),
        }
    }
    for parent in parents_to_sync {
        if let Err(error) = sync_plain_directory(&parent, "verify raw-log parent after cleanup") {
            errors.push(error);
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// Idempotently remove the two raw inputs after the caller has durably
/// published [`RetainedVerifyLogPublication::retained`]. A cleanup error does
/// not erase that descriptor or the already-verified gzip, and a later call
/// resumes with whichever raw input remains.
pub fn cleanup_verify_log_sources(
    artifact_dir: &Path,
    publication: &RetainedVerifyLogPublication,
) -> Result<(), String> {
    cleanup_verify_log_sources_with(
        artifact_dir,
        publication,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        |path| fs::remove_file(path),
    )
}

/// Build one ordinary `hermit run` command for a harness-managed verify pair.
///
/// This is intentionally separate from [`build_spec`]'s still-live internal
/// `--verify` path. Callers may construct and test the replacement without
/// changing production scheduling until every selected backend can provide the
/// same authoritative artifacts and `/test` bind.
pub fn build_verify_run_spec(
    context: &RunContext,
    cell: &SelectedCell,
    dir: PathBuf,
    guest_argv: Vec<String>,
    attempt: u64,
    run: VerifyRun,
    timeout_seconds: u64,
) -> Result<VerifyRunSpec, String> {
    if attempt == 0 {
        return Err("harness-managed verify attempt must be positive".into());
    }
    if cell.id.mode != "verify" {
        return Err(format!(
            "harness-managed verify run requested for {} mode",
            cell.id.mode
        ));
    }
    let backend = cell.id.backend.as_deref().unwrap_or("native");
    match backend {
        "dbt" => {
            return Err(
                "harness-managed DBT verify cannot bind its isolated work directory at /test"
                    .into(),
            );
        }
        "sabre" => {
            return Err(
                "harness-managed SaBRe verify has no authoritative ordinary-run log sink".into(),
            );
        }
        "ptrace" | "kvm" | "liteinst" => {}
        other => {
            return Err(format!(
                "harness-managed verify is unsupported on backend {other}"
            ));
        }
    }

    let mode_recipe = &cell.test.modes["verify"];
    if mode_recipe
        .workdir
        .as_deref()
        .is_some_and(|workdir| workdir != HERMETIC_TEST_WORKDIR)
    {
        return Err(format!(
            "harness-managed verify requires the fixed guest workdir {HERMETIC_TEST_WORKDIR}; {} requests {}",
            cell.id.test,
            mode_recipe.workdir.as_deref().expect("checked above")
        ));
    }

    let attempt_label = attempt.to_string();
    let paths = verify_run_paths(&dir, &attempt_label, run)?;
    let capture_parent = paths
        .log
        .parent()
        .expect("a verify log path always has a capture directory");
    let capture_parent_relative = capture_parent.strip_prefix(&dir).map_err(|_| {
        format!(
            "verify capture directory {} escaped cell artifact directory {}",
            capture_parent.display(),
            dir.display()
        )
    })?;
    create_plain_relative_directory(&dir, capture_parent_relative, "verify capture directory")?;
    let env = execution_cell_env(context, &dir, true);
    let expected_determinism = GuestRunDeterminism {
        detlog_io_buffers: mode_recipe.compare_io_buffers != Some(false),
        virtualize_time: true,
    };
    let mut argv = vec![
        context.hermit_bin.to_string_lossy().into_owned(),
        "--log".into(),
        "info".into(),
        "--log-file".into(),
        paths.log.to_string_lossy().into_owned(),
        "run".into(),
        "--base-env=minimal".into(),
        "--backend".into(),
        backend.into(),
        "--strict".into(),
    ];
    if mode_recipe.compare_io_buffers == Some(false) {
        argv.push("--no-detlog-io-buffers".into());
    }
    if mode_recipe.rcb_time == Some(false) {
        argv.push("--no-rcb-time".into());
    }
    argv.extend([
        "--summary-json".into(),
        paths.summary.to_string_lossy().into_owned(),
        "--run-result-json".into(),
        paths.result.to_string_lossy().into_owned(),
        "--guest-stdout".into(),
        paths.stdout.to_string_lossy().into_owned(),
        "--guest-stderr".into(),
        paths.stderr.to_string_lossy().into_owned(),
        format!(
            "--mount=type=bind,source={},target={HERMETIC_TEST_WORKDIR}",
            paths.workdir.to_string_lossy()
        ),
        "--workdir".into(),
        HERMETIC_TEST_WORKDIR.into(),
    ]);
    append_guest_env_args(&mut argv, &env, true);
    argv.push("--".into());
    argv.extend(guest_argv.clone());

    let execution = CellRunSpec {
        id: cell.id.clone(),
        lane: cell.test.lane.clone(),
        category: cell.category.clone(),
        cwd: context.root.clone(),
        env,
        argv,
        guest_argv,
        timeout_seconds,
        verdict_path: None,
        verification_log_dir: None,
        sabre_path_evidence: None,
        cell_dir: dir,
        attempt: format!("{attempt_label}-{}", run.directory_name()),
        fixed_workdir_source: paths.workdir.clone(),
    };
    Ok(VerifyRunSpec {
        run,
        execution,
        paths,
        attempt,
        expected_determinism,
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::assert_minimal_guest_env;
    use super::super::tests::cell_result_that_located_nothing;
    use super::super::tests::ptrace_cell;
    use super::super::tests::run_context;
    use super::*;

    #[test]
    fn verify_pair_paths_are_distinct_and_stay_below_the_cell_directory() {
        let cell = Path::new("/result/run/cell");
        let run1 = verify_run_paths(cell, "2", VerifyRun::Run1).unwrap();
        let run2 = verify_run_paths(cell, "2", VerifyRun::Run2).unwrap();

        assert_eq!(run1.workdir, cell.join("workdir/2/run-1"));
        assert_eq!(run2.workdir, cell.join("workdir/2/run-2"));
        assert_eq!(run1.log, cell.join("captures/verify/2/run-1/detlog.log"));
        assert_eq!(run2.log, cell.join("captures/verify/2/run-2/detlog.log"));
        assert_ne!(run1, run2);
        for path in [
            &run1.workdir,
            &run1.log,
            &run1.result,
            &run1.stdout,
            &run1.stderr,
            &run1.summary,
            &run2.workdir,
            &run2.log,
            &run2.result,
            &run2.stdout,
            &run2.stderr,
            &run2.summary,
        ] {
            assert!(
                path.starts_with(cell),
                "{} escaped the cell",
                path.display()
            );
            assert!(
                !path.starts_with("/tmp"),
                "{} used host /tmp",
                path.display()
            );
        }
        assert!(verify_run_paths(cell, "../escape", VerifyRun::Run1).is_err());
    }

    #[test]
    fn harness_managed_verify_specs_are_two_ordinary_runs_with_fixed_test_binds() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        let cell_dir = directory.path().join("results/cell");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&cell_dir).unwrap();
        let context =
            run_context(&root).with_scheduled_worker_capacity(ScheduledWorkerCapacity::new(8));
        let cell = ptrace_cell("verify");
        let run1 = build_verify_run_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run1,
            15,
        )
        .unwrap();
        let run2 = build_verify_run_spec(
            &context,
            &cell,
            cell_dir.clone(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run2,
            15,
        )
        .unwrap();

        assert_eq!(run1.run, VerifyRun::Run1);
        assert_eq!(run2.run, VerifyRun::Run2);
        assert_ne!(run1.paths, run2.paths);
        assert_eq!(run1.execution.fixed_workdir_source, run1.paths.workdir);
        assert_eq!(run2.execution.fixed_workdir_source, run2.paths.workdir);
        for spec in [&run1, &run2] {
            let argv = &spec.execution.argv;
            assert!(argv.windows(2).any(|args| {
                args[0] == "--log-file" && args[1] == spec.paths.log.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--summary-json" && args[1] == spec.paths.summary.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--run-result-json" && args[1] == spec.paths.result.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--guest-stdout" && args[1] == spec.paths.stdout.to_string_lossy()
            }));
            assert!(argv.windows(2).any(|args| {
                args[0] == "--guest-stderr" && args[1] == spec.paths.stderr.to_string_lossy()
            }));
            assert!(argv.iter().any(|arg| {
                arg == &format!(
                    "--mount=type=bind,source={},target={HERMETIC_TEST_WORKDIR}",
                    spec.paths.workdir.display()
                )
            }));
            assert!(
                argv.windows(2)
                    .any(|args| { args[0] == "--workdir" && args[1] == HERMETIC_TEST_WORKDIR })
            );
            assert!(!argv.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "--verify" | "--verify-json" | "--verify-strict" | "--keep-logs"
                ) || arg.starts_with("--verify-log-dir")
            }));
            assert!(spec.paths.workdir.starts_with(&cell_dir));
            assert!(spec.paths.log.parent().unwrap().is_dir());
            File::create(&spec.paths.log)
                .expect("Hermit's pre-namespace host-opened log sink must be creatable");
            fs::remove_file(&spec.paths.log).unwrap();
            assert_minimal_guest_env(argv, &cell_dir.to_string_lossy(), "/test", "8");
        }
    }

    #[test]
    fn harness_managed_verify_executes_two_ptrace_runs_and_retains_run_1() {
        static REAL_HERMIT_RUN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = REAL_HERMIT_RUN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root must be readable");
        let hermit_bin = std::env::var_os("HERMIT_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("target/debug/hermit"));
        assert!(
            hermit_bin.is_file(),
            "real Hermit binary {} is missing; build it or set HERMIT_BIN",
            hermit_bin.display()
        );

        let directory = tempfile::tempdir_in(root.join("target")).unwrap();
        let artifact_dir = directory.path().join("cell");
        fs::create_dir_all(&artifact_dir).unwrap();
        let mut context = run_context(&root);
        context.hermit_bin = hermit_bin;
        let cell = ptrace_cell("verify");
        let guest = vec![
            "/bin/sh".into(),
            "-c".into(),
            concat!(
                "test \"$PWD\" = /test || exit 91; ",
                "test ! -e verify-pair-marker || exit 92; ",
                "printf same > verify-pair-marker; ",
                "printf '/test\\nexact-stdout'; ",
                "printf exact-stderr >&2"
            )
            .into(),
        ];
        let run1_spec = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.clone(),
            guest.clone(),
            1,
            VerifyRun::Run1,
            30,
        )
        .unwrap();
        let run2_spec = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.clone(),
            guest,
            1,
            VerifyRun::Run2,
            30,
        )
        .unwrap();

        assert_ne!(run1_spec.paths.workdir, run2_spec.paths.workdir);
        for spec in [&run1_spec, &run2_spec] {
            assert!(spec.execution.argv.iter().any(|argument| {
                argument
                    == &format!(
                        "--mount=type=bind,source={},target={HERMETIC_TEST_WORKDIR}",
                        spec.paths.workdir.display()
                    )
            }));
            let attempt = execute_spec(&spec.execution).unwrap();
            assert_eq!(attempt.outcome, "PASS", "{}", attempt.stderr);
            assert_eq!(attempt.status, Some(0), "{}", attempt.stderr);
        }
        for workdir in [&run1_spec.paths.workdir, &run2_spec.paths.workdir] {
            assert_eq!(
                fs::read(workdir.join("verify-pair-marker")).unwrap(),
                b"same"
            );
        }

        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        for (spec, observation) in [(&run1_spec, &run1), (&run2_spec, &run2)] {
            assert_eq!(
                observation.result.disposition,
                crate::canonical_verdict::GuestDisposition::Exited { code: 0 }
            );
            assert_eq!(observation.stdout, b"/test\nexact-stdout");
            assert_eq!(observation.stderr, b"exact-stderr");
            assert_eq!(fs::read(&spec.paths.stdout).unwrap(), observation.stdout);
            assert_eq!(fs::read(&spec.paths.stderr).unwrap(), observation.stderr);
            let summary: RunSummary =
                serde_json::from_slice(&fs::read(&spec.paths.summary).unwrap()).unwrap();
            assert_eq!(RuntimeStats::from(&summary), observation.runtime);
        }

        let run1_raw = fs::read(&run1_spec.paths.log).unwrap();
        let run2_raw = fs::read(&run2_spec.paths.log).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let comparison = compared.comparison();
        assert_eq!(comparison.report.verdict, Verdict::Matched);
        assert!(comparison.report.bitwise_parity);
        assert!(comparison.stdout_equal);
        assert!(comparison.stderr_equal);
        assert!(comparison.disposition_equal);
        let counts = comparison
            .report
            .compared_log_messages
            .as_ref()
            .expect("canonical comparison must record both INFO counts");
        assert!(counts.left > 0 && counts.right > 0, "{counts:?}");

        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let results_path = directory.path().join("results.jsonl");
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(compared, &retention_budget, &results_path, &mut result)
                .unwrap();
        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());
        verify_retained_verify_log(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
        )
        .unwrap();

        let retained_path = artifact_dir.join(&publication.retained.relative_path);
        let compressed = fs::read(&retained_path).unwrap();
        assert!(compressed.len() >= 10);
        assert_eq!(&compressed[4..8], &[0, 0, 0, 0], "gzip mtime must be zero");
        assert_eq!(compressed[3] & 0x08, 0, "gzip must not carry a filename");
        assert_eq!(
            publication.retained.compressed_sha256,
            hex_digest(&compressed)
        );
        assert_eq!(
            publication.retained.compressed_bytes,
            u64::try_from(compressed.len()).unwrap()
        );
        assert_eq!(
            publication.retained.uncompressed_sha256,
            hex_digest(&run1_raw)
        );
        assert_eq!(
            publication.retained.peer_uncompressed_sha256,
            hex_digest(&run2_raw)
        );
        let mut decoded = Vec::new();
        MultiGzDecoder::new(compressed.as_slice())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, run1_raw);
        let mut expected = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::new(6));
        expected.write_all(&run1_raw).unwrap();
        assert_eq!(compressed, expected.finish().unwrap());

        let published_result: CellResult =
            serde_json::from_str(fs::read_to_string(&results_path).unwrap().trim_end()).unwrap();
        assert_eq!(
            published_result.schema,
            RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA
        );
        assert_eq!(published_result.outcome, "PASS");
        assert_eq!(published_result.result, Some(ObservedResult::Pass));
        let report_json = published_result.attempts[0]
            .verification_report
            .as_deref()
            .expect("the exact pair report is published with the attempt");
        assert_eq!(
            published_result.attempts[0]
                .verification_report_sha256
                .as_deref(),
            Some(hex_digest(report_json.as_bytes()).as_str())
        );
        let report = VerificationReport::from_json_slice(report_json.as_bytes()).unwrap();
        assert_eq!(report.verdict, Verdict::Matched);
        assert_eq!(
            published_result.attempts[0].retained_verify_log,
            Some(publication.retained.clone())
        );
        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());

        cleanup_verify_log_sources(&artifact_dir, &publication).unwrap();
        assert!(!run1_spec.paths.log.exists());
        assert!(!run2_spec.paths.log.exists());
        assert!(retained_path.is_file());
        verify_retained_verify_log(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
        )
        .unwrap();
    }

    #[test]
    fn harness_managed_verify_refuses_backends_without_both_required_channels() {
        let context = run_context(Path::new("/repo"));
        for (backend, expected) in [
            (
                "dbt",
                "harness-managed DBT verify cannot bind its isolated work directory at /test",
            ),
            (
                "sabre",
                "harness-managed SaBRe verify has no authoritative ordinary-run log sink",
            ),
        ] {
            let mut cell = ptrace_cell("verify");
            cell.id.backend = Some(backend.into());
            let error = build_verify_run_spec(
                &context,
                &cell,
                PathBuf::from("/results/cell"),
                vec!["/bin/true".into()],
                1,
                VerifyRun::Run1,
                15,
            )
            .unwrap_err();
            assert_eq!(error, expected);
        }
    }

    fn write_verify_run_fixture(
        spec: &VerifyRunSpec,
        disposition: crate::canonical_verdict::GuestDisposition,
        stdout: &[u8],
        stderr: &[u8],
        log_body: &str,
    ) {
        let paths = &spec.paths;
        fs::create_dir_all(paths.log.parent().unwrap()).unwrap();
        fs::create_dir_all(&paths.workdir).unwrap();
        fs::write(&paths.stdout, stdout).unwrap();
        fs::write(&paths.stderr, stderr).unwrap();
        fs::write(&paths.log, log_body).unwrap();
        let result = GuestRunResult {
            schema: GuestRunResult::SCHEMA,
            disposition,
            determinism: spec.expected_determinism,
            stdout: crate::canonical_verdict::CapturedGuestStream {
                bytes: u64::try_from(stdout.len()).unwrap(),
                sha256: hex_digest(stdout),
            },
            stderr: crate::canonical_verdict::CapturedGuestStream {
                bytes: u64::try_from(stderr.len()).unwrap(),
                sha256: hex_digest(stderr),
            },
        };
        fs::write(&paths.result, serde_json::to_vec(&result).unwrap()).unwrap();
        fs::write(
            &paths.summary,
            serde_json::to_vec(&RunSummary::default()).unwrap(),
        )
        .unwrap();
    }

    fn verify_pair_fixture(
        artifact_dir: &Path,
        run1_log: &str,
        run2_log: &str,
        detlog_io_buffers: bool,
    ) -> (VerifyRunSpec, VerifyRunSpec) {
        fs::create_dir_all(artifact_dir).unwrap();
        let root = artifact_dir.parent().unwrap();
        let context = run_context(root);
        let mut cell = ptrace_cell("verify");
        if !detlog_io_buffers {
            let mode = cell.test.modes.get_mut("verify").unwrap();
            mode.compare_io_buffers = Some(false);
            mode.compare_io_buffers_disabled_reason = Some("fixture opt-out".into());
        }
        let run1 = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.to_owned(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run1,
            15,
        )
        .unwrap();
        let run2 = build_verify_run_spec(
            &context,
            &cell,
            artifact_dir.to_owned(),
            vec!["/bin/true".into()],
            1,
            VerifyRun::Run2,
            15,
        )
        .unwrap();
        let disposition = crate::canonical_verdict::GuestDisposition::Exited { code: 0 };
        write_verify_run_fixture(&run1, disposition, b"out", b"err", run1_log);
        write_verify_run_fixture(&run2, disposition, b"out", b"err", run2_log);
        (run1, run2)
    }

    fn structured_info_record(body: &str) -> String {
        format!(
            "Apr 09 06:08:01.100  INFO detcore: {body}{}\n",
            detcore::detlog::record_suffix(detcore::detlog::DetLogEvent::Other)
        )
    }

    fn verify_log_retention_budget(
        retention_root: &Path,
        maximum_total_compressed_bytes: u64,
    ) -> VerifyLogRetentionBudget {
        let results_path = retention_root.join("results.jsonl");
        prepare_result_path(&results_path).unwrap();
        VerifyLogRetentionBudget::open(
            retention_root,
            results_path,
            VerifyLogRetentionPolicy::new(maximum_total_compressed_bytes),
        )
        .unwrap()
    }

    fn verify_cell_result(spec: &VerifyRunSpec) -> CellResult {
        let mut result = cell_result_that_located_nothing();
        result.test = spec.execution.id.test.clone();
        result.mode = spec.execution.id.mode.clone();
        result.backend = spec.execution.id.backend.clone();
        result.attempt = spec.attempt;
        result.artifact_dir = spec.execution.cell_dir.to_string_lossy().into_owned();
        result.argv = spec.execution.argv.clone();
        result.guest_argv = spec.execution.guest_argv.clone();
        result.env = spec.execution.env.clone();
        result.cwd = spec.execution.cwd.to_string_lossy().into_owned();
        result.shell_command = shell_command(
            &spec.execution.cwd.to_string_lossy(),
            &spec.execution.env,
            &spec.execution.argv,
        );
        let attempt = &mut result.attempts[0];
        attempt.index = "1".into();
        attempt.argv = result.argv.clone();
        attempt.guest_argv = result.guest_argv.clone();
        attempt.env = result.env.clone();
        attempt.cwd = result.cwd.clone();
        attempt.shell_command = result.shell_command.clone();
        attempt.sabre_path_evidence = None;
        attempt.sabre_path_evidence_sha256 = None;
        attempt.retained_verify_log = None;
        result
    }

    fn deterministic_gzip_size(bytes: &[u8]) -> u64 {
        let mut encoder = GzBuilder::new()
            .mtime(0)
            .write(Vec::new(), Compression::new(6));
        encoder.write_all(bytes).unwrap();
        u64::try_from(encoder.finish().unwrap().len()).unwrap()
    }

    #[test]
    fn harness_pair_loader_binds_sidecars_to_exact_bytes_and_shared_comparison() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, run2_spec) = verify_pair_fixture(&root, &log, &log, true);

        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        assert_eq!(compared.comparison.report.verdict, Verdict::Matched);
        assert!(compared.comparison.report.bitwise_parity);
        assert_eq!(
            compared.comparison.report.compared_log_messages,
            Some(crate::canonical_verdict::ComparedLogMessages { left: 1, right: 1 })
        );

        fs::write(&run2_spec.paths.stdout, b"forged after sidecar").unwrap();
        let error = load_verify_run(&run2_spec)
            .expect_err("captured bytes changed after the sidecar must be refused");
        assert!(
            error.contains("stdout does not match its typed result"),
            "{error}"
        );
    }

    #[test]
    fn harness_pair_policy_comes_from_both_typed_run_results() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, run2_spec) = verify_pair_fixture(&root, &log, &log, false);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        assert_eq!(compared.comparison.report.verdict, Verdict::Matched);
        assert!(!compared.comparison.report.bitwise_parity);
        assert_eq!(
            compared
                .comparison
                .report
                .comparison
                .as_ref()
                .unwrap()
                .compare_io_buffers,
            Some(false)
        );

        let mut forged_spec = run1_spec;
        forged_spec
            .execution
            .argv
            .retain(|argument| argument != "--no-detlog-io-buffers");
        let error = load_verify_run(&forged_spec)
            .expect_err("a command that omits its recorded opt-out must be refused");
        assert!(
            error.contains("does not match its detlog_io_buffers"),
            "{error}"
        );
    }

    #[test]
    fn harness_pair_requires_a_complete_run_summary() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, _) = verify_pair_fixture(&root, &log, &log, true);
        fs::remove_file(&run1_spec.paths.summary).unwrap();
        let missing = load_verify_run(&run1_spec).unwrap_err();
        assert!(missing.contains("summary.json"), "{missing}");

        fs::write(&run1_spec.paths.summary, b"not json").unwrap();
        let malformed = load_verify_run(&run1_spec).unwrap_err();
        assert!(malformed.contains("cannot parse"), "{malformed}");
    }

    #[test]
    fn harness_pair_loader_refuses_symlink_and_non_regular_sidecars() {
        for selected in ["result", "stdout", "stderr", "summary"] {
            let directory = tempfile::tempdir().unwrap();
            let artifact_dir = directory.path().join(selected);
            let log = structured_info_record("DETLOG stable");
            let (run1_spec, _) = verify_pair_fixture(&artifact_dir, &log, &log, true);
            let selected_path = match selected {
                "result" => &run1_spec.paths.result,
                "stdout" => &run1_spec.paths.stdout,
                "stderr" => &run1_spec.paths.stderr,
                "summary" => &run1_spec.paths.summary,
                _ => unreachable!(),
            };
            let target = artifact_dir.join("preserved-target");
            fs::write(&target, b"preserve me").unwrap();
            fs::remove_file(selected_path).unwrap();
            std::os::unix::fs::symlink(&target, selected_path).unwrap();

            let error = load_verify_run(&run1_spec).unwrap_err();
            assert!(error.contains("cannot open verify ordinary-run"), "{error}");
            assert_eq!(fs::read(&target).unwrap(), b"preserve me");
            assert!(
                fs::symlink_metadata(selected_path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }

        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("non-regular");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, _) = verify_pair_fixture(&artifact_dir, &log, &log, true);
        fs::remove_file(&run1_spec.paths.summary).unwrap();
        fs::create_dir(&run1_spec.paths.summary).unwrap();
        let error = load_verify_run(&run1_spec).unwrap_err();
        assert!(error.contains("is not a regular file"), "{error}");
    }

    #[test]
    fn harness_pair_loader_refuses_each_oversize_sidecar_before_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let log = structured_info_record("DETLOG stable");
        let (run1_spec, _) = verify_pair_fixture(&artifact_dir, &log, &log, true);

        for (name, limits) in [
            (
                "result",
                VerifyRunReadLimits {
                    result: 1,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
            (
                "stdout",
                VerifyRunReadLimits {
                    stdout: 2,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
            (
                "stderr",
                VerifyRunReadLimits {
                    stderr: 2,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
            (
                "summary",
                VerifyRunReadLimits {
                    summary: 1,
                    ..VERIFY_RUN_READ_LIMITS
                },
            ),
        ] {
            let error = load_verify_run_with_limits(&run1_spec, limits).unwrap_err();
            assert!(
                error.contains("exceeds the") && error.contains(name),
                "{error}"
            );
        }
    }

    #[test]
    fn retained_verify_log_is_deterministic_bound_and_verified_before_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG retained");
        let mut compressed = Vec::new();

        for name in ["first", "second"] {
            let artifact_dir = directory.path().join(name);
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let run1 = load_verify_run(&run1_spec).unwrap();
            let run2 = load_verify_run(&run2_spec).unwrap();
            let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
            let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
            let results_path = retention_budget.results_path.clone();
            let mut result = verify_cell_result(&run1_spec);
            let publication = retain_verify_log_with_limit(
                compared,
                4096,
                &retention_budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks::default(),
            )
            .unwrap();
            let retained = &publication.retained;

            assert_eq!(retained.role, RetainedVerifyLogRole::Run1);
            assert_eq!(retained.cell_id, run1_spec.execution.id);
            assert_eq!(retained.attempt, 1);
            assert_eq!(retained.compared_info_messages, 1);
            assert_eq!(retained.uncompressed_bytes, body.len() as u64);
            assert_eq!(retained.peer_uncompressed_bytes, body.len() as u64);
            assert_eq!(
                retained.uncompressed_sha256,
                retained.peer_uncompressed_sha256
            );
            assert!(run1_spec.paths.log.exists());
            assert!(run2_spec.paths.log.exists());
            verify_retained_verify_log_with_limit(
                &artifact_dir,
                retained,
                &run1_spec.execution.id,
                1,
                4096,
            )
            .unwrap();

            let bytes = fs::read(artifact_dir.join(&retained.relative_path)).unwrap();
            assert_eq!(&bytes[4..8], &[0, 0, 0, 0], "gzip mtime must be zero");
            assert_eq!(bytes[3] & 0x08, 0, "gzip must not carry a filename");
            compressed.push(bytes);
            cleanup_verify_log_sources_with(&artifact_dir, &publication, 4096, |path| {
                fs::remove_file(path)
            })
            .unwrap();
            assert!(!run1_spec.paths.log.exists());
            assert!(!run2_spec.paths.log.exists());
        }
        assert_eq!(compressed[0], compressed[1]);
    }

    #[test]
    fn retained_verify_log_single_open_read_and_copy_return_the_descriptor_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG retained reader");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &budget.results_path, &mut result).unwrap();
        let retained = &publication.retained;

        assert_eq!(
            read_verified_retained_verify_log(&artifact_dir, retained, &run1_spec.execution.id, 1,)
                .unwrap(),
            body.as_bytes()
        );
        let expected_compressed = fs::read(artifact_dir.join(&retained.relative_path)).unwrap();
        let mut copied = Vec::new();
        let verified = copy_verified_retained_verify_log(
            &artifact_dir,
            retained,
            &run1_spec.execution.id,
            1,
            &mut copied,
            retained.compressed_bytes,
        )
        .unwrap();
        assert_eq!(copied, expected_compressed);
        assert_eq!(verified.compressed_sha256, retained.compressed_sha256);
        assert_eq!(verified.compressed_bytes, retained.compressed_bytes);
        assert_eq!(verified.uncompressed_sha256, retained.uncompressed_sha256);
        assert_eq!(verified.uncompressed_bytes, retained.uncompressed_bytes);
    }

    #[test]
    fn retained_verify_log_single_open_copy_refuses_cap_hardlink_and_path_swap() {
        struct SwapWriter {
            bytes: Vec<u8>,
            source: PathBuf,
            moved: PathBuf,
            swapped: bool,
        }

        impl Write for SwapWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if !self.swapped {
                    fs::rename(&self.source, &self.moved)?;
                    fs::write(&self.source, b"replacement")?;
                    self.swapped = true;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG retained copy refusal");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &budget.results_path, &mut result).unwrap();
        let retained = &publication.retained;
        let source = artifact_dir.join(&retained.relative_path);

        let error = copy_verified_retained_verify_log(
            &artifact_dir,
            retained,
            &run1_spec.execution.id,
            1,
            &mut Vec::new(),
            retained.compressed_bytes - 1,
        )
        .unwrap_err();
        assert!(error.contains("exceeds the"), "{error}");

        let link = artifact_dir.join("retained-copy-hardlink.log.gz");
        fs::hard_link(&source, &link).unwrap();
        let error =
            read_verified_retained_verify_log(&artifact_dir, retained, &run1_spec.execution.id, 1)
                .unwrap_err();
        assert!(error.contains("hard links"), "{error}");
        fs::remove_file(link).unwrap();

        let moved = artifact_dir.join("retained-copy-original.log.gz");
        let mut writer = SwapWriter {
            bytes: Vec::new(),
            source: source.clone(),
            moved,
            swapped: false,
        };
        let error = copy_verified_retained_verify_log(
            &artifact_dir,
            retained,
            &run1_spec.execution.id,
            1,
            &mut writer,
            retained.compressed_bytes,
        )
        .unwrap_err();
        assert!(error.contains("changed identity"), "{error}");
    }

    #[test]
    fn aggregate_retention_budget_accepts_exact_bound_and_refuses_one_byte_over() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG aggregate retention bound");
        let compressed_bytes = deterministic_gzip_size(body.as_bytes());

        let exact_root = directory.path().join("exact-root");
        fs::create_dir(&exact_root).unwrap();
        let exact_artifact_dir = exact_root.join("exact");
        let (exact_run1, exact_run2) = verify_pair_fixture(&exact_artifact_dir, &body, &body, true);
        let exact_pair = compare_verify_runs(
            &exact_run1,
            load_verify_run(&exact_run1).unwrap(),
            &exact_run2,
            load_verify_run(&exact_run2).unwrap(),
        )
        .unwrap();
        let exact_budget = verify_log_retention_budget(&exact_root, compressed_bytes);
        let mut exact_result = verify_cell_result(&exact_run1);
        let exact_results_path = exact_budget.results_path.clone();
        let exact_publication = publish_retained_verify_log(
            exact_pair,
            &exact_budget,
            &exact_results_path,
            &mut exact_result,
        )
        .unwrap();
        assert_eq!(
            exact_publication.retained.compressed_bytes,
            compressed_bytes
        );
        assert!(
            exact_artifact_dir
                .join(&exact_publication.retained.relative_path)
                .is_file()
        );
        assert_eq!(
            exact_budget.accounted_compressed_bytes().unwrap(),
            compressed_bytes
        );

        let maximum = compressed_bytes
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_sub(1))
            .unwrap();
        let shared_root = directory.path().join("shared-root");
        fs::create_dir(&shared_root).unwrap();
        let first_artifact_dir = shared_root.join("first");
        let (first_run1, first_run2) = verify_pair_fixture(&first_artifact_dir, &body, &body, true);
        let first_pair = compare_verify_runs(
            &first_run1,
            load_verify_run(&first_run1).unwrap(),
            &first_run2,
            load_verify_run(&first_run2).unwrap(),
        )
        .unwrap();
        let shared_budget = verify_log_retention_budget(&shared_root, maximum);
        let mut first_result = verify_cell_result(&first_run1);
        let first_results_path = shared_budget.results_path.clone();
        let first_publication = publish_retained_verify_log(
            first_pair,
            &shared_budget,
            &first_results_path,
            &mut first_result,
        )
        .unwrap();
        let first_retained = first_artifact_dir.join(&first_publication.retained.relative_path);
        let first_results_bytes = fs::read(&first_results_path).unwrap();

        let refused_artifact_dir = shared_root.join("refused");
        let (refused_run1, refused_run2) =
            verify_pair_fixture(&refused_artifact_dir, &body, &body, true);
        let refused_pair = compare_verify_runs(
            &refused_run1,
            load_verify_run(&refused_run1).unwrap(),
            &refused_run2,
            load_verify_run(&refused_run2).unwrap(),
        )
        .unwrap();
        let required = compressed_bytes.checked_mul(2).unwrap();
        assert_eq!(required, maximum.checked_add(1).unwrap());
        let mut refused_result = verify_cell_result(&refused_run1);
        let refused_results_path = shared_budget.results_path.clone();
        let error = publish_retained_verify_log(
            refused_pair,
            &shared_budget,
            &refused_results_path,
            &mut refused_result,
        )
        .unwrap_err();
        assert!(error.contains("exceeding the"), "{error}");
        assert!(error.contains(&format!("require {required} compressed bytes")));
        assert!(error.contains(&format!("{}-byte aggregate limit", maximum)));
        assert_eq!(
            shared_budget.accounted_compressed_bytes().unwrap(),
            compressed_bytes
        );
        assert!(
            first_retained.is_file(),
            "an earlier log must not be evicted"
        );
        assert!(refused_run1.paths.log.is_file());
        assert!(refused_run2.paths.log.is_file());
        let refused_attempt_dir = refused_artifact_dir.join("retained/verify/1");
        assert!(
            !refused_attempt_dir.join("run-1.log.gz").exists(),
            "an over-limit pair must not publish a retained log"
        );
        assert!(
            !refused_artifact_dir.join("retained").exists(),
            "an over-limit staging file and its empty directory chain must be removed before its charge is released"
        );
        assert!(refused_result.attempts[0].retained_verify_log.is_none());
        assert_eq!(
            fs::read(&refused_results_path).unwrap(),
            first_results_bytes
        );
    }

    #[test]
    fn retained_verify_log_transaction_rolls_back_before_descriptor_publication() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG transaction rollback");
        for (name, failure) in [
            (
                "before-staging-file-creation",
                VerifyLogTransactionFailurePoint::BeforeStagingFileCreation,
            ),
            (
                "after-rename",
                VerifyLogTransactionFailurePoint::AfterFinalRename,
            ),
            (
                "after-directory-sync",
                VerifyLogTransactionFailurePoint::AfterDirectorySync,
            ),
            (
                "after-final-verification",
                VerifyLogTransactionFailurePoint::AfterFinalVerification,
            ),
            (
                "descriptor-publication-boundary",
                VerifyLogTransactionFailurePoint::BeforeResultPublication,
            ),
        ] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = retention_root.join("results.jsonl");
            let mut result = verify_cell_result(&run1_spec);
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    failure: Some(failure),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(error.contains("injected failure"), "{error}");
            assert!(run1_spec.paths.log.is_file());
            assert!(run2_spec.paths.log.is_file());
            assert!(result.attempts[0].retained_verify_log.is_none());
            assert_eq!(fs::read(&results_path).unwrap(), b"");
            let retained_attempt_dir = artifact_dir.join("retained/verify/1");
            assert!(!retained_attempt_dir.join("run-1.log.gz").exists());
            assert!(!artifact_dir.join("retained").exists());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }

        for (name, publication_failure) in [
            (
                "result-temporary-write",
                ResultPublicationFailurePoint::TemporaryWrite,
            ),
            (
                "result-temporary-file-sync",
                ResultPublicationFailurePoint::TemporaryFileSync,
            ),
            (
                "result-before-rename",
                ResultPublicationFailurePoint::BeforeRename,
            ),
            (
                "result-parent-directory-sync",
                ResultPublicationFailurePoint::ParentDirectorySync,
            ),
        ] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = retention_root.join("results.jsonl");
            let previous = cell_result_that_located_nothing();
            append_result(&results_path, &previous).unwrap();
            let previous_bytes = fs::read(&results_path).unwrap();
            let mut result = verify_cell_result(&run1_spec);
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    result_publication_failure: Some(publication_failure),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(error.contains("injected failure"), "{error}");
            assert_eq!(fs::read(&results_path).unwrap(), previous_bytes);
            assert!(result.attempts[0].retained_verify_log.is_none());
            let retained_attempt_dir = artifact_dir.join("retained/verify/1");
            assert!(!retained_attempt_dir.join("run-1.log.gz").exists());
            assert!(!artifact_dir.join("retained").exists());
            assert!(!fs::read_dir(&retention_root).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".results.jsonl.")
            }));
            assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }
    }

    #[test]
    fn retained_verify_log_refuses_destination_aliases_before_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG alias preflight");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);

        let retained_path = artifact_dir.join("retained/verify/1/run-1.log.gz");
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            pair.clone(),
            4096,
            &budget,
            &retained_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("aliases"), "{error}");
        assert!(!artifact_dir.join("retained").exists());

        let symlink_result = directory.path().join("symlink-results.jsonl");
        std::os::unix::fs::symlink(&run1_spec.paths.log, &symlink_result).unwrap();
        let error = retain_verify_log_with_limit(
            pair.clone(),
            4096,
            &budget,
            &symlink_result,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("is a symlink"), "{error}");
        assert_eq!(fs::read(&run1_spec.paths.log).unwrap(), body.as_bytes());

        let hardlink_result = directory.path().join("hardlink-results.jsonl");
        fs::hard_link(&run1_spec.paths.log, &hardlink_result).unwrap();
        let error = retain_verify_log_with_limit(
            pair.clone(),
            4096,
            &budget,
            &hardlink_result,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("aliases"), "{error}");

        fs::remove_file(&run2_spec.paths.summary).unwrap();
        fs::hard_link(&run1_spec.paths.log, &run2_spec.paths.summary).unwrap();
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("aliases"), "{error}");
        assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
    }

    #[test]
    fn retained_verify_log_keeps_the_final_fd_across_a_path_swap() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG final path swap");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let swap = |path: &Path| {
            let original = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
            let replacement = path.with_extension("replacement");
            fs::write(&replacement, b"replacement is intentionally not gzip").unwrap();
            let replacement_identity =
                FileIdentity::from_metadata(&fs::metadata(&replacement).unwrap());
            assert_ne!(original, replacement_identity);
            fs::rename(replacement, path).unwrap();
        };
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks {
                before_descriptor_publication: Some(&swap),
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap_err();
        assert!(
            error.contains("changed identity before publication"),
            "{error}"
        );
        assert!(result.attempts[0].retained_verify_log.is_none());
        assert_eq!(fs::read(&budget.results_path).unwrap(), b"");
        assert!(!artifact_dir.join("retained/verify/1/run-1.log.gz").exists());
        assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
        let results_path = budget.results_path.clone();
        drop(budget);
        let restarted = VerifyLogRetentionBudget::open(
            directory.path(),
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap();
        assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
    }

    #[test]
    fn retained_verify_log_detects_in_place_mutation_before_descriptor_publication() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG final in-place mutation");
        for (name, same_size) in [("same-size", true), ("different-size", false)] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = budget.results_path.clone();
            let mut result = verify_cell_result(&run1_spec);
            let mutate = |path: &Path| {
                let before = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                if same_size {
                    let mut bytes = fs::read(path).unwrap();
                    let last = bytes.last_mut().expect("gzip output is not empty");
                    *last ^= 1;
                    fs::write(path, bytes).unwrap();
                } else {
                    OpenOptions::new()
                        .append(true)
                        .open(path)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap();
                }
                let after = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                assert_eq!(before, after, "the mutation must preserve the inode");
            };
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    before_descriptor_publication: Some(&mutate),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(
                error.contains("descriptor publication")
                    || error.contains("gzip")
                    || error.contains("retained compressed verify log"),
                "{error}"
            );
            assert!(result.attempts[0].retained_verify_log.is_none());
            assert_eq!(fs::read(&results_path).unwrap(), b"");
            assert!(!artifact_dir.join("retained").exists());
            assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }
    }

    #[test]
    fn retained_and_result_paths_refuse_injected_ancestor_sync_failures() {
        let directory = tempfile::tempdir().unwrap();
        let result_path = directory.path().join("new/parents/results.jsonl");
        let error =
            prepare_result_path_from_root_with_failure(directory.path(), &result_path, Some(1))
                .unwrap_err();
        assert!(error.contains("injected failure syncing result directory ancestor"));
        assert!(!result_path.exists());

        let retention_root = directory.path().join("retention");
        fs::create_dir(&retention_root).unwrap();
        let artifact_dir = retention_root.join("cell");
        let body = structured_info_record("DETLOG ancestor sync failure");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(&retention_root, u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks {
                directory_sync_failure_at: Some(1),
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap_err();
        assert!(
            error.contains("injected failure syncing retained verify-log directory ancestor"),
            "{error}"
        );
        assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
        assert!(result.attempts[0].retained_verify_log.is_none());
        assert_eq!(fs::read(&budget.results_path).unwrap(), b"");
        assert!(!artifact_dir.join("retained/verify/1/run-1.log.gz").exists());
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
        let results_path = budget.results_path.clone();
        drop(budget);
        let restarted = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap();
        assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
    }

    #[test]
    fn retained_verify_log_write_faults_preserve_inputs_and_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG staging write faults");
        for (name, fault) in [
            ("zero", StagingWriteFault::ZeroFirst),
            (
                "mid-stream-error",
                StagingWriteFault::ErrorAfterFirstSuccessfulWrite,
            ),
        ] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let observed_charge = std::sync::atomic::AtomicU64::new(0);
            let on_write = || {
                observed_charge.store(
                    budget.accounted_compressed_bytes().unwrap(),
                    std::sync::atomic::Ordering::SeqCst,
                );
            };
            let mut result = verify_cell_result(&run1_spec);
            let error = retain_verify_log_with_limit(
                pair,
                4096,
                &budget,
                &budget.results_path,
                &mut result,
                VerifyLogTransactionHooks {
                    on_first_staging_write: Some(&on_write),
                    staging_write_fault: Some(fault),
                    ..VerifyLogTransactionHooks::default()
                },
            )
            .unwrap_err();
            assert!(observed_charge.load(std::sync::atomic::Ordering::SeqCst) > 0);
            assert!(error.contains("write") || error.contains("gzip"), "{error}");
            assert!(run1_spec.paths.log.is_file() && run2_spec.paths.log.is_file());
            assert!(result.attempts[0].retained_verify_log.is_none());
            assert_eq!(fs::read(&budget.results_path).unwrap(), b"");
            assert!(!artifact_dir.join("retained").exists());
            assert_eq!(budget.accounted_compressed_bytes().unwrap(), 0);
            let results_path = budget.results_path.clone();
            drop(budget);
            let restarted = VerifyLogRetentionBudget::open(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
            )
            .unwrap();
            assert_eq!(restarted.accounted_compressed_bytes().unwrap(), 0);
        }

        let retention_root = directory.path().join("short");
        fs::create_dir(&retention_root).unwrap();
        let artifact_dir = retention_root.join("cell");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(&retention_root, u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &budget.results_path,
            &mut result,
            VerifyLogTransactionHooks {
                staging_write_fault: Some(StagingWriteFault::ShortFirst),
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap();
        assert_eq!(
            budget.accounted_compressed_bytes().unwrap(),
            publication.retained.compressed_bytes
        );
        verify_retained_verify_log(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
        )
        .unwrap();
    }

    #[test]
    fn failed_final_removal_keeps_the_unreferenced_file_charged() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG retained cleanup failure");
        let compressed_bytes = deterministic_gzip_size(body.as_bytes());
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), compressed_bytes);
        let results_path = directory.path().join("results.jsonl");
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            pair,
            4096,
            &budget,
            &results_path,
            &mut result,
            VerifyLogTransactionHooks {
                failure: Some(VerifyLogTransactionFailurePoint::AfterFinalRename),
                fail_final_removal: true,
                ..VerifyLogTransactionHooks::default()
            },
        )
        .unwrap_err();
        assert!(error.contains("remain charged"), "{error}");
        assert_eq!(
            budget.accounted_compressed_bytes().unwrap(),
            compressed_bytes
        );
        assert!(
            artifact_dir
                .join("retained/verify/1/run-1.log.gz")
                .is_file()
        );
        assert!(run1_spec.paths.log.is_file());
        assert!(run2_spec.paths.log.is_file());
        assert!(result.attempts[0].retained_verify_log.is_none());
        assert_eq!(fs::read(&results_path).unwrap(), b"");
        drop(budget);
        let error = VerifyLogRetentionBudget::open(
            directory.path(),
            &results_path,
            VerifyLogRetentionPolicy::new(compressed_bytes),
        )
        .unwrap_err();
        assert!(error.contains("no result-row descriptor"), "{error}");
    }

    #[test]
    fn restart_accounting_reconciles_descriptors_and_refuses_impossible_layouts() {
        let directory = tempfile::tempdir().unwrap();
        let retention_root = directory.path().join("run");
        fs::create_dir(&retention_root).unwrap();
        let artifact_dir = retention_root.join("cell-a");
        let body = structured_info_record("DETLOG restart accounting");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(&retention_root, u64::MAX);
        let results_path = budget.results_path.clone();
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &results_path, &mut result).unwrap();
        let expected = publication.retained.compressed_bytes;
        let final_path = artifact_dir.join(&publication.retained.relative_path);
        let final_bytes = fs::read(&final_path).unwrap();
        drop(budget);

        let reopened = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(expected),
        )
        .unwrap();
        assert_eq!(reopened.accounted_compressed_bytes().unwrap(), expected);
        drop(reopened);
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(expected - 1),
        )
        .unwrap_err();
        assert!(error.contains("exceeding the"), "{error}");
        assert!(final_path.is_file());

        let second_artifact = retention_root.join("cell-b");
        let (second_run1, second_run2) = verify_pair_fixture(&second_artifact, &body, &body, true);
        let second_pair = compare_verify_runs(
            &second_run1,
            load_verify_run(&second_run1).unwrap(),
            &second_run2,
            load_verify_run(&second_run2).unwrap(),
        )
        .unwrap();
        let second_budget = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap();
        let mut second_result = verify_cell_result(&second_run1);
        let second_publication = publish_retained_verify_log(
            second_pair,
            &second_budget,
            &results_path,
            &mut second_result,
        )
        .unwrap();
        let second_final = second_artifact.join(&second_publication.retained.relative_path);
        let second_final_bytes = fs::read(&second_final).unwrap();
        drop(second_budget);
        fs::remove_file(&second_final).unwrap();
        fs::hard_link(&final_path, &second_final).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("finals reuse one inode"), "{error}");
        fs::remove_file(&second_final).unwrap();
        fs::write(&second_final, &second_final_bytes).unwrap();

        let descriptor_without_final = fs::read(&results_path).unwrap();
        fs::remove_file(&final_path).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("contains no canonical final"), "{error}");
        fs::write(&final_path, &final_bytes).unwrap();

        let mut duplicate_descriptor = descriptor_without_final.clone();
        duplicate_descriptor.extend_from_slice(&descriptor_without_final);
        fs::write(&results_path, &duplicate_descriptor).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(
            error.contains("duplicate retained verify-log descriptors"),
            "{error}"
        );
        fs::write(&results_path, &descriptor_without_final).unwrap();

        let mut wrong_path_result = result.clone();
        wrong_path_result.attempts[0]
            .retained_verify_log
            .as_mut()
            .unwrap()
            .relative_path = "retained/verify/1/other.log.gz".into();
        let mut wrong_path_bytes = serde_json::to_vec(&wrong_path_result).unwrap();
        wrong_path_bytes.push(b'\n');
        fs::write(&results_path, wrong_path_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("must be exactly"), "{error}");
        fs::write(&results_path, &descriptor_without_final).unwrap();

        let staging_path = final_path.parent().unwrap().join(format!(
            "{VERIFY_LOG_STAGING_PREFIX}crash{VERIFY_LOG_STAGING_SUFFIX}"
        ));
        fs::write(&staging_path, &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(
            error.contains("a final and staging file coexist"),
            "{error}"
        );
        assert!(staging_path.is_file());
        fs::remove_file(&staging_path).unwrap();

        fs::write(&final_path, b"not gzip").unwrap();
        let error = VerifyLogRetentionBudget::open(
            &retention_root,
            &results_path,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("deterministic gzip header"), "{error}");

        let bad_root = directory.path().join("bad-run");
        fs::create_dir(&bad_root).unwrap();
        let bad_results = bad_root.join("results.jsonl");
        prepare_result_path(&bad_results).unwrap();
        fs::write(bad_root.join("unexpected"), b"bytes").unwrap();
        let error = VerifyLogRetentionBudget::open(
            &bad_root,
            &bad_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("unexpected root-level file"), "{error}");

        let root_level_retained = directory.path().join("root-level-retained-run");
        let root_level_results = root_level_retained.join("results.jsonl");
        prepare_result_path(&root_level_results).unwrap();
        let root_level_attempt = root_level_retained.join("retained/verify/1");
        fs::create_dir_all(&root_level_attempt).unwrap();
        fs::write(root_level_attempt.join("run-1.log.gz"), &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &root_level_retained,
            &root_level_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("root-level retained"), "{error}");

        let unreferenced_root = directory.path().join("unreferenced-retained-run");
        let unreferenced_results = unreferenced_root.join("results.jsonl");
        prepare_result_path(&unreferenced_results).unwrap();
        let unreferenced_retained = unreferenced_root.join("cell/retained");
        fs::create_dir_all(&unreferenced_retained).unwrap();
        fs::write(
            unreferenced_retained.join("unreferenced.log.gz"),
            &final_bytes,
        )
        .unwrap();
        let error = VerifyLogRetentionBudget::open(
            &unreferenced_root,
            &unreferenced_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("unreferenced bytes"), "{error}");

        let noncanonical_root = directory.path().join("noncanonical-run");
        let noncanonical_directory = noncanonical_root.join("cell/retained/verify/01");
        fs::create_dir_all(&noncanonical_directory).unwrap();
        let noncanonical_results = noncanonical_root.join("results.jsonl");
        prepare_result_path(&noncanonical_results).unwrap();
        fs::write(noncanonical_directory.join("run-1.log.gz"), &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &noncanonical_root,
            &noncanonical_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("is not canonical"), "{error}");

        let staging_root = directory.path().join("staging-run");
        let staging_directory = staging_root.join("cell/retained/verify/1");
        fs::create_dir_all(&staging_directory).unwrap();
        let staging_results = staging_root.join("results.jsonl");
        prepare_result_path(&staging_results).unwrap();
        let first_staging = staging_directory.join(format!(
            "{VERIFY_LOG_STAGING_PREFIX}one{VERIFY_LOG_STAGING_SUFFIX}"
        ));
        fs::write(&first_staging, &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &staging_root,
            &staging_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("unreferenced staging file"), "{error}");
        let second_staging = staging_directory.join(format!(
            "{VERIFY_LOG_STAGING_PREFIX}two{VERIFY_LOG_STAGING_SUFFIX}"
        ));
        fs::write(&second_staging, &final_bytes).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &staging_root,
            &staging_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("multiple staging files coexist"), "{error}");

        let symlink_root = directory.path().join("symlink-run");
        let symlink_artifact = symlink_root.join("cell");
        let symlink_target = directory.path().join("retained-target");
        fs::create_dir_all(&symlink_artifact).unwrap();
        fs::create_dir_all(symlink_target.join("verify")).unwrap();
        let symlink_results = symlink_root.join("results.jsonl");
        prepare_result_path(&symlink_results).unwrap();
        std::os::unix::fs::symlink(&symlink_target, symlink_artifact.join("retained")).unwrap();
        let error = VerifyLogRetentionBudget::open(
            &symlink_root,
            &symlink_results,
            VerifyLogRetentionPolicy::new(u64::MAX),
        )
        .unwrap_err();
        assert!(error.contains("non-symlink directory"), "{error}");
    }

    #[test]
    fn restart_scan_detects_in_place_final_mutation_during_reconciliation() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG restart in-place mutation");
        for (name, same_size) in [("same-size", true), ("different-size", false)] {
            let retention_root = directory.path().join(name);
            fs::create_dir(&retention_root).unwrap();
            let artifact_dir = retention_root.join("cell");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let budget = verify_log_retention_budget(&retention_root, u64::MAX);
            let results_path = budget.results_path.clone();
            let mut result = verify_cell_result(&run1_spec);
            publish_retained_verify_log(pair, &budget, &results_path, &mut result).unwrap();
            drop(budget);

            let mutate = |path: &Path| {
                let before = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                if same_size {
                    let mut bytes = fs::read(path).unwrap();
                    let last = bytes.last_mut().expect("gzip output is not empty");
                    *last ^= 1;
                    fs::write(path, bytes).unwrap();
                } else {
                    OpenOptions::new()
                        .append(true)
                        .open(path)
                        .unwrap()
                        .write_all(b"x")
                        .unwrap();
                }
                let after = FileIdentity::from_metadata(&fs::metadata(path).unwrap());
                assert_eq!(before, after, "the mutation must preserve the inode");
            };
            let error = scan_existing_verify_log_bytes_with_hook(
                &retention_root,
                &results_path,
                VerifyLogRetentionPolicy::new(u64::MAX),
                Some(&mutate),
            )
            .unwrap_err();
            assert!(
                error.contains("restart scan")
                    || error.contains("gzip")
                    || error.contains("digest/size mismatch"),
                "{error}"
            );
        }
    }

    #[test]
    fn aggregate_retention_budget_refuses_checked_add_overflow() {
        let directory = tempfile::tempdir().unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        budget.reserve_additional(u64::MAX).unwrap();
        let error = budget.reserve_additional(1).unwrap_err();
        assert!(error.contains("accounting overflow"), "{error}");
        assert_eq!(budget.accounted_compressed_bytes().unwrap(), u64::MAX);
    }

    #[test]
    fn concurrent_retention_publications_cannot_race_past_the_aggregate_limit() {
        const PAIRS: usize = 12;
        const ALLOWED: u64 = 3;

        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG concurrent retention bound");
        let compressed_bytes = deterministic_gzip_size(body.as_bytes());
        let results = Mutex::new(Vec::new());
        let mut pairs = Vec::new();
        for index in 0..PAIRS {
            let artifact_dir = directory.path().join(format!("pair-{index}"));
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            let pair = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            pairs.push((artifact_dir, run1_spec, run2_spec, pair));
        }
        let budget = verify_log_retention_budget(
            directory.path(),
            compressed_bytes.checked_mul(ALLOWED).unwrap(),
        );
        let barrier = std::sync::Barrier::new(PAIRS);
        let active_staging_writers = std::sync::atomic::AtomicUsize::new(0);
        let maximum_active_staging_writers = std::sync::atomic::AtomicUsize::new(0);

        thread::scope(|scope| {
            for (artifact_dir, run1_spec, run2_spec, pair) in pairs {
                let budget = budget.clone();
                let barrier = &barrier;
                let results = &results;
                let active_staging_writers = &active_staging_writers;
                let maximum_active_staging_writers = &maximum_active_staging_writers;
                scope.spawn(move || {
                    let on_first_staging_write = || {
                        assert!(
                            budget.accounted_compressed_bytes().unwrap() > 0,
                            "the requested staging write must be charged before I/O begins"
                        );
                        let active = active_staging_writers
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            + 1;
                        maximum_active_staging_writers
                            .fetch_max(active, std::sync::atomic::Ordering::SeqCst);
                        barrier.wait();
                        active_staging_writers.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    };
                    let results_path = budget.results_path.clone();
                    let mut cell_result = verify_cell_result(&run1_spec);
                    let result = retain_verify_log_with_limit(
                        pair,
                        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
                        &budget,
                        &results_path,
                        &mut cell_result,
                        VerifyLogTransactionHooks {
                            on_first_staging_write: Some(&on_first_staging_write),
                            ..VerifyLogTransactionHooks::default()
                        },
                    )
                    .map(|publication| publication.retained.compressed_bytes);
                    results.lock().unwrap().push((
                        artifact_dir,
                        run1_spec.paths.log,
                        run2_spec.paths.log,
                        result,
                    ));
                });
            }
        });

        let results = results.into_inner().unwrap();
        assert_eq!(results.len(), PAIRS);
        assert_eq!(
            maximum_active_staging_writers.load(std::sync::atomic::Ordering::SeqCst),
            PAIRS,
            "all staging writers must be able to overlap without holding the accounting lock"
        );
        let successes = results
            .iter()
            .filter(|(_, _, _, result)| result.is_ok())
            .count();
        assert!(successes > 0);
        assert!(successes <= usize::try_from(ALLOWED).unwrap());
        assert!(
            budget.accounted_compressed_bytes().unwrap()
                <= compressed_bytes.checked_mul(ALLOWED).unwrap()
        );
        let published_rows = fs::read_to_string(&budget.results_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<CellResult>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(published_rows.len(), successes);
        assert!(
            published_rows
                .iter()
                .all(|result| result.attempts[0].retained_verify_log.is_some())
        );
        for (artifact_dir, run1_log, run2_log, result) in results {
            assert!(run1_log.is_file(), "retention must not remove run 1");
            assert!(run2_log.is_file(), "retention must not remove run 2");
            let retained = artifact_dir.join("retained/verify/1/run-1.log.gz");
            match result {
                Ok(bytes) => {
                    assert_eq!(bytes, compressed_bytes);
                    assert!(retained.is_file());
                }
                Err(error) => {
                    assert!(error.contains("aggregate limit"), "{error}");
                    assert!(!retained.exists());
                }
            }
        }
    }

    #[test]
    fn retained_verify_log_refuses_mutation_oversize_symlink_alias_and_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let body = structured_info_record("DETLOG retained");

        let mutated = directory.path().join("mutated");
        let (run1_spec, run2_spec) = verify_pair_fixture(&mutated, &body, &body, true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let before = FileIdentity::from_metadata(&fs::metadata(&run2_spec.paths.log).unwrap());
        let replacement = structured_info_record("DETLOG changed!");
        assert_eq!(replacement.len(), body.len());
        fs::write(&run2_spec.paths.log, &replacement).unwrap();
        let after = FileIdentity::from_metadata(&fs::metadata(&run2_spec.paths.log).unwrap());
        assert_eq!(before, after, "the mutation must preserve the inode");
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            compared,
            4096,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("changed after comparison"), "{error}");
        assert!(!mutated.join("retained").exists());
        assert!(result.attempts[0].retained_verify_log.is_none());

        let oversized = directory.path().join("oversized");
        let (run1_spec, run2_spec) = verify_pair_fixture(&oversized, &body, &body, true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let error = retain_verify_log_with_limit(
            compared,
            4,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap_err();
        assert!(error.contains("exceeds the 4-byte limit"), "{error}");
        assert!(run1_spec.paths.log.exists() && run2_spec.paths.log.exists());
        assert!(!oversized.join("retained").exists());

        let linked = directory.path().join("linked");
        let (run1_spec, run2_spec) = verify_pair_fixture(&linked, &body, &body, true);
        fs::remove_file(&run2_spec.paths.log).unwrap();
        std::fs::hard_link(&run1_spec.paths.log, &run2_spec.paths.log).unwrap();
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let error = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap_err();
        assert!(error.contains("same file"), "{error}");

        let symlinked = directory.path().join("symlinked");
        let (run1_spec, _) = verify_pair_fixture(&symlinked, &body, &body, true);
        let target = symlinked.join("target.log");
        fs::write(&target, b"do not change").unwrap();
        fs::remove_file(&run1_spec.paths.log).unwrap();
        std::os::unix::fs::symlink(&target, &run1_spec.paths.log).unwrap();
        let error = load_verify_run(&run1_spec).unwrap_err();
        assert!(
            error.contains("cannot open verify ordinary-run log"),
            "{error}"
        );
        assert_eq!(fs::read(target).unwrap(), b"do not change");

        let corrupted = directory.path().join("corrupted");
        let (run1_spec, run2_spec) = verify_pair_fixture(&corrupted, &body, &body, true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication = retain_verify_log_with_limit(
            compared,
            4096,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap();
        fs::write(
            corrupted.join(&publication.retained.relative_path),
            b"not gzip",
        )
        .unwrap();
        let error = verify_retained_verify_log_with_limit(
            &corrupted,
            &publication.retained,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap_err();
        assert!(error.contains("deterministic gzip header"), "{error}");

        let mut traversal = publication.retained;
        traversal.relative_path = "../escape.log.gz".into();
        let error = verify_retained_verify_log_with_limit(
            &corrupted,
            &traversal,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap_err();
        assert!(error.contains("must be exactly"), "{error}");

        traversal.relative_path = "retained/verify/1/other.log.gz".into();
        let error = verify_retained_verify_log_with_limit(
            &corrupted,
            &traversal,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap_err();
        assert!(error.contains("must be exactly"), "{error}");
    }

    #[test]
    fn refused_empty_pair_is_still_retained_and_cleanup_failure_keeps_its_descriptor() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("empty");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, "", "", true);
        let run1 = load_verify_run(&run1_spec).unwrap();
        let run2 = load_verify_run(&run2_spec).unwrap();
        let compared = compare_verify_runs(&run1_spec, run1, &run2_spec, run2).unwrap();
        assert_eq!(compared.comparison.report.verdict, Verdict::NoResult);
        let retention_budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication = retain_verify_log_with_limit(
            compared,
            4096,
            &retention_budget,
            &retention_budget.results_path,
            &mut result,
            VerifyLogTransactionHooks::default(),
        )
        .unwrap();
        assert_eq!(publication.retained.compared_info_messages, 0);
        assert_eq!(publication.retained.uncompressed_bytes, 0);
        assert_eq!(result.schema, RETAINED_VERIFY_LOG_CELL_RESULT_SCHEMA);
        assert_eq!(result.outcome, "ERROR");
        assert_eq!(result.result, None);
        assert_eq!(result.failure_class, Some(FailureClass::NoResult));
        assert_eq!(
            result.error_kind.as_deref(),
            Some("incomplete-verification-evidence")
        );
        assert_eq!(result.attempts[0].outcome, "ERROR");
        let report_json = result.attempts[0]
            .verification_report
            .as_deref()
            .expect("the no-result report is retained");
        assert_eq!(
            result.attempts[0].verification_report_sha256.as_deref(),
            Some(hex_digest(report_json.as_bytes()).as_str())
        );
        assert_eq!(
            VerificationReport::from_json_slice(report_json.as_bytes())
                .unwrap()
                .verdict,
            Verdict::NoResult
        );
        verify_retained_verify_log_with_limit(
            &artifact_dir,
            &publication.retained,
            &run1_spec.execution.id,
            1,
            4096,
        )
        .unwrap();

        let mut calls = 0;
        let error = cleanup_verify_log_sources_with(&artifact_dir, &publication, 4096, |path| {
            calls += 1;
            if calls == 2 {
                Err(std::io::Error::other("injected second unlink failure"))
            } else {
                fs::remove_file(path)
            }
        })
        .unwrap_err();
        assert!(error.contains("injected second unlink failure"), "{error}");
        assert!(
            artifact_dir
                .join(&publication.retained.relative_path)
                .is_file()
        );
        assert_eq!(publication.retained.role, RetainedVerifyLogRole::Run1);
        assert!(!run2_spec.paths.log.exists());
        assert!(run1_spec.paths.log.exists());
        cleanup_verify_log_sources(&artifact_dir, &publication).unwrap();
        assert!(!run1_spec.paths.log.exists());
        assert!(!run2_spec.paths.log.exists());
        cleanup_verify_log_sources(&artifact_dir, &publication)
            .expect("repeating completed raw-log cleanup must be harmless");
    }

    #[test]
    fn divergent_pair_publishes_the_exact_report_and_first_divergence() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("diverged");
        let left = structured_info_record("DETLOG left");
        let right = structured_info_record("DETLOG right");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &left, &right, true);
        let compared = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let expected_report = compared.comparison.report.clone();
        assert_eq!(expected_report.verdict, Verdict::Diverged);
        assert_eq!(expected_report.first_divergent_record, Some(1));
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        publish_retained_verify_log(compared, &budget, &budget.results_path, &mut result).unwrap();

        assert_eq!(result.outcome, "FAIL");
        assert_eq!(result.result, Some(ObservedResult::DeterminismFailure));
        assert_eq!(result.failure_class, Some(FailureClass::ProductFailure));
        assert_eq!(result.first_divergent_record, Some(1));
        assert_eq!(result.attempts[0].first_divergent_record, Some(1));
        let report_json = result.attempts[0]
            .verification_report
            .as_deref()
            .expect("diverged comparison report must be retained");
        assert_eq!(
            result.attempts[0].verification_report_sha256.as_deref(),
            Some(hex_digest(report_json.as_bytes()).as_str())
        );
        assert_eq!(
            VerificationReport::from_json_slice(report_json.as_bytes()).unwrap(),
            expected_report
        );
    }

    #[test]
    fn matching_nonzero_and_signaled_pairs_remain_product_failures() {
        use crate::canonical_verdict::GuestDisposition;

        for (name, disposition, expected_status, expected_signal, expected_reason) in [
            (
                "nonzero",
                GuestDisposition::Exited { code: 7 },
                Some(7),
                None,
                "verify exited with status 7",
            ),
            (
                "signal",
                GuestDisposition::Signaled { signal: 11 },
                None,
                Some(11),
                "verify was killed by signal 11",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let artifact_dir = directory.path().join(name);
            let body = structured_info_record("DETLOG identical failure");
            let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
            write_verify_run_fixture(&run1_spec, disposition, b"", b"", &body);
            write_verify_run_fixture(&run2_spec, disposition, b"", b"", &body);
            let compared = compare_verify_runs(
                &run1_spec,
                load_verify_run(&run1_spec).unwrap(),
                &run2_spec,
                load_verify_run(&run2_spec).unwrap(),
            )
            .unwrap();
            let expected_report = compared.comparison.report.clone();
            assert_eq!(expected_report.verdict, Verdict::Matched);
            assert!(expected_report.bitwise_parity);

            let budget = verify_log_retention_budget(directory.path(), u64::MAX);
            let mut result = verify_cell_result(&run1_spec);
            publish_retained_verify_log(compared, &budget, &budget.results_path, &mut result)
                .unwrap();

            assert_eq!(result.outcome, "FAIL");
            assert_eq!(result.result, Some(ObservedResult::CrashError));
            assert_eq!(result.failure_class, Some(FailureClass::ProductFailure));
            assert_eq!(result.error_kind, None);
            assert_eq!(result.reason.as_deref(), Some(expected_reason));
            assert_eq!(result.attempts[0].outcome, "FAIL");
            assert_eq!(result.attempts[0].status, expected_status);
            assert_eq!(result.attempts[0].signal, expected_signal);
            assert_eq!(result.attempts[0].reason.as_deref(), Some(expected_reason));
            let report_json = result.attempts[0]
                .verification_report
                .as_deref()
                .expect("the matching failure report must be retained");
            assert_eq!(
                VerificationReport::from_json_slice(report_json.as_bytes()).unwrap(),
                expected_report,
                "classification must not rewrite the comparator report"
            );
        }
    }

    #[test]
    fn raw_log_cleanup_refuses_replacement_before_removing_either_input() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG raw cleanup replacement");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &budget.results_path, &mut result).unwrap();
        let replacement = || {
            let replacement_path = run1_spec.paths.log.with_extension("replacement");
            fs::write(&replacement_path, fs::read(&run1_spec.paths.log).unwrap()).unwrap();
            fs::rename(&replacement_path, &run1_spec.paths.log).unwrap();
        };
        let error = cleanup_verify_log_sources_with_hook(
            &artifact_dir,
            &publication,
            4096,
            Some(&replacement),
            |path| fs::remove_file(path),
        )
        .unwrap_err();
        assert!(error.contains("changed identity"), "{error}");
        assert!(
            run1_spec.paths.log.is_file(),
            "the replacement must remain for recovery"
        );
        assert!(
            run2_spec.paths.log.is_file(),
            "cleanup must not remove the other raw log after detecting replacement"
        );
        assert_eq!(fs::read(&run1_spec.paths.log).unwrap(), body.as_bytes());
        assert_eq!(fs::read(&run2_spec.paths.log).unwrap(), body.as_bytes());
        cleanup_verify_log_sources(&artifact_dir, &publication).unwrap();
    }

    #[test]
    fn raw_log_cleanup_is_reconstructed_from_the_durable_result_row() {
        let directory = tempfile::tempdir().unwrap();
        let artifact_dir = directory.path().join("cell");
        let body = structured_info_record("DETLOG restart cleanup");
        let (run1_spec, run2_spec) = verify_pair_fixture(&artifact_dir, &body, &body, true);
        let pair = compare_verify_runs(
            &run1_spec,
            load_verify_run(&run1_spec).unwrap(),
            &run2_spec,
            load_verify_run(&run2_spec).unwrap(),
        )
        .unwrap();
        let budget = verify_log_retention_budget(directory.path(), u64::MAX);
        let mut result = verify_cell_result(&run1_spec);
        let publication =
            publish_retained_verify_log(pair, &budget, &budget.results_path, &mut result).unwrap();
        drop(publication);
        drop(budget);

        let persisted: CellResult = serde_json::from_str(
            fs::read_to_string(directory.path().join("results.jsonl"))
                .unwrap()
                .trim_end(),
        )
        .unwrap();
        let reopened = reopen_retained_verify_log_publication(&persisted).unwrap();
        assert_eq!(
            reopened.run1_raw,
            verify_run_paths(&artifact_dir, "1", VerifyRun::Run1)
                .unwrap()
                .log
        );
        assert_eq!(
            reopened.run2_raw,
            verify_run_paths(&artifact_dir, "1", VerifyRun::Run2)
                .unwrap()
                .log
        );
        cleanup_verify_log_sources(&artifact_dir, &reopened).unwrap();
        assert!(!run1_spec.paths.log.exists());
        assert!(!run2_spec.paths.log.exists());
        assert!(
            artifact_dir
                .join(&reopened.retained.relative_path)
                .is_file()
        );
        let restarted = VerifyLogRetentionBudget::open(
            directory.path(),
            directory.path().join("results.jsonl"),
            VerifyLogRetentionPolicy::new(reopened.retained.compressed_bytes),
        )
        .unwrap();
        assert_eq!(
            restarted.accounted_compressed_bytes().unwrap(),
            reopened.retained.compressed_bytes
        );
    }
}
