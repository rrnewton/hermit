/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;

// Compile and lint the standalone guest as part of this Cargo integration target.
#[allow(dead_code)]
#[path = "../../tests/stress/concurrency.rs"]
mod concurrency_guest;

const FAST_SEED_COUNT: u64 = 10;
const SLOW_SEED_COUNT: u64 = 100;
const FAST_SEEDS: std::ops::Range<u64> = 0..FAST_SEED_COUNT;
const SLOW_SEEDS: std::ops::Range<u64> = 0..SLOW_SEED_COUNT;
const THREAD_COUNTS: [usize; 4] = [2, 4, 8, 16];
const COMMAND_TIMEOUT_SECONDS: u64 = 10;
const CAS_REPLAY_TIMEOUT_SECONDS: u64 = 60;

static HERMIT_RUN_LOCK: Mutex<()> = Mutex::new(());
static STRESS_BINARIES: OnceLock<StressBinaries> = OnceLock::new();

struct StressBinaries {
    concurrency: PathBuf,
    cas: PathBuf,
}

#[derive(Clone, Copy)]
enum FastExpectation {
    ExposedAtEveryThreadCount,
    ExposedBelowSixteenThreads,
    NeverExposed,
}

struct FastCategory {
    name: &'static str,
    expectation: FastExpectation,
}

const FAST_CATEGORIES: [FastCategory; 8] = [
    FastCategory {
        name: "atomic-lost-update",
        expectation: FastExpectation::ExposedAtEveryThreadCount,
    },
    FastCategory {
        name: "publish-ordering",
        expectation: FastExpectation::ExposedAtEveryThreadCount,
    },
    FastCategory {
        name: "producer-consumer",
        expectation: FastExpectation::ExposedBelowSixteenThreads,
    },
    FastCategory {
        name: "missing-barrier",
        expectation: FastExpectation::ExposedAtEveryThreadCount,
    },
    FastCategory {
        name: "condvar-lost-wakeup",
        expectation: FastExpectation::ExposedBelowSixteenThreads,
    },
    FastCategory {
        name: "mutex-correctness",
        expectation: FastExpectation::NeverExposed,
    },
    FastCategory {
        name: "rwlock-fairness",
        expectation: FastExpectation::NeverExposed,
    },
    FastCategory {
        name: "store-buffer",
        expectation: FastExpectation::NeverExposed,
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestOutcome {
    Clean,
    Exposed,
}

fn hermit_run_lock() -> MutexGuard<'static, ()> {
    HERMIT_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn command_output(mut command: Command, label: &str) -> Output {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{label} failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn compile_rust(source: &Path, output: &Path) {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("failed to create stress build directory");
    }
    let mut command = Command::new("rustc");
    command
        .args(["--edition=2024", "-C", "opt-level=2", "-C", "debuginfo=1"])
        .arg(source)
        .arg("-o")
        .arg(output);
    command_output(command, "stress guest compilation");
}

fn stress_binaries() -> &'static StressBinaries {
    STRESS_BINARIES.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("hermit-stress-workloads");
        let concurrency = build_root.join("concurrency");
        let cas = build_root.join("cas-sequence");
        compile_rust(
            &repository.join("tests/stress/concurrency.rs"),
            &concurrency,
        );
        compile_rust(&repository.join("flaky-tests/cas_sequence_easy.rs"), &cas);
        StressBinaries { concurrency, cas }
    })
}

fn timed_hermit_command(timeout_seconds: u64) -> Command {
    let mut command = Command::new("timeout");
    command
        .arg(format!("{timeout_seconds}s"))
        .arg(env!("CARGO_BIN_EXE_hermit"));
    command
}

fn stress_command(category: &str, threads: usize, seed: u64) -> Command {
    let mut command = timed_hermit_command(COMMAND_TIMEOUT_SECONDS);
    command
        .args([
            "run",
            "--base-env=minimal",
            "--chaos",
            "--sched-heuristic=random",
            "--preemption-timeout=disabled",
            "--no-virtualize-cpuid",
        ])
        .arg(format!("--seed={seed}"))
        .arg(&stress_binaries().concurrency)
        .arg(category)
        .arg(threads.to_string());
    command
}

fn run_guest(mut command: Command, label: &str) -> GuestOutcome {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {label}: {rendered}: {error}"));
    match output.status.code() {
        Some(0) => GuestOutcome::Clean,
        Some(1) => GuestOutcome::Exposed,
        Some(124) => panic!(
            "{label} exceeded its timeout: {rendered}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ),
        _ => panic!(
            "{label} failed unexpectedly: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ),
    }
}

fn count_exposures(category: &str, threads: usize, seeds: std::ops::Range<u64>) -> usize {
    seeds
        .map(|seed| {
            run_guest(
                stress_command(category, threads, seed),
                &format!("{category}, threads={threads}, seed={seed}"),
            )
        })
        .filter(|outcome| *outcome == GuestOutcome::Exposed)
        .count()
}

#[test]
#[ignore = "explicit fast stress tier"]
fn fast_chaos_matrix() {
    let _guard = hermit_run_lock();
    for category in FAST_CATEGORIES {
        for threads in THREAD_COUNTS {
            let exposures = count_exposures(category.name, threads, FAST_SEEDS);
            println!(
                "fast category={} threads={} exposed={}/{}",
                category.name, threads, exposures, FAST_SEED_COUNT
            );
            match category.expectation {
                FastExpectation::ExposedAtEveryThreadCount => assert!(
                    exposures > 0,
                    "chaos did not expose {} with {} threads in {} seeds",
                    category.name,
                    threads,
                    FAST_SEED_COUNT
                ),
                FastExpectation::ExposedBelowSixteenThreads if threads < 16 => assert!(
                    exposures > 0,
                    "chaos did not expose {} with {} threads in {} seeds",
                    category.name,
                    threads,
                    FAST_SEED_COUNT
                ),
                FastExpectation::ExposedBelowSixteenThreads => {}
                FastExpectation::NeverExposed => assert_eq!(
                    exposures, 0,
                    "correctness category {} failed with {} threads",
                    category.name, threads
                ),
            }
        }
    }
}

#[test]
#[ignore = "explicit slow stress tier"]
fn slow_race_matrix() {
    let _guard = hermit_run_lock();
    for category in ["producer-consumer", "condvar-lost-wakeup"] {
        let exposures = count_exposures(category, 16, SLOW_SEEDS);
        println!(
            "slow category={} threads=16 exposed={}/{}",
            category, exposures, SLOW_SEED_COUNT
        );
        assert!(
            exposures > 0,
            "chaos did not expose {category} with 16 threads in {} seeds",
            SLOW_SEED_COUNT
        );
    }
}

fn publish_ordering_schedule_command(seed: u64, schedule: &Path) -> Command {
    let mut command = timed_hermit_command(COMMAND_TIMEOUT_SECONDS);
    command
        .args([
            "run",
            "--base-env=minimal",
            "--chaos",
            "--sched-heuristic=random",
            "--preemption-timeout=disabled",
            "--no-virtualize-cpuid",
        ])
        .arg(format!("--seed={seed}"))
        .arg(format!("--record-preemptions-to={}", schedule.display()))
        .arg(&stress_binaries().concurrency)
        .args(["publish-ordering", "2"]);
    command
}

#[test]
fn schedule_bisect_localizes_publish_ordering_race() {
    let _guard = hermit_run_lock();
    let schedules = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create schedule-bisection directory");
    let mut good = None;
    let mut bad = None;

    for seed in FAST_SEEDS {
        let schedule = schedules
            .path()
            .join(format!("publish-ordering-{seed}.json"));
        let outcome = run_guest(
            publish_ordering_schedule_command(seed, &schedule),
            &format!("publish-ordering schedule seed={seed}"),
        );
        match outcome {
            GuestOutcome::Clean if good.is_none() => good = Some(schedule),
            GuestOutcome::Exposed if bad.is_none() => bad = Some(schedule),
            _ => {}
        }
        if good.is_some() && bad.is_some() {
            break;
        }
    }

    let good = good.expect("publish-ordering did not produce a passing schedule in 10 seeds");
    let bad = bad.expect("publish-ordering did not expose its race in 10 seeds");
    let report = schedules.path().join("bisect-report.json");
    let mut command = timed_hermit_command(CAS_REPLAY_TIMEOUT_SECONDS);
    command
        .arg("bisect")
        .arg(format!("--good={}", good.display()))
        .arg(format!("--bad={}", bad.display()))
        .arg(format!("--report-file={}", report.display()))
        .arg("--")
        .args([
            "--base-env=minimal",
            "--preemption-timeout=disabled",
            "--no-virtualize-cpuid",
        ])
        .arg(&stress_binaries().concurrency)
        .args(["publish-ordering", "2"]);

    let output = command_output(command, "publish-ordering schedule bisection");
    let stdout = String::from_utf8(output.stdout).expect("bisect stdout should be UTF-8");
    assert!(
        stdout.contains("Schedule divergence localized at bad event"),
        "missing localized event in:\n{stdout}"
    );
    assert!(
        stdout.matches("Stack trace for thread").count() >= 2,
        "missing divergence stack traces in:\n{stdout}"
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(&report).expect("failed to read schedule-bisection report"),
    )
    .expect("schedule-bisection report should be JSON");
    let first = report["critical_event1"]["event_index"]
        .as_u64()
        .expect("first critical event index should be an integer");
    let second = report["critical_event2"]["event_index"]
        .as_u64()
        .expect("second critical event index should be an integer");
    assert_eq!(second, first + 1);
    assert!(!report["critical_event1"]["stack"].is_null());
    assert!(!report["critical_event2"]["stack"].is_null());
}

fn cas_search_command(seed: u64, schedule: &Path) -> Command {
    let mut command = timed_hermit_command(COMMAND_TIMEOUT_SECONDS);
    command
        .args([
            "run",
            "--base-env=minimal",
            "--chaos",
            "--imprecise-timers",
            "--preemption-timeout=10000000",
            "--no-virtualize-cpuid",
        ])
        .arg(format!("--seed={seed}"))
        .arg(format!("--record-preemptions-to={}", schedule.display()))
        .arg(&stress_binaries().cas);
    command
}

fn cas_replay_command(seed: u64, schedule: &Path) -> Command {
    let mut command = timed_hermit_command(CAS_REPLAY_TIMEOUT_SECONDS);
    command
        .args([
            "run",
            "--base-env=minimal",
            "--chaos",
            "--preemption-timeout=10000000",
            "--no-virtualize-cpuid",
        ])
        .arg(format!("--seed={seed}"))
        .arg(format!("--replay-preemptions-from={}", schedule.display()))
        .arg(&stress_binaries().cas);
    command
}

#[test]
#[ignore = "explicit PMU-dependent slow stress tier"]
fn slow_cas_search_and_replay() {
    let _guard = hermit_run_lock();
    let schedules = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .expect("failed to create CAS schedule directory");
    let mut clean_seed = None;
    let mut failing = None;
    let mut exposures = 0;

    for seed in SLOW_SEEDS {
        let schedule = schedules.path().join(format!("preemptions-{seed}.json"));
        match run_guest(
            cas_search_command(seed, &schedule),
            &format!("imprecise CAS search seed={seed}"),
        ) {
            GuestOutcome::Clean => {
                clean_seed.get_or_insert(seed);
            }
            GuestOutcome::Exposed => {
                exposures += 1;
                if failing.is_none() {
                    failing = Some((seed, schedule));
                }
            }
        }
    }

    let clean_seed = clean_seed.expect("CAS search did not find a passing schedule");
    let (failing_seed, schedule) =
        failing.expect("CAS search did not expose the race in 100 seeds");
    assert!(
        schedule.is_file(),
        "CAS failure did not record preemptions to {}",
        schedule.display()
    );
    println!(
        "CAS search clean_seed={clean_seed} failing_seed={failing_seed} exposed={exposures}/100"
    );

    assert_eq!(
        run_guest(
            cas_replay_command(failing_seed, &schedule),
            &format!("precise CAS replay seed={failing_seed}"),
        ),
        GuestOutcome::Exposed,
        "precise replay did not reproduce the recorded CAS failure"
    );
}
