// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the producer-owned test results behind one validation ledger row.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use hermit_manifest_plan::ledger::NodeTestResultsRecord;
use hermit_manifest_plan::ledger::TestResultRecord;
use hermit_manifest_plan::ledger::TestResultVerdict;
use hermit_manifest_plan::ledger::TestResultsArtifact;
use hermit_manifest_plan::ledger::TestResultsArtifactBody;
use hermit_manifest_plan::ledger::TestResultsEvidence;
use hermit_manifest_plan::ledger::TestResultsRecord;
use sha2::Digest;
use sha2::Sha256;

pub const TEST_RESULTS_LEDGER_SCHEMA_VERSION: i64 = 8;
const ARTIFACT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TestTotals {
    pub executed_tests: u64,
    pub passed_tests: u64,
    pub filtered_tests: u64,
}

#[derive(Debug)]
pub struct RetainedTestResults {
    pub schema_version: i64,
    pub evidence: TestResultsEvidence,
}

pub fn result_record(result: &dagrun::TestResult) -> TestResultRecord {
    TestResultRecord {
        id: result.id.clone(),
        result: if result.passed {
            TestResultVerdict::Pass
        } else {
            TestResultVerdict::Fail
        },
        attempts: result.attempts,
    }
}

pub fn results_record(results: &dagrun::TestResults) -> Result<TestResultsRecord, String> {
    let rows = results.results.as_ref().ok_or_else(|| {
        "structured test-result producer retained count-only schema".to_string()
    })?;
    Ok(TestResultsRecord {
        executed_tests: results.executed_tests,
        filtered_tests: results.filtered_tests,
        results: rows.iter().map(result_record).collect(),
    })
}

fn checked_totals(body: &TestResultsArtifactBody) -> Result<TestTotals, String> {
    if body.schema_version != ARTIFACT_SCHEMA_VERSION {
        return Err(format!(
            "retained test results use unsupported schema {}",
            body.schema_version
        ));
    }
    let mut nodes = BTreeSet::new();
    let mut executed_tests = 0u64;
    let mut passed_tests = 0u64;
    let mut filtered_tests = 0u64;
    let mut add = |record: &TestResultsRecord, source: &str| -> Result<(), String> {
        let result_count = u64::try_from(record.results.len())
            .map_err(|_| format!("{source} result count does not fit u64"))?;
        if result_count != record.executed_tests {
            return Err(format!(
                "{source} reports {} executed tests but retains {result_count} results",
                record.executed_tests
            ));
        }
        let mut ids = BTreeSet::new();
        for result in &record.results {
            if result.id.is_empty() || result.id.trim() != result.id {
                return Err(format!("{source} retains an empty or untrimmed test id"));
            }
            if result.attempts == 0 {
                return Err(format!(
                    "{source} test {} has zero attempts",
                    result.id
                ));
            }
            if !ids.insert(result.id.as_str()) {
                return Err(format!(
                    "{source} retains duplicate test id {}",
                    result.id
                ));
            }
        }
        executed_tests = executed_tests
            .checked_add(record.executed_tests)
            .ok_or("retained executed_tests overflowed u64")?;
        filtered_tests = filtered_tests
            .checked_add(record.filtered_tests)
            .ok_or("retained filtered_tests overflowed u64")?;
        let passed = u64::try_from(
            record
                .results
                .iter()
                .filter(|result| result.result == TestResultVerdict::Pass)
                .count(),
        )
        .map_err(|_| format!("{source} passed result count does not fit u64"))?;
        passed_tests = passed_tests
            .checked_add(passed)
            .ok_or("retained passed_tests overflowed u64")?;
        Ok(())
    };

    for node in &body.nodes {
        if node.node.is_empty() || node.node.trim() != node.node {
            return Err("retained test results contain an empty or untrimmed node".into());
        }
        if node.outer_attempt == 0 {
            return Err(format!(
                "retained test results node {} has zero outer_attempt",
                node.node
            ));
        }
        if !nodes.insert(node.node.as_str()) {
            return Err(format!(
                "retained test results contain duplicate node {}",
                node.node
            ));
        }
        add(&node.test_results, &format!("node {}", node.node))?;
    }
    if let Some(compatibility) = &body.compatibility {
        add(compatibility, "compatibility")?;
    }
    Ok(TestTotals {
        executed_tests,
        passed_tests,
        filtered_tests,
    })
}

fn canonical_body(mut body: TestResultsArtifactBody) -> Result<(TestResultsArtifactBody, Vec<u8>), String> {
    body.nodes.sort_by(|left, right| {
        (&left.node, left.outer_attempt).cmp(&(&right.node, right.outer_attempt))
    });
    for node in &mut body.nodes {
        node.test_results.results.sort_by(|left, right| left.id.cmp(&right.id));
    }
    if let Some(compatibility) = &mut body.compatibility {
        compatibility.results.sort_by(|left, right| left.id.cmp(&right.id));
    }
    checked_totals(&body)?;
    let mut bytes = serde_json::to_vec(&body)
        .map_err(|error| format!("cannot encode retained test results: {error}"))?;
    bytes.push(b'\n');
    Ok((body, bytes))
}

pub fn verify_artifact(
    evidence: &TestResultsEvidence,
    bytes: &[u8],
) -> Result<TestTotals, String> {
    if !bytes.ends_with(b"\n") || bytes == b"\n" {
        return Err("retained test-results artifact is empty or lacks its final newline".into());
    }
    let body: TestResultsArtifactBody = serde_json::from_slice(bytes)
        .map_err(|error| format!("retained test-results artifact is malformed: {error}"))?;
    let (body, canonical) = canonical_body(body)?;
    if canonical != bytes {
        return Err("retained test-results artifact is not canonical".into());
    }
    let digest = format!("{:x}", Sha256::digest(bytes));
    if evidence.artifact.sha256 != digest {
        return Err("retained test-results artifact sha256 mismatch".into());
    }
    let node_count = u64::try_from(body.nodes.len())
        .map_err(|_| "retained test-results node count does not fit u64")?;
    if evidence.node_count != node_count {
        return Err("retained test-results node_count mismatch".into());
    }
    let totals = checked_totals(&body)?;
    if evidence.recorded_count != totals.executed_tests
        || evidence.executed_tests != totals.executed_tests
        || evidence.passed_tests != totals.passed_tests
        || evidence.filtered_tests != totals.filtered_tests
    {
        return Err("retained test-results totals mismatch".into());
    }
    Ok(totals)
}

#[allow(clippy::too_many_arguments)]
pub fn retain(
    parent: &Path,
    run_id: &str,
    hermit_sha: &str,
    source_tree_dirty: bool,
    nodes: Vec<NodeTestResultsRecord>,
    compatibility: Option<TestResultsRecord>,
    expected: TestTotals,
) -> Result<RetainedTestResults, String> {
    let (body, bytes) = canonical_body(TestResultsArtifactBody {
        schema_version: ARTIFACT_SCHEMA_VERSION,
        nodes,
        compatibility,
    })?;
    let totals = checked_totals(&body)?;
    if totals != expected {
        return Err(format!(
            "retained test-results totals {totals:?} differ from run totals {expected:?}"
        ));
    }
    let artifact_dir = parent
        .join("ignored")
        .join("validate")
        .join("artifacts")
        .join(run_id);
    fs::create_dir_all(&artifact_dir).map_err(|error| {
        format!(
            "cannot create retained test-results directory {}: {error}",
            artifact_dir.display()
        )
    })?;
    let artifact = artifact_dir.join("test-results.json");
    fs::write(&artifact, &bytes).map_err(|error| {
        format!(
            "cannot write retained test-results artifact {}: {error}",
            artifact.display()
        )
    })?;
    let relative = artifact
        .strip_prefix(parent)
        .map_err(|_| "retained test-results artifact is outside parent root")?
        .to_string_lossy()
        .into_owned();
    let node_count = u64::try_from(body.nodes.len())
        .map_err(|_| "retained test-results node count does not fit u64")?;
    let evidence = TestResultsEvidence {
        run_id: run_id.into(),
        hermit_sha: hermit_sha.into(),
        source_tree_dirty,
        node_count,
        recorded_count: totals.executed_tests,
        executed_tests: totals.executed_tests,
        passed_tests: totals.passed_tests,
        filtered_tests: totals.filtered_tests,
        artifact: TestResultsArtifact {
            path: relative,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
        },
    };
    verify_artifact(&evidence, &bytes)?;
    Ok(RetainedTestResults {
        schema_version: TEST_RESULTS_LEDGER_SCHEMA_VERSION,
        evidence,
    })
}

pub fn self_test() -> Result<String, String> {
    let fixture = tempfile::tempdir()
        .map_err(|error| format!("retained test results: cannot create fixture: {error}"))?;
    let count_only = dagrun::TestResults::from_json_slice(
        br#"{"schema":1,"executed_tests":0,"filtered_tests":0}"#,
    )?;
    if results_record(&count_only).is_ok() {
        return Err("retained test results: count-only producer evidence was accepted".into());
    }
    let result = |id: &str, verdict: TestResultVerdict| TestResultRecord {
        id: id.into(),
        result: verdict,
        attempts: 1,
    };
    let nodes = vec![
        NodeTestResultsRecord {
            node: "test.beta".into(),
            outer_attempt: 2,
            test_results: TestResultsRecord {
                executed_tests: 1,
                filtered_tests: 3,
                results: vec![result("same-id", TestResultVerdict::Pass)],
            },
        },
        NodeTestResultsRecord {
            node: "test.alpha".into(),
            outer_attempt: 1,
            test_results: TestResultsRecord {
                executed_tests: 2,
                filtered_tests: 4,
                results: vec![
                    result("z", TestResultVerdict::Fail),
                    result("a", TestResultVerdict::Pass),
                ],
            },
        },
    ];
    let compatibility = TestResultsRecord {
        executed_tests: 1,
        filtered_tests: 0,
        // The same id in a distinct producer population is not a duplicate.
        results: vec![result("same-id", TestResultVerdict::Pass)],
    };
    let expected = TestTotals {
        executed_tests: 4,
        passed_tests: 3,
        filtered_tests: 7,
    };
    let retained = retain(
        fixture.path(),
        "fixture-run",
        "0123456789abcdef0123456789abcdef01234567",
        false,
        nodes.clone(),
        Some(compatibility.clone()),
        expected,
    )?;
    let artifact = fixture.path().join(&retained.evidence.artifact.path);
    let bytes = fs::read(&artifact)
        .map_err(|error| format!("retained test results: cannot read fixture: {error}"))?;
    if verify_artifact(&retained.evidence, &bytes)? != expected {
        return Err("retained test results: verified totals changed".into());
    }

    let wrong = TestTotals {
        executed_tests: 5,
        ..expected
    };
    if retain(
        fixture.path(),
        "spoofed-total",
        "0123456789abcdef0123456789abcdef01234567",
        false,
        nodes.clone(),
        Some(compatibility.clone()),
        wrong,
    )
    .is_ok()
    {
        return Err("retained test results: spoofed aggregate was accepted".into());
    }

    let mut duplicate = nodes.clone();
    duplicate[0]
        .test_results
        .results
        .push(result("same-id", TestResultVerdict::Pass));
    duplicate[0].test_results.executed_tests = 2;
    if retain(
        fixture.path(),
        "duplicate-id",
        "0123456789abcdef0123456789abcdef01234567",
        false,
        duplicate,
        Some(compatibility.clone()),
        TestTotals {
            executed_tests: 5,
            passed_tests: 4,
            filtered_tests: 7,
        },
    )
    .is_ok()
    {
        return Err("retained test results: duplicate producer id was accepted".into());
    }

    let mut malformed = bytes.clone();
    malformed.pop();
    if verify_artifact(&retained.evidence, &malformed).is_ok() {
        return Err("retained test results: truncated artifact was accepted".into());
    }
    let mut spoofed = retained.evidence.clone();
    spoofed.passed_tests = 4;
    if verify_artifact(&spoofed, &bytes).is_ok() {
        return Err("retained test results: mutated evidence total was accepted".into());
    }

    Ok("retained test results: canonical 4/3/7 totals verified; count-only, duplicate, truncated, and spoofed evidence refused".into())
}
