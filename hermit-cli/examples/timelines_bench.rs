/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): executions per second through the
//! `Timelines` trait on the KVM implementation, against re-execution.
//!
//! ```text
//! timelines_bench dir=DIR launcher=CMD global=ARGS run=ARGS
//!     [at=I] [copies=N] [rounds=R] [placement=P] [copy=C] [fresh=1] [settle=MS]
//!     [backups=N1:N2:... [resume=K]] [chain=K] -- GUEST...
//! ```
//!
//! `placement=` is the copies' placement policy: `self` (the default),
//! `unpinned`, or `pool:A:B` (`cpus=A:B` is the same pool). `copy=` is
//! their copy policy: `fork-and-run` (the default), `eager` or `backup`.
//! `backups=` measures dormant backups instead of rounds: at request I, for
//! each N, it makes N copies under `copy=` and waits until every one is
//! ready, reading the fork's wall time and the run's cgroup memory before and
//! after; then it resumes K of them (default 2) to the end and discards the
//! rest. `chain=K` measures checkpoints along one run instead, rr's way: it
//! captures the run at request I, 2I, ... KI, each time running on in a copy
//! of the last checkpoint, then goes back to the first and the middle one.
//!
//! `launcher`, `global` and `run` are as for `timelines_gate`. The bench
//! captures a run paused at choice request I, then runs R rounds; a round
//! forks N copies and resumes them at once to the end, copy k with its own
//! answer to request I (copy 0 the run's own). A round's wall time counts
//! the fork, every copy's repair and run, and its exit. With `fresh=1` it
//! then runs R rounds of N fresh launches at once (re-execution: each runs
//! the whole guest, prefix included). Prints each round and the executions
//! per second of each kind. Without `cpus=`, each copy places itself on an
//! idle CPU of its own, as each fresh launch does (`KvmTimelines::cpus`).
//! Before that default a copy kept the template's one pinned CPU, so copies
//! resumed at once shared it (MEASURED, race 60 at request 3, 4 at once: 8.3
//! to 11.4 executions per second then, 21.8 to 26.8 placed, 24.6 to 27.2
//! pinned by hand with `cpus=`). Every same-future execution
//! (copy 0 of a round, every fresh launch) must reproduce the reference
//! run's standard output and exit status, or the bench fails;
//! `timelines_gate` is the full equivalence check.

use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use hermit::timelines::Batch;
use hermit::timelines::Checkpoint;
use hermit::timelines::CopyPolicy;
use hermit::timelines::Observation;
use hermit::timelines::Part;
use hermit::timelines::PendingRequest;
use hermit::timelines::Placement;
use hermit::timelines::RunOutcome;
use hermit::timelines::RunSpec;
use hermit::timelines::Stop;
use hermit::timelines::StopAt;
use hermit::timelines::Timelines;
use hermit::timelines::kvm::KvmCheckpoint;
use hermit::timelines::kvm::KvmTimelines;

struct Options {
    dir: PathBuf,
    launcher: Vec<String>,
    global: Vec<String>,
    run: Vec<String>,
    at: u64,
    copies: usize,
    rounds: usize,
    placement: Placement,
    copy: CopyPolicy,
    backups: Vec<usize>,
    resume: usize,
    chain: usize,
    fresh: bool,
    /// A pause after each round, not counted: lets the round's processes
    /// finish exiting before the next round starts.
    settle: Duration,
    guest: Vec<String>,
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn options() -> Result<Options, String> {
    let mut options = Options {
        dir: PathBuf::new(),
        launcher: Vec::new(),
        global: Vec::new(),
        run: Vec::new(),
        at: 3,
        copies: 4,
        rounds: 5,
        placement: Placement::default(),
        copy: CopyPolicy::default(),
        backups: Vec::new(),
        resume: 2,
        chain: 0,
        fresh: false,
        settle: Duration::ZERO,
        guest: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    for arg in args.by_ref() {
        if arg == "--" {
            break;
        }
        let (key, value) = arg
            .split_once('=')
            .ok_or(format!("{arg:?} is not key=value"))?;
        let number = |what: &str| -> Result<usize, String> {
            value.parse().map_err(|e| format!("{what}: {e}"))
        };
        match key {
            "dir" => options.dir = PathBuf::from(value),
            "launcher" => options.launcher = words(value),
            "global" => options.global = words(value),
            "run" => options.run = words(value),
            "at" => options.at = number("at")? as u64,
            "copies" => options.copies = number("copies")?,
            "rounds" => options.rounds = number("rounds")?,
            "cpus" => options.placement = Placement::parse(&format!("pool:{value}"))?,
            "placement" => options.placement = Placement::parse(value)?,
            "copy" => {
                options.copy = match value {
                    "fork-and-run" => CopyPolicy::FORK_AND_RUN,
                    "eager" => CopyPolicy::EAGER,
                    "backup" => CopyPolicy::BACKUP,
                    other => {
                        return Err(format!(
                            "copy: {other:?} is not fork-and-run, eager or backup"
                        ));
                    }
                }
            }
            "backups" => {
                options.backups = value
                    .split(':')
                    .map(|n| n.parse::<usize>().map_err(|e| format!("backups: {e}")))
                    .collect::<Result<_, _>>()?
            }
            "resume" => options.resume = number("resume")?,
            "chain" => options.chain = number("chain")?,
            "fresh" => options.fresh = value == "1",
            "settle" => options.settle = Duration::from_millis(number("settle")? as u64),
            other => return Err(format!("unknown key {other:?}")),
        }
    }
    options.guest = args.collect();
    if options.guest.is_empty() || options.launcher.is_empty() {
        return Err("launcher= and a guest after -- are required".into());
    }
    if options.copies == 0 || options.rounds == 0 {
        return Err("copies= and rounds= must be at least 1".into());
    }
    Ok(options)
}

/// The answer tape byte `b` gives `request`, as detcore's TapeSource gives it.
fn tape_answer(request: &PendingRequest, b: u8) -> u64 {
    use detcore::choice::ChoiceSource;
    let kind = match request.kind.as_str() {
        "sched.priority" => detcore::choice::ChoiceKind::SchedPriority,
        _ => detcore::choice::ChoiceKind::SchedSlice,
    };
    let request = detcore::choice::ChoiceRequest {
        index: request.index,
        kind,
        lo: request.lo,
        hi: request.hi,
        default: request.default,
        scale: request.scale,
        dtid: detcore::DetTid::from_raw(0),
        thread_rcbs: 0,
        site: "timelines-bench",
    };
    detcore::choice::TapeSource::new(vec![b])
        .answer(&request)
        .unwrap_or(request.default)
}

fn bytes(part: &Option<Part>) -> Vec<u8> {
    part.as_ref()
        .map(|p| p.read().unwrap_or_default())
        .unwrap_or_default()
}

fn exited<P>(stop: Stop<P>, what: &str) -> Result<RunOutcome, String> {
    match stop {
        Stop::Exited(outcome) => Ok(outcome),
        Stop::Paused(_) => Err(format!(
            "{what}: paused where it should have run to the end"
        )),
    }
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

/// "min / median / max" of the durations.
fn spread(mut walls: Vec<Duration>) -> String {
    walls.sort();
    match (walls.first(), walls.get(walls.len() / 2), walls.last()) {
        (Some(min), Some(median), Some(max)) => {
            format!("{} / {} / {}", ms(*min), ms(*median), ms(*max))
        }
        _ => "none".into(),
    }
}

/// What one kind of execution measured over all of its rounds.
struct Tally {
    executions: usize,
    total: Duration,
    walls: Vec<Duration>,
    failures: usize,
}

impl Tally {
    fn new() -> Self {
        Self {
            executions: 0,
            total: Duration::ZERO,
            walls: Vec::new(),
            failures: 0,
        }
    }

    fn report(self, what: &str) -> usize {
        println!(
            "{what}: {} executions in {}: {:.2} per second; each, min / median / max: {}",
            self.executions,
            ms(self.total),
            self.executions as f64 / self.total.as_secs_f64(),
            spread(self.walls)
        );
        self.failures
    }
}

/// A counter of the run's cgroup now (`memory.current`, `pids.current`): the
/// bench, hermit and every copy, which safehermit runs in place inside a
/// boxed scope.
fn cgroup_counter(name: &str) -> Result<u64, String> {
    let (file, text) = cgroup_file(name)?;
    text.trim().parse().map_err(|e| format!("{file}: {e}"))
}

/// The path and text of a file of the run's cgroup.
fn cgroup_file(name: &str) -> Result<(String, String), String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string())?;
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("no cgroup v2 line in /proc/self/cgroup")?;
    let file = format!("/sys/fs/cgroup{path}/{name}");
    let text = std::fs::read_to_string(&file).map_err(|e| format!("{file}: {e}"))?;
    Ok((file, text))
}

/// `throttled_usec` of the run's cgroup `cpu.stat`: how long its CPU quota
/// has held the run's tasks back. `None` without a CPU controller.
fn throttled() -> Option<Duration> {
    let (_, text) = cgroup_file("cpu.stat").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("throttled_usec "))?
        .trim()
        .parse()
        .ok()
        .map(Duration::from_micros)
}

fn memory_current() -> Result<u64, String> {
    cgroup_counter("memory.current")
}

fn mib(bytes: i64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// Waits until each copy in `ids` has written its summary (it is repaired
/// and placed), and returns the largest fork-to-ready time they report.
fn wait_ready(dir: &std::path::Path, ids: &[u64]) -> Result<Duration, String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut slowest = 0;
    for id in ids {
        let path = dir.join(format!("timeline-{id}.json"));
        let summary: serde_json::Value = loop {
            if let Ok(text) = std::fs::read_to_string(&path)
                && let Ok(summary) = serde_json::from_str(&text)
            {
                break summary;
            }
            if Instant::now() > deadline {
                return Err(format!("{}: no summary within 300 s", path.display()));
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        slowest = slowest.max(summary["ready_us"].as_u64().unwrap_or(0));
    }
    Ok(Duration::from_micros(slowest))
}

/// Dormant backups: for each N, N copies of `checkpoint` kept, K resumed.
fn backups(
    timelines: &mut KvmTimelines,
    checkpoint: &KvmCheckpoint,
    request: &PendingRequest,
    options: &Options,
    same: &dyn Fn(&Observation, i32) -> bool,
) -> Result<usize, String> {
    let mut failures = 0;
    for (batch_number, &n) in options.backups.iter().enumerate() {
        std::thread::sleep(Duration::from_millis(500));
        let before = memory_current()? as i64;
        let tasks_before = cgroup_counter("pids.current")?;
        let throttled_before = throttled();
        let started = Instant::now();
        let mut kept = timelines
            .fork_batch(
                checkpoint,
                n,
                &Batch {
                    copy: Some(options.copy),
                    placement: None,
                },
            )
            .map_err(|e| e.to_string())?;
        let fork_wall = started.elapsed();
        let ready = if options.copy == CopyPolicy::BACKUP {
            None
        } else {
            let ids: Vec<u64> = kept.iter().map(|t| t.timeline()).collect();
            Some(wait_ready(&options.dir, &ids)?)
        };
        let all_ready = started.elapsed();
        let throttled_forking = throttled()
            .zip(throttled_before)
            .map(|(now, then)| now.saturating_sub(then));
        std::thread::sleep(Duration::from_millis(200));
        let held = memory_current()? as i64 - before;
        let tasks = cgroup_counter("pids.current")? as i64 - tasks_before as i64;
        let k = options.resume.min(n);
        let resumed_at = Instant::now();
        let work = kept
            .iter_mut()
            .take(k)
            .enumerate()
            .map(|(j, t)| {
                let answer = (j > 0)
                    .then(|| tape_answer(request, ((batch_number * 31 + j * 7) % 255 + 1) as u8));
                (t, answer, StopAt::End)
            })
            .collect::<Vec<_>>();
        let results = timelines.resume_many(work);
        let resume_wall = resumed_at.elapsed();
        let mut codes = Vec::new();
        for (j, result) in results.into_iter().enumerate() {
            let outcome = exited(
                result.map_err(|e| e.to_string())?,
                &format!("backup {j} of {n}"),
            )?;
            codes.push(outcome.code);
            if j == 0 {
                let joined = checkpoint
                    .observation()
                    .join(
                        &outcome.observation,
                        &options.dir,
                        &format!("joined-backups-{n}"),
                    )
                    .map_err(|e| e.to_string())?;
                if !same(&joined, outcome.code) {
                    failures += 1;
                    println!("backups {n}: copy 0 FAIL differs from the reference");
                }
            }
        }
        for t in kept.drain(k..) {
            timelines.discard(t);
        }
        // A discarded copy's memory is freed as it exits, which can outlast
        // the discard: wait until the template has reaped every copy.
        let discarded = Instant::now();
        let deadline = discarded + Duration::from_secs(60);
        let mut gone = cgroup_counter("pids.current")? <= tasks_before;
        while !gone && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            gone = cgroup_counter("pids.current")? <= tasks_before;
        }
        let gone_after = discarded.elapsed();
        std::thread::sleep(Duration::from_millis(200));
        let after = memory_current()? as i64 - before;
        println!(
            "backups {n} ({}): fork {} returned, all ready {} (slowest copy's fork to ready {}; \
             the CPU quota held the run back {} meanwhile); memory held {} ({} per copy), \
             tasks held {tasks}; resumed {k} in {} (exits {codes:?}); after discarding the rest \
             {} ({})",
            options.copy.describe(),
            ms(fork_wall),
            ms(all_ready),
            ready.map_or("none: dormant".into(), ms),
            throttled_forking.map_or("unknown".into(), ms),
            mib(held),
            mib(held / n as i64),
            ms(resume_wall),
            mib(after),
            if gone {
                format!("every copy gone {} after the discards", ms(gone_after))
            } else {
                "copies still present 60 s after the discards".into()
            },
        );
    }
    Ok(failures)
}

/// Checkpoints along one run, rr's way: capture at request I, 2I, ... KI,
/// each time running on in a copy of the last checkpoint; then go back to
/// the first and to the middle one, each with an answer of its own.
fn chain(
    timelines: &mut KvmTimelines,
    start: &KvmCheckpoint,
    options: &Options,
) -> Result<usize, String> {
    let before = memory_current()? as i64;
    let mut checkpoints: Vec<(KvmCheckpoint, PendingRequest)> = Vec::new();
    let mut run = timelines.fork(start, 1).map_err(|e| e.to_string())?;
    let mut copy = run.remove(0);
    for i in 1..=options.chain {
        let stepped = Instant::now();
        let stop = timelines
            .resume(&mut copy, None, StopAt::Index(options.at * i as u64))
            .map_err(|e| e.to_string())?;
        let paused = match stop {
            Stop::Paused(paused) => paused,
            Stop::Exited(outcome) => {
                println!(
                    "chain: the run ended (exit {}) before checkpoint {i}",
                    outcome.code
                );
                break;
            }
        };
        let request = paused.request().clone();
        let ran = stepped.elapsed();
        let captured_at = Instant::now();
        let checkpoint = timelines
            .capture(paused)
            .map_err(|(refusal, _)| refusal.to_string())?;
        let captured = captured_at.elapsed();
        std::thread::sleep(Duration::from_millis(200));
        let held = memory_current()? as i64 - before;
        println!(
            "chain checkpoint {i} at request {}: ran there in {}, captured in {}; memory held {}",
            request.index,
            ms(ran),
            ms(captured),
            mib(held)
        );
        let mut next = timelines.fork(&checkpoint, 1).map_err(|e| e.to_string())?;
        copy = next.remove(0);
        checkpoints.push((checkpoint, request));
    }
    timelines.discard(copy);
    let mut failures = 0;
    let mut picks = vec![0, checkpoints.len() / 2];
    picks.dedup();
    picks.retain(|&j| j < checkpoints.len());
    for j in picks {
        let (checkpoint, request) = &checkpoints[j];
        let started = Instant::now();
        let mut back = timelines.fork(checkpoint, 1).map_err(|e| e.to_string())?;
        let answer = tape_answer(request, (j * 37 % 255 + 1) as u8);
        let outcome = exited(
            timelines
                .resume(&mut back[0], Some(answer), StopAt::End)
                .map_err(|e| e.to_string())?,
            &format!("back to checkpoint {}", j + 1),
        )?;
        if outcome.code != 0 {
            failures += 1;
        }
        println!(
            "chain: back to checkpoint {} (request {}) and on to the end with answer {answer}: {} (exit {})",
            j + 1,
            request.index,
            ms(started.elapsed()),
            outcome.code
        );
    }
    for (checkpoint, _) in checkpoints.into_iter().rev() {
        timelines.release(checkpoint);
    }
    Ok(failures)
}

fn bench(options: &Options) -> Result<usize, String> {
    let mut timelines = KvmTimelines::new(options.dir.clone()).map_err(|e| e.to_string())?;
    let tape = options.dir.join("tape-default");
    std::fs::write(&tape, []).map_err(|e| e.to_string())?;
    let spec = RunSpec {
        launcher: options.launcher.clone(),
        global_args: options.global.clone(),
        run_args: options
            .run
            .iter()
            .cloned()
            .chain([format!("--choice-tape={}", tape.display())])
            .collect(),
        guest: options.guest.clone(),
        dir: options.dir.clone(),
        timeout: Duration::from_secs(600),
        copy: if options.backups.is_empty() {
            options.copy
        } else {
            CopyPolicy::default()
        },
        placement: options.placement.clone(),
    };
    println!(
        "policies: placement {}, copy {}",
        spec.placement.describe(),
        options.copy.describe()
    );
    let start = timelines.start(&spec).map_err(|e| e.to_string())?;

    // The reference: one fresh run with its own answers.
    let mut fresh = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
    let reference = exited(
        timelines
            .resume(&mut fresh[0], None, StopAt::End)
            .map_err(|e| e.to_string())?,
        "reference",
    )?;
    let want = bytes(&reference.observation.stdout);
    println!(
        "reference: exit {} in {}",
        reference.code,
        ms(reference.wall)
    );
    let same = |observation: &Observation, code: i32| {
        bytes(&observation.stdout) == want && code == reference.code
    };
    if options.chain > 0 {
        return chain(&mut timelines, &start, options);
    }

    // The template: a fresh run paused at request `at`, then captured.
    let mut first = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
    let started = Instant::now();
    let paused = match timelines
        .resume(&mut first[0], None, StopAt::Index(options.at))
        .map_err(|e| e.to_string())?
    {
        Stop::Paused(paused) => paused,
        Stop::Exited(outcome) => {
            return Err(format!(
                "the run ended (exit {}) before choice request {}",
                outcome.code, options.at
            ));
        }
    };
    let to_pause = started.elapsed();
    let request = paused.request().clone();
    let checkpoint = timelines
        .capture(paused)
        .map_err(|(refusal, _)| refusal.to_string())?;
    println!(
        "template: paused at request {} ({}) after {}",
        request.index,
        request.kind,
        ms(to_pause)
    );
    if !options.backups.is_empty() {
        let failures = backups(&mut timelines, &checkpoint, &request, options, &same)?;
        timelines.release(checkpoint);
        return Ok(failures);
    }

    let mut copies = Tally::new();
    for round in 0..options.rounds {
        let started = Instant::now();
        let mut forked = timelines
            .fork(&checkpoint, options.copies)
            .map_err(|e| e.to_string())?;
        // Copy 0 takes the run's own future; copy k answers as tape byte
        // round * N + k would, so every copy of every round differs.
        let answers: Vec<Option<u64>> = (0..options.copies)
            .map(|k| {
                (k > 0).then(|| tape_answer(&request, ((round * options.copies + k) % 256) as u8))
            })
            .collect();
        let work = forked
            .iter_mut()
            .zip(answers.iter())
            .map(|(copy, answer)| (copy, *answer, StopAt::End))
            .collect::<Vec<_>>();
        let results = timelines.resume_many(work);
        let wall = started.elapsed();
        copies.total += wall;
        let mut codes = Vec::new();
        for (k, result) in results.into_iter().enumerate() {
            let outcome = exited(
                result.map_err(|e| e.to_string())?,
                &format!("round {round} copy {k}"),
            )?;
            codes.push(outcome.code);
            copies.executions += 1;
            copies.walls.push(outcome.wall);
            if k == 0 {
                let joined = checkpoint
                    .observation()
                    .join(
                        &outcome.observation,
                        &options.dir,
                        &format!("joined-{round}"),
                    )
                    .map_err(|e| e.to_string())?;
                if !same(&joined, outcome.code) {
                    copies.failures += 1;
                    println!("round {round} copy 0: FAIL differs from the reference");
                }
            }
        }
        println!(
            "copies round {round}: {} in {} (exits {codes:?})",
            options.copies,
            ms(wall)
        );
        std::thread::sleep(options.settle);
    }
    let mut failures = copies.report("copies");

    if options.fresh {
        let mut launches = Tally::new();
        for round in 0..options.rounds {
            let started = Instant::now();
            let mut forked = timelines
                .fork(&start, options.copies)
                .map_err(|e| e.to_string())?;
            let work = forked
                .iter_mut()
                .map(|copy| (copy, None, StopAt::End))
                .collect::<Vec<_>>();
            let results = timelines.resume_many(work);
            let wall = started.elapsed();
            launches.total += wall;
            for (k, result) in results.into_iter().enumerate() {
                let outcome = exited(
                    result.map_err(|e| e.to_string())?,
                    &format!("fresh round {round} launch {k}"),
                )?;
                launches.executions += 1;
                launches.walls.push(outcome.wall);
                if !same(&outcome.observation, outcome.code) {
                    launches.failures += 1;
                    println!("fresh round {round} launch {k}: FAIL differs from the reference");
                }
            }
            println!("fresh round {round}: {} in {}", options.copies, ms(wall));
            std::thread::sleep(options.settle);
        }
        failures += launches.report("fresh");
    }
    timelines.release(checkpoint);
    Ok(failures)
}

fn main() {
    let options = match options() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("timelines_bench: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = std::fs::create_dir_all(&options.dir) {
        eprintln!("timelines_bench: {}: {error}", options.dir.display());
        std::process::exit(2);
    }
    match bench(&options) {
        Ok(0) => println!("PASS: 0 failure(s)"),
        Ok(failures) => {
            println!("FAIL: {failures} failure(s)");
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("timelines_bench: {error}");
            std::process::exit(2);
        }
    }
}
