//! A DERIVED wall backstop must reach the step that runs, not merely the pure function.
//!
//! `resolved_wall_timeout` was pinned by unit tests and by the pre-flight `--run-timeout`
//! ordering check, and by nothing else. Replacing the call at the ENFORCEMENT site in
//! `scheduler.rs` with the pre-derivation fallback (`step.timeout`, else the document default,
//! else 1800) left every suite in both engines green: a step declaring only a CPU budget went on
//! running under 1800 s while the docs and the manifest said otherwise. `make cross` cannot see
//! that either, because it is a py-vs-rs differential and the defect is symmetric.
//!
//! This is the Rust half of `py/tests/test_wall_backstop_enforcement.py`, and it pins the chain in
//! two links, because a derived ceiling is floored at `DEFAULT_STEP_TIMEOUT` and so cannot be
//! waited out inside a test:
//!
//! 1. a real run of a step that declares only `cpu_timeout` journals the DERIVED ceiling as the
//!    bound it ran under; and
//! 2. the number journalled under that name is the number the wall killer actually enforces —
//!    a step given a 2-second ceiling is reaped at 2 seconds and reports 2.
//!
//! Both legs run `--unsafe-no-cgroups`, so neither can self-skip: the wall bound is a scheduler
//! `wait` with a deadline and is in force on the uncontained lane, which `capabilities` already
//! says out loud.

use std::io::Write;
use std::process::Command;

/// 900 declared CPU-seconds derive `3 * 900 = 2700`, which is above the 1800 floor, so the
/// derivation is what governs. The command exits immediately: this leg is about the bound the
/// step RAN UNDER, not about breaching it.
const DERIVED_DAG: &str = r#"{"steps": [{"group": "g", "job": "derived",
  "desc": "declares only a CPU budget", "cmd": "true", "cpu_timeout": 900}]}"#;

/// An explicitly declared 2-second ceiling against a command that will not finish.
const DECLARED_HANG_DAG: &str = r#"{"steps": [{"group": "g", "job": "hang",
  "desc": "outlives its declared wall ceiling", "cmd": "sleep 30", "timeout": 2}]}"#;

fn run_unboxed(name: &str, dag_text: &str) -> (Option<i32>, String, String) {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let dir = std::env::temp_dir().join(format!(
        "dagrun_wall_backstop_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dag = dir.join("dag.json");
    let mut f = std::fs::File::create(&dag).unwrap();
    f.write_all(dag_text.as_bytes()).unwrap();
    drop(f);
    let evidence = dir.join("evidence");

    let output = Command::new(bin)
        .args([
            "run",
            "--dag",
            dag.to_str().unwrap(),
            "-q",
            "--no-profile",
            "--unsafe-no-cgroups",
        ])
        .env("DAGRUN_LOG_DIR", &evidence)
        .output()
        .expect("failed to spawn the built binary");

    let journal = std::fs::read_to_string(evidence.join("journal.jsonl"))
        .expect("the run should have written a journal");
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let _ = std::fs::remove_dir_all(&dir);
    (output.status.code(), journal, stderr)
}

fn record<'a>(journal: &'a str, event: &str) -> &'a str {
    journal
        .lines()
        .find(|line| line.contains(&format!(r#""event":"{event}""#)))
        .unwrap_or_else(|| panic!("no {event} record in the journal:\n{journal}"))
}

fn field(rec: &str, key: &str) -> String {
    let needle = format!("\"{key}\":\"");
    let start = rec
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key} in record:\n{rec}"))
        + needle.len();
    let rest = &rec[start..];
    let end = rest.find('"').expect("unterminated JSON string");
    rest[..end].to_string()
}

#[test]
fn a_step_declaring_only_a_cpu_budget_runs_under_the_derived_ceiling() {
    let (code, journal, stderr) = run_unboxed("derived", DERIVED_DAG);
    assert_eq!(code, Some(0), "the step should succeed:\n{stderr}");
    let end = record(&journal, "step_end");
    // 2700, named literally. 1800 here is the pre-derivation fallback still being enforced while
    // the derivation sits unused one call away; 900 or 300 would be an unscaled or unfactored
    // budget reaching the runner.
    assert_eq!(
        field(end, "wall_limit_s"),
        "2700",
        "the ceiling the step actually ran under must be the DERIVED one:\n{end}"
    );
}

#[test]
fn the_journalled_wall_ceiling_is_the_one_the_killer_enforces() {
    // The second link. Without it, leg one only shows that a number was written down; this shows
    // that the number written down under `wall_limit_s` is the deadline the step is reaped on.
    let started = std::time::Instant::now();
    let (code, journal, stderr) = run_unboxed("declared", DECLARED_HANG_DAG);
    let elapsed = started.elapsed().as_secs_f64();
    assert_eq!(code, Some(1), "a wall breach must fail the run:\n{stderr}");

    let breach = record(&journal, "step_timeout");
    assert_eq!(field(breach, "limit_s"), "2", "{breach}");
    assert_eq!(field(breach, "unit"), "wall_seconds", "{breach}");
    let end = record(&journal, "step_end");
    assert_eq!(
        field(end, "wall_limit_s"),
        field(breach, "limit_s"),
        "the journalled ceiling and the enforced deadline must be one number:\n{end}\n{breach}"
    );
    assert!(
        elapsed < 25.0,
        "the step was reaped on its 2-second ceiling, not left to finish its 30-second sleep; \
         the whole run took {elapsed}s"
    );
}
