//! The terminal step record must say WHY the step ended that way, not only that it did.
//!
//! `step_end` exists so the journal alone can answer what a run was doing, and it is the stream
//! that survives when the checkout and the console are both gone. It carried `ok` and `aborted`
//! and no cause, so a cancelled step could be COUNTED and never NAMED — and an unknown nobody can
//! name is an unknown nobody can drive down.
//!
//! The two cancellations are the case that makes this more than a missing field. They call for
//! completely different follow-up, and the outcome's `reason` used to say eager-exit for both, so
//! recording it verbatim would have put a confidently WRONG cause in the record: a reader
//! hunting for a failing peer that does not exist. Every case below is a real run.
//!
//! | case | asserts |
//! |---|---|
//! | `a_step_that_fails_on_its_own` | the record names its own exit |
//! | `a_peer_failure_abort` | the record names eager-exit |
//! | `an_outer_budget_abort` | the record names the run budget, and NOT eager-exit |
//! | `a_passing_step` (control) | no `reason` key at all, rather than an empty one |

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dagrun::cgroup::CgroupManager;
use dagrun::model::{ABORTED_BY_PEER_FAILURE_REASON, ABORTED_BY_RUN_BUDGET_REASON};
use dagrun::scheduler::{run_dag_boxed_deadline_with_cpu, start_run_cpu_budget, BoxedCgroups};

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_reason_journal_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fixture { dir }
    }

    fn write_dag(&self, name: &str, steps: &str) -> PathBuf {
        let path = self.dir.join(format!("{name}.json"));
        std::fs::write(&path, format!(r#"{{"steps":[{steps}]}}"#)).unwrap();
        path
    }

    fn logs(&self) -> PathBuf {
        self.dir.join("logs")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Run the real binary with its journal directed into the fixture, and hand back every
/// `step_end` record as (step, whole line).
fn step_end_records(fx: &Fixture, dag: &Path, jobs: &str, extra: &[&str]) -> Vec<(String, String)> {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let mut args: Vec<&str> = vec![
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
        .env("DAGRUN_LOG_DIR", fx.logs())
        .output()
        .expect("failed to run dagrun");
    let journal = std::fs::read_to_string(fx.logs().join("journal.jsonl")).unwrap_or_else(|e| {
        panic!(
            "no journal at {}: {e}\n{}{}",
            fx.logs().join("journal.jsonl").display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    journal
        .lines()
        .filter(|line| line.contains(r#""event":"step_end""#))
        .map(|line| {
            let step = line
                .split(r#""step":""#)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_default()
                .to_string();
            (step, line.to_string())
        })
        .collect()
}

fn record_for<'a>(records: &'a [(String, String)], step: &str) -> &'a str {
    records
        .iter()
        .find(|(tag, _)| tag == step)
        .map(|(_, line)| line.as_str())
        .unwrap_or_else(|| panic!("no step_end for {step} in {records:?}"))
}

fn strict_ok(record: &str) -> Result<bool, String> {
    let value: serde_json::Value =
        serde_json::from_str(record).map_err(|error| format!("invalid journal JSON: {error}"))?;
    dagrun::require_step_end_ok(&value)
}

#[test]
fn a_step_that_fails_on_its_own_records_its_own_cause() {
    let fx = Fixture::new("fail");
    let dag = fx.write_dag(
        "fail",
        r#"{"group":"a","job":"boom","cmd":"exit 3","timeout":30,"cpu_timeout":60}"#,
    );
    let records = step_end_records(&fx, &dag, "1", &[]);
    let record = record_for(&records, "a.boom");
    assert_eq!(strict_ok(record), Ok(false), "{record}");
    assert!(
        record.contains(r#""reason":"exit 3""#),
        "a failed step must record the cause it already computed: {record}"
    );
}

#[test]
fn a_peer_failure_abort_records_eager_exit() {
    let fx = Fixture::new("peer");
    // `slow` is still running when `boom` fails, so eager-exit cancels it. Both are ready at
    // once, which is what makes the cancellation a peer cancellation rather than a skip.
    let dag = fx.write_dag(
        "peer",
        concat!(
            r#"{"group":"a","job":"slow","cmd":"sleep 20","timeout":30,"cpu_timeout":60},"#,
            r#"{"group":"a","job":"boom","cmd":"exit 1","timeout":30,"cpu_timeout":60}"#
        ),
    );
    let records = step_end_records(&fx, &dag, "2", &[]);
    let record = record_for(&records, "a.slow");
    assert!(
        record.contains(r#""aborted":"true""#),
        "the fixture must really have been cancelled: {record}"
    );
    assert!(
        record.contains("eager-exit after another step failed"),
        "a peer-failure cancellation must name the peer failure: {record}"
    );
    assert!(
        !record.contains("OUTER run budget"),
        "a peer-failure cancellation must not blame the run budget: {record}"
    );
}

#[test]
fn an_outer_budget_abort_records_the_budget_and_never_a_peer() {
    let fx = Fixture::new("outer");
    // No step here fails. The only thing that ends the run is its own outer budget, so any
    // mention of a failing peer in the record would be describing something that never happened.
    // A predecessor spends most of the budget so the run bound fires while the successor is
    // still inside its OWN step budget. The runner refuses to start a DAG whose step budgets
    // equal or exceed the run budget, precisely so a cut stays attributable to a node.
    let dag = fx.write_dag(
        "outer",
        concat!(
            r#"{"group":"a","job":"first","cmd":"sleep 4","timeout":7,"cpu_timeout":60},"#,
            r#"{"group":"a","job":"long","deps":["a.first"],"cmd":"sleep 30","timeout":7,"cpu_timeout":60}"#
        ),
    );
    let records = step_end_records(&fx, &dag, "1", &["--run-timeout", "8"]);
    let record = record_for(&records, "a.long");
    assert!(
        record.contains(r#""aborted":"true""#),
        "the fixture must really have been cancelled: {record}"
    );
    assert!(
        record.contains("cut short by the OUTER run budget"),
        "a run-budget cut must name the run budget: {record}"
    );
    // The control that makes the assertion above mean something. Before the reason distinguished
    // the two cancellations, this record would have carried the eager-exit wording and sent a
    // reader looking for a failing step that does not exist in this DAG.
    assert!(
        !record.contains("eager-exit"),
        "a run-budget cut must not claim a peer failed: {record}"
    );
}

#[test]
fn a_passing_step_carries_no_reason_key_at_all() {
    let fx = Fixture::new("pass");
    let dag = fx.write_dag(
        "pass",
        r#"{"group":"a","job":"fine","cmd":"true","timeout":30,"cpu_timeout":60}"#,
    );
    let records = step_end_records(&fx, &dag, "1", &[]);
    let record = record_for(&records, "a.fine");
    assert_eq!(strict_ok(record), Ok(true), "{record}");
    // Absent rather than empty, for the same reason an unset budget is absent: `"reason":""` in
    // the record would read as a cause that was looked for and not found.
    assert!(
        !record.contains(r#""reason""#),
        "a passing step must not carry an empty reason: {record}"
    );
}

/// Isolate journal environment from the other integration tests in this process.
fn cancellation_child(name: &str) -> Option<PathBuf> {
    const CHILD: &str = "DAGRUN_ABORT_CAUSE_TEST_CHILD";
    if let Some(root) = std::env::var_os(CHILD) {
        return Some(PathBuf::from(root));
    }
    let fx = Fixture::new(name);
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, &fx.dir)
        .env("DAGRUN_LOG_DIR", fx.logs())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

fn journal_records(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(root.join("logs/journal.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
}

#[derive(Clone, Copy)]
enum AccountingFault {
    None,
    Unreadable,
    Backwards,
}

/// Only result collection and the explicitly faulted CPU reading are controlled.
/// Commands, fail-fast decisions, wall deadlines and process-group signals are real.
/// This manager does not establish cgroup containment: kill returns false.
struct CancellationControl {
    root: PathBuf,
    delay_peer_cleanup: bool,
    observed_deadline: AtomicBool,
    accounting_fault: AccountingFault,
}

impl CgroupManager for CancellationControl {
    fn enabled(&self) -> bool {
        true
    }
    fn prepare_command(
        &self,
        _tag: &str,
        cmd: &str,
        _mem: Option<i64>,
        _cpus: Option<i64>,
    ) -> String {
        cmd.to_string()
    }
    fn kill(&self, _tag: &str) -> bool {
        false
    }
    fn cleanup(&self, tag: &str) {
        if !self.delay_peer_cleanup || tag != "a.peer" {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let text = std::fs::read_to_string(self.root.join("logs/journal.jsonl")).unwrap();
            if text
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|record| record["event"] == "run_timeout")
            {
                self.observed_deadline.store(true, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the real run deadline never fired during peer cleanup");
    }
    fn oom_kills(&self, _tag: &str) -> i64 {
        0
    }
    fn peak_bytes(&self, _tag: &str) -> Option<i64> {
        None
    }
    fn cpu_stats(&self, _tag: &str) -> Option<std::collections::BTreeMap<String, i64>> {
        None
    }
    fn run_cpu_usage_usec(&self) -> Option<i64> {
        if !self.root.join("cpu-ready").exists() {
            return Some(1_000_000);
        }
        match self.accounting_fault {
            AccountingFault::None => Some(1_000_000),
            AccountingFault::Unreadable => None,
            AccountingFault::Backwards => Some(0),
        }
    }
    fn cpu_pressure(&self, _tag: &str) -> Option<std::collections::BTreeMap<String, f64>> {
        None
    }
    fn thread_count(&self, _tag: &str) -> Option<i64> {
        None
    }
    fn kill_all_remaining(&self) -> i64 {
        0
    }
}

#[test]
fn peer_cancellation_survives_a_later_deadline_in_the_same_run() {
    let Some(root) =
        cancellation_child("peer_cancellation_survives_a_later_deadline_in_the_same_run")
    else {
        return;
    };
    let ready = shell_path(&root.join("ready"));
    let cfg = dagrun::dag_from_value(&serde_json::json!({"steps":[
        {"group":"a","job":"first","cmd":"sleep 2","timeout":3,"cpu_timeout":30},
        {"group":"a","job":"peer","cmd":format!("touch {ready}; sleep 30"),
         "deps":["a.first"],"timeout":3,"cpu_timeout":30,"fail_fast_family":"failed"},
        {"group":"a","job":"boom","cmd":format!("while [ ! -f {ready} ]; do sleep 0.01; done; exit 7"),
         "deps":["a.first"],"timeout":3,"cpu_timeout":30,"fail_fast_family":"failed"},
        {"group":"a","job":"independent","cmd":"sleep 30",
         "deps":["a.first"],"timeout":3,"cpu_timeout":30,"fail_fast_family":"independent"}
    ]})).unwrap();
    let manager = Arc::new(CancellationControl {
        root: root.clone(),
        delay_peer_cleanup: true,
        observed_deadline: AtomicBool::new(false),
        accounting_fault: AccountingFault::None,
    });
    let result = run_dag_boxed_deadline_with_cpu(
        &cfg,
        3,
        false,
        0,
        Some(manager.clone()),
        None,
        Some(3),
        Some(4),
        None,
    );
    assert!(!result.ok && result.run_timed_out);
    assert!(!result.run_cpu_timed_out && !result.run_cpu_accounting_failed);
    assert!(manager.observed_deadline.load(Ordering::SeqCst));
    assert_eq!(result.outcomes.len(), 4);
    let outcome = |tag: &str| {
        result
            .outcomes
            .iter()
            .find(|outcome| outcome.tag == tag)
            .unwrap()
    };
    assert!(outcome("a.first").ok);
    assert_eq!(outcome("a.boom").returncode, Some(7));
    assert!(!outcome("a.boom").aborted);
    let records = journal_records(&root);
    for (tag, reason) in [
        ("a.peer", ABORTED_BY_PEER_FAILURE_REASON),
        ("a.independent", ABORTED_BY_RUN_BUDGET_REASON),
    ] {
        assert!(outcome(tag).aborted);
        assert!(
            outcome(tag).returncode.is_some_and(|code| code < 0),
            "the real child must have been signalled"
        );
        assert!(!outcome(tag).timed_out && !outcome(tag).cpu_timed_out);
        assert_eq!(outcome(tag).reason, reason);
        let ends = records
            .iter()
            .filter(|record| record["event"] == "step_end" && record["step"] == tag)
            .collect::<Vec<_>>();
        assert_eq!(ends.len(), 1);
        assert_eq!(ends[0]["reason"], reason);
    }
    let timeout = records
        .iter()
        .position(|record| record["event"] == "run_timeout")
        .unwrap();
    let peer_failure = records
        .iter()
        .position(|record| record["event"] == "step_end" && record["step"] == "a.boom")
        .unwrap();
    let peer_end = records
        .iter()
        .position(|record| record["event"] == "step_end" && record["step"] == "a.peer")
        .unwrap();
    assert!(
        peer_failure < timeout && timeout < peer_end,
        "peer failure, real deadline, delayed peer completion must occur in that order"
    );
}

fn accounting_loss_cancels_a_live_step(root: PathBuf, fault: AccountingFault) {
    let cfg = dagrun::dag_from_value(&serde_json::json!({"steps":[{
        "group":"a","job":"live",
        "cmd":format!("touch {}; sleep 30", shell_path(&root.join("cpu-ready"))),
        "timeout":3,"cpu_timeout":30
    }]}))
    .unwrap();
    let cgroups: BoxedCgroups = Some(Arc::new(CancellationControl {
        root: root.clone(),
        delay_peer_cleanup: false,
        observed_deadline: AtomicBool::new(false),
        accounting_fault: fault,
    }));
    let budget = start_run_cpu_budget(&cgroups, Some(1)).unwrap();
    let result =
        run_dag_boxed_deadline_with_cpu(&cfg, 1, false, 0, cgroups, None, Some(1), Some(4), budget);
    assert!(
        root.join("cpu-ready").exists(),
        "the real child must have started"
    );
    assert!(!result.ok && result.run_timed_out && result.run_cpu_accounting_failed);
    assert!(!result.run_cpu_timed_out);
    assert_eq!(result.outcomes.len(), 1);
    let outcome = &result.outcomes[0];
    assert!(outcome.aborted && !outcome.timed_out && !outcome.cpu_timed_out);
    assert!(
        outcome.returncode.is_some_and(|code| code < 0),
        "the real child must have been signalled"
    );
    let expected = "ABORTED (required whole-run CPU accounting was lost; CPU budget exhaustion was not established)";
    assert_eq!(outcome.reason, expected);
    let records = journal_records(&root);
    assert!(records
        .iter()
        .any(|record| record["event"] == "run_cpu_accounting_lost"));
    assert!(!records
        .iter()
        .any(|record| record["event"] == "run_timeout" || record["event"] == "run_cpu_timeout"));
    let ends = records
        .iter()
        .filter(|record| record["event"] == "step_end")
        .collect::<Vec<_>>();
    assert_eq!(ends.len(), 1);
    assert_eq!(ends[0]["reason"], expected);
}

#[test]
fn unreadable_run_cpu_accounting_is_not_budget_exhaustion() {
    if let Some(root) = cancellation_child("unreadable_run_cpu_accounting_is_not_budget_exhaustion")
    {
        accounting_loss_cancels_a_live_step(root, AccountingFault::Unreadable);
    }
}

#[test]
fn backwards_run_cpu_accounting_is_not_budget_exhaustion() {
    if let Some(root) = cancellation_child("backwards_run_cpu_accounting_is_not_budget_exhaustion")
    {
        accounting_loss_cancels_a_live_step(root, AccountingFault::Backwards);
    }
}
