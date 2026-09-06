//! Typed evidence for one ordinary `hermit run` invocation.
//!
//! This is deliberately distinct from [`crate::canonical_verdict`]: verification
//! compares two runs, while this report binds one run's disposition to one
//! complete canonical-INFO input. A consumer must still compare two validated
//! reports before making a determinism claim.

use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use crate::canonical_verdict::RecordEnvelopeReport;

pub const RUN_EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const RUN_EVIDENCE_MANIFEST: &str = "manifest.json";
pub const RUN_EVIDENCE_INFO_ARTIFACT: &str = "canonical-info-v1.log";
const RUN_EVIDENCE_MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
pub const RUN_EVIDENCE_INFO_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// A terminal manifest is readable only after its directory entry is durable.
/// Mode zero is the producer's unpublished state.
pub const RUN_EVIDENCE_MANIFEST_MODE: u32 = 0o400;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEvidenceBackend {
    Ptrace,
    Dbt,
    Liteinst,
    Sabre,
    Kvm,
    E9patch,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalInfoPolicy {
    BitwiseInfoV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionLimitation {
    /// Reverie KVM currently returns one integer and does not distinguish an
    /// ordinary exit from signal death or report a core-dump bit.
    KvmExitCodeOnly,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum GuestDisposition {
    Exited {
        code: i32,
    },
    Signaled {
        signal: i32,
        core_dumped: bool,
    },
    ExitCodeOnly {
        code: i32,
        limitation: DispositionLimitation,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunEvidenceNoResultReason {
    RunFailed,
    UnsupportedBackend,
    MissingCanonicalInfo,
    ZeroCanonicalInfo,
    TruncatedCanonicalInfo,
    MalformedCanonicalInfo,
    ArtifactWriteFailed,
    UnsupportedDisposition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RunEvidenceOutcome {
    Complete {
        disposition: GuestDisposition,
    },
    NoResult {
        reason: RunEvidenceNoResultReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_disposition: Option<GuestDisposition>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CanonicalInfoEvidence {
    pub policy: CanonicalInfoPolicy,
    pub record_envelope: RecordEnvelopeReport,
    pub message_count: u64,
    pub byte_count: u64,
    pub sha256: Option<String>,
    pub artifact: String,
}

impl CanonicalInfoEvidence {
    pub fn no_result() -> Self {
        Self {
            policy: CanonicalInfoPolicy::BitwiseInfoV1,
            record_envelope: RecordEnvelopeReport::AllRecordsV1,
            message_count: 0,
            byte_count: 0,
            sha256: None,
            artifact: RUN_EVIDENCE_INFO_ARTIFACT.to_string(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunEvidenceReport {
    pub schema_version: u32,
    pub invocation_id: Uuid,
    pub backend: RunEvidenceBackend,
    pub attempt: u32,
    pub outcome: RunEvidenceOutcome,
    pub canonical_info: CanonicalInfoEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunEvidenceInspectionFailure {
    MissingManifest,
    ManifestTooLarge,
    MalformedManifest,
    UnsupportedSchema,
    InvalidManifest,
    ReportedNoResult(RunEvidenceNoResultReason),
    MissingArtifact,
    ArtifactTooLarge,
    ArtifactSizeMismatch,
    DigestMismatch,
    TruncatedCanonicalInfo,
    MalformedCanonicalInfo,
    ZeroCanonicalInfo,
    MessageCountMismatch,
}

impl std::fmt::Display for RunEvidenceInspectionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunEvidenceFileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Digest and exact inode identity of one harness-owned guest stream.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGuestStream {
    pub bytes: u64,
    pub sha256: String,
    pub identity: RunEvidenceFileIdentity,
}

/// Determinism settings bound by one ordinary-run result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRunDeterminism {
    pub detlog_io_buffers: bool,
    pub virtualize_time: bool,
}

/// Typed terminal result for one harness-managed ordinary execution.
///
/// The named stdout/stderr files are separate from Hermit's own diagnostic
/// descriptors. A producer publishes this sidecar only after both held output
/// descriptors are final and their visible paths still name the same inodes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GuestRunResult {
    pub schema_version: u32,
    pub disposition: GuestDisposition,
    pub determinism: GuestRunDeterminism,
    pub stdout: CapturedGuestStream,
    pub stderr: CapturedGuestStream,
}

impl GuestRunResult {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn from_current_json_slice(bytes: &[u8]) -> Result<Self, String> {
        let result: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid guest run result: {error}"))?;
        result.validate_current()?;
        Ok(result)
    }

    pub fn validate_current(&self) -> Result<(), String> {
        if self.schema_version != Self::SCHEMA_VERSION {
            return Err(format!(
                "unsupported guest run result schema {}; expected {}",
                self.schema_version,
                Self::SCHEMA_VERSION
            ));
        }
        if matches!(
            self.disposition,
            GuestDisposition::Signaled { signal, .. } if signal <= 0
        ) {
            return Err("guest run result signal must be positive".into());
        }
        for (name, stream) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            let valid_sha = stream.sha256.len() == 64
                && stream
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            if !valid_sha {
                return Err(format!(
                    "guest run result {name} sha256 is not lowercase hex"
                ));
            }
            if stream.identity.inode == 0 {
                return Err(format!("guest run result {name} inode must be nonzero"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedRunEvidence {
    pub report: RunEvidenceReport,
    pub canonical_info: Vec<u8>,
    pub artifact_identity: RunEvidenceFileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunEvidenceInspection {
    Complete(RunEvidenceReport),
    NoResult(RunEvidenceInspectionFailure),
}

fn backend_supports_evidence(backend: RunEvidenceBackend) -> bool {
    matches!(
        backend,
        RunEvidenceBackend::Ptrace | RunEvidenceBackend::Liteinst | RunEvidenceBackend::Kvm
    )
}

fn disposition_matches_backend(backend: RunEvidenceBackend, disposition: GuestDisposition) -> bool {
    match backend {
        RunEvidenceBackend::Kvm => matches!(
            disposition,
            GuestDisposition::ExitCodeOnly {
                limitation: DispositionLimitation::KvmExitCodeOnly,
                ..
            }
        ),
        RunEvidenceBackend::Ptrace | RunEvidenceBackend::Liteinst => matches!(
            disposition,
            GuestDisposition::Exited { .. } | GuestDisposition::Signaled { .. }
        ),
        RunEvidenceBackend::Dbt | RunEvidenceBackend::Sabre | RunEvidenceBackend::E9patch => false,
    }
}

fn static_manifest_fields_are_valid(report: &RunEvidenceReport) -> bool {
    report.schema_version == RUN_EVIDENCE_SCHEMA_VERSION
        && !report.invocation_id.is_nil()
        && report.attempt == 1
        && report.canonical_info.policy == CanonicalInfoPolicy::BitwiseInfoV1
        && report.canonical_info.record_envelope == RecordEnvelopeReport::AllRecordsV1
        && report.canonical_info.artifact == RUN_EVIDENCE_INFO_ARTIFACT
        && match report.outcome {
            RunEvidenceOutcome::Complete { disposition } => {
                backend_supports_evidence(report.backend)
                    && disposition_matches_backend(report.backend, disposition)
            }
            RunEvidenceOutcome::NoResult {
                reason,
                observed_disposition,
            } => {
                observed_disposition.is_none_or(|disposition| {
                    disposition_matches_backend(report.backend, disposition)
                }) && (reason == RunEvidenceNoResultReason::UnsupportedBackend)
                    != backend_supports_evidence(report.backend)
            }
        }
}
fn component_cstring(component: &OsStr) -> io::Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn owned_file(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative open/openat return is one newly owned fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_evidence_directory(path: &Path) -> io::Result<File> {
    let path = component_cstring(path.as_os_str())?;
    owned_file(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

struct RegularChild {
    bytes: Vec<u8>,
    identity: RunEvidenceFileIdentity,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RegularChildReadFailure {
    Unavailable,
    TooLarge,
    SizeChanged,
}

fn read_regular_child(
    directory: &File,
    name: &OsStr,
    required_mode: Option<u32>,
    maximum_bytes: u64,
) -> Result<RegularChild, RegularChildReadFailure> {
    let name = component_cstring(name).map_err(|_| RegularChildReadFailure::Unavailable)?;
    let mut file = owned_file(unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
    .map_err(|_| RegularChildReadFailure::Unavailable)?;
    let mut stat = MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(file.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(RegularChildReadFailure::Unavailable);
    }
    // SAFETY: fstat initialized the complete structure on success.
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || required_mode.is_some_and(|mode| stat.st_mode & 0o777 != mode)
    {
        return Err(RegularChildReadFailure::Unavailable);
    }
    let initial_size =
        u64::try_from(stat.st_size).map_err(|_| RegularChildReadFailure::Unavailable)?;
    if initial_size > maximum_bytes {
        return Err(RegularChildReadFailure::TooLarge);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| RegularChildReadFailure::Unavailable)?;
    if u64::try_from(bytes.len()).map_err(|_| RegularChildReadFailure::TooLarge)? > maximum_bytes {
        return Err(RegularChildReadFailure::TooLarge);
    }
    let metadata = file
        .metadata()
        .map_err(|_| RegularChildReadFailure::Unavailable)?;
    if metadata.len() != initial_size || metadata.len() != bytes.len() as u64 {
        return Err(RegularChildReadFailure::SizeChanged);
    }
    Ok(RegularChild {
        bytes,
        identity: RunEvidenceFileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    })
}

/// Read and independently validate an ordinary-run sidecar.
///
/// Every unreadable or incomplete state is a typed no-result. In particular,
/// this function never returns `Complete` merely because the manifest says so:
/// it re-reads the artifact, verifies its length and SHA-256 digest, and runs
/// the fixed `BitwiseInfoV1` parser over the exact bytes.
pub fn inspect_run_evidence(directory: &Path) -> RunEvidenceInspection {
    match load_run_evidence(directory) {
        Ok(evidence) => RunEvidenceInspection::Complete(evidence.report),
        Err(failure) => RunEvidenceInspection::NoResult(failure),
    }
}

#[cfg(test)]
fn inspect_run_evidence_directory(directory: &File) -> RunEvidenceInspection {
    match load_run_evidence_directory(directory) {
        Ok(evidence) => RunEvidenceInspection::Complete(evidence.report),
        Err(failure) => RunEvidenceInspection::NoResult(failure),
    }
}

/// Load a complete report and the exact canonical bytes through one held
/// directory descriptor.
pub fn load_run_evidence(
    directory: &Path,
) -> Result<ValidatedRunEvidence, RunEvidenceInspectionFailure> {
    let directory = open_evidence_directory(directory)
        .map_err(|_| RunEvidenceInspectionFailure::MissingManifest)?;
    load_run_evidence_directory(&directory)
}

fn load_run_evidence_directory(
    directory: &File,
) -> Result<ValidatedRunEvidence, RunEvidenceInspectionFailure> {
    let manifest = read_regular_child(
        directory,
        OsStr::new(RUN_EVIDENCE_MANIFEST),
        Some(RUN_EVIDENCE_MANIFEST_MODE),
        RUN_EVIDENCE_MANIFEST_MAX_BYTES,
    )
    .map_err(|failure| match failure {
        RegularChildReadFailure::Unavailable => RunEvidenceInspectionFailure::MissingManifest,
        RegularChildReadFailure::TooLarge => RunEvidenceInspectionFailure::ManifestTooLarge,
        RegularChildReadFailure::SizeChanged => RunEvidenceInspectionFailure::MalformedManifest,
    })?
    .bytes;
    let report: RunEvidenceReport = serde_json::from_slice(&manifest)
        .map_err(|_| RunEvidenceInspectionFailure::MalformedManifest)?;
    if report.schema_version != RUN_EVIDENCE_SCHEMA_VERSION {
        return Err(RunEvidenceInspectionFailure::UnsupportedSchema);
    }
    if !static_manifest_fields_are_valid(&report) {
        return Err(RunEvidenceInspectionFailure::InvalidManifest);
    }
    match report.outcome {
        RunEvidenceOutcome::NoResult { reason, .. } => {
            if report.canonical_info.message_count != 0
                || report.canonical_info.byte_count != 0
                || report.canonical_info.sha256.is_some()
            {
                return Err(RunEvidenceInspectionFailure::InvalidManifest);
            }
            return Err(RunEvidenceInspectionFailure::ReportedNoResult(reason));
        }
        RunEvidenceOutcome::Complete { .. } => {}
    }

    if report.canonical_info.message_count == 0 {
        return Err(RunEvidenceInspectionFailure::ZeroCanonicalInfo);
    }
    let Some(expected_digest) = report.canonical_info.sha256.as_deref() else {
        return Err(RunEvidenceInspectionFailure::InvalidManifest);
    };
    if report.canonical_info.byte_count > RUN_EVIDENCE_INFO_MAX_BYTES {
        return Err(RunEvidenceInspectionFailure::ArtifactTooLarge);
    }
    let artifact = read_regular_child(
        directory,
        OsStr::new(RUN_EVIDENCE_INFO_ARTIFACT),
        None,
        RUN_EVIDENCE_INFO_MAX_BYTES,
    )
    .map_err(|failure| match failure {
        RegularChildReadFailure::Unavailable => RunEvidenceInspectionFailure::MissingArtifact,
        RegularChildReadFailure::TooLarge => RunEvidenceInspectionFailure::ArtifactTooLarge,
        RegularChildReadFailure::SizeChanged => RunEvidenceInspectionFailure::ArtifactSizeMismatch,
    })?;
    if artifact.bytes.len() as u64 != report.canonical_info.byte_count {
        return Err(RunEvidenceInspectionFailure::ArtifactSizeMismatch);
    }
    if detcore::Digest::new(&artifact.bytes).to_string() != expected_digest {
        return Err(RunEvidenceInspectionFailure::DigestMismatch);
    }
    if std::str::from_utf8(&artifact.bytes)
        .ok()
        .is_some_and(detcore::logdiff::log_was_truncated)
    {
        return Err(RunEvidenceInspectionFailure::TruncatedCanonicalInfo);
    }
    let mut canonical = Vec::new();
    let count = detcore::logdiff::write_bitwise_info_v1_bytes(
        &artifact.bytes,
        RUN_EVIDENCE_INFO_ARTIFACT,
        &mut canonical,
    )
    .map_err(|_| RunEvidenceInspectionFailure::MalformedCanonicalInfo)? as u64;
    if count == 0 {
        return Err(RunEvidenceInspectionFailure::ZeroCanonicalInfo);
    }
    if count != report.canonical_info.message_count {
        return Err(RunEvidenceInspectionFailure::MessageCountMismatch);
    }
    Ok(ValidatedRunEvidence {
        report,
        canonical_info: artifact.bytes,
        artifact_identity: artifact.identity,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn valid_log() -> Vec<u8> {
        b"Apr 09 06:08:01.100  INFO hermit_test: first evidence record\n\
Apr 09 06:08:02.100  INFO hermit_test: second evidence record\n"
            .to_vec()
    }

    fn write_complete_fixture(directory: &Path, artifact: &[u8], digest: String, count: u64) {
        fs::write(directory.join(RUN_EVIDENCE_INFO_ARTIFACT), artifact).unwrap();
        let report = RunEvidenceReport {
            schema_version: RUN_EVIDENCE_SCHEMA_VERSION,
            invocation_id: Uuid::from_u128(1),
            backend: RunEvidenceBackend::Ptrace,
            attempt: 1,
            outcome: RunEvidenceOutcome::Complete {
                disposition: GuestDisposition::Exited { code: 0 },
            },
            canonical_info: CanonicalInfoEvidence {
                policy: CanonicalInfoPolicy::BitwiseInfoV1,
                record_envelope: RecordEnvelopeReport::AllRecordsV1,
                message_count: count,
                byte_count: artifact.len() as u64,
                sha256: Some(digest),
                artifact: RUN_EVIDENCE_INFO_ARTIFACT.to_string(),
            },
        };
        let manifest = directory.join(RUN_EVIDENCE_MANIFEST);
        fs::write(&manifest, serde_json::to_vec(&report).unwrap()).unwrap();
        let mut permissions = fs::metadata(&manifest).unwrap().permissions();
        permissions.set_mode(RUN_EVIDENCE_MANIFEST_MODE);
        fs::set_permissions(manifest, permissions).unwrap();
    }

    #[test]
    fn complete_report_requires_the_bound_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            directory.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        assert!(matches!(
            inspect_run_evidence(directory.path()),
            RunEvidenceInspection::Complete(_)
        ));
    }

    #[test]
    fn missing_zero_truncated_malformed_and_digest_mismatch_are_no_result() {
        let missing = tempfile::tempdir().unwrap();
        assert_eq!(
            inspect_run_evidence(missing.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );

        let zero = tempfile::tempdir().unwrap();
        write_complete_fixture(zero.path(), b"", detcore::Digest::new(b"").to_string(), 0);
        assert_eq!(
            inspect_run_evidence(zero.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ZeroCanonicalInfo)
        );

        let truncated = tempfile::tempdir().unwrap();
        let truncated_log = format!(
            "{}{}\n",
            String::from_utf8(valid_log()).unwrap(),
            detcore::logdiff::TRUNCATION_MARKER
        );
        write_complete_fixture(
            truncated.path(),
            truncated_log.as_bytes(),
            detcore::Digest::new(truncated_log.as_bytes()).to_string(),
            2,
        );
        assert_eq!(
            inspect_run_evidence(truncated.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::TruncatedCanonicalInfo)
        );

        let malformed = tempfile::tempdir().unwrap();
        let invalid_utf8 = [0x80];
        write_complete_fixture(
            malformed.path(),
            &invalid_utf8,
            detcore::Digest::new(&invalid_utf8).to_string(),
            1,
        );
        assert_eq!(
            inspect_run_evidence(malformed.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MalformedCanonicalInfo)
        );

        let mismatch = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            mismatch.path(),
            &log,
            detcore::Digest::new(b"different").to_string(),
            2,
        );
        assert_eq!(
            inspect_run_evidence(mismatch.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::DigestMismatch)
        );
    }

    #[test]
    fn oversized_manifest_and_artifact_are_no_result_without_reading_their_bodies() {
        let oversized_manifest = tempfile::tempdir().unwrap();
        let manifest = oversized_manifest.path().join(RUN_EVIDENCE_MANIFEST);
        let file = fs::File::create(&manifest).unwrap();
        file.set_len(RUN_EVIDENCE_MANIFEST_MAX_BYTES + 1).unwrap();
        let mut permissions = fs::metadata(&manifest).unwrap().permissions();
        permissions.set_mode(RUN_EVIDENCE_MANIFEST_MODE);
        fs::set_permissions(&manifest, permissions).unwrap();
        assert_eq!(
            inspect_run_evidence(oversized_manifest.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ManifestTooLarge)
        );

        let oversized_artifact = tempfile::tempdir().unwrap();
        let log = valid_log();
        write_complete_fixture(
            oversized_artifact.path(),
            &log,
            detcore::Digest::new(&log).to_string(),
            2,
        );
        let artifact = oversized_artifact.path().join(RUN_EVIDENCE_INFO_ARTIFACT);
        fs::OpenOptions::new()
            .write(true)
            .open(&artifact)
            .unwrap()
            .set_len(RUN_EVIDENCE_INFO_MAX_BYTES + 1)
            .unwrap();
        assert_eq!(
            inspect_run_evidence(oversized_artifact.path()),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ArtifactTooLarge)
        );
    }
    #[test]
    fn inspector_pins_one_directory_identity() {
        let parent = tempfile::tempdir().unwrap();
        let requested = parent.path().join("requested");
        fs::create_dir(&requested).unwrap();
        let log = valid_log();
        write_complete_fixture(&requested, &log, detcore::Digest::new(&log).to_string(), 2);
        let held = open_evidence_directory(&requested).unwrap();

        let original = parent.path().join("original");
        fs::rename(&requested, &original).unwrap();
        fs::create_dir(&requested).unwrap();

        assert!(matches!(
            inspect_run_evidence_directory(&held),
            RunEvidenceInspection::Complete(_)
        ));
        assert_eq!(
            inspect_run_evidence(&requested),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );
    }

    #[test]
    fn inspector_refuses_symlinked_directory_and_children() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let source = parent.path().join("source");
        fs::create_dir(&source).unwrap();
        let log = valid_log();
        write_complete_fixture(&source, &log, detcore::Digest::new(&log).to_string(), 2);

        let directory_link = parent.path().join("directory-link");
        symlink(&source, &directory_link).unwrap();
        assert_eq!(
            inspect_run_evidence(&directory_link),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );

        let manifest_link_case = parent.path().join("manifest-link");
        fs::create_dir(&manifest_link_case).unwrap();
        symlink(
            source.join(RUN_EVIDENCE_MANIFEST),
            manifest_link_case.join(RUN_EVIDENCE_MANIFEST),
        )
        .unwrap();
        assert_eq!(
            inspect_run_evidence(&manifest_link_case),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
        );

        let artifact_link_case = parent.path().join("artifact-link");
        fs::create_dir(&artifact_link_case).unwrap();
        fs::copy(
            source.join(RUN_EVIDENCE_MANIFEST),
            artifact_link_case.join(RUN_EVIDENCE_MANIFEST),
        )
        .unwrap();
        symlink(
            source.join(RUN_EVIDENCE_INFO_ARTIFACT),
            artifact_link_case.join(RUN_EVIDENCE_INFO_ARTIFACT),
        )
        .unwrap();
        assert_eq!(
            inspect_run_evidence(&artifact_link_case),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingArtifact)
        );
    }
}
