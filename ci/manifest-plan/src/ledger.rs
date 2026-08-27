//! Validation ledger schema emitted by Hermit's validation driver and consumed
//! by ci-hub.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;

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
    #[serde(default)]
    pub checks: Option<u64>,
    /// Explicit completed/expected outer-gate counts. `checks` is retained for
    /// old rows; new failure evidence carries both names so completeness is
    /// observable rather than inferred from a profile name.
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

/// A supported cell-results shape, or the exact JSON from a newer shape that
/// this reader does not understand yet. The typed arm is deliberately first:
/// supported rows must keep their established serialization and receipt digest.
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
        let CellVerdict::ComparedAndMatched { comparison, .. } =
            serde_json::from_str::<CellVerdict>(&current).unwrap()
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
}
