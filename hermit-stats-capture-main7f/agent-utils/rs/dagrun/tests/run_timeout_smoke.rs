//! Two-sided bracket for the OUTER run budget and its ordering against the per-step budget.
//!
//! WHY AN OUTER BOUND EXISTS AT ALL. Per-step budgets cannot bound a run: any number of
//! individually-legal steps can sum past any ceiling. Before this, the only thing that stopped
//! such a run was an external job kill — and an external kill discards the very logs that would
//! explain why it was needed. So the bound that fires FIRST has to be one the runner enforces on
//! itself, because that is the only kind that can still write rows and hand back a verdict.
//!
//! THE ORDERING IS THE FEATURE: per-step < in-process run budget < scope `RuntimeMaxSec` < job
//! kill. Each level exists to stop the next one from being the thing that fires.
//!
//! | case                      | asserts |
//! |---------------------------|---------|
//! | `outer_budget_*`          | a run longer than its budget is cut EARLY, reports, and still writes a profile row per step |
//! | `misordered_*` (negative) | a step allowed to outlive the run is REFUSED before anything starts, and named |
//! | `clean_run_*` (control)   | a run inside its budget is untouched: exit 0, no cut, nothing aborted |
//! | `inner_before_outer_*`    | a single slow node dies at ITS OWN bound and the run continues — the inner bound fires first, so the failure is attributable to the node |

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("dagrun_rt_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Fixture { dir }
    }

    /// A three-step serial DAG: each step sleeps `sleep_s`, each declares `step_timeout_s`.
    fn dag(&self, name: &str, sleep_s: u32, step_timeout_s: u32) -> PathBuf {
        let steps: Vec<String> = ["one", "two", "three"]
            .iter()
            .map(|job| {
                format!(
                    r#"{{"group":"a","job":"{job}","cmd":"sleep {sleep_s}","timeout":{step_timeout_s},"cpu_timeout":600}}"#
                )
            })
            .collect();
        let path = self.dir.join(format!("{name}.json"));
        std::fs::write(&path, format!(r#"{{"steps":[{}]}}"#, steps.join(","))).unwrap();
        path
    }

    fn profiles(&self) -> PathBuf {
        self.dir.join("profiles")
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
    wall_s: f64,
}

fn run_dag(fx: &Fixture, dag: &Path, extra: &[&str]) -> Run {
    let bin = env!("CARGO_BIN_EXE_dagrun");
    let mut args: Vec<&str> = vec![
        "run",
        "--dag",
        dag.to_str().unwrap(),
        "-j",
        "1",
        "--no-profile-feedback",
        // The unboxed path is deliberate: the scheduler's own budget must bound it, not only the
        // boxed path (whose scope-level backstop is covered by the cgroup smoke tests). Use the
        // explicit opt-out so an inherited parent delegation cannot silently change this test's
        // route.
        "--unsafe-no-cgroups",
        "--perf-dir",
    ];
    let profiles = fx.profiles();
    args.push(profiles.to_str().unwrap());
    args.extend_from_slice(extra);

    let start = Instant::now();
    let out = Command::new(bin)
        .args(&args)
        .output()
        .expect("failed to spawn the built binary");
    Run {
        code: out.status.code(),
        text: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
        wall_s: start.elapsed().as_secs_f64(),
    }
}

/// Data rows across every profile CSV the run wrote (header excluded).
fn profile_rows(fx: &Fixture) -> usize {
    let Ok(entries) = std::fs::read_dir(fx.profiles()) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("step_profiles_")
        })
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .map(|text| text.lines().filter(|l| !l.trim().is_empty()).count() - 1)
        .sum()
}

/// POSITIVE: a run that would take ~12s is cut at its 6s budget, and REPORTS.
#[test]
fn outer_budget_cuts_a_long_run_early_and_still_writes_rows() {
    let fx = Fixture::new("outer");
    let dag = fx.dag("bounded", 4, 5); // 3 x 4s serial = ~12s natural; each step bounded at 5s
    let r = run_dag(&fx, &dag, &["--run-timeout", "6"]);

    assert_ne!(
        r.code,
        Some(0),
        "a run cut short by its outer budget must not report success:\n{}",
        r.text
    );
    // EARLY is the load-bearing word. Finishing at ~12s would mean the budget did nothing and the
    // next thing to stop the run would have been an outside killer.
    assert!(
        r.wall_s < 10.0,
        "the run should have been cut near its 6s budget, took {:.1}s:\n{}",
        r.wall_s,
        r.text
    );
    assert!(
        r.text.contains("RUN TIMEOUT"),
        "the run must say its own budget stopped it:\n{}",
        r.text
    );
    assert!(
        r.text.contains("cut short by the OUTER run budget"),
        "a step cancelled by the run budget must not be reported as eager-exit after a peer \
         failure — that sends a reader hunting for a failing peer that does not exist:\n{}",
        r.text
    );
    // THE EVIDENCE SURVIVES. This is the whole difference from an external kill.
    let rows = profile_rows(&fx);
    assert_eq!(
        rows, 2,
        "the completed step and the cut step must each leave a profile row; got {rows}:\n{}",
        r.text
    );
}

/// NEGATIVE: a step allowed to run as long as the whole run is refused before anything starts.
#[test]
fn misordered_budgets_are_refused_and_the_offenders_named() {
    let fx = Fixture::new("misordered");
    let dag = fx.dag("wide", 1, 600); // each step may run 600s under a 6s run budget
    let r = run_dag(&fx, &dag, &["--run-timeout", "6"]);

    assert_ne!(
        r.code,
        Some(0),
        "a mis-ordered budget must be refused, not accepted:\n{}",
        r.text
    );
    assert!(
        r.wall_s < 3.0,
        "the refusal must happen BEFORE any step runs, took {:.1}s:\n{}",
        r.wall_s,
        r.text
    );
    assert!(
        r.text.contains("REFUSING to run"),
        "the refusal must be loud:\n{}",
        r.text
    );
    assert!(
        r.text.contains("a.one (600s)") && r.text.contains("a.three (600s)"),
        "the refusal must NAME the offending steps and their budgets, or it is unactionable:\n{}",
        r.text
    );
    assert_eq!(
        profile_rows(&fx),
        0,
        "a refused run must not have executed anything"
    );
}

/// CONTROL: a run comfortably inside its budget is completely unaffected.
///
/// A fix that only shows the positive is indistinguishable from one that cuts every run short.
#[test]
fn clean_run_inside_its_budget_is_untouched() {
    let fx = Fixture::new("clean");
    let dag = fx.dag("quick", 1, 10); // ~3s natural, each step bounded at 10s
    let r = run_dag(&fx, &dag, &["--run-timeout", "60"]);

    assert_eq!(
        r.code,
        Some(0),
        "a run inside its outer budget must still pass:\n{}",
        r.text
    );
    assert!(
        !r.text.contains("RUN TIMEOUT"),
        "no budget was breached, so nothing should claim one was:\n{}",
        r.text
    );
    assert!(
        !r.text.contains("ABORT"),
        "no step should be cancelled in a clean run:\n{}",
        r.text
    );
    assert_eq!(
        profile_rows(&fx),
        3,
        "every step must still report normally:\n{}",
        r.text
    );
}

/// ORDERING: the INNER bound fires first, so the failure is attributed to the NODE.
///
/// The same DAG under the same outer budget: with a tight per-step bound the slow node dies on
/// its own budget, the run continues, and the report names that node. That is the difference
/// between "the strict-compat step timed out" and "the run overran" — only the first is
/// actionable, and the ordering is what produces it.
#[test]
fn inner_bound_fires_before_the_outer_one_so_the_node_is_named() {
    let fx = Fixture::new("inner");
    let dag = fx.dag("slow", 30, 2); // each step wants 30s but is bounded at 2s
    let r = run_dag(&fx, &dag, &["--run-timeout", "60"]);

    assert_ne!(
        r.code,
        Some(0),
        "steps killed at their own budget must fail the run:\n{}",
        r.text
    );
    assert!(
        r.wall_s < 30.0,
        "the per-step bound should have cut each node at ~2s, run took {:.1}s:\n{}",
        r.wall_s,
        r.text
    );
    assert!(
        !r.text.contains("RUN TIMEOUT"),
        "the OUTER bound must not be what fired — if it does, the overrun is attributed to the \
         run instead of to the node that caused it:\n{}",
        r.text
    );
    assert!(
        r.text.contains("[a.one]"),
        "the failing NODE must be named:\n{}",
        r.text
    );
    assert!(
        profile_rows(&fx) >= 1,
        "a node killed at its own bound must still write its row:\n{}",
        r.text
    );
}
