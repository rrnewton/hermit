/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Versioned, content-addressed authority for backend-local strict verification.
//!
//! The legacy `verified` and `bitwise_parity` JSON booleans remain useful
//! diagnostics, but they are not an authority: they do not bind a verdict to the
//! producer, guest, effective configuration, raw streams, or the amount of work
//! that executed. This module deliberately derives a semantic decision from the
//! referenced evidence instead of accepting any cached boolean in the receipt.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;

use detcore::Digest;
use reverie::process::ExitStatus;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::Context;
use crate::Error;

pub const STRICT_RECEIPT_SCHEMA_V1: &str = "hermit.strict-verify-receipt/v1";
pub const STRICT_ARTIFACT_STORE_V1: &str = "adjacent-content-addressed-sha256/v1";
pub const INFO_STREAM_FRAMING_V1: &str = "hermit-info-length-prefixed-u64be/v1";
pub const WALL_CLOCK_PREFIX_V1: &str = "real-wall-clock-prefix/v1";
pub const HOST_ADDRESS_ORDINAL_V1: &str = "marked-host-address-to-first-appearance-ordinal/v1";
pub const STRICT_PROFILE_V1: &str = "backend-local-strict-repeat/v1";
pub const STRICT_RECORD_REPLAY_PROFILE_V1: &str = "record-replay-strict/v1";

const INFO_STREAM_MAGIC: &[u8] = b"HERMIT-INFO-V1\0";
const INFO_LEVEL: &[u8] = b"INFO ";
const DETCORE_TARGET: &[u8] = b"detcore";
const SCHEDULER_TARGET: &[u8] = b"detcore::scheduler";
const DETLOG_PREFIX: &[u8] = b"DETLOG ";
const MEMORY_DETLOG_PREFIX: &[u8] = b"DETLOG [memory]";
const SCHED_STEP_COMMIT_PREFIX: &[u8] = b"[sched-step5] >>>>>>>\n\n COMMIT ";
const BACKGROUND_COMMIT_PREFIX: &[u8] = b"[scheduler] >>>>>>>\n\n COMMIT ";
// Stable tags emitted by `Detcore::detlog_memory_maps` through
// `procmaps::display`: preserve their exact punctuation in the receipt class
// classifier rather than guessing from a human-readable region name.
const HEAP_TAG: &[u8] = b"[heap]->";
const STACK_TAG: &[u8] = b"[stack]->";

/// A file identity is a digest plus exact byte length and path spelling. The
/// path is diagnostic; the digest is the identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    pub sha256: String,
    pub bytes: u64,
    pub path_bytes_hex: String,
}

impl FileIdentity {
    pub fn from_path(path: &Path) -> Result<Self, Error> {
        let bytes =
            fs::read(path).with_context(|| format!("reading identity input {}", path.display()))?;
        Ok(Self {
            sha256: Digest::new(&bytes).to_string(),
            bytes: u64::try_from(bytes.len()).context("identity input length exceeds u64")?,
            path_bytes_hex: hex(path.as_os_str().as_bytes()),
        })
    }
}

/// A blob stored at `<receipt>.artifacts/sha256/<sha256>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub sha256: String,
    pub bytes: u64,
}

/// Typed process termination. Signal termination does not alias an integer exit
/// code, and the core-dump bit remains observable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TypedTermination {
    Exited { code: i32 },
    Signaled { signal: i32, core_dumped: bool },
}

impl From<ExitStatus> for TypedTermination {
    fn from(status: ExitStatus) -> Self {
        match status {
            ExitStatus::Exited(code) => Self::Exited { code },
            ExitStatus::Signaled(signal, core_dumped) => Self::Signaled {
                signal: signal as i32,
                core_dumped,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageClassEvidence {
    pub count: u64,
    pub framed_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageEvidence {
    pub info: MessageClassEvidence,
    pub commit: MessageClassEvidence,
    pub detlog: MessageClassEvidence,
    pub detlog_heap: MessageClassEvidence,
    pub detlog_stack: MessageClassEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvidence {
    pub stdout: ArtifactRef,
    /// Untouched process stderr bytes. In particular, SaBRe's multiplexed
    /// DETLOG transport is captured here before comparison-only extraction.
    pub stderr: ArtifactRef,
    pub raw_log: ArtifactRef,
    /// Length-prefixed canonical INFO messages. This is redundant with
    /// `raw_log` on purpose: the verifier re-derives it and refuses disagreement.
    pub ordered_info: ArtifactRef,
    pub termination: TypedTermination,
    pub messages: MessageEvidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationPolicy {
    pub framing: String,
    pub stripped_prefix: String,
    pub canonicalization: String,
    pub selected_level: String,
    pub exact_remainder: bool,
    pub strips_numeric_hex_or_path_bytes: bool,
}

impl ObservationPolicy {
    fn strict_v1() -> Self {
        Self {
            framing: INFO_STREAM_FRAMING_V1.to_owned(),
            stripped_prefix: WALL_CLOCK_PREFIX_V1.to_owned(),
            canonicalization: HOST_ADDRESS_ORDINAL_V1.to_owned(),
            selected_level: "INFO".to_owned(),
            exact_remainder: true,
            strips_numeric_hex_or_path_bytes: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptIdentity {
    pub source_revision: String,
    pub producer_binary: FileIdentity,
    pub guest_binary: FileIdentity,
    /// Content-addressed, versioned serialization of the exact guest command.
    pub guest_command: ArtifactRef,
    /// Defined as `sha256:<guest_command.sha256>`.
    pub test_id: String,
    /// Content-addressed effective run configuration, including Detcore config.
    pub effective_run_config: ArtifactRef,
    pub selected_backend: String,
    pub runtime_backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageEvidence {
    pub profile: String,
    pub discovered_tests: u64,
    pub selected_tests: u64,
    pub executed_runs: u64,
    pub filtered_tests: u64,
    pub comparison_failures: u64,
    pub compared_messages_left: u64,
    pub compared_messages_right: u64,
    pub require_detlog_heap: bool,
    pub require_detlog_stack: bool,
    pub fail_closed_strict_requested: bool,
    pub verify_strict_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssuranceLevel {
    L2,
    L3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceDimension {
    Stdout,
    Stderr,
    Termination,
    OrderedInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoResultReason {
    UnknownSchema { found: String },
    PolicyMismatch,
    InvalidSourceRevision { found: String },
    IdentityMismatch { field: String },
    InvalidArtifactReference { field: String },
    MissingArtifact { sha256: String },
    ArtifactLengthMismatch { sha256: String },
    ArtifactDigestMismatch { sha256: String },
    MalformedLog { side: String, detail: String },
    MalformedInfoFraming { side: String, detail: String },
    DerivedInfoMismatch { side: String },
    MessageEvidenceMismatch { side: String },
    ZeroSelectedTests,
    CoverageCountsMismatch,
    ExecutedRunCountMismatch { found: u64 },
    ZeroInfoMessages { side: String },
    MissingMessageClass { side: String, class: String },
    StrictModeNotRequested,
    LosslessEventTransportOrderUnavailable { backend: String },
    ReceiptDecisionMismatch,
    MalformedReceipt { detail: String },
}

/// The only decision a green consumer may use. It is derived by
/// [`verify_strict_receipt`], never read from `verified`, `bitwise_parity`, or
/// `declared_decision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StrictReceiptDecision {
    Qualified {
        assurance: AssuranceLevel,
    },
    Diverged {
        dimensions: Vec<DivergenceDimension>,
    },
    NoResult {
        reasons: Vec<NoResultReason>,
    },
}

impl StrictReceiptDecision {
    pub fn is_qualified(&self) -> bool {
        matches!(self, Self::Qualified { .. })
    }
}

/// Versioned receipt. `legacy` is flattened solely to keep the historic JSON
/// keys readable by diagnostic tools. The semantic verifier never consults it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrictVerificationReceipt {
    pub schema: String,
    pub artifact_store: String,
    pub identity: ReceiptIdentity,
    pub policy: ObservationPolicy,
    pub coverage: CoverageEvidence,
    pub runs: [RunEvidence; 2],
    pub declared_decision: StrictReceiptDecision,
    #[serde(flatten)]
    pub legacy: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictReceiptExpectation {
    pub source_revision: String,
    pub profile: String,
    pub producer_binary_sha256: String,
    pub producer_binary_bytes: u64,
    pub guest_binary_sha256: String,
    pub guest_binary_bytes: u64,
    pub test_id: String,
    pub effective_run_config_sha256: String,
    pub selected_backend: String,
    pub runtime_backend: String,
}

pub struct StrictRunInput<'a> {
    pub stdout: &'a [u8],
    pub stderr: &'a [u8],
    pub raw_log: &'a [u8],
    pub termination: TypedTermination,
}

pub struct StrictReceiptBuildInput<'a> {
    pub source_revision: &'a str,
    pub profile: &'a str,
    pub producer_binary: FileIdentity,
    pub guest_binary: FileIdentity,
    pub guest_command: &'a [u8],
    pub effective_run_config: &'a [u8],
    pub selected_backend: &'a str,
    pub runtime_backend: &'a str,
    pub selected_tests: u64,
    pub executed_runs: u64,
    pub require_detlog_heap: bool,
    pub require_detlog_stack: bool,
    pub fail_closed_strict_requested: bool,
    pub verify_strict_requested: bool,
    pub left: StrictRunInput<'a>,
    pub right: StrictRunInput<'a>,
    pub legacy: BTreeMap<String, Value>,
}

struct ContentAddressedStore {
    root: PathBuf,
}

impl ContentAddressedStore {
    fn new(receipt_path: &Path) -> Result<Self, Error> {
        let root = artifact_root_for_receipt(receipt_path);
        fs::create_dir_all(root.join("sha256"))
            .with_context(|| format!("creating receipt artifact store {}", root.display()))?;
        Ok(Self { root })
    }

    fn put(&self, bytes: &[u8]) -> Result<ArtifactRef, Error> {
        let sha256 = Digest::new(bytes).to_string();
        let reference = ArtifactRef {
            sha256: sha256.clone(),
            bytes: u64::try_from(bytes.len()).context("artifact length exceeds u64")?,
        };
        let directory = self.root.join("sha256");
        let destination = directory.join(&sha256);

        if destination.exists() {
            verify_existing_blob(&destination, bytes)?;
            return Ok(reference);
        }

        let mut temporary = NamedTempFile::new_in(&directory)
            .with_context(|| format!("creating artifact beside {}", destination.display()))?;
        temporary
            .write_all(bytes)
            .with_context(|| format!("writing artifact {}", destination.display()))?;
        temporary
            .as_file_mut()
            .sync_all()
            .with_context(|| format!("syncing artifact {}", destination.display()))?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => {}
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_existing_blob(&destination, bytes)?;
            }
            Err(error) => {
                return Err(error.error).with_context(|| {
                    format!(
                        "publishing content-addressed artifact {}",
                        destination.display()
                    )
                });
            }
        }
        Ok(reference)
    }
}

fn verify_existing_blob(path: &Path, expected: &[u8]) -> Result<(), Error> {
    let actual = fs::read(path).with_context(|| {
        format!(
            "reading existing content-addressed artifact {}",
            path.display()
        )
    })?;
    if actual != expected {
        anyhow::bail!(
            "content-addressed artifact collision or corruption at {}",
            path.display()
        );
    }
    Ok(())
}

pub fn artifact_root_for_receipt(receipt_path: &Path) -> PathBuf {
    let mut name = receipt_path
        .file_name()
        .unwrap_or_else(|| OsStr::new("verify.json"))
        .to_os_string();
    name.push(".artifacts");
    receipt_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}

/// Publish every content-addressed artifact, derive the decision through the
/// semantic verifier, and only then atomically publish the receipt.
pub fn publish_strict_receipt(
    receipt_path: &Path,
    input: StrictReceiptBuildInput<'_>,
) -> Result<StrictReceiptDecision, Error> {
    validate_legacy_keys(&input.legacy)?;
    let store = ContentAddressedStore::new(receipt_path)?;
    let guest_command = store.put(input.guest_command)?;
    let effective_run_config = store.put(input.effective_run_config)?;
    let test_id = format!("sha256:{}", guest_command.sha256);

    let left = build_run(&store, &input.left)?;
    let right = build_run(&store, &input.right)?;
    let identity = ReceiptIdentity {
        source_revision: input.source_revision.to_owned(),
        producer_binary: input.producer_binary,
        guest_binary: input.guest_binary,
        guest_command,
        test_id: test_id.clone(),
        effective_run_config,
        selected_backend: input.selected_backend.to_owned(),
        runtime_backend: input.runtime_backend.to_owned(),
    };
    let expectation = StrictReceiptExpectation {
        source_revision: input.source_revision.to_owned(),
        profile: input.profile.to_owned(),
        producer_binary_sha256: identity.producer_binary.sha256.clone(),
        producer_binary_bytes: identity.producer_binary.bytes,
        guest_binary_sha256: identity.guest_binary.sha256.clone(),
        guest_binary_bytes: identity.guest_binary.bytes,
        test_id,
        effective_run_config_sha256: identity.effective_run_config.sha256.clone(),
        selected_backend: input.selected_backend.to_owned(),
        runtime_backend: input.runtime_backend.to_owned(),
    };
    let coverage = CoverageEvidence {
        profile: input.profile.to_owned(),
        discovered_tests: 1,
        selected_tests: input.selected_tests,
        executed_runs: input.executed_runs,
        filtered_tests: 0,
        comparison_failures: u64::from(
            input.left.stdout != input.right.stdout
                || input.left.stderr != input.right.stderr
                || input.left.termination != input.right.termination
                || left.ordered_info.sha256 != right.ordered_info.sha256,
        ),
        compared_messages_left: left.messages.info.count,
        compared_messages_right: right.messages.info.count,
        require_detlog_heap: input.require_detlog_heap,
        require_detlog_stack: input.require_detlog_stack,
        fail_closed_strict_requested: input.fail_closed_strict_requested,
        verify_strict_requested: input.verify_strict_requested,
    };

    let mut receipt = StrictVerificationReceipt {
        schema: STRICT_RECEIPT_SCHEMA_V1.to_owned(),
        artifact_store: STRICT_ARTIFACT_STORE_V1.to_owned(),
        identity,
        policy: ObservationPolicy::strict_v1(),
        coverage,
        runs: [left, right],
        declared_decision: StrictReceiptDecision::NoResult {
            reasons: vec![NoResultReason::ReceiptDecisionMismatch],
        },
        legacy: input.legacy,
    };

    let derived = verify_strict_receipt(&receipt, &store.root, &expectation);
    receipt.declared_decision = derived.clone();
    let rederived = verify_strict_receipt(&receipt, &store.root, &expectation);
    if rederived != derived {
        anyhow::bail!("strict receipt verifier was not stable across decision publication");
    }

    write_receipt_atomically(receipt_path, &receipt)?;
    let published: StrictVerificationReceipt = serde_json::from_slice(
        &fs::read(receipt_path)
            .with_context(|| format!("reading published receipt {}", receipt_path.display()))?,
    )
    .with_context(|| format!("parsing published receipt {}", receipt_path.display()))?;
    let published_decision = verify_strict_receipt(&published, &store.root, &expectation);
    if published_decision != derived || published.declared_decision != derived {
        anyhow::bail!("published strict receipt failed its semantic self-verification");
    }
    Ok(derived)
}

fn validate_legacy_keys(legacy: &BTreeMap<String, Value>) -> Result<(), Error> {
    for reserved in [
        "schema",
        "artifact_store",
        "identity",
        "policy",
        "coverage",
        "runs",
        "declared_decision",
    ] {
        if legacy.contains_key(reserved) {
            anyhow::bail!("legacy diagnostic map contains reserved receipt key {reserved}");
        }
    }
    Ok(())
}

fn write_receipt_atomically(
    receipt_path: &Path,
    receipt: &StrictVerificationReceipt,
) -> Result<(), Error> {
    let directory = receipt_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(directory)
        .with_context(|| format!("creating receipt beside {}", receipt_path.display()))?;
    serde_json::to_writer(&mut temporary, receipt)?;
    temporary.write_all(b"\n")?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(receipt_path)
        .map_err(|error| error.error)
        .with_context(|| format!("publishing strict receipt {}", receipt_path.display()))?;
    Ok(())
}

fn build_run(
    store: &ContentAddressedStore,
    input: &StrictRunInput<'_>,
) -> Result<RunEvidence, Error> {
    let canonical = canonical_info_stream(input.raw_log)
        .map_err(|error| Error::msg(format!("malformed strict verification log: {error}")))?;
    Ok(RunEvidence {
        stdout: store.put(input.stdout)?,
        stderr: store.put(input.stderr)?,
        raw_log: store.put(input.raw_log)?,
        ordered_info: store.put(&canonical.framed)?,
        termination: input.termination.clone(),
        messages: canonical.evidence,
    })
}

/// Load a receipt and derive its semantic decision. Malformed JSON is a typed
/// no-result, not a successful legacy fallback.
pub fn load_and_verify_strict_receipt(
    receipt_path: &Path,
    expectation: &StrictReceiptExpectation,
) -> StrictReceiptDecision {
    let bytes = match fs::read(receipt_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return no_result(NoResultReason::MalformedReceipt {
                detail: error.to_string(),
            });
        }
    };
    let receipt: StrictVerificationReceipt = match serde_json::from_slice(&bytes) {
        Ok(receipt) => receipt,
        Err(error) => {
            return no_result(NoResultReason::MalformedReceipt {
                detail: error.to_string(),
            });
        }
    };
    let root = artifact_root_for_receipt(receipt_path);
    let derived = verify_strict_receipt(&receipt, &root, expectation);
    if receipt.declared_decision != derived {
        no_result(NoResultReason::ReceiptDecisionMismatch)
    } else {
        derived
    }
}

/// The single semantic verifier for this authority. It ignores flattened legacy
/// booleans and independently dereferences and re-derives every load-bearing
/// fact.
pub fn verify_strict_receipt(
    receipt: &StrictVerificationReceipt,
    artifact_root: &Path,
    expectation: &StrictReceiptExpectation,
) -> StrictReceiptDecision {
    if receipt.schema != STRICT_RECEIPT_SCHEMA_V1 {
        return no_result(NoResultReason::UnknownSchema {
            found: receipt.schema.clone(),
        });
    }
    if receipt.artifact_store != STRICT_ARTIFACT_STORE_V1
        || receipt.policy != ObservationPolicy::strict_v1()
    {
        return no_result(NoResultReason::PolicyMismatch);
    }
    if !is_source_revision_identity(&receipt.identity.source_revision) {
        return no_result(NoResultReason::InvalidSourceRevision {
            found: receipt.identity.source_revision.clone(),
        });
    }
    for (field, file) in [
        ("producer_binary", &receipt.identity.producer_binary),
        ("guest_binary", &receipt.identity.guest_binary),
    ] {
        if !is_sha256(&file.sha256) || file.bytes == 0 {
            return no_result(NoResultReason::IdentityMismatch {
                field: field.to_owned(),
            });
        }
    }
    if let Some(field) = identity_mismatch(&receipt.identity, expectation) {
        return no_result(NoResultReason::IdentityMismatch { field });
    }
    if receipt.coverage.selected_tests == 0 {
        return no_result(NoResultReason::ZeroSelectedTests);
    }
    if !matches!(
        receipt.coverage.profile.as_str(),
        STRICT_PROFILE_V1 | STRICT_RECORD_REPLAY_PROFILE_V1
    ) {
        return no_result(NoResultReason::CoverageCountsMismatch);
    }
    if receipt.coverage.profile != expectation.profile {
        return no_result(NoResultReason::IdentityMismatch {
            field: "profile".to_owned(),
        });
    }
    if receipt.coverage.discovered_tests < receipt.coverage.selected_tests
        || receipt.coverage.filtered_tests
            != receipt
                .coverage
                .discovered_tests
                .saturating_sub(receipt.coverage.selected_tests)
    {
        return no_result(NoResultReason::CoverageCountsMismatch);
    }
    if receipt.coverage.executed_runs != 2 {
        return no_result(NoResultReason::ExecutedRunCountMismatch {
            found: receipt.coverage.executed_runs,
        });
    }
    if !receipt.coverage.fail_closed_strict_requested || !receipt.coverage.verify_strict_requested {
        return no_result(NoResultReason::StrictModeNotRequested);
    }
    for (field, reference) in [
        ("guest_command", &receipt.identity.guest_command),
        (
            "effective_run_config",
            &receipt.identity.effective_run_config,
        ),
    ] {
        if let Err(reason) = read_artifact(artifact_root, reference, field) {
            return no_result(reason);
        }
    }
    if receipt.identity.test_id != format!("sha256:{}", receipt.identity.guest_command.sha256) {
        return no_result(NoResultReason::IdentityMismatch {
            field: "test_id/guest_command".to_owned(),
        });
    }

    let left = match verify_run(artifact_root, "left", &receipt.runs[0]) {
        Ok(run) => run,
        Err(reason) => return no_result(reason),
    };
    let right = match verify_run(artifact_root, "right", &receipt.runs[1]) {
        Ok(run) => run,
        Err(reason) => return no_result(reason),
    };
    // This is derived from the independently expected runtime backend, not a
    // producer boolean. SaBRe currently appends stderr-forwarded DETLOG events
    // to the coordinator log after exit, so their cross-transport order is not
    // observable and cannot qualify.
    if receipt.identity.runtime_backend == "sabre" {
        return no_result(NoResultReason::LosslessEventTransportOrderUnavailable {
            backend: receipt.identity.runtime_backend.clone(),
        });
    }

    if receipt.coverage.compared_messages_left != left.message_count
        || receipt.coverage.compared_messages_right != right.message_count
    {
        return no_result(NoResultReason::MessageEvidenceMismatch {
            side: "coverage".to_owned(),
        });
    }
    for (side, run) in [("left", &left), ("right", &right)] {
        if run.message_count == 0 {
            return no_result(NoResultReason::ZeroInfoMessages {
                side: side.to_owned(),
            });
        }
        if run.commit_count == 0 {
            return no_result(NoResultReason::MissingMessageClass {
                side: side.to_owned(),
                class: "commit".to_owned(),
            });
        }
        if run.detlog_count == 0 {
            return no_result(NoResultReason::MissingMessageClass {
                side: side.to_owned(),
                class: "detlog".to_owned(),
            });
        }
        if receipt.coverage.require_detlog_heap && run.heap_count == 0 {
            return no_result(NoResultReason::MissingMessageClass {
                side: side.to_owned(),
                class: "detlog_heap".to_owned(),
            });
        }
        if receipt.coverage.require_detlog_stack && run.stack_count == 0 {
            return no_result(NoResultReason::MissingMessageClass {
                side: side.to_owned(),
                class: "detlog_stack".to_owned(),
            });
        }
    }

    let mut dimensions = Vec::new();
    if left.stdout != right.stdout {
        dimensions.push(DivergenceDimension::Stdout);
    }
    if left.stderr != right.stderr {
        dimensions.push(DivergenceDimension::Stderr);
    }
    if receipt.runs[0].termination != receipt.runs[1].termination {
        dimensions.push(DivergenceDimension::Termination);
    }
    if left.ordered_info != right.ordered_info {
        dimensions.push(DivergenceDimension::OrderedInfo);
    }
    if !dimensions.is_empty() {
        if receipt.coverage.comparison_failures != 1 {
            return no_result(NoResultReason::CoverageCountsMismatch);
        }
        return StrictReceiptDecision::Diverged { dimensions };
    }
    if receipt.coverage.comparison_failures != 0 {
        return no_result(NoResultReason::CoverageCountsMismatch);
    }

    StrictReceiptDecision::Qualified {
        assurance: if receipt.coverage.require_detlog_heap && receipt.coverage.require_detlog_stack
        {
            AssuranceLevel::L3
        } else {
            AssuranceLevel::L2
        },
    }
}

fn identity_mismatch(
    actual: &ReceiptIdentity,
    expected: &StrictReceiptExpectation,
) -> Option<String> {
    let string_mismatch = [
        (
            "source_revision",
            actual.source_revision.as_str(),
            expected.source_revision.as_str(),
        ),
        (
            "producer_binary_sha256",
            actual.producer_binary.sha256.as_str(),
            expected.producer_binary_sha256.as_str(),
        ),
        (
            "guest_binary_sha256",
            actual.guest_binary.sha256.as_str(),
            expected.guest_binary_sha256.as_str(),
        ),
        (
            "test_id",
            actual.test_id.as_str(),
            expected.test_id.as_str(),
        ),
        (
            "effective_run_config_sha256",
            actual.effective_run_config.sha256.as_str(),
            expected.effective_run_config_sha256.as_str(),
        ),
        (
            "selected_backend",
            actual.selected_backend.as_str(),
            expected.selected_backend.as_str(),
        ),
        (
            "runtime_backend",
            actual.runtime_backend.as_str(),
            expected.runtime_backend.as_str(),
        ),
    ]
    .into_iter()
    .find_map(|(field, actual, expected)| (actual != expected).then(|| field.to_owned()));
    string_mismatch.or_else(|| {
        (actual.producer_binary.bytes != expected.producer_binary_bytes)
            .then(|| "producer_binary_bytes".to_owned())
            .or_else(|| {
                (actual.guest_binary.bytes != expected.guest_binary_bytes)
                    .then(|| "guest_binary_bytes".to_owned())
            })
    })
}

fn is_source_revision_identity(revision: &str) -> bool {
    // A commit SHA identifies one exact source tree. `<sha>-dirty` identifies an
    // unbounded family of worktrees and therefore cannot authorize a receipt,
    // even when the producer binary itself has an exact digest.
    revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct VerifiedRun {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    ordered_info: Vec<u8>,
    message_count: u64,
    commit_count: u64,
    detlog_count: u64,
    heap_count: u64,
    stack_count: u64,
}

fn verify_run(
    artifact_root: &Path,
    side: &str,
    evidence: &RunEvidence,
) -> Result<VerifiedRun, NoResultReason> {
    let stdout = read_artifact(artifact_root, &evidence.stdout, &format!("{side}.stdout"))?;
    let stderr = read_artifact(artifact_root, &evidence.stderr, &format!("{side}.stderr"))?;
    let raw_log = read_artifact(artifact_root, &evidence.raw_log, &format!("{side}.raw_log"))?;
    let ordered_info = read_artifact(
        artifact_root,
        &evidence.ordered_info,
        &format!("{side}.ordered_info"),
    )?;
    decode_framed_messages(&ordered_info).map_err(|detail| {
        NoResultReason::MalformedInfoFraming {
            side: side.to_owned(),
            detail,
        }
    })?;
    let derived =
        canonical_info_stream(&raw_log).map_err(|detail| NoResultReason::MalformedLog {
            side: side.to_owned(),
            detail,
        })?;
    if derived.framed != ordered_info {
        return Err(NoResultReason::DerivedInfoMismatch {
            side: side.to_owned(),
        });
    }
    if derived.evidence != evidence.messages {
        return Err(NoResultReason::MessageEvidenceMismatch {
            side: side.to_owned(),
        });
    }
    Ok(VerifiedRun {
        stdout,
        stderr,
        ordered_info,
        message_count: derived.evidence.info.count,
        commit_count: derived.evidence.commit.count,
        detlog_count: derived.evidence.detlog.count,
        heap_count: derived.evidence.detlog_heap.count,
        stack_count: derived.evidence.detlog_stack.count,
    })
}

fn read_artifact(
    artifact_root: &Path,
    reference: &ArtifactRef,
    field: &str,
) -> Result<Vec<u8>, NoResultReason> {
    if !is_sha256(&reference.sha256) {
        return Err(NoResultReason::InvalidArtifactReference {
            field: field.to_owned(),
        });
    }
    let path = artifact_root.join("sha256").join(&reference.sha256);
    let bytes = fs::read(&path).map_err(|_| NoResultReason::MissingArtifact {
        sha256: reference.sha256.clone(),
    })?;
    if u64::try_from(bytes.len()).ok() != Some(reference.bytes) {
        return Err(NoResultReason::ArtifactLengthMismatch {
            sha256: reference.sha256.clone(),
        });
    }
    if Digest::new(&bytes).to_string() != reference.sha256 {
        return Err(NoResultReason::ArtifactDigestMismatch {
            sha256: reference.sha256.clone(),
        });
    }
    Ok(bytes)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn no_result(reason: NoResultReason) -> StrictReceiptDecision {
    StrictReceiptDecision::NoResult {
        reasons: vec![reason],
    }
}

struct CanonicalInfoStream {
    framed: Vec<u8>,
    evidence: MessageEvidence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InfoEventClass {
    Other,
    Commit,
    Detlog,
    DetlogHeap,
    DetlogStack,
}

struct CanonicalInfoMessage {
    bytes: Vec<u8>,
    class: InfoEventClass,
}

fn canonical_info_stream(raw: &[u8]) -> Result<CanonicalInfoStream, String> {
    let messages = parse_log_messages(raw)?;
    let mut address_ordinals = HashMap::<Vec<u8>, u64>::new();
    let mut next_ordinal = 1_u64;
    let mut infos = Vec::new();
    for message in messages {
        if message.starts_with(INFO_LEVEL) {
            let bytes =
                canonicalize_marked_addresses(&message, &mut address_ordinals, &mut next_ordinal)?;
            let class = classify_info_event(&bytes)?;
            infos.push(CanonicalInfoMessage { bytes, class });
        }
    }

    let commits: Vec<Vec<u8>> = infos
        .iter()
        .filter(|message| message.class == InfoEventClass::Commit)
        .map(|message| message.bytes.clone())
        .collect();
    let detlogs: Vec<Vec<u8>> = infos
        .iter()
        .filter(|message| {
            matches!(
                message.class,
                InfoEventClass::Detlog | InfoEventClass::DetlogHeap | InfoEventClass::DetlogStack
            )
        })
        .map(|message| message.bytes.clone())
        .collect();
    let heaps: Vec<Vec<u8>> = infos
        .iter()
        .filter(|message| message.class == InfoEventClass::DetlogHeap)
        .map(|message| message.bytes.clone())
        .collect();
    let stacks: Vec<Vec<u8>> = infos
        .iter()
        .filter(|message| message.class == InfoEventClass::DetlogStack)
        .map(|message| message.bytes.clone())
        .collect();
    let ordered: Vec<Vec<u8>> = infos.into_iter().map(|message| message.bytes).collect();

    let framed = frame_messages(&ordered);
    Ok(CanonicalInfoStream {
        evidence: MessageEvidence {
            info: class_evidence(&ordered),
            commit: class_evidence(&commits),
            detlog: class_evidence(&detlogs),
            detlog_heap: class_evidence(&heaps),
            detlog_stack: class_evidence(&stacks),
        },
        framed,
    })
}

/// Parse the tracing record into its target and payload before assigning an
/// event class. A human-readable substring is not an event identity: only the
/// exact Detcore target plus the exact producer prefix can satisfy COMMIT,
/// DETLOG, heap, or stack coverage.
fn classify_info_event(message: &[u8]) -> Result<InfoEventClass, String> {
    let remainder = message
        .strip_prefix(INFO_LEVEL)
        .ok_or_else(|| "event classifier received a non-INFO message".to_owned())?;
    let separator = find_subslice(remainder, b": ")
        .ok_or_else(|| "INFO message has no target/payload separator".to_owned())?;
    let target = &remainder[..separator];
    let payload = &remainder[separator + 2..];

    if target == SCHEDULER_TARGET
        && (payload.starts_with(SCHED_STEP_COMMIT_PREFIX)
            || payload.starts_with(BACKGROUND_COMMIT_PREFIX))
    {
        return Ok(InfoEventClass::Commit);
    }

    let detcore_target = target == DETCORE_TARGET || target.starts_with(b"detcore::");
    if !detcore_target || !payload.starts_with(DETLOG_PREFIX) {
        return Ok(InfoEventClass::Other);
    }
    if target != DETCORE_TARGET || !payload.starts_with(MEMORY_DETLOG_PREFIX) {
        return Ok(InfoEventClass::Detlog);
    }

    let heap = contains(payload, HEAP_TAG) || contains(payload, b"[heap] ->");
    let stack = contains(payload, STACK_TAG) || contains(payload, b"[stack] ->");
    match (heap, stack) {
        (true, false) => Ok(InfoEventClass::DetlogHeap),
        (false, true) => Ok(InfoEventClass::DetlogStack),
        (false, false) => Ok(InfoEventClass::Detlog),
        (true, true) => Err("memory DETLOG ambiguously names both heap and stack".to_owned()),
    }
}

fn class_evidence(messages: &[Vec<u8>]) -> MessageClassEvidence {
    MessageClassEvidence {
        count: u64::try_from(messages.len()).unwrap_or(u64::MAX),
        framed_sha256: Digest::new(&frame_messages(messages)).to_string(),
    }
}

fn parse_log_messages(raw: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut messages = Vec::<Vec<u8>>::new();
    for physical in raw.split_inclusive(|byte| *byte == b'\n') {
        let line = physical.strip_suffix(b"\n").unwrap_or(physical);
        match parse_wall_clock_prefix(line)? {
            Some(message_start) => messages.push(line[message_start..].to_vec()),
            None => {
                let Some(current) = messages.last_mut() else {
                    return Err("log begins without a versioned wall-clock frame".to_owned());
                };
                current.push(b'\n');
                current.extend_from_slice(line);
            }
        }
    }
    Ok(messages)
}

fn parse_wall_clock_prefix(line: &[u8]) -> Result<Option<usize>, String> {
    let prefix_end = if looks_like_rfc3339_start(line) {
        Some(parse_rfc3339_prefix(line)?)
    } else if looks_like_month_start(line) {
        Some(parse_month_prefix(line)?)
    } else {
        None
    };
    let Some(mut cursor) = prefix_end else {
        return Ok(None);
    };
    let before_spaces = cursor;
    while line.get(cursor) == Some(&b' ') {
        cursor += 1;
    }
    if cursor == before_spaces {
        return Err("wall-clock prefix is not followed by a space".to_owned());
    }
    let levels: [&[u8]; 5] = [b"ERROR ", b"WARN ", b"INFO ", b"DEBUG ", b"TRACE "];
    if !levels.iter().any(|level| line[cursor..].starts_with(level)) {
        return Err("wall-clock frame has an unknown or missing level tag".to_owned());
    }
    Ok(Some(cursor))
}

fn looks_like_rfc3339_start(line: &[u8]) -> bool {
    line.len() >= 5 && line[..4].iter().all(u8::is_ascii_digit) && line.get(4) == Some(&b'-')
}

fn parse_rfc3339_prefix(line: &[u8]) -> Result<usize, String> {
    let fixed_digits = [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18];
    let fixed_bytes = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if line.len() < 22
        || fixed_digits
            .iter()
            .any(|index| !line.get(*index).is_some_and(u8::is_ascii_digit))
        || fixed_bytes
            .iter()
            .any(|(index, byte)| line.get(*index) != Some(byte))
        || line.get(19) != Some(&b'.')
    {
        return Err("malformed RFC3339 wall-clock prefix".to_owned());
    }
    let mut cursor = 20;
    let fraction_start = cursor;
    while line.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == fraction_start || line.get(cursor) != Some(&b'Z') {
        return Err("malformed RFC3339 fractional seconds".to_owned());
    }
    Ok(cursor + 1)
}

fn looks_like_month_start(line: &[u8]) -> bool {
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    line.len() >= 4 && MONTHS.contains(&&line[..3]) && line.get(3) == Some(&b' ')
}

fn parse_month_prefix(line: &[u8]) -> Result<usize, String> {
    let digits = [4, 5, 7, 8, 10, 11, 13, 14];
    let fixed = [(3, b' '), (6, b' '), (9, b':'), (12, b':'), (15, b'.')];
    if line.len() < 18
        || digits
            .iter()
            .any(|index| !line.get(*index).is_some_and(u8::is_ascii_digit))
        || fixed
            .iter()
            .any(|(index, byte)| line.get(*index) != Some(byte))
    {
        return Err("malformed legacy wall-clock prefix".to_owned());
    }
    let mut cursor = 16;
    let fraction_start = cursor;
    while line.get(cursor).is_some_and(u8::is_ascii_digit) {
        cursor += 1;
    }
    if cursor == fraction_start {
        return Err("malformed legacy fractional seconds".to_owned());
    }
    Ok(cursor)
}

fn canonicalize_marked_addresses(
    message: &[u8],
    ordinals: &mut HashMap<Vec<u8>, u64>,
    next_ordinal: &mut u64,
) -> Result<Vec<u8>, String> {
    const START: &[u8] = b"<hostaddr 0x";
    let mut output = Vec::with_capacity(message.len());
    let mut cursor = 0;
    while let Some(relative) = find_subslice(&message[cursor..], START) {
        let start = cursor + relative;
        output.extend_from_slice(&message[cursor..start]);
        let digits_start = start + START.len();
        let mut end = digits_start;
        while message.get(end).is_some_and(u8::is_ascii_hexdigit) {
            end += 1;
        }
        if end == digits_start || message.get(end) != Some(&b'>') {
            return Err("malformed marked host address".to_owned());
        }
        let address = message[digits_start..end].to_vec();
        let ordinal = *ordinals.entry(address).or_insert_with(|| {
            let ordinal = *next_ordinal;
            *next_ordinal += 1;
            ordinal
        });
        output.extend_from_slice(format!("<addr{ordinal}>").as_bytes());
        cursor = end + 1;
    }
    output.extend_from_slice(&message[cursor..]);
    Ok(output)
}

fn frame_messages(messages: &[Vec<u8>]) -> Vec<u8> {
    let mut framed = INFO_STREAM_MAGIC.to_vec();
    for message in messages {
        framed.extend_from_slice(
            &u64::try_from(message.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        framed.extend_from_slice(message);
    }
    framed
}

fn decode_framed_messages(framed: &[u8]) -> Result<Vec<&[u8]>, String> {
    if !framed.starts_with(INFO_STREAM_MAGIC) {
        return Err("missing strict INFO framing magic".to_owned());
    }
    let mut cursor = INFO_STREAM_MAGIC.len();
    let mut messages = Vec::new();
    while cursor < framed.len() {
        let length_bytes: [u8; 8] = framed
            .get(cursor..cursor + 8)
            .ok_or_else(|| "truncated INFO frame length".to_owned())?
            .try_into()
            .map_err(|_| "invalid INFO frame length".to_owned())?;
        cursor += 8;
        let length = usize::try_from(u64::from_be_bytes(length_bytes))
            .map_err(|_| "INFO frame length exceeds usize".to_owned())?;
        let end = cursor
            .checked_add(length)
            .ok_or_else(|| "INFO frame length overflow".to_owned())?;
        let message = framed
            .get(cursor..end)
            .ok_or_else(|| "truncated INFO frame payload".to_owned())?;
        if !message.starts_with(INFO_LEVEL) {
            return Err("framed message is not INFO".to_owned());
        }
        messages.push(message);
        cursor = end;
    }
    Ok(messages)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    find_subslice(haystack, needle).is_some()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const SOURCE_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
    const COMMAND: &[u8] =
        br#"{"schema":"hermit-guest-command/v1","program":"/bin/fixture","args":[]}"#;
    const CONFIG: &[u8] = br#"{"schema":"hermit-effective-run-config/v1","strict":true,"seed":0}"#;

    fn identity(label: &[u8]) -> FileIdentity {
        FileIdentity {
            sha256: Digest::new(label).to_string(),
            bytes: label.len() as u64,
            path_bytes_hex: hex(label),
        }
    }

    fn base_log(timestamp: &str, pointer: &str) -> Vec<u8> {
        format!(
            "{timestamp} INFO detcore: DETLOG [syscall] write(fd=1, ptr=0x2, count=17, path=/tmp/exact-a, host=<hostaddr {pointer}>)\n\
             {timestamp} INFO detcore::scheduler: [sched-step5] >>>>>>>\n\n COMMIT turn 9 at time 946684800123\n\
             {timestamp} INFO detcore: DETLOG [memory] 0x602000-0x603000 [heap]->0123456789abcdef\n\
             {timestamp} INFO detcore: DETLOG [memory] 0x7fff0000-0x7fff1000 [stack]->fedcba9876543210\n"
        )
        .into_bytes()
    }

    fn no_memory_log(timestamp: &str) -> Vec<u8> {
        format!(
            "{timestamp} INFO detcore: DETLOG [syscall] write(fd=1, count=17)\n\
             {timestamp} INFO detcore::scheduler: [sched-step5] >>>>>>>\n\n COMMIT turn 9 at time 946684800123\n"
        )
        .into_bytes()
    }

    fn legacy() -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("verified".to_owned(), json!(true)),
            ("bitwise_parity".to_owned(), json!(true)),
            ("verdict".to_owned(), json!("matched")),
        ])
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the receipt bracket tests vary each independent evidence dimension explicitly"
    )]
    fn publish(
        directory: &TempDir,
        left_log: &[u8],
        right_log: &[u8],
        left_stdout: &[u8],
        right_stdout: &[u8],
        left_stderr: &[u8],
        right_stderr: &[u8],
        left_termination: TypedTermination,
        right_termination: TypedTermination,
        require_heap: bool,
        require_stack: bool,
    ) -> (PathBuf, StrictReceiptDecision) {
        let path = directory.path().join("receipt.json");
        let decision = publish_strict_receipt(
            &path,
            StrictReceiptBuildInput {
                source_revision: SOURCE_REVISION,
                profile: STRICT_PROFILE_V1,
                producer_binary: identity(b"producer"),
                guest_binary: identity(b"guest"),
                guest_command: COMMAND,
                effective_run_config: CONFIG,
                selected_backend: "ptrace",
                runtime_backend: "ptrace",
                selected_tests: 1,
                executed_runs: 2,
                require_detlog_heap: require_heap,
                require_detlog_stack: require_stack,
                fail_closed_strict_requested: true,
                verify_strict_requested: true,
                left: StrictRunInput {
                    stdout: left_stdout,
                    stderr: left_stderr,
                    raw_log: left_log,
                    termination: left_termination,
                },
                right: StrictRunInput {
                    stdout: right_stdout,
                    stderr: right_stderr,
                    raw_log: right_log,
                    termination: right_termination,
                },
                legacy: legacy(),
            },
        )
        .unwrap();
        (path, decision)
    }

    fn read_receipt(path: &Path) -> StrictVerificationReceipt {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn expectation(receipt: &StrictVerificationReceipt) -> StrictReceiptExpectation {
        StrictReceiptExpectation {
            source_revision: receipt.identity.source_revision.clone(),
            profile: receipt.coverage.profile.clone(),
            producer_binary_sha256: receipt.identity.producer_binary.sha256.clone(),
            producer_binary_bytes: receipt.identity.producer_binary.bytes,
            guest_binary_sha256: receipt.identity.guest_binary.sha256.clone(),
            guest_binary_bytes: receipt.identity.guest_binary.bytes,
            test_id: receipt.identity.test_id.clone(),
            effective_run_config_sha256: receipt.identity.effective_run_config.sha256.clone(),
            selected_backend: receipt.identity.selected_backend.clone(),
            runtime_backend: receipt.identity.runtime_backend.clone(),
        }
    }

    fn assert_no_result_reason(
        decision: StrictReceiptDecision,
        predicate: impl Fn(&NoResultReason) -> bool,
    ) {
        let StrictReceiptDecision::NoResult { reasons } = decision else {
            panic!("expected no-result, got {decision:?}");
        };
        assert!(
            reasons.iter().any(predicate),
            "unexpected reasons: {reasons:?}"
        );
    }

    #[test]
    fn qualifying_l3_is_binary_safe_and_rederived_from_artifacts() {
        let directory = TempDir::new().unwrap();
        let left_log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let right_log = base_log("2026-08-06T05:59:59.999999Z", "0xbbbb");
        let (path, decision) = publish(
            &directory,
            &left_log,
            &right_log,
            b"stdout\0\xff",
            b"stdout\0\xff",
            b"stderr\xfe",
            b"stderr\xfe",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            true,
            true,
        );
        assert_eq!(
            decision,
            StrictReceiptDecision::Qualified {
                assurance: AssuranceLevel::L3
            }
        );

        let receipt = read_receipt(&path);
        assert_eq!(receipt.runs[0].messages.info.count, 4);
        assert_eq!(receipt.runs[0].messages.commit.count, 1);
        assert_eq!(receipt.runs[0].messages.detlog.count, 3);
        assert_eq!(receipt.runs[0].messages.detlog_heap.count, 1);
        assert_eq!(receipt.runs[0].messages.detlog_stack.count, 1);
        assert!(artifact_root_for_receipt(&path).join("sha256").is_dir());
        assert_eq!(
            load_and_verify_strict_receipt(&path, &expectation(&receipt)),
            decision
        );
    }

    #[test]
    fn legacy_booleans_are_diagnostic_not_authority() {
        let directory = TempDir::new().unwrap();
        let log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let (path, _) = publish(
            &directory,
            &log,
            &log,
            b"out",
            b"out",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        let mut receipt = read_receipt(&path);
        let expected = expectation(&receipt);
        receipt.legacy.insert("verified".to_owned(), json!(false));
        receipt
            .legacy
            .insert("bitwise_parity".to_owned(), json!(false));
        assert!(
            verify_strict_receipt(&receipt, &artifact_root_for_receipt(&path), &expected)
                .is_qualified()
        );
    }

    #[test]
    fn zero_selection_execution_and_messages_are_typed_no_results() {
        let directory = TempDir::new().unwrap();
        let log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let (path, _) = publish(
            &directory,
            &log,
            &log,
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        let root = artifact_root_for_receipt(&path);
        let receipt = read_receipt(&path);
        let expected = expectation(&receipt);

        let mut zero_selected = receipt.clone();
        zero_selected.coverage.selected_tests = 0;
        assert_no_result_reason(
            verify_strict_receipt(&zero_selected, &root, &expected),
            |reason| matches!(reason, NoResultReason::ZeroSelectedTests),
        );

        let mut one_run = receipt.clone();
        one_run.coverage.executed_runs = 1;
        assert_no_result_reason(
            verify_strict_receipt(&one_run, &root, &expected),
            |reason| {
                matches!(
                    reason,
                    NoResultReason::ExecutedRunCountMismatch { found: 1 }
                )
            },
        );

        let empty_directory = TempDir::new().unwrap();
        let (empty_path, empty_decision) = publish(
            &empty_directory,
            b"",
            b"",
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        assert_no_result_reason(empty_decision, |reason| {
            matches!(reason, NoResultReason::ZeroInfoMessages { .. })
        });
        assert!(empty_path.exists());

        for (class_log, missing_class) in [
            (
                b"2026-08-06T01:00:00.000001Z INFO detcore: DETLOG [syscall] write(fd=1)\n"
                    .as_slice(),
                "commit",
            ),
            (
                b"2026-08-06T01:00:00.000001Z INFO detcore::scheduler: [sched-step5] >>>>>>>\n\n COMMIT turn 1\n".as_slice(),
                "detlog",
            ),
        ] {
            let class_directory = TempDir::new().unwrap();
            let (_, class_decision) = publish(
                &class_directory,
                class_log,
                class_log,
                b"",
                b"",
                b"",
                b"",
                TypedTermination::Exited { code: 0 },
                TypedTermination::Exited { code: 0 },
                false,
                false,
            );
            assert_no_result_reason(
                class_decision,
                |reason| matches!(reason, NoResultReason::MissingMessageClass { class, .. } if class == missing_class),
            );
        }
    }

    #[test]
    fn malformed_log_and_framing_are_refused() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("receipt.json");
        let error = publish_strict_receipt(
            &path,
            StrictReceiptBuildInput {
                source_revision: SOURCE_REVISION,
                profile: STRICT_PROFILE_V1,
                producer_binary: identity(b"producer"),
                guest_binary: identity(b"guest"),
                guest_command: COMMAND,
                effective_run_config: CONFIG,
                selected_backend: "ptrace",
                runtime_backend: "ptrace",
                selected_tests: 1,
                executed_runs: 2,
                require_detlog_heap: false,
                require_detlog_stack: false,
                fail_closed_strict_requested: true,
                verify_strict_requested: true,
                left: StrictRunInput {
                    stdout: b"",
                    stderr: b"",
                    raw_log: b"2026-bad INFO detcore: DETLOG malformed\n",
                    termination: TypedTermination::Exited { code: 0 },
                },
                right: StrictRunInput {
                    stdout: b"",
                    stderr: b"",
                    raw_log: b"",
                    termination: TypedTermination::Exited { code: 0 },
                },
                legacy: legacy(),
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("malformed strict verification log")
        );
        assert!(!path.exists());

        let good_directory = TempDir::new().unwrap();
        let log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let (good_path, _) = publish(
            &good_directory,
            &log,
            &log,
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        let root = artifact_root_for_receipt(&good_path);
        let mut receipt = read_receipt(&good_path);
        let expected = expectation(&receipt);
        let malformed = b"not-a-framed-info-stream";
        let malformed_ref = ArtifactRef {
            sha256: Digest::new(malformed).to_string(),
            bytes: malformed.len() as u64,
        };
        fs::write(root.join("sha256").join(&malformed_ref.sha256), malformed).unwrap();
        receipt.runs[0].ordered_info = malformed_ref;
        assert_no_result_reason(
            verify_strict_receipt(&receipt, &root, &expected),
            |reason| matches!(reason, NoResultReason::MalformedInfoFraming { .. }),
        );
    }

    #[test]
    fn identity_and_strict_configuration_mismatches_are_no_results() {
        let directory = TempDir::new().unwrap();
        let log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let (path, _) = publish(
            &directory,
            &log,
            &log,
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        let root = artifact_root_for_receipt(&path);
        let mut receipt = read_receipt(&path);
        for field in [
            "source_revision",
            "profile",
            "producer_binary_sha256",
            "producer_binary_bytes",
            "guest_binary_sha256",
            "guest_binary_bytes",
            "test_id",
            "effective_run_config_sha256",
            "selected_backend",
            "runtime_backend",
        ] {
            let mut mismatched = expectation(&receipt);
            match field {
                "source_revision" => mismatched.source_revision = "f".repeat(40),
                "profile" => mismatched.profile = STRICT_RECORD_REPLAY_PROFILE_V1.to_owned(),
                "producer_binary_sha256" => mismatched.producer_binary_sha256 = "f".repeat(64),
                "producer_binary_bytes" => mismatched.producer_binary_bytes += 1,
                "guest_binary_sha256" => mismatched.guest_binary_sha256 = "f".repeat(64),
                "guest_binary_bytes" => mismatched.guest_binary_bytes += 1,
                "test_id" => mismatched.test_id = format!("sha256:{}", "f".repeat(64)),
                "effective_run_config_sha256" => {
                    mismatched.effective_run_config_sha256 = "f".repeat(64)
                }
                "selected_backend" => mismatched.selected_backend = "sabre".to_owned(),
                "runtime_backend" => mismatched.runtime_backend = "sabre".to_owned(),
                _ => unreachable!(),
            }
            assert_no_result_reason(
                verify_strict_receipt(&receipt, &root, &mismatched),
                |reason| matches!(reason, NoResultReason::IdentityMismatch { field: found } if found == field),
            );
        }

        let expected = expectation(&receipt);
        receipt.coverage.verify_strict_requested = false;
        assert_no_result_reason(
            verify_strict_receipt(&receipt, &root, &expected),
            |reason| matches!(reason, NoResultReason::StrictModeNotRequested),
        );

        let mut lossy_transport = read_receipt(&path);
        lossy_transport.identity.selected_backend = "sabre".to_owned();
        lossy_transport.identity.runtime_backend = "sabre".to_owned();
        let mut lossy_expected = expected.clone();
        lossy_expected.selected_backend = "sabre".to_owned();
        lossy_expected.runtime_backend = "sabre".to_owned();
        assert_no_result_reason(
            verify_strict_receipt(&lossy_transport, &root, &lossy_expected),
            |reason| {
                matches!(
                    reason,
                    NoResultReason::LosslessEventTransportOrderUnavailable { .. }
                )
            },
        );
    }

    #[test]
    fn numeric_hex_path_order_stderr_and_termination_drift_are_detected() {
        let baseline = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        for (name, changed) in [
            (
                "numeric",
                String::from_utf8(baseline.clone())
                    .unwrap()
                    .replace("count=17", "count=18")
                    .into_bytes(),
            ),
            (
                "hex",
                String::from_utf8(baseline.clone())
                    .unwrap()
                    .replace("ptr=0x2", "ptr=0x6")
                    .into_bytes(),
            ),
            (
                "path",
                String::from_utf8(baseline.clone())
                    .unwrap()
                    .replace("/tmp/exact-a", "/tmp/exact-b")
                    .into_bytes(),
            ),
            ("order", {
                let text = String::from_utf8(baseline.clone()).unwrap();
                let mut lines: Vec<&str> = text.lines().collect();
                // Swap two one-line DETLOG frames while leaving the scheduler
                // COMMIT's multiline frame structurally intact.
                lines.swap(0, 4);
                format!("{}\n", lines.join("\n")).into_bytes()
            }),
        ] {
            let directory = TempDir::new().unwrap();
            let (_, decision) = publish(
                &directory,
                &baseline,
                &changed,
                b"",
                b"",
                b"",
                b"",
                TypedTermination::Exited { code: 0 },
                TypedTermination::Exited { code: 0 },
                false,
                false,
            );
            assert_eq!(
                decision,
                StrictReceiptDecision::Diverged {
                    dimensions: vec![DivergenceDimension::OrderedInfo]
                },
                "{name} drift was normalized away"
            );
        }

        let directory = TempDir::new().unwrap();
        let (_, decision) = publish(
            &directory,
            &baseline,
            &baseline,
            b"stdout-left",
            b"stdout-right",
            b"left",
            b"right",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Signaled {
                signal: 9,
                core_dumped: false,
            },
            false,
            false,
        );
        assert_eq!(
            decision,
            StrictReceiptDecision::Diverged {
                dimensions: vec![
                    DivergenceDimension::Stdout,
                    DivergenceDimension::Stderr,
                    DivergenceDimension::Termination
                ]
            }
        );
    }

    #[test]
    fn requested_heap_and_stack_coverage_is_fail_closed() {
        let directory = TempDir::new().unwrap();
        let log = no_memory_log("2026-08-06T01:00:00.000001Z");
        let (_, decision) = publish(
            &directory,
            &log,
            &log,
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            true,
            true,
        );
        assert_no_result_reason(
            decision,
            |reason| matches!(reason, NoResultReason::MissingMessageClass { class, .. } if class == "detlog_heap"),
        );

        let stack_directory = TempDir::new().unwrap();
        let (_, stack_decision) = publish(
            &stack_directory,
            &log,
            &log,
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            true,
        );
        assert_no_result_reason(
            stack_decision,
            |reason| matches!(reason, NoResultReason::MissingMessageClass { class, .. } if class == "detlog_stack"),
        );
    }

    #[test]
    fn missing_and_tampered_blobs_are_no_results() {
        let directory = TempDir::new().unwrap();
        let log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let (path, _) = publish(
            &directory,
            &log,
            &log,
            b"unique-stdout",
            b"unique-stdout",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        let root = artifact_root_for_receipt(&path);
        let receipt = read_receipt(&path);
        let expected = expectation(&receipt);

        for class in ["info", "commit", "detlog", "detlog_heap", "detlog_stack"] {
            for tamper_digest in [false, true] {
                let mut tampered = receipt.clone();
                let evidence = match class {
                    "info" => &mut tampered.runs[0].messages.info,
                    "commit" => &mut tampered.runs[0].messages.commit,
                    "detlog" => &mut tampered.runs[0].messages.detlog,
                    "detlog_heap" => &mut tampered.runs[0].messages.detlog_heap,
                    "detlog_stack" => &mut tampered.runs[0].messages.detlog_stack,
                    _ => unreachable!(),
                };
                if tamper_digest {
                    evidence.framed_sha256 = "f".repeat(64);
                } else {
                    evidence.count += 1;
                }
                assert_no_result_reason(
                    verify_strict_receipt(&tampered, &root, &expected),
                    |reason| matches!(reason, NoResultReason::MessageEvidenceMismatch { side } if side == "left"),
                );
            }
        }

        let stdout_path = root.join("sha256").join(&receipt.runs[0].stdout.sha256);
        fs::write(&stdout_path, b"tampered").unwrap();
        assert_no_result_reason(
            verify_strict_receipt(&receipt, &root, &expected),
            |reason| {
                matches!(
                    reason,
                    NoResultReason::ArtifactLengthMismatch { .. }
                        | NoResultReason::ArtifactDigestMismatch { .. }
                )
            },
        );

        fs::remove_file(&stdout_path).unwrap();
        assert_no_result_reason(
            verify_strict_receipt(&receipt, &root, &expected),
            |reason| matches!(reason, NoResultReason::MissingArtifact { .. }),
        );
    }

    #[test]
    fn stale_schema_unknown_source_and_cached_decision_are_refused() {
        let directory = TempDir::new().unwrap();
        let log = base_log("2026-08-06T01:00:00.000001Z", "0xaaaa");
        let (path, _) = publish(
            &directory,
            &log,
            &log,
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            false,
            false,
        );
        let root = artifact_root_for_receipt(&path);
        let receipt = read_receipt(&path);
        let expected = expectation(&receipt);

        let mut stale = receipt.clone();
        stale.schema = "hermit.strict-verify-receipt/v0".to_owned();
        assert_no_result_reason(verify_strict_receipt(&stale, &root, &expected), |reason| {
            matches!(reason, NoResultReason::UnknownSchema { .. })
        });

        let mut dirty = receipt.clone();
        dirty.identity.source_revision = "unknown".to_owned();
        let mut dirty_expected = expected.clone();
        dirty_expected.source_revision = dirty.identity.source_revision.clone();
        assert_no_result_reason(
            verify_strict_receipt(&dirty, &root, &dirty_expected),
            |reason| matches!(reason, NoResultReason::InvalidSourceRevision { .. }),
        );

        let mut dirty_sha = receipt.clone();
        dirty_sha.identity.source_revision = format!("{SOURCE_REVISION}-dirty");
        let mut dirty_sha_expected = expected.clone();
        dirty_sha_expected.source_revision = dirty_sha.identity.source_revision.clone();
        assert_no_result_reason(
            verify_strict_receipt(&dirty_sha, &root, &dirty_sha_expected),
            |reason| matches!(reason, NoResultReason::InvalidSourceRevision { .. }),
        );

        let mut cached = receipt.clone();
        cached.declared_decision = StrictReceiptDecision::Diverged {
            dimensions: vec![DivergenceDimension::Stdout],
        };
        assert!(verify_strict_receipt(&cached, &root, &expected).is_qualified());
        write_receipt_atomically(&path, &cached).unwrap();
        assert_no_result_reason(load_and_verify_strict_receipt(&path, &expected), |reason| {
            matches!(reason, NoResultReason::ReceiptDecisionMismatch)
        });
    }

    #[test]
    fn arbitrary_info_payloads_cannot_spoof_typed_event_coverage() {
        let timestamp = "2026-08-06T01:00:00.000001Z";
        let spoof = format!(
            "{timestamp} INFO detcore: ordinary diagnostic containing COMMIT turn 1 and DETLOG [memory] [heap]->abc [stack]->def\n\
             {timestamp} INFO guest::diagnostic: DETLOG [syscall] and COMMIT turn 2\n\
             {timestamp} INFO detcore::other: DETLOG [memory] [heap]->abc [stack]->def\n"
        );
        let derived = canonical_info_stream(spoof.as_bytes()).unwrap();
        assert_eq!(derived.evidence.info.count, 3);
        assert_eq!(derived.evidence.commit.count, 0);
        assert_eq!(derived.evidence.detlog.count, 1);
        assert_eq!(derived.evidence.detlog_heap.count, 0);
        assert_eq!(derived.evidence.detlog_stack.count, 0);

        let directory = TempDir::new().unwrap();
        let (_, decision) = publish(
            &directory,
            spoof.as_bytes(),
            spoof.as_bytes(),
            b"",
            b"",
            b"",
            b"",
            TypedTermination::Exited { code: 0 },
            TypedTermination::Exited { code: 0 },
            true,
            true,
        );
        assert_no_result_reason(
            decision,
            |reason| matches!(reason, NoResultReason::MissingMessageClass { class, .. } if class == "commit"),
        );
    }
}
