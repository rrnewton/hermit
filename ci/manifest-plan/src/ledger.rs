//! Validation ledger schema emitted by Hermit's validation driver and consumed
//! by ci-hub.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

use crate::runner::FailureClass;

/// The stable fields emitted by `validate/aggregate.py --json` and JSONL stores.
/// Optional fields reflect honest reconstructed rows where a measurement was not
/// available; unrecognized fields are retained for forward compatibility.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HistoryRow {
    #[serde(default)]
    pub schema_version: Option<u32>,
    /// Stable validation-run identity. Rows whose schema supports cell results
    /// require this separately from `record_id`, because finalization mints a
    /// correction record while preserving the run and its per-cell evidence.
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub slot: Option<String>,
    /// Product the run validated: "hermit" or "reverie". Absent on pre-`repo`
    /// hermit ledger rows (aggregate.py defaults those to "hermit").
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub selection_mode: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub commit_anchored: Option<bool>,
    #[serde(default)]
    pub tree_dirty: Option<bool>,
    #[serde(default)]
    pub result: Option<String>,
    /// Process exit from the validation driver. A signal/cancellation exit can
    /// truncate a run after only passing gates; it is not a product failure.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Tests EXECUTED per the run's structured producer records. `None` =
    /// unknown (no usable record), distinct from `Some(0)` = a demonstrably
    /// inert green (a `--features`-gated build that compiled the tests out). A
    /// green carries this so a reader — and the landing predicate — can tell a
    /// real pass from a no-result wearing a success badge.
    #[serde(default)]
    pub executed_tests: Option<i64>,
    /// Tests that passed according to the runner-owned structured result.
    /// This field is omitted when absent so retained receipt bytes and digests
    /// stay unchanged. A row without the field remains readable as `None` and
    /// gains no value reconstructed from presentation text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passed_tests: Option<i64>,
    /// Tests FILTERED OUT by the run's selection. `Some(0)` alongside
    /// `executed_tests == Some(0)` is an empty target; `Some(n>0)` is a filtered
    /// subset (the `1 passed; 154 filtered out` narrowed-scope trap).
    #[serde(default)]
    pub filtered_tests: Option<i64>,
    /// Per-DAG-node test-coverage obligation outcome, written by the producer
    /// (Hermit's Rust driver) from structured per-node counts and terminal outcomes. Carries
    /// the CONDITION with the value (Proxy Binding): the NAMES of any inert or
    /// absent nodes travel in the receipt, so the landing predicate decides from
    /// receipt fields alone and never re-reads a log. `None` on a pre-coverage
    /// receipt; a count-capable receipt (schema >= COUNTS_SCHEMA) that omits it is
    /// a writer defect. Supersedes the blunt `filtered_tests == 0` predicate.
    #[serde(default)]
    pub coverage: Option<CoverageRow>,
    /// Whether the run covered the FULL profile (`level == "full"`), not a
    /// partial `*-only` profile whose pass reads identically to a full green.
    #[serde(default)]
    pub full_coverage: Option<bool>,
    /// Historical gate-record count. A scheduler record may carry
    /// `execution = "unknown"`; this field therefore must not be presented as
    /// a count of nodes whose child process executed. Use [`Self::executed_nodes`]
    /// for that measurement on current rows.
    #[serde(default)]
    pub checks: Option<u64>,
    /// Explicit completed/expected outer-gate counts. `checks` is retained for
    /// old rows; `gates_run` counts entries in `gates`, including an explicit
    /// unknown-execution row, so completeness is observable rather than inferred
    /// from a profile name. It is not an executed-node count.
    #[serde(default)]
    pub gates_run: Option<u64>,
    #[serde(default)]
    pub gates_expected: Option<u64>,
    #[serde(default)]
    pub failures: Option<u64>,
    /// `-j` passed to the CI DAG lane, and the number of other validates
    /// observed concurrently. A failure without these conditions cannot be
    /// promoted to a durable known-bad verdict.
    #[serde(default)]
    pub dag_jobs: Option<u64>,
    #[serde(default)]
    pub concurrent_validates: Option<u64>,
    /// The failed cell was in the measured-flaky registry at run time. Such a
    /// red needs a solo `-j 4` confirmation before it is durable.
    #[serde(default)]
    pub known_flaky_failure: Option<bool>,
    /// This row is the required solo `-j 4` confirmation of an earlier
    /// contended or known-flaky red.
    #[serde(default)]
    pub solo_rerun_confirmation: Option<bool>,
    #[serde(default)]
    pub real_seconds: Option<f64>,
    #[serde(default)]
    pub user_seconds: Option<f64>,
    #[serde(default)]
    pub sys_seconds: Option<f64>,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    /// Receipt-bound per-cell comparison evidence. Historical rows predate this
    /// field and remain readable as `None`. Keep an unrecognized newer value as
    /// JSON so the outer `schema_version` can be consulted before deciding which
    /// typed shape applies; supported schemas still require a complete value.
    #[serde(default)]
    pub cell_results: Option<CellResultsValue>,
    /// Schema-9 producer-owned terminal test results, carried forward by the
    /// cumulative schema 10. Keep the raw value until the enclosing row version
    /// is known so schemas 6, 7, 8, and future rows retain their established
    /// serialization and receive no accidental authority from a shape that
    /// happens to parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_results: Option<TestResultsValue>,
    #[serde(default)]
    pub gates: Vec<GateHistoryRow>,
    /// `tree` deliberately remains in this map even though it has a shared
    /// accessor below. Receipt identity uses
    /// `serde_json::to_vec(HistoryRow)-v1`, whose bytes depend on struct-field
    /// order followed by this `BTreeMap`'s key order. Moving `tree` into an
    /// ordinary struct field would change existing receipt digests without
    /// changing their meaning.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl HistoryRow {
    fn schema10_evidence(&self) -> Result<(CellResultsEvidenceV10, TestResultsEvidenceV9), String> {
        let cell_results = self
            .cell_results
            .as_ref()
            .ok_or_else(|| "schema 10 row omitted cell_results".to_string())?
            .schema10()?;
        cell_results.validate_for_row(self)?;
        let test_results = self
            .test_results
            .as_ref()
            .ok_or_else(|| "schema 10 row omitted test_results".to_string())?
            .schema9()?;
        test_results.validate_for_row(self)?;
        Ok((cell_results, test_results))
    }

    /// Return the Git tree recorded for this validation row.
    ///
    /// Absence is valid for historical rows. A present value is either one
    /// forty-hex Git object ID or a malformed row; callers must not turn the
    /// malformed case into absence.
    pub fn tree(&self) -> Result<Option<&str>, &'static str> {
        let Some(value) = self.extra.get("tree") else {
            return Ok(None);
        };
        let Some(tree) = value.as_str() else {
            return Err("malformed HistoryRow tree: expected a string");
        };
        if tree.len() != 40 || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("malformed HistoryRow tree: expected exactly 40 hexadecimal characters");
        }
        Ok(Some(tree))
    }

    /// Return how many retry rounds the validation runner executed.
    ///
    /// The field deliberately remains in [`Self::extra`], like `tree`, because
    /// moving an extension into the ordered struct fields would change the
    /// canonical bytes and receipt digest of every retained row. Historical
    /// `env_block_retries` rows are not treated as this value: that older name
    /// described a narrower population, while current retries include every
    /// retry-eligible failure.
    pub fn retry_rounds(&self) -> Result<Option<u64>, &'static str> {
        let Some(value) = self.extra.get("retry_rounds") else {
            return Ok(None);
        };
        value
            .as_u64()
            .map(Some)
            .ok_or("malformed HistoryRow retry_rounds: expected a nonnegative integer")
    }

    /// Decode cell-results evidence only after the enclosing schema version is
    /// known. Schema 6 and 7 retain their historical type; schema 8 introduced
    /// the exact versioned shape that schema 9 carries forward unchanged while
    /// adding separate test-results evidence. Schema 10 adds a receipt-bound
    /// retained verify-log index. Newer schemas remain readable but receive no
    /// typed evidence authority.
    pub fn cell_results_evidence(&self) -> Option<Cow<'_, CellResultsEvidence>> {
        let value = self.cell_results.as_ref()?;
        match self.schema_version? {
            6 | 7 => value.typed().map(Cow::Borrowed),
            8 | 9 => value
                .schema8()
                .map(|evidence| Cow::Owned(evidence.into_evidence())),
            10 => self
                .schema10_evidence()
                .ok()
                .map(|(evidence, _)| Cow::Owned(evidence.into_evidence())),
            _ => None,
        }
    }

    /// Return the recorded validation path for schemas 8 through 10.
    pub fn cell_results_validate_path(&self) -> Option<ValidatePath> {
        let value = self.cell_results.as_ref()?;
        match self.schema_version? {
            8 | 9 => value.schema8().map(|evidence| evidence.path),
            10 => self
                .schema10_evidence()
                .ok()
                .map(|(evidence, _)| evidence.path),
            _ => None,
        }
    }

    /// Return the receipt-bound retained verify-log index for schema 10.
    pub fn retained_verify_logs_artifact(
        &self,
    ) -> Result<Option<RetainedVerifyLogsArtifact>, String> {
        if self.schema_version != Some(10) {
            return Ok(None);
        }
        let (evidence, _) = self.schema10_evidence()?;
        Ok(Some(evidence.retained_verify_logs))
    }

    /// Return the exact pinned-root image digest carried with schema-10
    /// retained verify logs.
    pub fn retained_verify_logs_hermetic_image_digest(&self) -> Result<Option<String>, String> {
        if self.schema_version != Some(10) {
            return Ok(None);
        }
        let (evidence, _) = self.schema10_evidence()?;
        Ok(Some(evidence.hermetic_image_digest))
    }

    /// Decode and validate producer-owned per-test evidence for schema 9 and
    /// its cumulative schema-10 extension.
    ///
    /// Artifact bytes are verified by the receipt consumer. This reader proves
    /// everything available from the ledger row itself: exact row identity,
    /// validation path, selected producer population, canonical summaries,
    /// checked totals, and the content-addressed artifact reference.
    pub fn test_results_evidence(&self) -> Result<Option<TestResultsEvidenceV9>, String> {
        if self.schema_version == Some(10) {
            return self.schema10_evidence().map(|(_, evidence)| Some(evidence));
        }
        if self.schema_version != Some(9) {
            return Ok(None);
        }
        let value = self
            .test_results
            .as_ref()
            .ok_or_else(|| "schema 9 row omitted test_results".to_string())?;
        let evidence = value.schema9()?;
        evidence.validate_for_row(self)?;
        Ok(Some(evidence))
    }

    /// Return the number of nodes for which at least one child execution
    /// produced an exit status.
    ///
    /// This stays in [`Self::extra`] to preserve existing receipt bytes. Missing
    /// means older evidence did not carry the measurement; it is not zero.
    pub fn executed_nodes(&self) -> Result<Option<u64>, &'static str> {
        let Some(value) = self.extra.get("executed_nodes") else {
            return Ok(None);
        };
        value
            .as_u64()
            .map(Some)
            .ok_or("malformed HistoryRow executed_nodes: expected a nonnegative integer")
    }
}

/// Per-DAG-node test-coverage obligation outcome. Current producers compute this
/// from dagrun's structured per-step test-count files and terminal outcomes,
/// crossed against the PLANNED set of `test.*` DAG nodes in the lane manifests.
/// Historical rows were reconstructed from human-readable banners. The
/// obligation is SATISFIED iff
/// `planned_test_nodes > 0` and both failure lists were reported and empty.
/// This is the per-node replacement for the blunt aggregate `filtered_tests == 0`
/// predicate, which could not distinguish a full run's legitimate cross-shard
/// filtering (~693 tests) from a narrowed-subset masquerade.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct CoverageRow {
    /// Test-bearing DAG nodes the run PLANNED (manifest `test.*` steps for the
    /// lanes actually run). `0` = the producer could not determine a planned set;
    /// never treated as a satisfied obligation.
    #[serde(default)]
    pub planned_test_nodes: u64,
    /// Planned test nodes that supplied a positive structured executed-test
    /// count. Diagnostic.
    #[serde(default)]
    pub executed_test_nodes: u64,
    /// NAMES of planned test nodes that supplied a structured zero executed-test
    /// count — an inert green (every crate filtered-to-empty or compiled-out).
    ///
    /// `None` = THE PRODUCER DID NOT REPORT THIS LIST, which is not the same as
    /// reporting an empty one. It must never satisfy the obligation: an absent
    /// list is unknown, and unknown is refused. This was `Vec<String>` with
    /// `#[serde(default)]`, so a receipt that simply omitted the field
    /// deserialized to `[]` and read as "no inert nodes" — a pass. Python's
    /// `cov.get("zero_executed_nodes") == []` is `False` for a missing key and
    /// therefore already refused it, so the two verifiers disagreed on exactly
    /// this input, with Rust the permissive one.
    #[serde(default)]
    pub zero_executed_nodes: Option<Vec<String>>,
    /// NAMES of planned test nodes that produced no usable structured count —
    /// never ran, were aborted, or ran without the required producer record.
    ///
    /// `None` = not reported; see [`Self::zero_executed_nodes`]. Refused, never
    /// treated as "no absent nodes".
    #[serde(default)]
    pub absent_nodes: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct GateHistoryRow {
    pub name: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub real_seconds: Option<f64>,
    /// For a test function extracted from a retained DAG log, the manifest node
    /// which emitted it. Ledger-native outer gates leave this unset.
    #[serde(default)]
    pub source_node: Option<String>,
    /// `outer_gate` when the named gate itself failed; `lane_substep` when the
    /// outer gate merely carried a failing DAG node.
    #[serde(default)]
    pub failure_origin: Option<String>,
    /// Canonical DAG node names for a lane-carried failure.
    ///
    /// Missing means the producer did not bind substep evidence. `Some([])` is
    /// a positive assertion that an `outer_gate` failure has no failing child
    /// nodes, while `Some(nonempty)` is required for `lane_substep`. A present
    /// JSON `null` is malformed rather than another spelling of missing.
    #[serde(
        default,
        deserialize_with = "deserialize_present_failed_substeps",
        skip_serializing_if = "Option::is_none"
    )]
    pub failed_substeps: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl GateHistoryRow {
    /// Return the producer's terminal failure attribution for this gate.
    ///
    /// The value stays in `extra` so retained receipt bytes do not change merely
    /// because the shared reader learned its meaning. Missing is valid for
    /// historical rows. An unknown present value is malformed, not absence; a
    /// future schema therefore remains deserializable and earns no authority
    /// until its failure class is understood.
    pub fn failure_class(&self) -> Result<Option<FailureClass>, String> {
        let Some(value) = self.extra.get("failure_class") else {
            return Ok(None);
        };
        let Some(value) = value.as_str() else {
            return Err("malformed GateHistoryRow failure_class: expected a string".into());
        };
        FailureClass::parse(value)
            .map(Some)
            .map_err(|error| format!("malformed GateHistoryRow failure_class: {error}"))
    }
}

fn deserialize_present_failed_substeps<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Vec::<String>::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonStrictness {
    Stripped,
    Canonical,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComparedLogScope {
    Deterministic,
    Info,
    FullTrace,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ComparisonTier {
    CanonicalBitwise,
    ExitAndStreamEquality,
    ExecutionOnlySelfConsistent,
    DeclaredButUnverifiable,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparisonSpec {
    pub strictness: ComparisonStrictness,
    // Added by the current Hermit verifier; absent on older schema-6 rows.
    #[serde(default)]
    pub display_name: Option<String>,
    pub compare_logs: bool,
    // Added by the current Hermit verifier; absent on older schema-6 rows.
    #[serde(default)]
    pub compare_io_buffers: Option<bool>,
    // Current canonical reports bind the complete record envelope.
    #[serde(default)]
    pub record_envelope: Option<String>,
    // A match without virtual time is replay evidence, not a determinism result.
    #[serde(default)]
    pub virtualize_time: Option<bool>,
    pub log_scope: ComparedLogScope,
    pub strip_lines: bool,
    pub canonicalize_addresses: bool,
    pub full_trace: bool,
    pub exact_remainder: bool,
    pub stripped_prefixes: Vec<String>,
    pub canonicalizations: Vec<String>,
    pub ignore_lines: bool,
    pub skip_commit: bool,
    pub skip_detlog: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ComparedLogCounts {
    pub left: u64,
    pub right: u64,
}

/// A required key whose JSON value may be null. A plain `Option<T>` would make
/// an omitted key deserialize exactly like an explicit null, recreating the
/// evidence collapse this schema exists to prevent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RequiredNullable<T> {
    Null,
    Value(T),
}

impl ComparisonSpec {
    /// Whether this is the complete canonical BitwiseInfoV1 comparison used by
    /// a `canonical-bitwise` cell verdict.
    ///
    /// Keep this check with the shared wire type. The validation producer and
    /// receipt readers must not maintain separate lists of comparison fields
    /// and then disagree about whether a cell carries usable evidence.
    pub fn is_canonical_bitwise_info_v1(
        &self,
        compared_log_messages: &RequiredNullable<ComparedLogCounts>,
    ) -> bool {
        let RequiredNullable::Value(counts) = compared_log_messages else {
            return false;
        };
        self.strictness == ComparisonStrictness::Canonical
            && self.display_name.as_deref() == Some("BitwiseInfoV1")
            && self.compare_logs
            && self.compare_io_buffers == Some(true)
            && self.record_envelope.as_deref() == Some("all_records_v1")
            && self.virtualize_time == Some(true)
            && self.log_scope == ComparedLogScope::Info
            && !self.strip_lines
            && self.canonicalize_addresses
            && self.full_trace
            && self.exact_remainder
            && self.stripped_prefixes == ["real-wall-clock-prefix/v1"]
            && self.canonicalizations == ["host-address-to-first-appearance-ordinal/v1"]
            && !self.ignore_lines
            && !self.skip_commit
            && !self.skip_detlog
            && counts.left > 0
            && counts.right > 0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CellVerdict {
    ComparedAndMatched {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpec,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    ComparedAndDiverged {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpec,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    PerformsNoComparisonByDesign {
        comparison_tier: ComparisonTier,
        reason: String,
    },
    UnavailableWithReason {
        comparison_tier: ComparisonTier,
        reason: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct CellIdentity {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResult {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
    pub cell_verdict: CellVerdict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsArtifact {
    /// Parent-root-relative JSONL path below `ignored/validate/artifacts/`.
    /// The validation checkout is disposable; binding there would leave a
    /// receipt whose evidence vanishes when the unit is removed.
    pub path: String,
    pub sha256: String,
    pub row_count: u64,
}

/// One gzip copied out of the harness result tree into retained validation
/// artifacts. The path is relative to the dev-hermit state root, matching the
/// existing [`CellResultsArtifact`] contract.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetainedVerifyLogArtifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

/// One row in the retained verify-log index.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetainedVerifyLogIndexRow {
    pub cell: CellIdentity,
    pub attempt: u64,
    pub retained_verify_log: crate::runner::RetainedVerifyLog,
    pub artifact: RetainedVerifyLogArtifact,
}

/// Receipt binding for the retained verify-log index and all gzip bytes it
/// names. The index rows carry each gzip's own digest and size; this aggregate
/// records the bounded total copied for the validation run.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetainedVerifyLogsArtifact {
    pub path: String,
    pub sha256: String,
    pub row_count: u64,
    pub compressed_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsEvidence {
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    /// SHA-256 over canonical JSON of the sorted `selected` identities.
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub selected: Vec<CellIdentity>,
    pub cells: Vec<CellResult>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ValidatePath {
    Quick,
    Full,
    Super,
}

impl ValidatePath {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Full => "full",
            Self::Super => "super",
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsEvidenceV8 {
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    /// SHA-256 over canonical JSON of the sorted `selected` identities.
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub selected: Vec<CellIdentity>,
    pub cells: Vec<CellResult>,
}

/// Schema-10 cell-result evidence. Schemas 8 and 9 already have exact reader
/// contracts, so retained verify-log publication uses the next unclaimed outer
/// version rather than changing either historical shape. The enclosing
/// schema-10 row also requires schema-9 `test_results`; schema 10 is cumulative,
/// not an alternative to that evidence.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CellResultsEvidenceV10 {
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    /// Exact image reference captured before the pinned-root validation run.
    pub hermetic_image_digest: String,
    pub selected_count: u64,
    pub recorded_count: u64,
    /// SHA-256 over canonical JSON of the sorted `selected` identities.
    pub population_sha256: String,
    pub artifact: CellResultsArtifact,
    pub retained_verify_logs: RetainedVerifyLogsArtifact,
    pub selected: Vec<CellIdentity>,
    pub cells: Vec<CellResult>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComparisonSpecV8 {
    strictness: ComparisonStrictness,
    display_name: RequiredNullable<String>,
    compare_logs: bool,
    compare_io_buffers: RequiredNullable<bool>,
    record_envelope: RequiredNullable<String>,
    virtualize_time: RequiredNullable<bool>,
    log_scope: ComparedLogScope,
    strip_lines: bool,
    canonicalize_addresses: bool,
    full_trace: bool,
    exact_remainder: bool,
    stripped_prefixes: Vec<String>,
    canonicalizations: Vec<String>,
    ignore_lines: bool,
    skip_commit: bool,
    skip_detlog: bool,
}

fn required_nullable_into_option<T>(value: RequiredNullable<T>) -> Option<T> {
    match value {
        RequiredNullable::Null => None,
        RequiredNullable::Value(value) => Some(value),
    }
}

impl From<ComparisonSpecV8> for ComparisonSpec {
    fn from(value: ComparisonSpecV8) -> Self {
        Self {
            strictness: value.strictness,
            display_name: required_nullable_into_option(value.display_name),
            compare_logs: value.compare_logs,
            compare_io_buffers: required_nullable_into_option(value.compare_io_buffers),
            record_envelope: required_nullable_into_option(value.record_envelope),
            virtualize_time: required_nullable_into_option(value.virtualize_time),
            log_scope: value.log_scope,
            strip_lines: value.strip_lines,
            canonicalize_addresses: value.canonicalize_addresses,
            full_trace: value.full_trace,
            exact_remainder: value.exact_remainder,
            stripped_prefixes: value.stripped_prefixes,
            canonicalizations: value.canonicalizations,
            ignore_lines: value.ignore_lines,
            skip_commit: value.skip_commit,
            skip_detlog: value.skip_detlog,
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
enum CellVerdictV8 {
    ComparedAndMatched {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpecV8,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    ComparedAndDiverged {
        comparison_tier: ComparisonTier,
        comparison: ComparisonSpecV8,
        bitwise_parity: bool,
        compared_log_messages: RequiredNullable<ComparedLogCounts>,
    },
    PerformsNoComparisonByDesign {
        comparison_tier: ComparisonTier,
        reason: String,
    },
    UnavailableWithReason {
        comparison_tier: ComparisonTier,
        reason: String,
    },
}

impl From<CellVerdictV8> for CellVerdict {
    fn from(value: CellVerdictV8) -> Self {
        match value {
            CellVerdictV8::ComparedAndMatched {
                comparison_tier,
                comparison,
                bitwise_parity,
                compared_log_messages,
            } => Self::ComparedAndMatched {
                comparison_tier,
                comparison: comparison.into(),
                bitwise_parity,
                compared_log_messages,
            },
            CellVerdictV8::ComparedAndDiverged {
                comparison_tier,
                comparison,
                bitwise_parity,
                compared_log_messages,
            } => Self::ComparedAndDiverged {
                comparison_tier,
                comparison: comparison.into(),
                bitwise_parity,
                compared_log_messages,
            },
            CellVerdictV8::PerformsNoComparisonByDesign {
                comparison_tier,
                reason,
            } => Self::PerformsNoComparisonByDesign {
                comparison_tier,
                reason,
            },
            CellVerdictV8::UnavailableWithReason {
                comparison_tier,
                reason,
            } => Self::UnavailableWithReason {
                comparison_tier,
                reason,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResultV8 {
    lane: String,
    category: String,
    test: String,
    mode: String,
    backend: String,
    cell_verdict: CellVerdictV8,
}

impl From<CellResultV8> for CellResult {
    fn from(value: CellResultV8) -> Self {
        Self {
            lane: value.lane,
            category: value.category,
            test: value.test,
            mode: value.mode,
            backend: value.backend,
            cell_verdict: value.cell_verdict.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResultsEvidenceV8Wire {
    path: ValidatePath,
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    selected_count: u64,
    recorded_count: u64,
    population_sha256: String,
    artifact: CellResultsArtifact,
    selected: Vec<CellIdentity>,
    cells: Vec<CellResultV8>,
}

impl<'de> Deserialize<'de> for CellResultsEvidenceV8 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CellResultsEvidenceV8Wire::deserialize(deserializer)?;
        Ok(Self {
            path: value.path,
            run_id: value.run_id,
            hermit_sha: value.hermit_sha,
            source_tree_dirty: value.source_tree_dirty,
            selected_count: value.selected_count,
            recorded_count: value.recorded_count,
            population_sha256: value.population_sha256,
            artifact: value.artifact,
            selected: value.selected,
            cells: value.cells.into_iter().map(CellResult::from).collect(),
        })
    }
}

impl CellResultsEvidenceV8 {
    fn into_evidence(self) -> CellResultsEvidence {
        CellResultsEvidence {
            run_id: self.run_id,
            hermit_sha: self.hermit_sha,
            source_tree_dirty: self.source_tree_dirty,
            selected_count: self.selected_count,
            recorded_count: self.recorded_count,
            population_sha256: self.population_sha256,
            artifact: self.artifact,
            selected: self.selected,
            cells: self.cells,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CellResultsEvidenceV10Wire {
    path: ValidatePath,
    run_id: String,
    hermit_sha: String,
    source_tree_dirty: bool,
    hermetic_image_digest: String,
    selected_count: u64,
    recorded_count: u64,
    population_sha256: String,
    artifact: CellResultsArtifact,
    retained_verify_logs: RetainedVerifyLogsArtifact,
    selected: Vec<CellIdentity>,
    cells: Vec<CellResultV8>,
}

impl<'de> Deserialize<'de> for CellResultsEvidenceV10 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = CellResultsEvidenceV10Wire::deserialize(deserializer)?;
        Ok(Self {
            path: value.path,
            run_id: value.run_id,
            hermit_sha: value.hermit_sha,
            source_tree_dirty: value.source_tree_dirty,
            hermetic_image_digest: value.hermetic_image_digest,
            selected_count: value.selected_count,
            recorded_count: value.recorded_count,
            population_sha256: value.population_sha256,
            artifact: value.artifact,
            retained_verify_logs: value.retained_verify_logs,
            selected: value.selected,
            cells: value.cells.into_iter().map(CellResult::from).collect(),
        })
    }
}

impl CellResultsEvidenceV10 {
    fn into_evidence(self) -> CellResultsEvidence {
        CellResultsEvidence {
            run_id: self.run_id,
            hermit_sha: self.hermit_sha,
            source_tree_dirty: self.source_tree_dirty,
            selected_count: self.selected_count,
            recorded_count: self.recorded_count,
            population_sha256: self.population_sha256,
            artifact: self.artifact,
            selected: self.selected,
            cells: self.cells,
        }
    }

    fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        if row.repo.as_deref() != Some("hermit") {
            return Err("schema 10 cell_results is not attached to a hermit row".into());
        }
        if self.run_id.is_empty()
            || self.run_id == "."
            || self.run_id == ".."
            || self.run_id.contains('/')
            || self.run_id.contains('\\')
        {
            return Err("schema 10 cell_results has a malformed run_id".into());
        }
        if row.profile.as_deref() != Some(self.path.as_str()) {
            return Err("schema 10 cell_results path differs from row profile".into());
        }
        if row.run_id.as_deref() != Some(self.run_id.as_str()) {
            return Err("schema 10 cell_results run_id differs from row run_id".into());
        }
        if !is_lower_hex(&self.hermit_sha, 40)
            || row.commit.as_deref() != Some(self.hermit_sha.as_str())
        {
            return Err("schema 10 cell_results hermit_sha differs from row commit".into());
        }
        if self.source_tree_dirty
            || row.tree_dirty != Some(false)
            || row.commit_anchored != Some(true)
            || row.tree().map_err(str::to_string)?.is_none()
        {
            return Err("schema 10 cell_results source is not the exact clean row tree".into());
        }
        if !is_canonical_hermetic_image_digest(&self.hermetic_image_digest) {
            return Err("schema 10 cell_results hermetic_image_digest is malformed".into());
        }
        let selected_count = u64::try_from(self.selected.len())
            .map_err(|_| "schema 10 selected cell count does not fit u64")?;
        let recorded_count = u64::try_from(self.cells.len())
            .map_err(|_| "schema 10 recorded cell count does not fit u64")?;
        if selected_count == 0
            || self.selected_count != selected_count
            || self.recorded_count != recorded_count
            || !self.selected.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err("schema 10 cell_results counts or selected ordering mismatch".into());
        }
        let cell_identities = self
            .cells
            .iter()
            .map(|cell| CellIdentity {
                lane: cell.lane.clone(),
                category: cell.category.clone(),
                test: cell.test.clone(),
                mode: cell.mode.clone(),
                backend: cell.backend.clone(),
            })
            .collect::<Vec<_>>();
        if cell_identities != self.selected {
            return Err("schema 10 cell_results cells differ from selected population".into());
        }
        let population_bytes = serde_json::to_vec(&self.selected)
            .map_err(|error| format!("cannot encode schema 10 selected population: {error}"))?;
        let population_sha256 = format!("{:x}", Sha256::digest(population_bytes));
        if self.population_sha256 != population_sha256 {
            return Err("schema 10 cell_results population_sha256 mismatch".into());
        }
        let expected_cell_path = format!(
            "ignored/validate/artifacts/{}/cell-results.jsonl",
            self.run_id
        );
        if self.artifact.path != expected_cell_path
            || !is_lower_hex(&self.artifact.sha256, 64)
            || self.artifact.row_count != self.recorded_count
        {
            return Err("schema 10 cell_results artifact binding is malformed".into());
        }
        let expected_path = format!(
            "ignored/validate/artifacts/{}/verify-logs/index.jsonl",
            self.run_id
        );
        if self.retained_verify_logs.path != expected_path
            || !is_lower_hex(&self.retained_verify_logs.sha256, 64)
            || self.retained_verify_logs.row_count == 0
            || self.retained_verify_logs.compressed_bytes == 0
        {
            return Err("schema 10 retained verify-log index binding is malformed".into());
        }
        Ok(())
    }
}

/// A supported cell-results shape, or the exact JSON from a newer shape that
/// this reader does not understand yet. Exact versioned shapes precede the raw
/// fallback so supported rows retain typed access while unknown extensions stay
/// readable.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum CellResultsValue {
    Typed(CellResultsEvidence),
    Other(Value),
}

impl CellResultsValue {
    pub fn typed(&self) -> Option<&CellResultsEvidence> {
        match self {
            Self::Typed(evidence) => Some(evidence),
            Self::Other(_) => None,
        }
    }

    pub fn typed_mut(&mut self) -> Option<&mut CellResultsEvidence> {
        match self {
            Self::Typed(evidence) => Some(evidence),
            Self::Other(_) => None,
        }
    }

    fn schema8(&self) -> Option<CellResultsEvidenceV8> {
        match self {
            Self::Other(value) => serde_json::from_value(value.clone()).ok(),
            Self::Typed(_) => None,
        }
    }

    fn schema10(&self) -> Result<CellResultsEvidenceV10, String> {
        match self {
            Self::Other(value) => serde_json::from_value(value.clone())
                .map_err(|error| format!("schema 10 cell_results is malformed: {error}")),
            Self::Typed(_) => {
                Err("schema 10 cell_results used the historical schema-7 shape".into())
            }
        }
    }
}

/// Raw test-results evidence. The enclosing [`HistoryRow::schema_version`]
/// decides whether these bytes have a supported typed interpretation.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(transparent)]
pub struct TestResultsValue(Value);

impl TestResultsValue {
    fn schema9(&self) -> Result<TestResultsEvidenceV9, String> {
        serde_json::from_value(self.0.clone())
            .map_err(|error| format!("schema 9 test_results is malformed: {error}"))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TestResultVerdict {
    Pass,
    Fail,
}

/// One producer-owned terminal test row in the retained JSONL artifact.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultArtifactRow {
    pub run_id: String,
    pub hermit_sha: String,
    pub path: ValidatePath,
    pub producer: TestResultProducer,
    pub id: String,
    pub result: TestResultVerdict,
    pub attempts: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TestResultArtifactRowWire {
    run_id: String,
    hermit_sha: String,
    path: ValidatePath,
    producer: TestResultProducer,
    id: String,
    result: TestResultVerdict,
    attempts: u64,
}

impl<'de> Deserialize<'de> for TestResultArtifactRow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let value = TestResultArtifactRowWire::deserialize(deserializer)?;
        if !nonblank_component(&value.run_id)
            || !is_lower_hex(&value.hermit_sha, 40)
            || value.id.is_empty()
            || value.id.trim() != value.id
            || value.attempts == 0
        {
            return Err(D::Error::custom(
                "schema 9 test-result artifact row has malformed identity or attempts",
            ));
        }
        if let TestResultProducer::Node {
            node,
            outer_attempt,
        } = &value.producer
        {
            if !nonblank_component(node) || *outer_attempt == 0 {
                return Err(D::Error::custom(
                    "schema 9 test-result artifact row has malformed node or outer_attempt",
                ));
            }
        }
        Ok(Self {
            run_id: value.run_id,
            hermit_sha: value.hermit_sha,
            path: value.path,
            producer: value.producer,
            id: value.id,
            result: value.result,
            attempts: value.attempts,
        })
    }
}

/// The existing two sources of structured test results remain distinct.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum TestResultProducer {
    Node { node: String, outer_attempt: u64 },
    Compatibility,
}

/// Exact count-bearing population selected before outcomes are observed.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultsSelectedPopulation {
    pub nodes: Vec<String>,
    pub compatibility: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultTotals {
    pub executed_tests: u64,
    pub passed_tests: u64,
    pub failed_tests: u64,
    pub filtered_tests: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeTestResultSummary {
    pub node: String,
    pub outer_attempt: u64,
    pub totals: TestResultTotals,
    pub row_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompatibilityTestResultSummary {
    pub totals: TestResultTotals,
    pub row_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultsArtifact {
    /// Parent-root-relative producer path. Receipt publication replaces this
    /// local location with an immutable receipt-repository artifact binding.
    pub path: String,
    pub sha256: String,
    pub row_count: u64,
}

/// Schema-9 test-result contract, carried forward unchanged by schema 10.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TestResultsEvidenceV9 {
    pub path: ValidatePath,
    pub run_id: String,
    pub hermit_sha: String,
    pub source_tree_dirty: bool,
    pub selected_count: u64,
    pub recorded_count: u64,
    pub population_sha256: String,
    pub selected: TestResultsSelectedPopulation,
    pub nodes: Vec<NodeTestResultSummary>,
    pub compatibility: Option<CompatibilityTestResultSummary>,
    pub totals: TestResultTotals,
    pub artifact: TestResultsArtifact,
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Whether a pinned-root image reference has the exact form written by
/// `ci/hermetic/build-image.sh` and consumed by `run-in-pinned-root.sh`.
pub fn is_canonical_hermetic_image_digest(value: &str) -> bool {
    value
        .strip_prefix("localhost/hermit-hermetic-validate@sha256:")
        .is_some_and(|digest| is_lower_hex(digest, 64))
}

fn nonblank_component(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
}

fn checked_summary(summary: &TestResultTotals, row_count: u64) -> Result<(), String> {
    if summary
        .passed_tests
        .checked_add(summary.failed_tests)
        .ok_or_else(|| "test-results pass/fail total overflowed u64".to_string())?
        != summary.executed_tests
    {
        return Err("test-results pass/fail total differs from executed_tests".into());
    }
    if row_count != summary.executed_tests {
        return Err("test-results row_count differs from executed_tests".into());
    }
    Ok(())
}

impl TestResultsEvidenceV9 {
    fn validate_for_row(&self, row: &HistoryRow) -> Result<(), String> {
        if row.profile.as_deref() != Some(self.path.as_str()) {
            return Err("schema 9 test_results path differs from row profile".into());
        }
        if !nonblank_component(&self.run_id) || row.run_id.as_deref() != Some(self.run_id.as_str())
        {
            return Err("schema 9 test_results run_id differs from row run_id".into());
        }
        if !is_lower_hex(&self.hermit_sha, 40)
            || row.commit.as_deref() != Some(self.hermit_sha.as_str())
        {
            return Err("schema 9 test_results hermit_sha differs from row commit".into());
        }
        if self.source_tree_dirty || row.tree_dirty != Some(false) {
            return Err("schema 9 test_results source is not the exact clean row tree".into());
        }
        if !self
            .selected
            .nodes
            .iter()
            .all(|node| nonblank_component(node))
            || !self.selected.nodes.windows(2).all(|pair| pair[0] < pair[1])
        {
            return Err(
                "schema 9 test_results selected nodes are empty, untrimmed, duplicate, or unsorted"
                    .into(),
            );
        }
        let selected_count = u64::try_from(self.selected.nodes.len())
            .map_err(|_| "schema 9 selected node count does not fit u64")?
            .checked_add(u64::from(self.selected.compatibility))
            .ok_or("schema 9 selected producer count overflowed u64")?;
        if self.selected_count != selected_count {
            return Err("schema 9 test_results selected_count mismatch".into());
        }
        let selected_nodes = self
            .selected
            .nodes
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let summary_nodes = self
            .nodes
            .iter()
            .map(|summary| summary.node.as_str())
            .collect::<BTreeSet<_>>();
        if self.nodes.len() != summary_nodes.len()
            || !self
                .nodes
                .windows(2)
                .all(|pair| pair[0].node < pair[1].node)
            || selected_nodes != summary_nodes
        {
            return Err("schema 9 test_results node summaries differ from selected nodes".into());
        }
        if self.selected.compatibility != self.compatibility.is_some() {
            return Err(
                "schema 9 test_results compatibility summary differs from selection".into(),
            );
        }

        let mut totals = TestResultTotals {
            executed_tests: 0,
            passed_tests: 0,
            failed_tests: 0,
            filtered_tests: 0,
        };
        let mut recorded_count = 0u64;
        let mut add = |summary: &TestResultTotals, row_count: u64| -> Result<(), String> {
            checked_summary(summary, row_count)?;
            totals.executed_tests = totals
                .executed_tests
                .checked_add(summary.executed_tests)
                .ok_or("schema 9 executed_tests overflowed u64")?;
            totals.passed_tests = totals
                .passed_tests
                .checked_add(summary.passed_tests)
                .ok_or("schema 9 passed_tests overflowed u64")?;
            totals.failed_tests = totals
                .failed_tests
                .checked_add(summary.failed_tests)
                .ok_or("schema 9 failed_tests overflowed u64")?;
            totals.filtered_tests = totals
                .filtered_tests
                .checked_add(summary.filtered_tests)
                .ok_or("schema 9 filtered_tests overflowed u64")?;
            recorded_count = recorded_count
                .checked_add(row_count)
                .ok_or("schema 9 recorded_count overflowed u64")?;
            Ok(())
        };
        for summary in &self.nodes {
            if summary.outer_attempt == 0 {
                return Err(format!(
                    "schema 9 test_results node {} has zero outer_attempt",
                    summary.node
                ));
            }
            add(&summary.totals, summary.row_count)?;
        }
        if let Some(summary) = &self.compatibility {
            add(&summary.totals, summary.row_count)?;
        }
        if totals != self.totals
            || self.recorded_count != recorded_count
            || self.artifact.row_count != recorded_count
        {
            return Err("schema 9 test_results checked totals or row counts mismatch".into());
        }
        let executed = i64::try_from(totals.executed_tests)
            .map_err(|_| "schema 9 executed_tests does not fit ledger i64")?;
        let passed = i64::try_from(totals.passed_tests)
            .map_err(|_| "schema 9 passed_tests does not fit ledger i64")?;
        let filtered = i64::try_from(totals.filtered_tests)
            .map_err(|_| "schema 9 filtered_tests does not fit ledger i64")?;
        if row.executed_tests != Some(executed)
            || row.passed_tests != Some(passed)
            || row.filtered_tests != Some(filtered)
        {
            return Err("schema 9 test_results totals differ from HistoryRow totals".into());
        }

        let population_bytes = serde_json::to_vec(&self.selected)
            .map_err(|error| format!("cannot encode schema 9 selected population: {error}"))?;
        let population_sha256 = format!("{:x}", Sha256::digest(population_bytes));
        if !is_lower_hex(&self.population_sha256, 64) || self.population_sha256 != population_sha256
        {
            return Err("schema 9 test_results population_sha256 mismatch".into());
        }
        let expected_path = format!(
            "ignored/validate/artifacts/{}/test-results.jsonl",
            self.run_id
        );
        if self.artifact.path != expected_path || !is_lower_hex(&self.artifact.sha256, 64) {
            return Err("schema 9 test_results artifact binding is malformed".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use sha2::Digest;
    use sha2::Sha256;

    use super::*;

    #[test]
    fn history_schema_accepts_unmeasured_cpu() {
        let row: HistoryRow = serde_json::from_str(
            r#"{
                "schema_version":1,
                "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "result":"pass",
                "checks":36,
                "real_seconds":123,
                "user_seconds":null,
                "sys_seconds":null
            }"#,
        )
        .unwrap();
        assert_eq!(row.checks, Some(36));
        assert_eq!(row.real_seconds, Some(123.0));
        assert_eq!(row.user_seconds, None);
    }

    #[test]
    fn history_tree_distinguishes_absent_valid_and_malformed() {
        let absent: HistoryRow = serde_json::from_str(r#"{"schema_version":1}"#).unwrap();
        assert_eq!(absent.tree(), Ok(None));

        let valid: HistoryRow = serde_json::from_str(
            r#"{"schema_version":1,"tree":"0123456789abcdef0123456789ABCDEF01234567"}"#,
        )
        .unwrap();
        assert_eq!(
            valid.tree(),
            Ok(Some("0123456789abcdef0123456789ABCDEF01234567"))
        );

        let malformed_string: HistoryRow =
            serde_json::from_str(r#"{"schema_version":1,"tree":"unknown"}"#).unwrap();
        assert_eq!(
            malformed_string.tree(),
            Err("malformed HistoryRow tree: expected exactly 40 hexadecimal characters")
        );

        let malformed_type: HistoryRow =
            serde_json::from_str(r#"{"schema_version":1,"tree":7}"#).unwrap();
        assert_eq!(
            malformed_type.tree(),
            Err("malformed HistoryRow tree: expected a string")
        );
    }

    #[test]
    fn history_retry_rounds_distinguishes_absent_valid_and_malformed() {
        let absent: HistoryRow = serde_json::from_str(r#"{"schema_version":5}"#).unwrap();
        assert_eq!(absent.retry_rounds(), Ok(None));

        let valid: HistoryRow =
            serde_json::from_str(r#"{"schema_version":7,"retry_rounds":2}"#).unwrap();
        assert_eq!(valid.retry_rounds(), Ok(Some(2)));

        for malformed in [
            r#"{"schema_version":7,"retry_rounds":"2"}"#,
            r#"{"schema_version":7,"retry_rounds":-1}"#,
            r#"{"schema_version":7,"retry_rounds":1.5}"#,
        ] {
            let row: HistoryRow = serde_json::from_str(malformed).unwrap();
            assert_eq!(
                row.retry_rounds(),
                Err("malformed HistoryRow retry_rounds: expected a nonnegative integer")
            );
        }

        let historical: HistoryRow =
            serde_json::from_str(r#"{"schema_version":5,"env_block_retries":2}"#).unwrap();
        assert_eq!(historical.retry_rounds(), Ok(None));
    }

    #[test]
    fn history_executed_nodes_distinguishes_absent_valid_and_malformed() {
        let absent: HistoryRow = serde_json::from_str(r#"{"schema_version":5}"#).unwrap();
        assert_eq!(absent.executed_nodes(), Ok(None));

        let valid: HistoryRow =
            serde_json::from_str(r#"{"schema_version":7,"executed_nodes":12}"#).unwrap();
        assert_eq!(valid.executed_nodes(), Ok(Some(12)));

        for malformed in [
            r#"{"schema_version":7,"executed_nodes":"12"}"#,
            r#"{"schema_version":7,"executed_nodes":-1}"#,
            r#"{"schema_version":7,"executed_nodes":1.5}"#,
        ] {
            let row: HistoryRow = serde_json::from_str(malformed).unwrap();
            assert_eq!(
                row.executed_nodes(),
                Err("malformed HistoryRow executed_nodes: expected a nonnegative integer")
            );
        }
    }

    #[test]
    fn gate_failure_class_distinguishes_absent_valid_and_malformed() {
        let absent: GateHistoryRow = serde_json::from_str(r#"{"name":"test.fixture"}"#).unwrap();
        assert_eq!(absent.failure_class(), Ok(None));

        let valid: GateHistoryRow =
            serde_json::from_str(r#"{"name":"test.fixture","failure_class":"product_failure"}"#)
                .unwrap();
        assert_eq!(
            valid.failure_class(),
            Ok(Some(FailureClass::ProductFailure))
        );

        let malformed_type: GateHistoryRow =
            serde_json::from_str(r#"{"name":"test.fixture","failure_class":7}"#).unwrap();
        assert_eq!(
            malformed_type.failure_class(),
            Err("malformed GateHistoryRow failure_class: expected a string".into())
        );

        let unknown: GateHistoryRow =
            serde_json::from_str(r#"{"name":"test.fixture","failure_class":"future_failure"}"#)
                .unwrap();
        assert_eq!(
            unknown.failure_class(),
            Err(
                "malformed GateHistoryRow failure_class: unknown failure_class `future_failure`"
                    .into()
            )
        );
    }

    #[test]
    fn history_tree_accessor_preserves_receipt_canonicalization_v1() {
        let row: HistoryRow = serde_json::from_str(
            r#"{
                "schema_version":5,
                "commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "tree":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "tree_dirty":false,
                "result":"pass",
                "z_extension":2,
                "a_extension":1
            }"#,
        )
        .unwrap();

        let canonical = serde_json::to_vec(&row).unwrap();
        assert_eq!(
            canonical,
            br#"{"schema_version":5,"run_id":null,"started_at":null,"finished_at":null,"host":null,"slot":null,"repo":null,"cwd":null,"profile":null,"selection_mode":null,"commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit_anchored":null,"tree_dirty":false,"result":"pass","exit_code":null,"executed_tests":null,"filtered_tests":null,"coverage":null,"full_coverage":null,"checks":null,"gates_run":null,"gates_expected":null,"failures":null,"dag_jobs":null,"concurrent_validates":null,"known_flaky_failure":null,"solo_rerun_confirmation":null,"real_seconds":null,"user_seconds":null,"sys_seconds":null,"log_file":null,"source":null,"cell_results":null,"gates":[],"a_extension":1,"tree":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","z_extension":2}"#.as_slice()
        );
        let digest = format!("{:x}", Sha256::digest(&canonical));
        assert_eq!(
            digest,
            "e909dc57a7de484727d13d556895ad1afd547c956700987ab1945d2d5fb7bb5d"
        );
    }

    #[test]
    fn passed_tests_is_typed_without_changing_retained_receipt_bytes() {
        let retained: HistoryRow =
            serde_json::from_str(r#"{"schema_version":5,"executed_tests":7,"filtered_tests":2}"#)
                .unwrap();
        assert_eq!(retained.passed_tests, None);
        assert!(
            !serde_json::to_string(&retained)
                .unwrap()
                .contains("passed_tests")
        );

        let current: HistoryRow = serde_json::from_str(
            r#"{"schema_version":7,"executed_tests":7,"passed_tests":5,"filtered_tests":2}"#,
        )
        .unwrap();
        assert_eq!(current.passed_tests, Some(5));
        assert_eq!(
            serde_json::to_value(&current).unwrap()["passed_tests"],
            serde_json::json!(5)
        );
    }

    #[test]
    fn cell_verdict_schema_keeps_all_four_states_distinct() {
        let compared = r#"{
            "state":"compared-and-matched",
            "comparison_tier":"canonical-bitwise",
            "comparison":{
                "strictness":"canonical","compare_logs":true,"log_scope":"info",
                "strip_lines":false,"canonicalize_addresses":true,"full_trace":true,
                "exact_remainder":true,"stripped_prefixes":["real-wall-clock-prefix/v1"],
                "canonicalizations":["host-address-to-first-appearance-ordinal/v1"],
                "ignore_lines":false,"skip_commit":false,"skip_detlog":false
            },
            "bitwise_parity":true,
            "compared_log_messages":{"left":7,"right":7}
        }"#;
        assert!(matches!(
            serde_json::from_str::<CellVerdict>(compared).unwrap(),
            CellVerdict::ComparedAndMatched { .. }
        ));
        let current = compared.replace(
            "\"compare_logs\":true,",
            "\"display_name\":\"BitwiseInfoV1\",\"compare_logs\":true,\"compare_io_buffers\":true,\"record_envelope\":\"all_records_v1\",\"virtualize_time\":true,",
        );
        let CellVerdict::ComparedAndMatched {
            comparison,
            compared_log_messages,
            ..
        } = serde_json::from_str::<CellVerdict>(&current).unwrap()
        else {
            panic!("current verifier report changed verdict state");
        };
        assert_eq!(comparison.compare_io_buffers, Some(true));
        assert_eq!(comparison.display_name.as_deref(), Some("BitwiseInfoV1"));
        assert_eq!(
            comparison.record_envelope.as_deref(),
            Some("all_records_v1")
        );
        assert_eq!(comparison.virtualize_time, Some(true));
        assert!(comparison.is_canonical_bitwise_info_v1(&compared_log_messages));
        let CellVerdict::ComparedAndMatched {
            comparison: older_comparison,
            compared_log_messages: older_counts,
            ..
        } = serde_json::from_str::<CellVerdict>(compared).unwrap()
        else {
            panic!("older verifier report changed verdict state");
        };
        assert!(!older_comparison.is_canonical_bitwise_info_v1(&older_counts));
        for (state, expected) in [
            ("compared-and-diverged", "diverged"),
            ("performs-no-comparison-by-design", "by-design"),
            ("unavailable-with-reason", "unavailable"),
        ] {
            let value = if state == "compared-and-diverged" {
                compared
                    .replace("compared-and-matched", state)
                    .replace("\"bitwise_parity\":true", "\"bitwise_parity\":false")
            } else {
                format!(
                    r#"{{"state":"{state}","comparison_tier":"declared-but-unverifiable","reason":"fixture reason"}}"#
                )
            };
            let verdict: CellVerdict = serde_json::from_str(&value).unwrap();
            assert_eq!(
                match verdict {
                    CellVerdict::ComparedAndDiverged { .. } => "diverged",
                    CellVerdict::PerformsNoComparisonByDesign { .. } => "by-design",
                    CellVerdict::UnavailableWithReason { .. } => "unavailable",
                    CellVerdict::ComparedAndMatched { .. } => "matched",
                },
                expected
            );
        }
    }

    #[test]
    fn compared_log_messages_key_is_required_but_explicit_null_is_retained() {
        let base = r#"{
            "state":"compared-and-matched",
            "comparison_tier":"execution-only-self-consistent",
            "comparison":{
                "strictness":"stripped","compare_logs":false,"log_scope":"deterministic",
                "strip_lines":true,"canonicalize_addresses":false,"full_trace":false,
                "exact_remainder":false,"stripped_prefixes":[],"canonicalizations":[],
                "ignore_lines":false,"skip_commit":false,"skip_detlog":false
            },
            "bitwise_parity":false,
            "compared_log_messages":null
        }"#;
        assert!(serde_json::from_str::<CellVerdict>(base).is_ok());
        let mut missing: serde_json::Value = serde_json::from_str(base).unwrap();
        missing
            .as_object_mut()
            .unwrap()
            .remove("compared_log_messages");
        assert!(serde_json::from_value::<CellVerdict>(missing).is_err());

        for retired in [
            "full-stdout-info-stack-heap",
            "stdout-info-stack-heap-spot-check",
            "legacy-unqualified",
            "unqualified-no-comparison",
            "unqualified-stdout-only",
            "unqualified-self-verify-only",
            "unqualified-tool-count-only",
        ] {
            let old_tier = base.replace("execution-only-self-consistent", retired);
            assert!(serde_json::from_str::<CellVerdict>(&old_tier).is_err());
        }
    }

    #[test]
    fn cell_results_reads_supported_shape_first_and_preserves_its_serialization() {
        let json = serde_json::json!({
            "run_id": "run-1",
            "hermit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_tree_dirty": false,
            "selected_count": 1,
            "recorded_count": 1,
            "population_sha256": "b".repeat(64),
            "artifact": {
                "path": "ignored/validate/artifacts/run-1/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 1
            },
            "selected": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "hello",
                "mode": "verify",
                "backend": "ptrace"
            }],
            "cells": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "hello",
                "mode": "verify",
                "backend": "ptrace",
                "cell_verdict": {
                    "state": "unavailable-with-reason",
                    "comparison_tier": "declared-but-unverifiable",
                    "reason": "fixture"
                }
            }]
        });
        let evidence: CellResultsEvidence = serde_json::from_value(json.clone()).unwrap();
        let value: CellResultsValue = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(value, CellResultsValue::Typed(_)));
        assert_eq!(
            serde_json::to_vec(&value).unwrap(),
            serde_json::to_vec(&evidence).unwrap(),
            "the wrapper must not change canonical receipt bytes for supported rows"
        );

        let mut newer = json;
        newer["future_field"] = serde_json::json!(true);
        let value: CellResultsValue = serde_json::from_value(newer.clone()).unwrap();
        assert!(matches!(value, CellResultsValue::Other(_)));
        assert_eq!(serde_json::to_value(value).unwrap(), newer);
    }

    #[test]
    fn schema8_cell_results_has_an_exact_validate_path_shape() {
        let json = serde_json::json!({
            "path": "quick",
            "run_id": "run-8",
            "hermit_sha": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "source_tree_dirty": false,
            "selected_count": 0,
            "recorded_count": 0,
            "population_sha256": "b".repeat(64),
            "artifact": {
                "path": "ignored/validate/artifacts/run-8/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 0
            },
            "selected": [],
            "cells": []
        });
        let evidence: CellResultsEvidenceV8 = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(evidence.path, ValidatePath::Quick);
        let value: CellResultsValue = serde_json::from_value(json.clone()).unwrap();
        assert!(matches!(value, CellResultsValue::Other(_)));
        assert_eq!(serde_json::to_value(value).unwrap(), json);

        let row: HistoryRow = serde_json::from_value(serde_json::json!({
            "schema_version": 8,
            "cell_results": json,
        }))
        .unwrap();
        assert_eq!(row.cell_results_validate_path(), Some(ValidatePath::Quick));
        assert_eq!(row.cell_results_evidence().unwrap().selected_count, 0);

        let mut wrong_outer_version = serde_json::to_value(&row).unwrap();
        wrong_outer_version["schema_version"] = serde_json::json!(7);
        let wrong_outer_version: HistoryRow = serde_json::from_value(wrong_outer_version).unwrap();
        assert!(wrong_outer_version.cell_results_evidence().is_none());

        let mut schema9_outer_version = serde_json::to_value(&row).unwrap();
        schema9_outer_version["schema_version"] = serde_json::json!(9);
        let schema9_outer_version: HistoryRow =
            serde_json::from_value(schema9_outer_version).unwrap();
        assert_eq!(
            schema9_outer_version.cell_results_validate_path(),
            Some(ValidatePath::Quick)
        );
        assert!(schema9_outer_version.cell_results_evidence().is_some());

        let mut future_outer_version = serde_json::to_value(&row).unwrap();
        future_outer_version["schema_version"] = serde_json::json!(10);
        let future_outer_version: HistoryRow =
            serde_json::from_value(future_outer_version).unwrap();
        assert!(future_outer_version.cell_results_evidence().is_none());

        let mut wrong_path = serde_json::to_value(&row).unwrap();
        wrong_path["schema_version"] = serde_json::json!(8);
        wrong_path["cell_results"]["path"] = serde_json::json!("selective");
        let wrong_path: HistoryRow = serde_json::from_value(wrong_path).unwrap();
        assert!(wrong_path.cell_results_evidence().is_none());

        let mut extension = serde_json::to_value(&row).unwrap();
        extension["cell_results"]["future_field"] = serde_json::json!(true);
        let extension: HistoryRow = serde_json::from_value(extension).unwrap();
        assert!(extension.cell_results_evidence().is_none());
    }

    fn schema10_cell_results() -> Value {
        let selected = vec![CellIdentity {
            lane: "portable".into(),
            category: "c-programs".into(),
            test: "hello".into(),
            mode: "verify".into(),
            backend: "ptrace".into(),
        }];
        let population_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&selected).unwrap())
        );
        serde_json::json!({
            "path": "full",
            "run_id": "run-10",
            "hermit_sha": "a".repeat(40),
            "source_tree_dirty": false,
            "hermetic_image_digest": format!(
                "localhost/hermit-hermetic-validate@sha256:{}",
                "e".repeat(64)
            ),
            "selected_count": 1,
            "recorded_count": 1,
            "population_sha256": population_sha256,
            "artifact": {
                "path": "ignored/validate/artifacts/run-10/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 1
            },
            "retained_verify_logs": {
                "path": "ignored/validate/artifacts/run-10/verify-logs/index.jsonl",
                "sha256": "d".repeat(64),
                "row_count": 1,
                "compressed_bytes": 23
            },
            "selected": selected,
            "cells": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "hello",
                "mode": "verify",
                "backend": "ptrace",
                "cell_verdict": {
                    "state": "unavailable-with-reason",
                    "comparison_tier": "declared-but-unverifiable",
                    "reason": "fixture"
                }
            }]
        })
    }

    fn schema10_row() -> HistoryRow {
        let mut test_results = schema9_row()["test_results"].clone();
        test_results["run_id"] = serde_json::json!("run-10");
        test_results["artifact"]["path"] =
            serde_json::json!("ignored/validate/artifacts/run-10/test-results.jsonl");
        serde_json::from_value(serde_json::json!({
            "schema_version": 10,
            "repo": "hermit",
            "run_id": "run-10",
            "profile": "full",
            "commit": "a".repeat(40),
            "commit_anchored": true,
            "tree_dirty": false,
            "tree": "f".repeat(40),
            "executed_tests": 4,
            "passed_tests": 3,
            "filtered_tests": 7,
            "cell_results": schema10_cell_results(),
            "test_results": test_results
        }))
        .unwrap()
    }

    #[test]
    fn schema10_binds_the_retained_verify_log_index_without_reinterpreting_old_rows() {
        let row = schema10_row();
        assert_eq!(row.cell_results_validate_path(), Some(ValidatePath::Full));
        assert!(row.cell_results_evidence().is_some());
        assert!(row.test_results_evidence().unwrap().is_some());
        assert_eq!(
            row.retained_verify_logs_hermetic_image_digest()
                .unwrap()
                .as_deref(),
            Some(concat!(
                "localhost/hermit-hermetic-validate@sha256:",
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
            ))
        );
        assert_eq!(
            row.retained_verify_logs_artifact().unwrap().unwrap(),
            RetainedVerifyLogsArtifact {
                path: "ignored/validate/artifacts/run-10/verify-logs/index.jsonl".into(),
                sha256: "d".repeat(64),
                row_count: 1,
                compressed_bytes: 23,
            }
        );

        for schema in [6, 7, 8, 9] {
            let mut historical = serde_json::to_value(&row).unwrap();
            historical["schema_version"] = serde_json::json!(schema);
            let historical: HistoryRow = serde_json::from_value(historical).unwrap();
            assert!(
                historical
                    .retained_verify_logs_artifact()
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn schema10_refuses_malformed_retained_verify_log_bindings() {
        for (field, value, expected) in [
            (
                "path",
                serde_json::json!("ignored/validate/artifacts/../index.jsonl"),
                "index binding is malformed",
            ),
            (
                "sha256",
                serde_json::json!("NOT-A-DIGEST"),
                "index binding is malformed",
            ),
            (
                "row_count",
                serde_json::json!(0),
                "index binding is malformed",
            ),
            (
                "compressed_bytes",
                serde_json::json!(0),
                "index binding is malformed",
            ),
        ] {
            let mut row = serde_json::to_value(schema10_row()).unwrap();
            row["cell_results"]["retained_verify_logs"][field] = value;
            let row: HistoryRow = serde_json::from_value(row).unwrap();
            let error = row.retained_verify_logs_artifact().unwrap_err();
            assert!(error.contains(expected), "{field}: {error}");
            assert!(row.cell_results_evidence().is_none());
        }

        let mut unknown = schema10_cell_results();
        unknown["retained_verify_logs"]["future"] = serde_json::json!(true);
        let row: HistoryRow = serde_json::from_value(serde_json::json!({
            "schema_version": 10,
            "repo": "hermit",
            "run_id": "run-10",
            "profile": "full",
            "commit": "a".repeat(40),
            "commit_anchored": true,
            "tree_dirty": false,
            "tree": "f".repeat(40),
            "executed_tests": 4,
            "passed_tests": 3,
            "filtered_tests": 7,
            "cell_results": unknown,
            "test_results": schema9_row()["test_results"].clone()
        }))
        .unwrap();
        assert!(
            row.retained_verify_logs_artifact()
                .unwrap_err()
                .contains("unknown field")
        );
    }

    #[test]
    fn schema10_recomputes_population_and_requires_exact_source_and_image_identity() {
        let assert_refused = |mutated: Value, expected: &str| {
            let row: HistoryRow = serde_json::from_value(mutated).unwrap();
            let error = row.retained_verify_logs_artifact().unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error}"
            );
            assert!(row.cell_results_evidence().is_none());
        };

        let mut digest = serde_json::to_value(schema10_row()).unwrap();
        digest["cell_results"]["hermetic_image_digest"] = serde_json::json!(
            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        );
        assert_refused(digest, "hermetic_image_digest is malformed");

        let mut count = serde_json::to_value(schema10_row()).unwrap();
        count["cell_results"]["selected_count"] = serde_json::json!(2);
        assert_refused(count, "counts or selected ordering mismatch");

        let mut population = serde_json::to_value(schema10_row()).unwrap();
        population["cell_results"]["population_sha256"] = serde_json::json!("b".repeat(64));
        assert_refused(population, "population_sha256 mismatch");

        let mut cells = serde_json::to_value(schema10_row()).unwrap();
        cells["cell_results"]["cells"][0]["test"] = serde_json::json!("different");
        assert_refused(cells, "cells differ from selected population");

        let mut unanchored = serde_json::to_value(schema10_row()).unwrap();
        unanchored["commit_anchored"] = serde_json::json!(false);
        assert_refused(unanchored, "not the exact clean row tree");

        let mut no_tree = serde_json::to_value(schema10_row()).unwrap();
        no_tree.as_object_mut().unwrap().remove("tree");
        assert_refused(no_tree, "not the exact clean row tree");

        let mut wrong_repo = serde_json::to_value(schema10_row()).unwrap();
        wrong_repo["repo"] = serde_json::json!("reverie");
        assert_refused(wrong_repo, "not attached to a hermit row");

        let mut missing_test_results = serde_json::to_value(schema10_row()).unwrap();
        missing_test_results
            .as_object_mut()
            .unwrap()
            .remove("test_results");
        assert_refused(missing_test_results, "omitted test_results");

        let mut mismatched_test_results = serde_json::to_value(schema10_row()).unwrap();
        mismatched_test_results["test_results"]["run_id"] = serde_json::json!("other-run");
        assert_refused(mismatched_test_results, "test_results run_id differs");
    }

    #[test]
    fn schema8_compared_verdict_requires_every_current_comparison_key() {
        let comparison = serde_json::json!({
            "strictness": "canonical",
            "display_name": "BitwiseInfoV1",
            "compare_logs": true,
            "compare_io_buffers": true,
            "record_envelope": "all_records_v1",
            "virtualize_time": true,
            "log_scope": "info",
            "strip_lines": false,
            "canonicalize_addresses": true,
            "full_trace": true,
            "exact_remainder": true,
            "stripped_prefixes": ["real-wall-clock-prefix/v1"],
            "canonicalizations": ["host-address-to-first-appearance-ordinal/v1"],
            "ignore_lines": false,
            "skip_commit": false,
            "skip_detlog": false
        });
        let identity = serde_json::json!({
            "lane": "portable",
            "category": "c-programs",
            "test": "example",
            "mode": "verify",
            "backend": "ptrace"
        });
        let mut json = serde_json::json!({
            "path": "full",
            "run_id": "run-8",
            "hermit_sha": "a".repeat(40),
            "source_tree_dirty": false,
            "selected_count": 1,
            "recorded_count": 1,
            "population_sha256": "b".repeat(64),
            "artifact": {
                "path": "ignored/validate/artifacts/run-8/cell-results.jsonl",
                "sha256": "c".repeat(64),
                "row_count": 1
            },
            "selected": [identity.clone()],
            "cells": [{
                "lane": "portable",
                "category": "c-programs",
                "test": "example",
                "mode": "verify",
                "backend": "ptrace",
                "cell_verdict": {
                    "state": "compared-and-matched",
                    "comparison_tier": "canonical-bitwise",
                    "comparison": comparison,
                    "bitwise_parity": true,
                    "compared_log_messages": {"left": 1, "right": 1}
                }
            }]
        });
        assert!(serde_json::from_value::<CellResultsEvidenceV8>(json.clone()).is_ok());

        for required in [
            "display_name",
            "compare_io_buffers",
            "record_envelope",
            "virtualize_time",
        ] {
            let mut missing = json.clone();
            missing["cells"][0]["cell_verdict"]["comparison"]
                .as_object_mut()
                .unwrap()
                .remove(required);
            assert!(
                serde_json::from_value::<CellResultsEvidenceV8>(missing).is_err(),
                "schema 8 must require comparison.{required}"
            );
        }

        json["cells"][0]["cell_verdict"]["comparison"]["display_name"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<CellResultsEvidenceV8>(json).is_ok());
    }

    fn schema9_row() -> Value {
        let selected = TestResultsSelectedPopulation {
            nodes: vec!["test.alpha".into(), "test.beta".into()],
            compatibility: true,
        };
        let population_sha256 = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&selected).unwrap())
        );
        serde_json::json!({
            "schema_version": 9,
            "run_id": "run-9",
            "profile": "full",
            "commit": "a".repeat(40),
            "tree_dirty": false,
            "executed_tests": 4,
            "passed_tests": 3,
            "filtered_tests": 7,
            "test_results": {
                "path": "full",
                "run_id": "run-9",
                "hermit_sha": "a".repeat(40),
                "source_tree_dirty": false,
                "selected_count": 3,
                "recorded_count": 4,
                "population_sha256": population_sha256,
                "selected": {
                    "nodes": ["test.alpha", "test.beta"],
                    "compatibility": true
                },
                "nodes": [{
                    "node": "test.alpha",
                    "outer_attempt": 2,
                    "totals": {
                        "executed_tests": 2,
                        "passed_tests": 1,
                        "failed_tests": 1,
                        "filtered_tests": 3
                    },
                    "row_count": 2
                }, {
                    "node": "test.beta",
                    "outer_attempt": 1,
                    "totals": {
                        "executed_tests": 1,
                        "passed_tests": 1,
                        "failed_tests": 0,
                        "filtered_tests": 4
                    },
                    "row_count": 1
                }],
                "compatibility": {
                    "totals": {
                        "executed_tests": 1,
                        "passed_tests": 1,
                        "failed_tests": 0,
                        "filtered_tests": 0
                    },
                    "row_count": 1
                },
                "totals": {
                    "executed_tests": 4,
                    "passed_tests": 3,
                    "failed_tests": 1,
                    "filtered_tests": 7
                },
                "artifact": {
                    "path": "ignored/validate/artifacts/run-9/test-results.jsonl",
                    "sha256": "b".repeat(64),
                    "row_count": 4
                }
            }
        })
    }

    fn assert_schema9_refused(value: Value, expected: &str) {
        let row: HistoryRow = serde_json::from_value(value).unwrap();
        let error = row
            .test_results_evidence()
            .expect_err("mutated schema 9 evidence must refuse");
        assert!(
            error.contains(expected),
            "schema 9 refusal {error:?} did not name {expected:?}"
        );
    }

    #[test]
    fn schema9_test_results_accept_exact_identity_population_path_and_totals() {
        let value = schema9_row();
        let row: HistoryRow = serde_json::from_value(value.clone()).unwrap();
        let evidence = row
            .test_results_evidence()
            .unwrap()
            .expect("schema 9 should expose typed evidence");
        assert_eq!(evidence.path, ValidatePath::Full);
        assert_eq!(evidence.selected_count, 3);
        assert_eq!(evidence.recorded_count, 4);
        assert_eq!(evidence.totals.executed_tests, 4);
        assert_eq!(evidence.totals.passed_tests, 3);
        assert_eq!(
            serde_json::to_value(&row).unwrap()["test_results"],
            value["test_results"]
        );
    }

    #[test]
    fn schema9_test_results_authority_is_version_first_and_old_bytes_omit_the_new_field() {
        for schema in [6, 7, 8] {
            let row: HistoryRow = serde_json::from_value(serde_json::json!({
                "schema_version": schema,
                "commit": "a".repeat(40)
            }))
            .unwrap();
            let bytes = serde_json::to_vec(&row).unwrap();
            assert!(
                !bytes
                    .windows(b"test_results".len())
                    .any(|window| window == b"test_results")
            );
            assert!(row.test_results_evidence().unwrap().is_none());
        }

        let mut older = schema9_row();
        older["schema_version"] = serde_json::json!(8);
        let older: HistoryRow = serde_json::from_value(older.clone()).unwrap();
        assert!(older.test_results_evidence().unwrap().is_none());
        assert!(serde_json::to_value(older).unwrap()["test_results"].is_object());

        let mut future = schema9_row();
        future["schema_version"] = serde_json::json!(11);
        future["test_results"]["future_field"] = serde_json::json!(true);
        let expected_test_results = future["test_results"].clone();
        let future: HistoryRow = serde_json::from_value(future).unwrap();
        assert!(future.test_results_evidence().unwrap().is_none());
        assert_eq!(
            serde_json::to_value(future).unwrap()["test_results"],
            expected_test_results
        );

        let mut missing = schema9_row();
        missing.as_object_mut().unwrap().remove("test_results");
        assert_schema9_refused(missing, "omitted test_results");
    }

    #[test]
    fn schema9_test_results_refuse_identity_population_and_path_mutations() {
        let mut wrong_path = schema9_row();
        wrong_path["test_results"]["path"] = serde_json::json!("quick");
        assert_schema9_refused(wrong_path, "path differs");

        let mut wrong_run = schema9_row();
        wrong_run["test_results"]["run_id"] = serde_json::json!("other-run");
        assert_schema9_refused(wrong_run, "run_id differs");

        let mut wrong_sha = schema9_row();
        wrong_sha["test_results"]["hermit_sha"] = serde_json::json!("c".repeat(40));
        assert_schema9_refused(wrong_sha, "hermit_sha differs");

        let mut dirty = schema9_row();
        dirty["test_results"]["source_tree_dirty"] = serde_json::json!(true);
        assert_schema9_refused(dirty, "exact clean row tree");

        let mut selected_count = schema9_row();
        selected_count["test_results"]["selected_count"] = serde_json::json!(2);
        assert_schema9_refused(selected_count, "selected_count mismatch");

        let mut unsorted = schema9_row();
        unsorted["test_results"]["selected"]["nodes"] =
            serde_json::json!(["test.beta", "test.alpha"]);
        assert_schema9_refused(unsorted, "duplicate, or unsorted");

        let mut missing_summary = schema9_row();
        missing_summary["test_results"]["nodes"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert_schema9_refused(missing_summary, "summaries differ");

        let mut compatibility = schema9_row();
        compatibility["test_results"]["selected"]["compatibility"] = serde_json::json!(false);
        compatibility["test_results"]["selected_count"] = serde_json::json!(2);
        assert_schema9_refused(compatibility, "compatibility summary differs");

        let mut population = schema9_row();
        population["test_results"]["population_sha256"] = serde_json::json!("d".repeat(64));
        assert_schema9_refused(population, "population_sha256 mismatch");

        let mut traversal = schema9_row();
        traversal["test_results"]["artifact"]["path"] =
            serde_json::json!("ignored/validate/artifacts/../test-results.jsonl");
        assert_schema9_refused(traversal, "artifact binding is malformed");

        let mut digest = schema9_row();
        digest["test_results"]["artifact"]["sha256"] = serde_json::json!("NOT-A-DIGEST");
        assert_schema9_refused(digest, "artifact binding is malformed");
    }

    #[test]
    fn schema9_test_results_refuse_attempt_count_and_total_mutations() {
        let mut attempt = schema9_row();
        attempt["test_results"]["nodes"][0]["outer_attempt"] = serde_json::json!(0);
        assert_schema9_refused(attempt, "zero outer_attempt");

        let mut summary = schema9_row();
        summary["test_results"]["nodes"][0]["totals"]["passed_tests"] = serde_json::json!(2);
        assert_schema9_refused(summary, "pass/fail total differs");

        let mut row_count = schema9_row();
        row_count["test_results"]["nodes"][0]["row_count"] = serde_json::json!(3);
        assert_schema9_refused(row_count, "row_count differs");

        let mut aggregate = schema9_row();
        aggregate["test_results"]["totals"]["filtered_tests"] = serde_json::json!(8);
        assert_schema9_refused(aggregate, "checked totals");

        let mut recorded = schema9_row();
        recorded["test_results"]["recorded_count"] = serde_json::json!(5);
        assert_schema9_refused(recorded, "checked totals");

        let mut artifact_count = schema9_row();
        artifact_count["test_results"]["artifact"]["row_count"] = serde_json::json!(5);
        assert_schema9_refused(artifact_count, "checked totals");

        let mut row_total = schema9_row();
        row_total["passed_tests"] = serde_json::json!(4);
        assert_schema9_refused(row_total, "HistoryRow totals");

        let mut overflow = schema9_row();
        overflow["test_results"]["nodes"][0]["totals"] = serde_json::json!({
            "executed_tests": u64::MAX,
            "passed_tests": u64::MAX,
            "failed_tests": 0,
            "filtered_tests": 3
        });
        overflow["test_results"]["nodes"][0]["row_count"] = serde_json::json!(u64::MAX);
        assert_schema9_refused(overflow, "executed_tests overflowed");
    }

    #[test]
    fn schema9_exact_shapes_refuse_missing_unknown_and_invalid_artifact_rows() {
        let mut unknown = schema9_row();
        unknown["test_results"]["unknown"] = serde_json::json!(true);
        assert_schema9_refused(unknown, "unknown field");

        let mut missing = schema9_row();
        missing["test_results"]
            .as_object_mut()
            .unwrap()
            .remove("totals");
        assert_schema9_refused(missing, "missing field");

        let row = serde_json::json!({
            "run_id": "run-9",
            "hermit_sha": "a".repeat(40),
            "path": "full",
            "producer": {
                "source": "node",
                "node": "test.alpha",
                "outer_attempt": 2
            },
            "id": "suite$case",
            "result": "pass",
            "attempts": 1
        });
        assert!(serde_json::from_value::<TestResultArtifactRow>(row.clone()).is_ok());
        for (field, value) in [
            ("attempts", serde_json::json!(0)),
            ("id", serde_json::json!("")),
        ] {
            let mut invalid = row.clone();
            invalid[field] = value;
            assert!(serde_json::from_value::<TestResultArtifactRow>(invalid).is_err());
        }
        let mut zero_outer = row.clone();
        zero_outer["producer"]["outer_attempt"] = serde_json::json!(0);
        assert!(serde_json::from_value::<TestResultArtifactRow>(zero_outer).is_err());
        let mut extra = row;
        extra["extra"] = serde_json::json!(true);
        assert!(serde_json::from_value::<TestResultArtifactRow>(extra).is_err());
    }
}
