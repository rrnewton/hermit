/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Integration coverage for `hermit analyze`, the root-cause search that locates
//! the racing instructions behind a nondeterministic failure.
//!
//! These mirror the Buck `analyze_*` targets from `tests/BUCK`, which drive
//! `tests/util/hermit_analyze_test.sh`: each scenario runs `hermit analyze
//! --search` over a chaotic schedule and asserts that a guest stack trace is
//! printed, optionally blaming a specific source line.
//!
//! The search bisects over chaos schedules and relies on PMU branch counters
//! plus working user/mount namespaces, so the tests are `#[ignore]`d by default
//! (like the `chaos_buck_*` cases in `hermit_modes.rs`) and are exercised
//! explicitly by `scripts/validate.rs`. Run them with:
//!
//! ```text
//! cargo test -p hermit-run --test analyze -- --ignored
//! ```

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

static ANALYZE_LOCK: Mutex<()> = Mutex::new(());
static WORKLOADS: OnceLock<AnalyzeWorkloads> = OnceLock::new();
const ANALYZE_SKID_MARGIN_ENV: &str = "HERMIT_ANALYZE_SKID_MARGIN";

/// Guest binaries whose races `hermit analyze` is expected to pinpoint.
///
/// They are compiled beneath `CARGO_TARGET_TMPDIR` (inside the repository's
/// `target/` directory) rather than `/tmp`, because `hermit analyze` bind-mounts
/// its own workspace over `/tmp` in the guest and would otherwise shadow the
/// binary.
struct AnalyzeWorkloads {
    hello_race: PathBuf,
    racewrite_nostdlib: PathBuf,
    nanosleep_nocrash: PathBuf,
    race_then: PathBuf,
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn analyze_lock() -> MutexGuard<'static, ()> {
    ANALYZE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn add_calibrated_skid_margin(command: &mut Command, label: &str) {
    let Some(value) = std::env::var_os(ANALYZE_SKID_MARGIN_ENV) else {
        return;
    };
    let value = value.to_string_lossy();
    let margin = value.parse::<u64>().unwrap_or_else(|error| {
        panic!(
            "{label}: {ANALYZE_SKID_MARGIN_ENV} must be a positive integer, got {value:?}: {error}"
        )
    });
    assert!(
        margin > 0,
        "{label}: {ANALYZE_SKID_MARGIN_ENV} must be greater than zero"
    );
    eprintln!("{label}: using calibrated PMU skid margin {margin} RCB");
    command.arg(format!("--run-arg=--skid-margin={margin}"));
}

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

fn compile_c_pthread(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-O0", "-g", "-pthread", "-D_GNU_SOURCE"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation");
}

fn compile_c_without_libc(source: &Path, output: &Path) {
    let mut command = Command::new("cc");
    command
        .args(["-g", "-nostdlib"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "C workload compilation without libc");
}

fn compile_rust(source: &Path, output: &Path) {
    let mut command = Command::new("rustc");
    command
        .args(["--edition=2024", "-C", "debuginfo=1"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "Rust workload compilation");
}

fn workloads() -> &'static AnalyzeWorkloads {
    WORKLOADS.get_or_init(|| {
        let repository = repository();
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-analyze-workloads");
        fs::create_dir_all(&build_root).expect("failed to create analyze workload directory");

        let hello_race = build_root.join("hello_race");
        compile_rust(&repository.join("flaky-tests/hello_race.rs"), &hello_race);

        let racewrite_nostdlib = build_root.join("racewrite_nostdlib");
        compile_c_without_libc(
            &repository.join("tests/c/simple/racewrite_nostdlib.c"),
            &racewrite_nostdlib,
        );

        let nanosleep_nocrash = build_root.join("nanosleep_nocrash");
        compile_c_pthread(
            &repository.join("tests/c/simple/nanosleep-threads-nocrash.c"),
            &nanosleep_nocrash,
        );

        let race_then = build_root.join("analyze_race_then");
        compile_c_pthread(
            &repository.join("hermit-cli/tests/fixtures/analyze_race_then.c"),
            &race_then,
        );

        AnalyzeWorkloads {
            hello_race,
            racewrite_nostdlib,
            nanosleep_nocrash,
            race_then,
        }
    })
}

/// Runs `hermit analyze --search` over `guest` and returns its report file,
/// asserting a successful exit and that at least one guest stack trace was
/// printed. `expected_output`, when non-empty, must also appear in the
/// combined stdout+stderr (used to pin the blamed source line).
///
/// Mirrors `tests/util/hermit_analyze_test.sh`: `--analyze-seed=0 --search`,
/// then a chaotic run with a tight preemption timeout so the search has enough
/// scheduling change points to expose the race.
fn run_analyze(
    label: &str,
    guest: &Path,
    analyze_opts: &[&str],
    expected_output: &str,
) -> serde_json::Value {
    let _guard = analyze_lock();
    let report_dir = tempfile::tempdir().expect("failed to create analyze report directory");
    let report_file = report_dir.path().join("report.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    add_calibrated_skid_margin(&mut command, label);
    command
        .arg(format!("--report-file={}", report_file.display()))
        .args(["--analyze-seed=0", "--search", "--"])
        // Arguments forwarded to the underlying `hermit run` invocations:
        .args(["--chaos", "--summary", "--max-timeslice=400000", "--"])
        .arg(guest);

    let output = command_output(command, label);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        combined.contains("Stack trace for thread"),
        "{label}: hermit analyze did not print a guest stack trace\n{combined}"
    );
    // --trial-timeout=auto: the runs after the target run are bounded.
    assert!(
        combined.contains("Runs from here on are bounded at 60 s of wall-clock time"),
        "{label}: no trial bound\n{combined}"
    );
    if !expected_output.is_empty() {
        assert!(
            combined.contains(expected_output),
            "{label}: expected {expected_output:?} in hermit analyze output\n{combined}"
        );
    }

    // Analyze writes the branch-counter warning to stderr at most once, before
    // its first trial, and the report repeats exactly that line, without its
    // newline; it carries none where analyze wrote none.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("WARNING: this host's retired-branch counter"))
        .collect();
    assert!(warnings.len() <= 1, "{label}: {warnings:?}\n{combined}");
    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report_file).expect("hermit analyze should write its report file"),
    )
    .expect("the analyze report is JSON");
    assert_eq!(
        report
            .get("branch_counter_warning")
            .map(|value| value.as_str().expect("branch_counter_warning is a string")),
        warnings.first().copied(),
        "{label}: {report}\n{combined}"
    );
    let report = fs::read_to_string(&report_file)
        .unwrap_or_else(|error| panic!("{label}: no report file: {error}\n{combined}"));
    serde_json::from_str(&report)
        .unwrap_or_else(|error| panic!("{label}: unreadable report file: {error}\n{report}"))
}

/// Exercises analyze's fail-closed endpoint check for a workload whose target
/// outcome now also occurs under the non-chaos baseline schedule.
fn run_analyze_expect_baseline_collision(label: &str, guest: &Path, analyze_opts: &[&str]) {
    let _guard = analyze_lock();
    let report_dir = tempfile::tempdir().expect("failed to create analyze report directory");
    let report_file = report_dir.path().join("report.json");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command.arg("analyze");
    command.args(analyze_opts);
    add_calibrated_skid_margin(&mut command, label);
    command
        .arg(format!("--report-file={}", report_file.display()))
        .args(["--analyze-seed=0", "--search", "--"])
        .args(["--chaos", "--summary", "--preemption-timeout=400000", "--"])
        .arg(guest);

    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    assert!(
        !output.status.success(),
        "{label} unexpectedly found distinct target and baseline schedules\n{combined}"
    );
    assert!(
        combined.contains("baseline run matched target criteria when it should not"),
        "{label}: analyze did not reject indistinguishable endpoints\n{combined}"
    );
}

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_hello_race() {
    run_analyze(
        "analyze hello_race",
        &workloads().hello_race,
        &["--run-arg=--base-env=host"],
        "flaky-tests/hello_race.rs:37",
    );
}

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_racewrite_nostdlib() {
    run_analyze(
        "analyze racewrite_nostdlib",
        &workloads().racewrite_nostdlib,
        &[
            "--selfcheck",
            "--run-arg=--base-env=empty",
            "--target-exit-code=0",
            // The current non-chaos baseline is barfoo; analyze the opposite
            // write order so the endpoints remain behaviorally distinct.
            "--target-stdout=foobar",
        ],
        // The racing `write` syscall lives at this line in the guest source.
        "racewrite_nostdlib.c:35",
    );
}

/// `--prefix-search` finds the same race from the target schedule alone: the
/// deciding event is the parent's return from fork, and the thread the
/// scheduler would run there instead is the child at its racing write. The
/// parent's slice ends right after that return in the target schedule, and a
/// trial that stops there leaves that slice end to the scheduler; one that
/// replayed it would name the parent's next event, one too late.
#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_racewrite_nostdlib_prefix_search() {
    let report = run_analyze(
        "analyze racewrite_nostdlib --prefix-search",
        &workloads().racewrite_nostdlib,
        &[
            "--prefix-search",
            "--run-arg=--base-env=empty",
            "--target-exit-code=0",
            "--target-stdout=foobar",
        ],
        "racewrite_nostdlib.c:35",
    );
    let header = report["header"].as_str().unwrap_or_default();
    assert!(header.contains("Syscall(fork, Posthook)"), "{report}");
    let thread = |event: &str| report[event]["stack"]["thread_id"].as_i64();
    assert!(thread("critical_event1").is_some(), "{report}");
    assert!(thread("critical_event2").is_some(), "{report}");
    assert_ne!(
        thread("critical_event1"),
        thread("critical_event2"),
        "{report}"
    );
}

#[test]
#[ignore = "slow: bisecting chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_nanosleep_threads_rejects_indistinguishable_baseline() {
    run_analyze_expect_baseline_collision(
        "analyze nanosleep-threads baseline collision",
        &workloads().nanosleep_nocrash,
        &["--run-arg=--base-env=empty", "--target-exit-code=0"],
    );
}

/// The first chaos seed under which `analyze_race_then THEN`, run with
/// `run_args`, takes the order that finishes ("b-first", exit 0). A run in
/// the other order may not end by itself, so each is bounded at 5 s.
fn race_then_target_seed(then: &str, run_args: &[&str]) -> u64 {
    let guest = &workloads().race_then;
    (0..40)
        .find(|seed| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
            command
                .args(["run", "--chaos", "--timeout=5"])
                .args(run_args)
                .arg(format!("--seed={seed}"))
                .arg("--")
                .arg(guest)
                .arg(then);
            let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
            output.status.success() && output.stdout == b"b-first\n"
        })
        .expect("no chaos seed in 0..40 lets thread b win the race")
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    )
}

/// `hermit analyze` over `analyze_race_then THEN` from a target seed, with
/// `--trial-timeout=3` and more analyze options and run arguments: its
/// output and report, if it wrote one.
fn analyze_race_then_with(
    then: &str,
    analyze_opts: &[&str],
    run_args: &[&str],
) -> (Output, Option<serde_json::Value>) {
    let seed = race_then_target_seed(then, run_args);
    let _guard = analyze_lock();
    let report_dir = tempfile::tempdir().expect("failed to create analyze report directory");
    let report_file = report_dir.path().join("report.json");
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .arg("analyze")
        .args([
            "--target-stdout=b-first",
            "--target-exit-code=0",
            "--trial-timeout=3",
        ])
        .args(analyze_opts)
        .arg(format!("--run1-seed={seed}"))
        .arg(format!("--report-file={}", report_file.display()))
        .args(["--", "--chaos"])
        .args(run_args)
        .arg("--")
        .arg(&workloads().race_then)
        .arg(then);
    let output = hermit_test::hermit_output(&format!("{command:?}"), &mut command);
    let report = fs::read_to_string(&report_file).ok().map(|text| {
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("unreadable report: {error}\n{}", combined_output(&output))
        })
    });
    (output, report)
}

/// [`analyze_race_then_with`], asserting that analyze succeeded and wrote a
/// report naming a critical event: the combined output and the report.
fn analyze_race_then(
    label: &str,
    then: &str,
    analyze_opts: &[&str],
) -> (String, serde_json::Value) {
    let (output, report) = analyze_race_then_with(then, analyze_opts, &[]);
    let combined = combined_output(&output);
    assert!(
        output.status.success(),
        "{label} failed: {}\n{combined}",
        output.status
    );
    let report = report.unwrap_or_else(|| panic!("{label}: no report\n{combined}"));
    assert!(
        report["critical_event1"]["event_index"].is_u64(),
        "{label}: {report}"
    );
    (combined, report)
}

const BOUND_REACHED: &str = "reached hermit analyze's bound of 3 s per run \
                             (--trial-timeout=SECONDS raises it, --trial-timeout=off lifts it)); \
                             counted as the baseline condition";

/// Every prefix trial in which thread a wins the race spins for ever, the
/// run without chaos (the prefix search's baseline) among them. Each is
/// bounded by `--trial-timeout` and counts as the baseline condition, so
/// the search still finds the deciding event. Before the bound, the first
/// such trial never ended.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_bounds_trials_that_never_end() {
    let label = "analyze --prefix-search analyze_race_then spin";
    let (combined, _) = analyze_race_then(label, "spin", &["--prefix-search"]);
    assert!(
        combined.contains(BOUND_REACHED),
        "{label}: no trial reached the bound\n{combined}"
    );
}

/// The default search's baseline is the run without chaos, which here spins
/// for ever. The bound ends it too, and with no baseline schedule analyze
/// searches within the target schedule instead. Before, the baseline run
/// never ended, whatever --trial-timeout said.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_searches_the_target_schedule_when_the_baseline_never_ends() {
    let label = "analyze analyze_race_then spin";
    let (combined, _) = analyze_race_then(label, "spin", &[]);
    assert!(
        combined.contains(
            "the baseline run (non-chaos (0 extra preemptions) run) did not finish, so it \
             recorded no schedule to bisect against"
        ) && combined.contains("Searching within the target schedule instead"),
        "{label}: no fallback\n{combined}"
    );
}

/// Every prefix trial in which thread a wins the race asks for something
/// Hermit refuses at run time (exit 122), the run without chaos (the prefix
/// search's baseline) among them. Each counts as the baseline condition, with
/// a warning, and the report says how many were refused. Before, the first
/// refused trial ended the whole analysis with exit 122. The refusal is
/// Detcore's for a process-directed signal while several threads live
/// (https://github.com/rrnewton/hermit/issues/4034); once that is supported,
/// the fixture needs another refused operation.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_counts_a_refused_trial_as_the_baseline() {
    let label = "analyze --prefix-search analyze_race_then refuse";
    let (combined, report) = analyze_race_then(label, "refuse", &["--prefix-search"]);
    assert!(
        combined.contains(
            "Hermit refused this trial when the guest reached an operation it does not support"
        ),
        "{label}: no trial was refused\n{combined}"
    );
    assert!(
        report["header"]
            .as_str()
            .unwrap()
            .contains("trial(s) were refused by Hermit (exit 122) and counted as not reaching"),
        "{label}: {report}"
    );
}

/// The target run itself is refused: it cannot be studied, so analyze ends
/// with Hermit's refusal (exit 122) rather than counting it.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_ends_with_a_refused_target_run() {
    let label = "analyze analyze_race_then refuse, target refused";
    let guest = &workloads().race_then;
    // The run without chaos takes the order Hermit refuses.
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(["analyze", "--prefix-search", "--target-stdout=a-first"])
        .args(["--", "--timeout=30", "--"])
        .arg(guest)
        .arg("refuse");
    let output = hermit_test::hermit_output(label, &mut command);
    let combined = combined_output(&output);
    assert_eq!(output.status.code(), Some(122), "{label}\n{combined}");
    assert!(
        combined.contains("HERMIT_POLICY_REFUSAL"),
        "{label}\n{combined}"
    );
}

/// A race found only at a short timeslice: at the run's own 100 us, thread a
/// is preempted before it claims even without chaos, so the scheduler alone
/// meets the target and gives the prefix search nothing to bisect against.
/// The search says so and continues at hermit's default 200 ms, where it
/// does not. Before, the analysis ended there with exit 125.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_continues_prefixes_at_a_coarser_timeslice() {
    let label = "analyze --prefix-search analyze_race_then late-a --max-timeslice=100000";
    let (output, report) =
        analyze_race_then_with("late-a", &["--prefix-search"], &["--max-timeslice=100000"]);
    let combined = combined_output(&output);
    assert!(
        output.status.success(),
        "{label}: {}\n{combined}",
        output.status
    );
    assert!(
        combined.contains(
            "Continuing without chaos at a 100000 ns timeslice, the scheduler alone, replaying \
             none of the target schedule, meets the target"
        ),
        "{label}\n{combined}"
    );
    let report = report.unwrap_or_else(|| panic!("{label}: no report\n{combined}"));
    assert!(
        report["header"]
            .as_str()
            .unwrap()
            .contains("scheduling without chaos at a 200000000 ns timeslice does not meet"),
        "{label}: {report}"
    );
}

/// The same race with `--continuation-timeslice=100000`: there is no other
/// continuation to try, so analyze fails, saying that the outcome does not
/// depend on a chaos decision and what to run or pass instead.
#[test]
#[ignore = "slow: replays chaos schedules; requires PMU branch counters and working mount namespaces"]
fn analyze_explains_a_target_the_scheduler_meets_alone() {
    let label = "analyze --prefix-search analyze_race_then late-a --continuation-timeslice=100000";
    let (output, _) = analyze_race_then_with(
        "late-a",
        &["--prefix-search", "--continuation-timeslice=100000"],
        &["--max-timeslice=100000"],
    );
    let combined = combined_output(&output);
    assert!(!output.status.success(), "{label}\n{combined}");
    for expected in [
        "no event of the target schedule decides the outcome: without chaos, the scheduler alone \
         already meets the target criteria at a 100000 ns timeslice",
        "prefix_round_001.repro",
        "pass --continuation-timeslice",
        "--run2-seed",
    ] {
        assert!(
            combined.contains(expected),
            "{label}: no {expected:?}\n{combined}"
        );
    }
}
