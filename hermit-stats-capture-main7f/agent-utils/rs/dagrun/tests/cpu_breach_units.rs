//! #98 dagrun-cpu-breach-flake: a CPU breach must be RECORDED in the unit it was measured in.
//!
//! # What is under test
//!
//! The CPU guard compares cgroup `cpu.stat` CPU-seconds against a CPU-second budget, correctly.
//! The RECORD of that decision used to carry an unlabelled `elapsed_s` — which was WALL — beside a
//! `limit_s` that was CPU-seconds. Unlabelled and side by side, the natural reading is that the two
//! are comparable; they are not, and a CPU breach could be quoted as having consumed more seconds
//! than its own run's whole CPU rollup contained.
//!
//! # Why the reading is INJECTED rather than burned
//!
//! The predecessor of this test burned four cores at once and asserted `measured > wall`, on the
//! reasoning that four burners consume CPU faster than wall time passes. That is true on an idle
//! machine and false on a busy one: under a full parallel workspace run the four burners do not
//! each get a core, CPU and wall converge, and the assertion that separates the two quantities
//! stops separating them. It was observed at 2.002 CPU-seconds against 2.004 wall, then passed 3/3
//! in isolation.
//!
//! No margin fixes that. A margin wide enough to survive full load is wide enough to stop catching
//! the regression, because under load the two numbers are genuinely equal — no assertion evaluated
//! at runtime can tell them apart once they are.
//!
//! So the CPU reading comes from the cgroup manager instead of from the scheduler's luck. The
//! guard, the monitor loop, and the record are all the real ones; only the kernel's number is
//! planted, and it is planted FAR from the wall figure the step will have accumulated, so a record
//! that substituted one for the other is visibly wrong rather than coincidentally right.
//!
//! This is what the Python edition already does — see `py/tests/test_termination_evidence_units.py`
//! and the matching `_FAKE_CPU_USED_S` — so the two editions now test this property the same way.
//!
//! # What this does NOT cover, on purpose
//!
//! That a REAL cgroup reading actually crosses a real budget and reaps the step is a different
//! claim, and it keeps its own test: `boxing_cpu_timeout_reaps_a_step_past_its_budget` in
//! `cpu_timeout_smoke.rs` burns for real. It asserts only that a CPU-TIMEOUT happened, never how
//! the two quantities compare, so it has nothing to converge.

use std::collections::BTreeMap;
use std::sync::Arc;

use dagrun::model::{DagConfig, ResourceHint, Step};
use dagrun::scheduler::run_dag_boxed_limited;
use dagrun::CgroupManager;

/// What the planted cgroup reports the step has burned.
///
/// Deliberately nowhere near the wall time the step can have accumulated when the monitor trips
/// (about one poll interval). The same value as the Python edition's `_FAKE_CPU_USED_S`, so the
/// two tests fail on the same number.
const FAKE_CPU_USED_S: f64 = 37.5;
const CPU_BUDGET_S: i64 = 2;
const WALL_BUDGET_S: i64 = 300;

/// A boxed manager that reports a step already far past its CPU budget.
///
/// `enabled()` is true so the scheduler takes exactly the boxed measurement path it takes on a real
/// cgroup-v2 host; the numbers come from here rather than from the kernel.
struct OverBudgetCgroups;

impl CgroupManager for OverBudgetCgroups {
    fn enabled(&self) -> bool {
        true
    }

    fn prepare_command(
        &self,
        _tag: &str,
        cmd: &str,
        _mem_max: Option<i64>,
        _cpu_count: Option<i64>,
    ) -> String {
        cmd.to_string()
    }

    fn kill(&self, _tag: &str) -> bool {
        false
    }

    fn cleanup(&self, _tag: &str) {}

    fn oom_kills(&self, _tag: &str) -> i64 {
        0
    }

    fn peak_bytes(&self, _tag: &str) -> Option<i64> {
        None
    }

    fn cpu_stats(&self, _tag: &str) -> Option<BTreeMap<String, i64>> {
        let usage = (FAKE_CPU_USED_S * 1_000_000.0) as i64;
        Some(BTreeMap::from([
            ("usage_usec".to_string(), usage),
            ("user_usec".to_string(), usage),
            ("system_usec".to_string(), 0),
            ("throttled_usec".to_string(), 0),
        ]))
    }

    fn cpu_pressure(&self, _tag: &str) -> Option<BTreeMap<String, f64>> {
        None
    }

    fn thread_count(&self, _tag: &str) -> Option<i64> {
        None
    }

    fn kill_all_remaining(&self) -> i64 {
        0
    }
}

/// One step that burns NO CPU of its own, so every CPU-second in the record came from the planted
/// reading and the wall figure is the only quantity the machine gets a vote in.
fn sleeping_step() -> DagConfig {
    let step = Step {
        group: "g".into(),
        job: "spin".into(),
        desc: "burn".into(),
        description: String::new(),
        labels: Vec::new(),
        cmd: "sleep 30".into(),
        cmdtype: dagrun::CmdType::Unknown,
        manifest: None,
        result_manifests: None,
        integration_test_binaries: None,
        deps: Vec::new(),
        env: BTreeMap::new(),
        hint: ResourceHint::default(),
        networkonly: false,
        engine_only: false,
        delegated_children: false,
        // Generous wall backstop: the CPU guard, not the wall clock, must be what fires.
        timeout: WALL_BUDGET_S,
        cpu_timeout: CPU_BUDGET_S,
        jobs_flag: None,
        jobs_env: None,
        skip_reason: None,
        write_domains: None,
        write_domain_guarantee: None,
        explains: Vec::new(),
        fail_fast_family: None,
    };
    DagConfig {
        steps: vec![step],
        default_step_cpu_timeout: 0,
        ..Default::default()
    }
}

/// ONE test function in its own file, because it sets a process-wide environment variable to place
/// the journal. Each integration test file is its own process, so nothing else can observe it.
#[test]
fn a_cpu_breach_records_the_cpu_quantity_it_compared_not_the_wall_clock() {
    let dir = std::env::temp_dir().join(format!("dagrun_cpu_units_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The evidence writer REFUSES a directory others can read, and answers by writing no journal
    // at all rather than by failing loudly. Without this the test reads as "the record is missing"
    // when the truth is "the record was never allowed to be written".
    std::fs::set_permissions(&dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    std::env::set_var(dagrun::attribution::LOG_DIR_ENV, &dir);

    let result = run_dag_boxed_limited(
        &sleeping_step(),
        1,
        1,
        false,
        0,
        Some(Arc::new(OverBudgetCgroups)),
    );
    assert!(
        !result.ok,
        "a step past its CPU budget must be cut: {}",
        result.outcomes[0].reason
    );
    assert!(
        result.outcomes[0].reason.contains("CPU-TIMEOUT"),
        "the CPU guard, not the wall clock, must be what fired: {}",
        result.outcomes[0].reason
    );

    let journal = std::fs::read_to_string(dir.join("journal.jsonl"))
        .expect("the run should have written a journal");
    let _ = std::fs::remove_dir_all(&dir);

    let record = journal
        .lines()
        .find(|line| line.contains(r#""event":"cpu_timeout""#))
        .unwrap_or_else(|| panic!("no cpu_timeout record in the journal:\n{journal}"))
        .to_string();

    // The compared quantity is the CPU reading the guard actually tripped on. Exact, not an
    // inequality: the planted reading is a number no wall clock in this test can produce.
    assert_eq!(
        json_number(&record, "measured_s"),
        FAKE_CPU_USED_S,
        "measured_s must be the planted CPU reading:\n{record}"
    );
    assert_eq!(
        json_number(&record, "limit_s"),
        CPU_BUDGET_S as f64,
        "{record}"
    );
    assert!(
        record.contains(r#""unit":"cpu_seconds""#),
        "a CPU breach must name its unit:\n{record}"
    );

    // Wall is still recorded — it is useful — but under a name that says what it is, and it is a
    // different number. The step slept; it burned no CPU of its own, so the wall figure is about
    // one monitor tick and cannot approach the planted reading however loaded the host is.
    let wall = json_number(&record, "wall_elapsed_s");
    assert!(
        wall < FAKE_CPU_USED_S,
        "the planted CPU reading must exceed the wall time, or this test cannot tell the two \
         apart:\n{record}"
    );
    assert!(
        json_number(&record, "measured_s") != wall,
        "the compared quantity and the wall context must not be the same number:\n{record}"
    );

    // The ambiguous field is GONE. Retaining it would preserve the exact misreading this fixes: an
    // unlabelled seconds figure sitting next to a limit in a different unit. `wall_elapsed_s` does
    // not match this needle, because the quote before `elapsed_s` is part of it.
    assert!(
        !record.contains(r#""elapsed_s":"#),
        "the unlabelled seconds field must not come back:\n{record}"
    );

    // The TERMINAL record must be able to stand alone: a hard kill destroys the end-of-run profile
    // flush, and then this is the only thing left that can say what the step consumed against the
    // budget it was given.
    let end = journal
        .lines()
        .find(|line| line.contains(r#""event":"step_end""#))
        .unwrap_or_else(|| panic!("no step_end record in the journal:\n{journal}"))
        .to_string();
    assert_eq!(
        json_number(&end, "cpu_limit_s"),
        CPU_BUDGET_S as f64,
        "{end}"
    );
    assert_eq!(
        json_number(&end, "wall_limit_s"),
        WALL_BUDGET_S as f64,
        "{end}"
    );
    assert_eq!(
        json_number(&end, "cpu_usage_usec"),
        FAKE_CPU_USED_S * 1_000_000.0,
        "the journalled cgroup CPU total must be the reading the budget was blown against:\n{end}"
    );
    assert!(json_number(&end, "wall_elapsed_s") > 0.0, "{end}");
}

/// Pull one `"key":"<number>"` out of a flat JSONL record without taking a JSON dependency on a
/// test target. Panics with the whole record when the key is absent, which is the failure a missing
/// field should produce.
fn json_number(record: &str, key: &str) -> f64 {
    let needle = format!("\"{key}\":\"");
    let start = record
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key} in record:\n{record}"))
        + needle.len();
    let rest = &record[start..];
    let end = rest.find('"').expect("unterminated JSON string");
    rest[..end]
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number in record:\n{record}"))
}
