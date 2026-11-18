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
//!     [at=I] [copies=N] [rounds=R] [cpus=A:B:...] [fresh=1] [settle=MS] -- GUEST...
//! ```
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

use hermit::timelines::Checkpoint;
use hermit::timelines::Observation;
use hermit::timelines::Part;
use hermit::timelines::PendingRequest;
use hermit::timelines::RunOutcome;
use hermit::timelines::RunSpec;
use hermit::timelines::Stop;
use hermit::timelines::StopAt;
use hermit::timelines::Timelines;
use hermit::timelines::kvm::KvmTimelines;

struct Options {
    dir: PathBuf,
    launcher: Vec<String>,
    global: Vec<String>,
    run: Vec<String>,
    at: u64,
    copies: usize,
    rounds: usize,
    cpus: Vec<usize>,
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
        cpus: Vec::new(),
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
            "cpus" => {
                options.cpus = value
                    .split(':')
                    .map(|c| c.parse::<usize>().map_err(|e| format!("cpus: {e}")))
                    .collect::<Result<_, _>>()?
            }
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

fn bench(options: &Options) -> Result<usize, String> {
    let mut timelines = KvmTimelines::new(options.dir.clone()).map_err(|e| e.to_string())?;
    timelines.cpus = options.cpus.clone();
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
    };
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
