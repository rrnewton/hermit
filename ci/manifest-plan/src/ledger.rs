//! Validation ledger schema emitted by Hermit's validation driver and consumed
//! by ci-hub.

use std::borrow::Cow;
use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;

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
    /// known. Schema 6 and 7 retain their historical type; schema 8 is parsed
    /// from the forward-compatible JSON arm into its exact versioned shape.
    /// Newer schemas remain readable but receive no typed evidence authority.
    pub fn cell_results_evidence(&self) -> Option<Cow<'_, CellResultsEvidence>> {
        let value = self.cell_results.as_ref()?;
        match self.schema_version? {
            6 | 7 => value.typed().map(Cow::Borrowed),
            8 => value
                .schema8()
                .map(|evidence| Cow::Owned(evidence.into_evidence())),
            _ => None,
        }
    }

    /// Return the recorded validation path only for an exact schema-8 row.
    pub fn cell_results_validate_path(&self) -> Option<ValidatePath> {
        if self.schema_version != Some(8) {
            return None;
        }
        self.cell_results
            .as_ref()?
            .schema8()
            .map(|evidence| evidence.path)
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

        let mut future_outer_version = serde_json::to_value(&row).unwrap();
        future_outer_version["schema_version"] = serde_json::json!(9);
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
}
