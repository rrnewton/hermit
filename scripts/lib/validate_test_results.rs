// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the producer-owned terminal test population behind one validation row.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use hermit_manifest_plan::ledger::CompatibilityTestResultSummary;
use hermit_manifest_plan::ledger::NodeTestResultSummary;
use hermit_manifest_plan::ledger::TestResultArtifactRow;
use hermit_manifest_plan::ledger::TestResultProducer;
use hermit_manifest_plan::ledger::TestResultTotals;
use hermit_manifest_plan::ledger::TestResultVerdict;
use hermit_manifest_plan::ledger::TestResultsArtifact;
use hermit_manifest_plan::ledger::TestResultsEvidenceV9;
use hermit_manifest_plan::ledger::TestResultsSelectedPopulation;
use hermit_manifest_plan::ledger::ValidatePath;
use sha2::Digest;
use sha2::Sha256;

pub const TEST_RESULTS_LEDGER_SCHEMA_VERSION: i64 = 9;
const TEST_RESULTS_ARTIFACT_NAME: &str = "test-results.jsonl";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactTestTotals {
    pub executed_tests: u64,
    pub passed_tests: u64,
    pub filtered_tests: u64,
}

#[derive(Clone, Debug)]
pub struct NodeTestResultsInput {
    pub node: String,
    pub outer_attempt: u64,
    pub test_results: dagrun::TestResults,
}

#[derive(Debug)]
pub struct RetainedTestResults {
    pub schema_version: i64,
    pub evidence: TestResultsEvidenceV9,
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn checked_results(
    producer: TestResultProducer,
    results: &dagrun::TestResults,
) -> Result<(Vec<TestResultArtifactRow>, TestResultTotals), String> {
    let rows = results
        .results
        .as_ref()
        .ok_or_else(|| "structured test-result producer retained count-only schema".to_string())?;
    let row_count = u64::try_from(rows.len())
        .map_err(|_| "structured test-result row count does not fit u64".to_string())?;
    if row_count != results.executed_tests {
        return Err(format!(
            "structured test-result producer reports {} executed tests but retains {row_count} rows",
            results.executed_tests
        ));
    }
    let mut sorted = rows.clone();
    sorted.sort_by(|left, right| left.id.cmp(&right.id));
    if sorted.windows(2).any(|pair| pair[0].id == pair[1].id) {
        return Err("structured test-result producer retains a duplicate test id".into());
    }
    let passed_tests = u64::try_from(sorted.iter().filter(|result| result.passed).count())
        .map_err(|_| "structured passed-test count does not fit u64".to_string())?;
    let failed_tests = row_count
        .checked_sub(passed_tests)
        .ok_or("structured failed-test count underflowed")?;
    let totals = TestResultTotals {
        executed_tests: results.executed_tests,
        passed_tests,
        failed_tests,
        filtered_tests: results.filtered_tests,
    };
    let rows = sorted
        .into_iter()
        .map(|result| TestResultArtifactRow {
            run_id: String::new(),
            hermit_sha: String::new(),
            path: ValidatePath::Full,
            producer: producer.clone(),
            id: result.id,
            result: if result.passed {
                TestResultVerdict::Pass
            } else {
                TestResultVerdict::Fail
            },
            attempts: result.attempts,
        })
        .collect();
    Ok((rows, totals))
}

fn add_totals(total: &mut TestResultTotals, add: TestResultTotals) -> Result<(), String> {
    total.executed_tests = total
        .executed_tests
        .checked_add(add.executed_tests)
        .ok_or("retained executed_tests overflowed u64")?;
    total.passed_tests = total
        .passed_tests
        .checked_add(add.passed_tests)
        .ok_or("retained passed_tests overflowed u64")?;
    total.failed_tests = total
        .failed_tests
        .checked_add(add.failed_tests)
        .ok_or("retained failed_tests overflowed u64")?;
    total.filtered_tests = total
        .filtered_tests
        .checked_add(add.filtered_tests)
        .ok_or("retained filtered_tests overflowed u64")?;
    Ok(())
}

fn canonical_jsonl(rows: &[TestResultArtifactRow]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row)
            .map_err(|error| format!("cannot encode retained test-result row: {error}"))?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

pub fn verify_artifact(
    evidence: &TestResultsEvidenceV9,
    bytes: &[u8],
) -> Result<Vec<TestResultArtifactRow>, String> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return Err("retained test-results artifact lacks its final newline".into());
    }
    let mut rows = Vec::new();
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    for (index, line) in body
        .split(|byte| *byte == b'\n')
        .filter(|_| !body.is_empty())
        .enumerate()
    {
        if line.is_empty() {
            return Err(format!(
                "retained test-results artifact row {} is empty",
                index + 1
            ));
        }
        rows.push(
            serde_json::from_slice::<TestResultArtifactRow>(line).map_err(|error| {
                format!(
                    "retained test-results artifact row {} is malformed: {error}",
                    index + 1
                )
            })?,
        );
    }
    if canonical_jsonl(&rows)? != bytes {
        return Err("retained test-results artifact is not canonical JSONL".into());
    }
    let row_count = u64::try_from(rows.len())
        .map_err(|_| "retained test-results row count does not fit u64".to_string())?;
    if row_count != evidence.recorded_count || row_count != evidence.artifact.row_count {
        return Err("retained test-results artifact row count mismatch".into());
    }
    if hex_digest(bytes) != evidence.artifact.sha256 {
        return Err("retained test-results artifact sha256 mismatch".into());
    }
    if rows.iter().any(|row| {
        row.run_id != evidence.run_id
            || row.hermit_sha != evidence.hermit_sha
            || row.path != evidence.path
    }) {
        return Err("retained test-results artifact identity mismatch".into());
    }
    let summary_nodes = evidence
        .nodes
        .iter()
        .map(|summary| summary.node.as_str())
        .collect::<Vec<_>>();
    if summary_nodes
        != evidence
            .selected
            .nodes
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
        || evidence.selected.compatibility != evidence.compatibility.is_some()
    {
        return Err(
            "retained test-results summaries differ from the selected producer population".into(),
        );
    }
    let mut cursor = 0usize;
    let mut observed_totals = TestResultTotals {
        executed_tests: 0,
        passed_tests: 0,
        failed_tests: 0,
        filtered_tests: 0,
    };
    let mut verify_group = |producer: TestResultProducer,
                            totals: TestResultTotals,
                            expected_rows: u64|
     -> Result<(), String> {
        if totals.passed_tests.checked_add(totals.failed_tests) != Some(totals.executed_tests)
            || expected_rows != totals.executed_tests
        {
            return Err("retained test-results summary totals are inconsistent".into());
        }
        let count = usize::try_from(expected_rows)
            .map_err(|_| "retained test-results group row count does not fit usize")?;
        let end = cursor
            .checked_add(count)
            .ok_or("retained test-results group row range overflowed")?;
        let group = rows
            .get(cursor..end)
            .ok_or("retained test-results artifact ended before its summaries")?;
        if group.iter().any(|row| row.producer != producer)
            || !group.windows(2).all(|pair| pair[0].id < pair[1].id)
        {
            return Err(
                "retained test-results artifact producer grouping or test-id order mismatch".into(),
            );
        }
        let passed = u64::try_from(
            group
                .iter()
                .filter(|row| row.result == TestResultVerdict::Pass)
                .count(),
        )
        .map_err(|_| "retained test-results passed count does not fit u64")?;
        if passed != totals.passed_tests
            || expected_rows.checked_sub(passed) != Some(totals.failed_tests)
        {
            return Err("retained test-results artifact verdict totals mismatch".into());
        }
        add_totals(&mut observed_totals, totals)?;
        cursor = end;
        Ok(())
    };
    for summary in &evidence.nodes {
        verify_group(
            TestResultProducer::Node {
                node: summary.node.clone(),
                outer_attempt: summary.outer_attempt,
            },
            summary.totals,
            summary.row_count,
        )?;
    }
    if let Some(summary) = &evidence.compatibility {
        verify_group(
            TestResultProducer::Compatibility,
            summary.totals,
            summary.row_count,
        )?;
    }
    if cursor != rows.len() || observed_totals != evidence.totals {
        return Err("retained test-results artifact differs from its producer summaries".into());
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub fn retain(
    parent: &Path,
    path: ValidatePath,
    run_id: &str,
    hermit_sha: &str,
    source_tree_dirty: bool,
    selected_nodes: &BTreeSet<String>,
    compatibility_selected: bool,
    mut nodes: Vec<NodeTestResultsInput>,
    compatibility: Option<dagrun::TestResults>,
    expected: ExactTestTotals,
) -> Result<RetainedTestResults, String> {
    if source_tree_dirty {
        return Err("retained test results require an exact clean source tree".into());
    }
    nodes.sort_by(|left, right| left.node.cmp(&right.node));
    let input_nodes = nodes
        .iter()
        .map(|node| node.node.clone())
        .collect::<BTreeSet<_>>();
    if input_nodes.len() != nodes.len() || &input_nodes != selected_nodes {
        return Err(
            "retained test-result producers differ from the selected node population".into(),
        );
    }
    if compatibility_selected != compatibility.is_some() {
        return Err("retained compatibility results differ from the selected population".into());
    }

    let selected = TestResultsSelectedPopulation {
        nodes: selected_nodes.iter().cloned().collect(),
        compatibility: compatibility_selected,
    };
    let population_sha256 = hex_digest(
        &serde_json::to_vec(&selected)
            .map_err(|error| format!("cannot encode selected test-result population: {error}"))?,
    );
    let mut artifact_rows = Vec::new();
    let mut summaries = Vec::new();
    let mut totals = TestResultTotals {
        executed_tests: 0,
        passed_tests: 0,
        failed_tests: 0,
        filtered_tests: 0,
    };
    for node in nodes {
        if node.outer_attempt == 0 {
            return Err(format!(
                "test-result node {} has zero outer attempt",
                node.node
            ));
        }
        let producer = TestResultProducer::Node {
            node: node.node.clone(),
            outer_attempt: node.outer_attempt,
        };
        let (mut rows, node_totals) = checked_results(producer, &node.test_results)?;
        for row in &mut rows {
            row.run_id = run_id.into();
            row.hermit_sha = hermit_sha.into();
            row.path = path;
        }
        let row_count = u64::try_from(rows.len())
            .map_err(|_| "node test-result row count does not fit u64".to_string())?;
        add_totals(&mut totals, node_totals)?;
        artifact_rows.extend(rows);
        summaries.push(NodeTestResultSummary {
            node: node.node,
            outer_attempt: node.outer_attempt,
            totals: node_totals,
            row_count,
        });
    }
    let compatibility_summary = if let Some(results) = compatibility {
        let (mut rows, compatibility_totals) =
            checked_results(TestResultProducer::Compatibility, &results)?;
        for row in &mut rows {
            row.run_id = run_id.into();
            row.hermit_sha = hermit_sha.into();
            row.path = path;
        }
        let row_count = u64::try_from(rows.len())
            .map_err(|_| "compatibility test-result row count does not fit u64".to_string())?;
        add_totals(&mut totals, compatibility_totals)?;
        artifact_rows.extend(rows);
        Some(CompatibilityTestResultSummary {
            totals: compatibility_totals,
            row_count,
        })
    } else {
        None
    };
    let expected_totals = TestResultTotals {
        executed_tests: expected.executed_tests,
        passed_tests: expected.passed_tests,
        failed_tests: expected
            .executed_tests
            .checked_sub(expected.passed_tests)
            .ok_or("expected passed_tests exceeds executed_tests")?,
        filtered_tests: expected.filtered_tests,
    };
    if totals != expected_totals {
        return Err(format!(
            "retained test-result totals {totals:?} differ from run totals {expected_totals:?}"
        ));
    }
    let artifact_bytes = canonical_jsonl(&artifact_rows)?;
    let recorded_count = u64::try_from(artifact_rows.len())
        .map_err(|_| "retained test-result count does not fit u64".to_string())?;
    let artifact_path = super::validate_cell_results::publish_run_artifact_noclobber(
        parent,
        run_id,
        TEST_RESULTS_ARTIFACT_NAME,
        &artifact_bytes,
        "retained test-results artifact",
    )?;
    let evidence = TestResultsEvidenceV9 {
        path,
        run_id: run_id.into(),
        hermit_sha: hermit_sha.into(),
        source_tree_dirty,
        selected_count: u64::try_from(selected.nodes.len())
            .map_err(|_| "selected test-result node count does not fit u64")?
            .checked_add(u64::from(selected.compatibility))
            .ok_or("selected test-result producer count overflowed")?,
        recorded_count,
        population_sha256,
        selected,
        nodes: summaries,
        compatibility: compatibility_summary,
        totals,
        artifact: TestResultsArtifact {
            path: artifact_path,
            sha256: hex_digest(&artifact_bytes),
            row_count: recorded_count,
        },
    };
    verify_artifact(&evidence, &artifact_bytes)?;
    Ok(RetainedTestResults {
        schema_version: TEST_RESULTS_LEDGER_SCHEMA_VERSION,
        evidence,
    })
}

pub fn self_test() -> Result<String, String> {
    let root = tempfile::tempdir()
        .map_err(|error| format!("retained test results: cannot create fixture: {error}"))?;
    let selected = BTreeSet::from(["test.fixture".to_string()]);
    let inputs = vec![NodeTestResultsInput {
        node: "test.fixture".into(),
        outer_attempt: 2,
        test_results: dagrun::TestResults::current(
            2,
            3,
            vec![
                dagrun::TestResult::new("z".into(), false, 1)?,
                dagrun::TestResult::new("a".into(), true, 2)?,
            ],
        )?,
    }];
    let retained = retain(
        root.path(),
        ValidatePath::Full,
        "self-test",
        "0123456789abcdef0123456789abcdef01234567",
        false,
        &selected,
        false,
        inputs.clone(),
        None,
        ExactTestTotals {
            executed_tests: 2,
            passed_tests: 1,
            filtered_tests: 3,
        },
    )?;
    let bytes = fs::read(root.path().join(&retained.evidence.artifact.path))
        .map_err(|error| format!("retained test results: cannot read fixture: {error}"))?;
    if verify_artifact(&retained.evidence, &bytes)?.len() != 2 {
        return Err("retained test results: canonical artifact lost rows".into());
    }
    if retain(
        root.path(),
        ValidatePath::Full,
        "self-test",
        "0123456789abcdef0123456789abcdef01234567",
        false,
        &selected,
        false,
        inputs,
        None,
        ExactTestTotals {
            executed_tests: 2,
            passed_tests: 1,
            filtered_tests: 3,
        },
    )
    .is_ok()
    {
        return Err("retained test results: duplicate publication clobbered evidence".into());
    }
    Ok("retained test results: canonical rows verified and duplicate publication refused".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, passed: bool) -> dagrun::TestResult {
        dagrun::TestResult::new(id.into(), passed, 1).unwrap()
    }

    #[test]
    fn cumulative_writer_is_canonical_complete_and_no_clobber() {
        let root = tempfile::tempdir().unwrap();
        let selected = BTreeSet::from(["test.alpha".into(), "test.beta".into()]);
        let nodes = vec![
            NodeTestResultsInput {
                node: "test.beta".into(),
                outer_attempt: 2,
                test_results: dagrun::TestResults::current(1, 4, vec![result("same-id", true)])
                    .unwrap(),
            },
            NodeTestResultsInput {
                node: "test.alpha".into(),
                outer_attempt: 1,
                test_results: dagrun::TestResults::current(
                    2,
                    3,
                    vec![result("z", false), result("a", true)],
                )
                .unwrap(),
            },
        ];
        let compatibility =
            dagrun::TestResults::current(1, 0, vec![result("same-id", true)]).unwrap();
        let retain_once = || {
            retain(
                root.path(),
                ValidatePath::Full,
                "fixture-run",
                "0123456789abcdef0123456789abcdef01234567",
                false,
                &selected,
                true,
                nodes.clone(),
                Some(compatibility.clone()),
                ExactTestTotals {
                    executed_tests: 4,
                    passed_tests: 3,
                    filtered_tests: 7,
                },
            )
        };
        let retained = retain_once().unwrap();
        assert_eq!(retained.schema_version, TEST_RESULTS_LEDGER_SCHEMA_VERSION);
        assert_eq!(
            retained.evidence.selected.nodes,
            ["test.alpha", "test.beta"]
        );
        assert_eq!(retained.evidence.recorded_count, 4);
        let bytes = fs::read(root.path().join(&retained.evidence.artifact.path)).unwrap();
        let rows = verify_artifact(&retained.evidence, &bytes).unwrap();
        assert_eq!(rows.len(), 4);
        let mut mutated_rows = rows;
        mutated_rows[0].result = TestResultVerdict::Fail;
        let mutated_bytes = canonical_jsonl(&mutated_rows).unwrap();
        let mut spoofed = retained.evidence.clone();
        spoofed.artifact.sha256 = hex_digest(&mutated_bytes);
        assert!(
            verify_artifact(&spoofed, &mutated_bytes)
                .unwrap_err()
                .contains("verdict totals"),
            "a verdict mutation with a recomputed artifact digest was accepted"
        );
        assert!(retain_once().unwrap_err().contains("already exists"));
    }

    #[test]
    fn cumulative_writer_refuses_incomplete_population_and_spoofed_totals() {
        let root = tempfile::tempdir().unwrap();
        let selected = BTreeSet::from(["test.alpha".into(), "test.missing".into()]);
        let nodes = vec![NodeTestResultsInput {
            node: "test.alpha".into(),
            outer_attempt: 1,
            test_results: dagrun::TestResults::current(1, 0, vec![result("a", true)]).unwrap(),
        }];
        let invoke = |selected: &BTreeSet<String>, expected| {
            retain(
                root.path(),
                ValidatePath::Full,
                "refusal-run",
                "0123456789abcdef0123456789abcdef01234567",
                false,
                selected,
                false,
                nodes.clone(),
                None,
                expected,
            )
        };
        assert!(
            invoke(
                &selected,
                ExactTestTotals {
                    executed_tests: 1,
                    passed_tests: 1,
                    filtered_tests: 0,
                }
            )
            .unwrap_err()
            .contains("selected node population")
        );
        let selected = BTreeSet::from(["test.alpha".into()]);
        assert!(
            invoke(
                &selected,
                ExactTestTotals {
                    executed_tests: 2,
                    passed_tests: 2,
                    filtered_tests: 0,
                }
            )
            .unwrap_err()
            .contains("differ from run totals")
        );
    }
}
