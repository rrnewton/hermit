#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! Measure what one system call costs natively and under each Hermit backend.
//!
//! The guest (`fixtures/getpid_loop.c`) makes N raw `getpid` calls and exits.
//! Hermit virtualizes the guest's clocks, so this harness times each whole run
//! from outside. For every variant it takes the median wall time at several N
//! and fits a least-squares line through those medians: the slope is the cost
//! of one call with process start-up removed, and the intercept is the fixed
//! cost of a run. The native fit is the baseline.
//!
//! After timing, one extra run per backend sets
//! `RUST_LOG=hermit::backend_stats=debug` and keeps the backend's own
//! statistics record, which says which path the calls took. That run must
//! succeed and print exactly one record for its backend, or the harness
//! exits 1. Timed runs inherit no logging variables, so they log nothing
//! beyond `--log=error`.
//!
//! Run `./benchmarks/getpid_cost.rs --help` for the flags.

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::fs;
use std::fs::File;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitCode;
use std::process::Stdio;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const BACKENDS: [&str; 2] = ["ptrace", "liteinst"];
const HERMIT_RUN_FLAGS: [&str; 6] = [
    "--log=error",
    "run",
    "--base-env=minimal",
    "--env=LC_ALL=C",
    "--no-virtualize-cpuid",
    "--max-timeslice=disabled",
];
const STATS_FILTER: &str = "hermit::backend_stats=debug";
const STATS_MARKER: &str = "backend run complete";
/// Removed from every run: an inherited `RUST_LOG` would make timed runs log,
/// and an inherited `HERMIT_LOG_FILE` would move the statistics record off
/// stderr. Only the statistics run sets `RUST_LOG`, to [`STATS_FILTER`].
const LOG_ENV_REMOVED: [&str; 3] = ["RUST_LOG", "HERMIT_LOG", "HERMIT_LOG_FILE"];
/// How often the harness checks whether a run has exited, so each wall time
/// can exceed the run by up to this much plus the host's timer slack.
const POLL_INTERVAL: Duration = Duration::from_micros(500);
const SIGKILL: i32 = 9;

unsafe extern "C" {
    fn kill(pid: i32, signal: i32) -> i32;
}

const USAGE: &str = "\
Usage: ./benchmarks/getpid_cost.rs [OPTIONS]

Measure the wall-clock cost of one raw getpid call natively and under each
selected Hermit backend. The cost is the least-squares slope of the median
run time across several call counts, so process start-up is excluded.

Options:
  --hermit CMD        Hermit command prefix, split on whitespace, so a wrapper
                      may come first; relative paths resolve from the current
                      directory (default: <repository>/target/release/hermit)
  --backends LIST     comma-separated subset of ptrace,liteinst
                      (default: ptrace,liteinst)
  --counts LIST       comma-separated call counts, at least two distinct
                      (default: 0,25000,50000,100000)
  --iterations N      measured runs per variant and count (default: 5)
  --warmups N         unmeasured runs per variant and count (default: 1)
  --timeout SECONDS   kill a run after this many seconds (default: 120)
  --cc CC             C compiler for the fixture (default: cc)
  --output PATH       JSON result file
                      (default: benchmarks/results/getpid-cost.json)
  -h, --help          print this help

Each Hermit run is:
  <hermit> --backend <backend> --log=error run --base-env=minimal \\
    --env=LC_ALL=C --no-virtualize-cpuid --max-timeslice=disabled -- \\
    <fixture> <count>

A sample counts only if it exits 0 and prints exactly \"calls=<count>\".
Failed and timed-out samples stay in the JSON; a variant with any failed
sample gets no fit, and the harness then exits 1.

Every run starts without RUST_LOG, HERMIT_LOG and HERMIT_LOG_FILE. After
timing, one run per backend at the largest count sets
RUST_LOG=hermit::backend_stats=debug; it must pass the same check and print
exactly one \"backend run complete backend=<backend> stats=\" record on
stderr, or the harness exits 1.

Wall times come from polling each run every 500 microseconds, so a sample
can read up to one poll interval long.

LiteInst needs libreverie_liteinst.so beside the Hermit binary, or the
HERMIT_LITEINST_RUNTIME environment variable naming it (read by Hermit).
";

struct Options {
    hermit: Option<Vec<String>>,
    backends: Vec<String>,
    counts: Vec<u64>,
    iterations: usize,
    warmups: usize,
    timeout: Duration,
    cc: String,
    output: Option<PathBuf>,
}

fn usage_error(message: &str) -> String {
    format!("{message}\nRun ./benchmarks/getpid_cost.rs --help for the accepted flags.")
}

fn parse_list<T>(
    raw: &str,
    flag: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Vec<T>, String> {
    let mut values = Vec::new();
    for item in raw
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        values.push(
            parse(item).ok_or_else(|| usage_error(&format!("{flag}: invalid value {item:?}")))?,
        );
    }
    if values.is_empty() {
        return Err(usage_error(&format!("{flag} needs at least one value")));
    }
    Ok(values)
}

/// Parses the flags after the program name; `Ok(None)` means print the help.
fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut options = Options {
        hermit: None,
        backends: BACKENDS.iter().map(|backend| backend.to_string()).collect(),
        counts: vec![0, 25_000, 50_000, 100_000],
        iterations: 5,
        warmups: 1,
        timeout: Duration::from_secs(120),
        cc: "cc".to_string(),
        output: None,
    };
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        if flag == "-h" || flag == "--help" {
            return Ok(None);
        }
        let mut value = || {
            args.next()
                .ok_or_else(|| usage_error(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--hermit" => {
                let words: Vec<String> = value()?.split_whitespace().map(str::to_string).collect();
                if words.is_empty() {
                    return Err(usage_error("--hermit needs a command"));
                }
                options.hermit = Some(words);
            }
            "--backends" => {
                options.backends = parse_list(&value()?, "--backends", |item| {
                    BACKENDS.contains(&item).then(|| item.to_string())
                })?;
            }
            "--counts" => {
                options.counts = parse_list(&value()?, "--counts", |item| item.parse().ok())?;
            }
            "--iterations" => {
                options.iterations = value()?
                    .parse()
                    .ok()
                    .filter(|iterations| *iterations >= 1)
                    .ok_or_else(|| {
                        usage_error("--iterations must be a whole number of at least 1")
                    })?;
            }
            "--warmups" => {
                options.warmups = value()?
                    .parse()
                    .map_err(|_| usage_error("--warmups must be a whole number"))?;
            }
            "--timeout" => {
                let seconds: f64 = value()?
                    .parse()
                    .ok()
                    .filter(|seconds: &f64| seconds.is_finite() && *seconds > 0.0)
                    .ok_or_else(|| usage_error("--timeout must be a positive number of seconds"))?;
                options.timeout = Duration::from_secs_f64(seconds);
            }
            "--cc" => options.cc = value()?,
            "--output" => options.output = Some(PathBuf::from(value()?)),
            _ => return Err(usage_error(&format!("unknown flag {flag:?}"))),
        }
    }
    let mut distinct = options.counts.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() != options.counts.len() || distinct.len() < 2 {
        return Err(usage_error(
            "--counts needs at least two distinct values and no duplicates",
        ));
    }
    let mut backends = options.backends.clone();
    backends.sort();
    backends.dedup();
    if backends.len() != options.backends.len() {
        return Err(usage_error("--backends contains a duplicate"));
    }
    if options.warmups.checked_add(options.iterations).is_none() {
        return Err(usage_error("--warmups plus --iterations is too large"));
    }
    Ok(Some(options))
}

fn capture(program: &str, args: &[&str], root: &Path) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("cannot run {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn repository_root() -> Result<PathBuf, String> {
    let cwd = std::env::current_dir()
        .map_err(|error| format!("cannot read the current directory: {error}"))?;
    capture("git", &["rev-parse", "--show-toplevel"], &cwd)
        .map(PathBuf::from)
        .map_err(|error| format!("{error}\nRun this harness from inside the Hermit checkout."))
}

enum Outcome {
    Ok,
    Failed(String),
    TimedOut,
}

struct Sample {
    variant: String,
    calls: u64,
    round: usize,
    warmup: bool,
    wall_ns: u128,
    outcome: Outcome,
}

/// Runs `argv` to completion with its output in files, so a large stderr can
/// never block the child on a full pipe while the parent polls.
fn run_bounded(
    argv: &[String],
    env: &[(&str, &str)],
    timeout: Duration,
    scratch: &Path,
) -> Result<(Duration, Option<std::process::ExitStatus>, String, String), String> {
    let stdout_path = scratch.join("stdout");
    let stderr_path = scratch.join("stderr");
    let stdout = File::create(&stdout_path)
        .map_err(|error| format!("cannot create {}: {error}", stdout_path.display()))?;
    let stderr = File::create(&stderr_path)
        .map_err(|error| format!("cannot create {}: {error}", stderr_path.display()))?;
    let mut command = Command::new(&argv[0]);
    for name in LOG_ENV_REMOVED {
        command.env_remove(name);
    }
    command
        .args(&argv[1..])
        .envs(env.iter().copied())
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        // Its own process group, so a timeout also kills anything a wrapper started.
        .process_group(0);
    let start = Instant::now();
    let mut child = command.spawn().map_err(|error| {
        format!(
            "cannot start {}: {error}\nCheck --hermit and --cc.",
            argv[0]
        )
    })?;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot wait for {}: {error}", argv[0]))?
        {
            break Some(status);
        }
        if start.elapsed() >= timeout {
            // SAFETY: the group leader has not been reaped, so its id still names this group.
            unsafe { kill(-(child.id() as i32), SIGKILL) };
            child
                .wait()
                .map_err(|error| format!("cannot reap {}: {error}", argv[0]))?;
            break None;
        }
        thread::sleep(POLL_INTERVAL);
    };
    let elapsed = start.elapsed();
    let read =
        |path: &Path| fs::read(path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let stdout = read(&stdout_path)
        .map_err(|error| format!("cannot read {}: {error}", stdout_path.display()))?;
    let stderr = read(&stderr_path)
        .map_err(|error| format!("cannot read {}: {error}", stderr_path.display()))?;
    Ok((elapsed, status, stdout, stderr))
}

fn classify(
    status: Option<std::process::ExitStatus>,
    stdout: &str,
    stderr: &str,
    calls: u64,
) -> Outcome {
    let Some(status) = status else {
        return Outcome::TimedOut;
    };
    let tail = stderr.lines().next_back().unwrap_or("").to_string();
    match (status.code(), status.signal()) {
        (Some(0), _) if stdout == format!("calls={calls}\n") => Outcome::Ok,
        (Some(0), _) => Outcome::Failed(format!("unexpected stdout {stdout:?}")),
        (Some(code), _) => Outcome::Failed(format!("exit {code}: {tail}")),
        (None, Some(signal)) => Outcome::Failed(format!("signal {signal}: {tail}")),
        (None, None) => Outcome::Failed(format!("unknown status: {tail}")),
    }
}

/// The lines of `stderr` that carry a backend statistics record.
fn stats_records(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .filter(|line| line.contains(STATS_MARKER))
        .map(str::to_string)
        .collect()
}

/// A statistics run counts only if the guest ran correctly and Hermit printed
/// exactly one record, for this backend. Anything else means the counters
/// describe another run, or nothing at all.
fn check_stats_run(backend: &str, outcome: &Outcome, records: &[String]) -> Result<(), String> {
    match outcome {
        Outcome::Ok => {}
        Outcome::Failed(reason) => return Err(format!("the run failed: {reason}")),
        Outcome::TimedOut => return Err("the run timed out".to_string()),
    }
    let own = format!("{STATS_MARKER} backend={backend} stats=");
    match records {
        [record] if record.contains(&own) => Ok(()),
        [record] => Err(format!("its only record is not {own:?}: {record:?}")),
        _ => Err(format!(
            "expected exactly one {STATS_MARKER:?} record, found {}",
            records.len()
        )),
    }
}

fn variant_argv(hermit: &[String], variant: &str, fixture: &Path, calls: u64) -> Vec<String> {
    let mut argv = Vec::new();
    if variant != "native" {
        argv.extend(hermit.iter().cloned());
        argv.push(format!("--backend={variant}"));
        argv.extend(HERMIT_RUN_FLAGS.iter().map(|flag| flag.to_string()));
        argv.push("--".to_string());
    }
    argv.push(fixture.display().to_string());
    argv.push(calls.to_string());
    argv
}

fn median(sorted: &[f64]) -> f64 {
    let middle = sorted.len() / 2;
    if !sorted.len().is_multiple_of(2) {
        sorted[middle]
    } else {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    }
}

/// Median and median absolute deviation, in the samples' unit.
fn median_and_mad(values: &[f64]) -> (f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let center = median(&sorted);
    let mut deviations: Vec<f64> = sorted.iter().map(|value| (value - center).abs()).collect();
    deviations.sort_by(f64::total_cmp);
    (center, median(&deviations))
}

/// Ordinary least squares through `(x, y)` points: `(slope, intercept)`, or
/// `None` when the line is undefined (fewer than two distinct x values) or
/// not finite.
fn fit_line(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let covariance: f64 = points
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let variance: f64 = points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum();
    let slope = covariance / variance;
    let intercept = mean_y - slope * mean_x;
    (slope.is_finite() && intercept.is_finite()).then_some((slope, intercept))
}

fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32))
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn json_list(items: impl IntoIterator<Item = String>) -> String {
    format!("[{}]", items.into_iter().collect::<Vec<_>>().join(", "))
}

fn short_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().split('.').next().unwrap_or("").to_string())
        .unwrap_or_default()
}

fn first_line_with(path: &str, prefix: &str) -> String {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with(prefix))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_string())
        })
        .unwrap_or_default()
}

fn sha256_of(path: &Path, root: &Path) -> String {
    capture("sha256sum", &[&path.display().to_string()], root)
        .ok()
        .and_then(|line| line.split_whitespace().next().map(str::to_string))
        .unwrap_or_default()
}

fn run(options: &Options) -> Result<bool, String> {
    let root = repository_root()?;
    let work = root
        .join("target")
        .join("hermit-benchmarks")
        .join("getpid-cost");
    fs::create_dir_all(&work)
        .map_err(|error| format!("cannot create {}: {error}", work.display()))?;
    let source = root
        .join("benchmarks")
        .join("fixtures")
        .join("getpid_loop.c");
    let fixture = work.join("getpid_loop");
    let source_arg = source.display().to_string();
    let fixture_arg = fixture.display().to_string();
    capture(
        &options.cc,
        &[
            "-O2",
            "-std=c11",
            "-Wall",
            "-Wextra",
            "-Werror",
            &source_arg,
            "-o",
            &fixture_arg,
        ],
        &root,
    )
    .map_err(|error| format!("{error}\nInstall a C11 compiler or pass --cc."))?;

    let hermit = options.hermit.clone().unwrap_or_else(|| {
        vec![
            root.join("target")
                .join("release")
                .join("hermit")
                .display()
                .to_string(),
        ]
    });
    let mut variants = vec!["native".to_string()];
    variants.extend(options.backends.iter().cloned());
    let started_utc = capture("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"], &root)?;
    let load_before = fs::read_to_string("/proc/loadavg").unwrap_or_default();

    // Rotate the variant order every round so no variant always runs first.
    let mut samples = Vec::new();
    for round in 0..options.warmups + options.iterations {
        let warmup = round < options.warmups;
        for &calls in &options.counts {
            for offset in 0..variants.len() {
                let variant = &variants[(round + offset) % variants.len()];
                let argv = variant_argv(&hermit, variant, &fixture, calls);
                let (elapsed, status, stdout, stderr) =
                    run_bounded(&argv, &[], options.timeout, &work)?;
                let outcome = classify(status, &stdout, &stderr, calls);
                if let Outcome::Failed(reason) = &outcome {
                    eprintln!("FAILED {variant} calls={calls} round={round}: {reason}");
                } else if let Outcome::TimedOut = outcome {
                    eprintln!("TIMED OUT {variant} calls={calls} round={round}");
                }
                samples.push(Sample {
                    variant: variant.clone(),
                    calls,
                    round,
                    warmup,
                    wall_ns: elapsed.as_nanos(),
                    outcome,
                });
            }
        }
    }

    // One unmeasured run per backend that asks the backend for its own counters.
    let stats_calls = *options.counts.iter().max().expect("at least two counts");
    let mut stats_runs = Vec::new();
    for backend in &options.backends {
        let argv = variant_argv(&hermit, backend, &fixture, stats_calls);
        let (_, status, stdout, stderr) =
            run_bounded(&argv, &[("RUST_LOG", STATS_FILTER)], options.timeout, &work)?;
        let records = stats_records(&stderr);
        let classified = classify(status, &stdout, &stderr, stats_calls);
        let verdict = check_stats_run(backend, &classified, &records);
        if let Err(reason) = &verdict {
            eprintln!("FAILED stats run {backend} calls={stats_calls}: {reason}");
        }
        let outcome = match classified {
            Outcome::Ok => "ok".to_string(),
            Outcome::Failed(reason) => format!("failed: {reason}"),
            Outcome::TimedOut => "timed out".to_string(),
        };
        stats_runs.push((backend.clone(), outcome, records, verdict));
    }
    let load_after = fs::read_to_string("/proc/loadavg").unwrap_or_default();

    // Summaries are computed from the raw rows above and nothing else.
    let mut all_ok = stats_runs.iter().all(|(.., verdict)| verdict.is_ok());
    let mut summaries = Vec::new();
    for variant in &variants {
        let measured: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.variant == *variant && !s.warmup)
            .collect();
        let failed = measured
            .iter()
            .filter(|s| !matches!(s.outcome, Outcome::Ok))
            .count();
        let mut points = Vec::new();
        for &calls in &options.counts {
            let walls: Vec<f64> = measured
                .iter()
                .filter(|s| s.calls == calls && matches!(s.outcome, Outcome::Ok))
                .map(|s| s.wall_ns as f64)
                .collect();
            if !walls.is_empty() {
                let (center, mad) = median_and_mad(&walls);
                points.push((calls, center, mad, walls.len()));
            }
        }
        let fit = (failed == 0)
            .then(|| {
                fit_line(
                    &points
                        .iter()
                        .map(|(calls, center, _, _)| (*calls as f64, *center))
                        .collect::<Vec<_>>(),
                )
            })
            .flatten();
        all_ok &= fit.is_some();
        summaries.push((variant.clone(), measured.len(), failed, points, fit));
    }
    let native_slope = summaries
        .iter()
        .find(|(variant, ..)| variant == "native")
        .and_then(|(.., fit)| fit.map(|(slope, _)| slope));

    println!(
        "{} measured runs per point; median wall time at each call count; fit = least squares over the medians",
        options.iterations
    );
    println!(
        "{:<10} {:>12} {:>16} {:>12}  per-count median ms (MAD ms)",
        "variant", "us/call", "extra us/call", "fixed ms"
    );
    for (variant, _, failed, points, fit) in &summaries {
        let per_count = points
            .iter()
            .map(|(calls, center, mad, n)| {
                format!("{calls}: {:.3} ({:.3}, n={n})", center / 1e6, mad / 1e6)
            })
            .collect::<Vec<_>>()
            .join("; ");
        match fit {
            Some((slope, intercept)) => {
                let extra = native_slope
                    .filter(|_| variant != "native")
                    .map(|native| format!("{:.3}", (slope - native) / 1e3))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{variant:<10} {:>12.3} {extra:>16} {:>12.2}  {per_count}",
                    slope / 1e3,
                    intercept / 1e6
                );
            }
            None => {
                let reason = if *failed > 0 {
                    format!("{failed} failed sample(s)")
                } else {
                    "least-squares line undefined".to_string()
                };
                println!(
                    "{variant:<10} {:>12} {:>16} {:>12}  {reason}; {per_count}",
                    "no fit", "-", "-"
                );
            }
        }
    }
    for (backend, outcome, records, verdict) in &stats_runs {
        let accepted = match verdict {
            Ok(()) => "accepted".to_string(),
            Err(reason) => format!("REJECTED: {reason}"),
        };
        println!("stats run {backend} calls={stats_calls} ({outcome}; {accepted}):");
        if records.is_empty() {
            println!("  no {STATS_MARKER:?} record on stderr");
        }
        for record in records {
            println!("  {record}");
        }
    }

    let output = options.output.clone().unwrap_or_else(|| {
        root.join("benchmarks")
            .join("results")
            .join("getpid-cost.json")
    });
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let head = capture("git", &["rev-parse", "HEAD"], &root)?;
    let dirty = !capture(
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
        &root,
    )?
    .is_empty();
    let script = root.join("benchmarks").join("getpid_cost.rs");
    // Relative prefix words resolve against the caller's directory, as they do when spawned.
    let prefix_digests = json_list(hermit.iter().filter(|word| Path::new(word).is_file()).map(
        |word| {
            format!(
                "{{\"path\": {}, \"sha256\": {}}}",
                json_string(word),
                json_string(
                    &std::path::absolute(word)
                        .map(|path| sha256_of(&path, &root))
                        .unwrap_or_default()
                )
            )
        },
    ));
    let summary_json = json_list(summaries.iter().map(|(variant, measured, failed, points, fit)| {
        let points_json = json_list(points.iter().map(|(calls, center, mad, n)| {
            format!("{{\"calls\": {calls}, \"median_ns\": {center}, \"mad_ns\": {mad}, \"samples\": {n}}}")
        }));
        let fit_json = match fit {
            Some((slope, intercept)) => format!("{{\"ns_per_call\": {slope}, \"intercept_ns\": {intercept}}}"),
            None => "null".to_string(),
        };
        format!(
            "{{\"variant\": {}, \"measured_samples\": {measured}, \"failed_samples\": {failed}, \"points\": {points_json}, \"fit\": {fit_json}}}",
            json_string(variant)
        )
    }));
    let samples_json = json_list(samples.iter().map(|sample| {
        let (status, reason) = match &sample.outcome {
            Outcome::Ok => ("ok", String::new()),
            Outcome::Failed(reason) => ("failed", reason.clone()),
            Outcome::TimedOut => ("timed_out", String::new()),
        };
        format!(
            "{{\"variant\": {}, \"calls\": {}, \"round\": {}, \"warmup\": {}, \"wall_ns\": {}, \"status\": {}, \"reason\": {}}}",
            json_string(&sample.variant),
            sample.calls,
            sample.round,
            sample.warmup,
            sample.wall_ns,
            json_string(status),
            json_string(&reason)
        )
    }));
    let stats_json = json_list(stats_runs.iter().map(|(backend, outcome, records, verdict)| {
        format!(
            "{{\"backend\": {}, \"calls\": {stats_calls}, \"rust_log\": {}, \"outcome\": {}, \"accepted\": {}, \"rejection\": {}, \"records\": {}}}",
            json_string(backend),
            json_string(STATS_FILTER),
            json_string(outcome),
            verdict.is_ok(),
            json_string(verdict.as_ref().err().map(String::as_str).unwrap_or("")),
            json_list(records.iter().map(|record| json_string(record)))
        )
    }));
    let example_argv = |variant: &str| {
        json_list(
            variant_argv(&hermit, variant, &fixture, 0)
                .iter()
                .map(|word| json_string(word)),
        )
    };
    let json = format!(
        "{{\n  \"schema\": \"hermit-getpid-cost/1\",\n  \"started_utc\": {},\n  \"host\": {},\n  \"kernel\": {},\n  \"cpu_model\": {},\n  \"load_before\": {},\n  \"load_after\": {},\n  \"cgroup_isolation\": \"none: this harness does not create a cgroup; record ambient load\",\n  \"repository_head\": {},\n  \"repository_dirty\": {},\n  \"producer_sha256\": {},\n  \"fixture_sha256\": {},\n  \"hermit_prefix\": {},\n  \"hermit_prefix_digests\": {},\n  \"command_shapes\": {{\"native\": {}, \"hermit\": {}}},\n  \"counts\": {},\n  \"warmups\": {},\n  \"iterations\": {},\n  \"timeout_seconds\": {},\n  \"order\": \"each round runs every count; within a count the variant order rotates by round\",\n  \"statistic\": \"median wall ns per (variant, count) with median absolute deviation; fit = ordinary least squares of median against count\",\n  \"poll_interval_ns\": {},\n  \"wall_clock_resolution\": \"each run is polled every poll_interval_ns, so a wall time can exceed the run by up to one poll interval plus the host's timer slack\",\n  \"log_environment\": {{\"removed_from_every_run\": {}, \"statistics_run_rust_log\": {}}},\n  \"summaries\": {},\n  \"stats_runs\": {},\n  \"samples\": {}\n}}\n",
        json_string(&started_utc),
        json_string(&short_hostname()),
        json_string(
            fs::read_to_string("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
        ),
        json_string(&first_line_with("/proc/cpuinfo", "model name")),
        json_string(load_before.trim()),
        json_string(load_after.trim()),
        json_string(&head),
        dirty,
        json_string(&sha256_of(&script, &root)),
        json_string(&sha256_of(&source, &root)),
        json_list(hermit.iter().map(|word| json_string(word))),
        prefix_digests,
        example_argv("native"),
        example_argv(
            options
                .backends
                .first()
                .map(String::as_str)
                .unwrap_or("ptrace")
        ),
        json_list(options.counts.iter().map(u64::to_string)),
        options.warmups,
        options.iterations,
        options.timeout.as_secs_f64(),
        POLL_INTERVAL.as_nanos(),
        json_list(LOG_ENV_REMOVED.iter().map(|name| json_string(name))),
        json_string(STATS_FILTER),
        summary_json,
        stats_json,
        samples_json,
    );
    fs::write(&output, json)
        .map_err(|error| format!("cannot write {}: {error}", output.display()))?;
    println!("raw rows and metadata: {}", output.display());
    Ok(all_ok)
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let options = match parse_options(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("getpid_cost: {message}");
            return ExitCode::from(2);
        }
    };
    match run(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => {
            eprintln!(
                "getpid_cost: some samples or statistics runs failed; see the FAILED lines above and the JSON"
            );
            ExitCode::from(1)
        }
        Err(message) => {
            eprintln!("getpid_cost: {message}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    fn exited(code: i32) -> Option<std::process::ExitStatus> {
        Some(std::process::ExitStatus::from_raw(code << 8))
    }

    #[test]
    fn classify_accepts_only_a_clean_exit_with_the_exact_count() {
        assert!(matches!(
            classify(exited(0), "calls=7\n", "", 7),
            Outcome::Ok
        ));
        assert!(matches!(
            classify(exited(0), "calls=6\n", "", 7),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            classify(exited(0), "calls=7", "", 7),
            Outcome::Failed(_)
        ));
        assert!(matches!(
            classify(exited(1), "calls=7\n", "boom\n", 7),
            Outcome::Failed(reason) if reason == "exit 1: boom"
        ));
        assert!(matches!(
            classify(
                Some(std::process::ExitStatus::from_raw(SIGKILL)),
                "",
                "",
                7
            ),
            Outcome::Failed(reason) if reason.starts_with("signal 9")
        ));
        assert!(matches!(
            classify(None, "calls=7\n", "", 7),
            Outcome::TimedOut
        ));
    }

    #[test]
    fn parse_options_rejects_counts_that_cannot_define_a_line() {
        assert!(parse_options(args(&["--counts", "0,100"])).is_ok());
        assert!(parse_options(args(&["--counts", "100"])).is_err());
        assert!(parse_options(args(&["--counts", "5,5"])).is_err());
        assert!(parse_options(args(&["--counts", "0,100,100"])).is_err());
    }

    #[test]
    fn parse_options_rejects_overflowing_round_counts_and_duplicates() {
        let max = usize::MAX.to_string();
        assert!(parse_options(args(&["--warmups", &max])).is_err());
        assert!(parse_options(args(&["--backends", "ptrace,ptrace"])).is_err());
        assert!(parse_options(args(&["--backends", "kvm"])).is_err());
        assert!(parse_options(args(&["--iterations", "0"])).is_err());
        assert!(parse_options(args(&["--timeout", "0"])).is_err());
        assert!(parse_options(args(&["--bogus"])).is_err());
        assert!(parse_options(args(&["--cc"])).is_err());
    }

    #[test]
    fn parse_options_help_and_defaults() {
        assert!(matches!(parse_options(args(&["--help"])), Ok(None)));
        assert!(matches!(parse_options(args(&["-h"])), Ok(None)));
        let options = parse_options(args(&[])).unwrap().unwrap();
        assert_eq!(options.backends, ["ptrace", "liteinst"]);
        assert_eq!(options.counts, [0, 25_000, 50_000, 100_000]);
        assert_eq!((options.iterations, options.warmups), (5, 1));
    }

    #[test]
    fn fit_line_recovers_an_exact_line_and_refuses_undefined_ones() {
        assert_eq!(fit_line(&[(0.0, 1.0), (10.0, 21.0)]), Some((2.0, 1.0)));
        assert_eq!(
            fit_line(&[(0.0, 1.0), (10.0, 21.0), (20.0, 41.0)]),
            Some((2.0, 1.0))
        );
        assert_eq!(fit_line(&[]), None);
        assert_eq!(fit_line(&[(3.0, 4.0)]), None);
        assert_eq!(fit_line(&[(3.0, 4.0), (3.0, 9.0)]), None);
    }

    #[test]
    fn median_and_mad_on_odd_and_even_samples() {
        assert_eq!(median_and_mad(&[3.0, 1.0, 2.0]), (2.0, 1.0));
        // Median 2.5; deviations 1.5, 0.5, 0.5, 97.5, so the MAD is 1.0.
        assert_eq!(median_and_mad(&[1.0, 2.0, 3.0, 100.0]), (2.5, 1.0));
    }

    #[test]
    fn check_stats_run_needs_one_record_for_its_own_backend() {
        let own = "x INFO backend run complete backend=liteinst stats=LiteInst ...".to_string();
        let other = "x INFO backend run complete backend=ptrace stats=metrics=none".to_string();
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[own.clone()]).is_ok());
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[]).is_err());
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[own.clone(), own.clone()]).is_err());
        assert!(check_stats_run("liteinst", &Outcome::Ok, &[other]).is_err());
        assert!(
            check_stats_run(
                "liteinst",
                &Outcome::Failed("exit 1".into()),
                &[own.clone()]
            )
            .is_err()
        );
        assert!(check_stats_run("liteinst", &Outcome::TimedOut, &[own]).is_err());
    }

    #[test]
    fn stats_records_keeps_only_marked_lines() {
        let stderr = "noise\nbackend run complete backend=ptrace stats=metrics=none\nmore\n";
        assert_eq!(
            stats_records(stderr),
            ["backend run complete backend=ptrace stats=metrics=none"]
        );
        assert!(stats_records("").is_empty());
    }

    #[test]
    fn hermit_argv_has_the_documented_shape() {
        let hermit = args(&["hermit"]);
        let fixture = Path::new("/f");
        assert_eq!(variant_argv(&hermit, "native", fixture, 3), ["/f", "3"]);
        assert_eq!(
            variant_argv(&hermit, "liteinst", fixture, 3),
            [
                "hermit",
                "--backend=liteinst",
                "--log=error",
                "run",
                "--base-env=minimal",
                "--env=LC_ALL=C",
                "--no-virtualize-cpuid",
                "--max-timeslice=disabled",
                "--",
                "/f",
                "3",
            ]
        );
    }
}
