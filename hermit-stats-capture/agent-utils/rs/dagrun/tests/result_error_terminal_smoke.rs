//! Black-box controls for terminal reporting of required structured-result failures.
//!
//! A missing result file is an independent failure fact when a step is stopped by the outer run
//! budget, cancelled after a peer fails, or exits unsuccessfully itself. Each path must print the
//! refusal without turning an aborted step into a `DAGRUN STEP ERROR`. When the refusal already is
//! the primary failure, it must appear only once.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};

const PASS_RESULT: &str = r#"{"schema":3,"executed_tests":1,"filtered_tests":0,"results":[{"id":"suite$setup","result":"pass","attempts":1,"attempt_results":[{"attempt":1,"outcome":"passed","detail":null}]}]}"#;
const REFUSAL: &str =
    "STRUCTURED TEST RESULTS REFUSED: required structured test results were not written";

fn result_manifest(owner: &str) -> Value {
    json!([{
        "kind": "structured-test-results",
        "schema": 3,
        "path_env": "DAGRUN_TEST_COUNTS_PATH",
        "owner": owner,
    }])
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_result_terminal_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn write_dag(&self, name: &str, document: Value) -> PathBuf {
        let path = self.dir.join(format!("{name}.json"));
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Run {
    code: Option<i32>,
    text: String,
}

fn run(dag: &Path, jobs: &str, extra: &[&str]) -> Run {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let mut args = vec![
        "run",
        "--dag",
        dag.to_str().unwrap(),
        "-j",
        jobs,
        "--unsafe-no-cgroups",
        "--no-profile",
        "--no-profile-feedback",
    ];
    args.extend_from_slice(extra);
    let output = Command::new(bin)
        .args(args)
        .output()
        .expect("failed to run dagrun");
    Run {
        code: output.status.code(),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    }
}

fn occurrences(text: &str, needle: &str) -> usize {
    text.match_indices(needle).count()
}

#[test]
fn outer_budget_abort_prints_its_distinct_result_refusal_without_a_step_error_marker() {
    let fx = Fixture::new("outer");
    let dag = fx.write_dag(
        "outer",
        json!({"steps": [
            {
                "group": "outer",
                "job": "setup",
                "cmd": format!("printf '%s' '{PASS_RESULT}' > \"$DAGRUN_TEST_COUNTS_PATH\"; sleep 2"),
                "timeout": 3,
                "cpu_timeout": 60,
                "result_manifests": result_manifest("outer.setup"),
            },
            {
                "group": "outer",
                "job": "cut",
                "deps": ["outer.setup"],
                "cmd": "sleep 30",
                "timeout": 3,
                "cpu_timeout": 60,
                "result_manifests": result_manifest("outer.cut"),
            }
        ]}),
    );
    let result = run(&dag, "1", &["--run-timeout", "4"]);

    assert_ne!(
        result.code,
        Some(0),
        "outer budget must fail the run:\n{}",
        result.text
    );
    assert!(
        result.text.contains("[outer.cut] ⊘ ABORT")
            && result.text.contains("cut short by the OUTER run budget"),
        "the primary outer-budget reason must remain visible:\n{}",
        result.text
    );
    assert_eq!(
        occurrences(&result.text, &format!("[outer.cut] ↳ {REFUSAL}")),
        1,
        "the distinct required-result refusal must be printed beside the abort:\n{}",
        result.text
    );
    assert_eq!(
        occurrences(&result.text, "DAGRUN STEP ERROR"),
        0,
        "a run-budget cancellation is not a failing step:\n{}",
        result.text
    );
}

#[test]
fn peer_abort_and_ordinary_failure_each_print_their_distinct_result_refusal() {
    let fx = Fixture::new("peer");
    let dag = fx.write_dag(
        "peer",
        json!({"steps": [
            {
                "group": "peer",
                "job": "boom",
                "cmd": "sleep 0.2; exit 7",
                "timeout": 30,
                "cpu_timeout": 60,
                "result_manifests": result_manifest("peer.boom"),
            },
            {
                "group": "peer",
                "job": "sleeper",
                "cmd": "sleep 30",
                "timeout": 30,
                "cpu_timeout": 60,
                "result_manifests": result_manifest("peer.sleeper"),
            }
        ]}),
    );
    let result = run(&dag, "2", &[]);

    assert_ne!(
        result.code,
        Some(0),
        "the failing step must fail the run:\n{}",
        result.text
    );
    assert!(
        result.text.contains("[peer.boom] ✗ FAIL") && result.text.contains("exit 7"),
        "the ordinary process failure must remain primary:\n{}",
        result.text
    );
    assert_eq!(
        occurrences(&result.text, &format!("[peer.boom] ↳ {REFUSAL}")),
        1,
        "the failing step's distinct result refusal is missing:\n{}",
        result.text
    );
    assert!(
        result.text.contains("[peer.sleeper] ⊘ ABORT")
            && result.text.contains("eager-exit after another step failed"),
        "the peer cancellation must remain an abort:\n{}",
        result.text
    );
    assert_eq!(
        occurrences(&result.text, &format!("[peer.sleeper] ↳ {REFUSAL}")),
        1,
        "the aborted peer's distinct result refusal is missing:\n{}",
        result.text
    );
    assert_eq!(
        occurrences(&result.text, "DAGRUN STEP ERROR"),
        1,
        "only the genuinely failing step gets the searchable marker:\n{}",
        result.text
    );
    assert!(
        result.text.contains("DAGRUN STEP ERROR [peer.boom]"),
        "the sole marker must name the ordinary failure:\n{}",
        result.text
    );
}

#[test]
fn a_result_refusal_that_is_already_primary_is_not_printed_twice() {
    let fx = Fixture::new("primary");
    let dag = fx.write_dag(
        "primary",
        json!({"steps": [{
            "group": "primary",
            "job": "missing",
            "cmd": "true",
            "timeout": 30,
            "cpu_timeout": 60,
            "result_manifests": result_manifest("primary.missing"),
        }]}),
    );
    let result = run(&dag, "1", &[]);

    assert_ne!(
        result.code,
        Some(0),
        "missing required evidence must fail:\n{}",
        result.text
    );
    assert_eq!(
        occurrences(&result.text, REFUSAL),
        1,
        "the secondary printer must suppress a reason already used as primary:\n{}",
        result.text
    );
    assert_eq!(occurrences(&result.text, "DAGRUN STEP ERROR"), 1);
}
