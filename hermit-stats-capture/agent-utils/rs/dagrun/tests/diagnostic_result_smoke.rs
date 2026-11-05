//! Black-box control for schema-4 diagnostic test failures through the real `dagrun run`.
//!
//! A diagnostic failure must not fail its step or the run, and it must stay visible where an
//! operator looks: the step's own DIAGNOSTIC lines and the end-of-run summary. A blocking failure
//! in the same report must still fail the run.

use std::path::PathBuf;
use std::process::Command;

use serde_json::{json, Value};

const MIXED_PASSING: &str = r#"{"schema":4,"executed_tests":2,"filtered_tests":0,"results":[{"id":"smoke$fine","result":"pass","attempts":1,"attempt_results":[{"attempt":1,"outcome":"passed","detail":null}],"diagnostic_reason":null},{"id":"smoke$probe","result":"diagnostic_fail","attempts":1,"attempt_results":[{"attempt":1,"outcome":"failed","detail":"exit 4"}],"diagnostic_reason":"bounded host probe"}]}"#;

fn result_manifest(owner: &str) -> Value {
    json!([{
        "kind": "structured-test-results",
        "schema": 4,
        "path_env": "DAGRUN_TEST_COUNTS_PATH",
        "owner": owner,
    }])
}

fn run(name: &str, payload: &str) -> (Option<i32>, String) {
    let dir = std::env::temp_dir().join(format!(
        "dagrun_diagnostic_smoke_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dag: PathBuf = dir.join("dag.json");
    let document = json!({"steps": [{
        "group": "smoke",
        "job": "bucket",
        "cmd": format!("printf '%s' '{payload}' > \"$DAGRUN_TEST_COUNTS_PATH\""),
        "timeout": 30,
        "cpu_timeout": 60,
        "result_manifests": result_manifest("smoke.bucket"),
    }]});
    std::fs::write(&dag, serde_json::to_vec(&document).unwrap()).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dagrun"))
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-j",
            "1",
            "--unsafe-no-cgroups",
            "--no-profile",
            "--no-profile-feedback",
        ])
        .output()
        .expect("failed to run dagrun");
    let _ = std::fs::remove_dir_all(&dir);
    (
        output.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

#[test]
fn a_diagnostic_failure_passes_the_run_and_is_printed_by_the_step_and_the_summary() {
    let (code, text) = run("pass", MIXED_PASSING);
    assert_eq!(
        code,
        Some(0),
        "a diagnostic failure must not fail the run:\n{text}"
    );
    assert!(text.contains("[smoke.bucket] ✓ PASS"), "{text}");
    assert!(
        text.contains("[smoke.bucket] ⚠ DIAGNOSTIC 1 test failure(s)"),
        "the step must print its diagnostic count:\n{text}"
    );
    assert!(
        text.contains(
            "smoke$probe (attempt 1 failed: exit 4; non-blocking because: bounded host probe)"
        ),
        "the step must name the failure, its cause and its reason:\n{text}"
    );
    assert!(
        text.contains("1 non-blocking diagnostic test failure(s) in smoke.bucket (1)"),
        "the end-of-run summary must count it:\n{text}"
    );
}

#[test]
fn the_same_failure_without_the_designation_fails_the_run() {
    let blocking = MIXED_PASSING
        .replace(r#""result":"diagnostic_fail""#, r#""result":"fail""#)
        .replace(
            r#""diagnostic_reason":"bounded host probe""#,
            r#""diagnostic_reason":null"#,
        );
    let (code, text) = run("fail", &blocking);
    assert_ne!(
        code,
        Some(0),
        "a blocking failure must fail the run:\n{text}"
    );
    assert!(
        text.contains("STRUCTURED TEST FAILURE: smoke$probe attempt 1 failed: exit 4"),
        "{text}"
    );
    assert!(!text.contains("DIAGNOSTIC"), "{text}");
    assert!(!text.contains("non-blocking diagnostic"), "{text}");
}
