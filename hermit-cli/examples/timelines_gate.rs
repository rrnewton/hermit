/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (claude-multiverse lane): the equivalence gate of the
//! multiverse timelines design (section 7), written once against the
//! `Timelines` trait and run on an implementation.
//!
//! ```text
//! timelines_gate kvm|fork-server dir=DIR launcher=CMD global=ARGS run=ARGS
//!     [at=I] [bytes=B1:B2:...] [copies=K] [placement=P] [copy=C] [parallel=1] -- GUEST...
//!
//! `placement=` (`unpinned`, the default; `self` or `pool:A:B` to pin, for
//! experiments; `cpus=A:B` is that pool) and `copy=` (`fork-and-run`,
//! `eager`) are the KVM copies' policies.
//! ```
//!
//! `launcher`, `global` and `run` are space-separated argument lists (the
//! hermit command with its wrapper, the arguments before `run`, and those of
//! `run`). With `kvm`, the gate captures a run paused at choice request I,
//! forks copies, and checks P1 for each: copy 0 takes the same future (its
//! own source answers), copy k > 0 answers request I as tape byte Bk would;
//! the checkpoint's observation followed by the copy's must equal a fresh
//! run's (tape of I zero bytes and Bk) on standard output, standard error,
//! choice record, exit status and canonical information log. With
//! `fork-server`, START copies must each equal a plain run. Prints one line
//! per check and exits 1 if any differs.
//!
//! Standard error is compared byte for byte, so a launcher must not write
//! its own lines there: give safehermit `--sh-report FILE`.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

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
use hermit::timelines::fork_server::ForkServerTimelines;
use hermit::timelines::kvm::KvmTimelines;

struct Options {
    mode: String,
    dir: PathBuf,
    launcher: Vec<String>,
    global: Vec<String>,
    run: Vec<String>,
    at: u64,
    bytes: Vec<u8>,
    copies: usize,
    placement: Placement,
    copy: CopyPolicy,
    parallel: bool,
    guest: Vec<String>,
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .ok_or("usage: timelines_gate kvm|fork-server key=value... -- guest")?;
    let mut options = Options {
        mode,
        dir: PathBuf::new(),
        launcher: Vec::new(),
        global: Vec::new(),
        run: Vec::new(),
        at: 3,
        bytes: vec![1, 255],
        copies: 3,
        placement: Placement::default(),
        copy: CopyPolicy::default(),
        parallel: false,
        guest: Vec::new(),
    };
    for arg in args.by_ref() {
        if arg == "--" {
            break;
        }
        let (key, value) = arg
            .split_once('=')
            .ok_or(format!("{arg:?} is not key=value"))?;
        match key {
            "dir" => options.dir = PathBuf::from(value),
            "launcher" => options.launcher = words(value),
            "global" => options.global = words(value),
            "run" => options.run = words(value),
            "at" => options.at = value.parse().map_err(|e| format!("at: {e}"))?,
            "bytes" => {
                options.bytes = value
                    .split(':')
                    .map(|b| b.parse::<u8>().map_err(|e| format!("bytes: {e}")))
                    .collect::<Result<_, _>>()?
            }
            "copies" => options.copies = value.parse().map_err(|e| format!("copies: {e}"))?,
            "cpus" => options.placement = Placement::parse(&format!("pool:{value}"))?,
            "placement" => options.placement = Placement::parse(value)?,
            "copy" => {
                options.copy = match value {
                    "fork-and-run" => CopyPolicy::FORK_AND_RUN,
                    "eager" => CopyPolicy::EAGER,
                    other => return Err(format!("copy: {other:?} is not fork-and-run or eager")),
                }
            }
            "parallel" => options.parallel = value == "1",
            other => return Err(format!("unknown key {other:?}")),
        }
    }
    options.guest = args.collect();
    if options.guest.is_empty() || options.launcher.is_empty() {
        return Err("launcher= and a guest after -- are required".into());
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
        site: "timelines-gate",
    };
    detcore::choice::TapeSource::new(vec![b])
        .answer(&request)
        .unwrap_or(request.default)
}

struct Gate {
    launcher: Vec<String>,
    dir: PathBuf,
    failures: usize,
    /// Runs refused for a skid overshoot and run again (never silently).
    retried: usize,
}

/// How many times a run Hermit refused for a skid overshoot is run again.
const OVERSHOOT_RETRIES: usize = 2;

/// Whether Hermit refused this run because a precise PMU timer event passed
/// its target (`HERMIT_POLICY_REFUSAL ... cause=skid-overshoot`, exit 122): a
/// host timer event, after which the run is no evidence either way. A copy
/// is refused the same way as a fresh run.
fn overshot(outcome: &RunOutcome) -> bool {
    outcome.code == 122
        && String::from_utf8_lossy(&Gate::bytes(&outcome.observation.stderr))
            .contains("cause=skid-overshoot")
}

impl Gate {
    fn bytes(part: &Option<Part>) -> Vec<u8> {
        part.as_ref()
            .map(|p| p.read().unwrap_or_default())
            .unwrap_or_default()
    }

    /// `hermit log-diff --canonical-info` of two logs: (matched, records).
    fn log_diff(&self, a: &Path, b: &Path, name: &str) -> (bool, String) {
        let out = self.dir.join(format!("{name}.logdiff.json"));
        let (program, rest) = self.launcher.split_first().expect("launcher");
        let status = Command::new(program)
            .args(rest)
            .arg("log-diff")
            .arg("--canonical-info")
            .arg("--json")
            .arg(&out)
            .arg(a)
            .arg(b)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        let verdict = text
            .lines()
            .last()
            .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok());
        let matched = verdict
            .as_ref()
            .and_then(|v| v.get("verdict"))
            .and_then(|v| v.as_str())
            == Some("matched");
        let records = verdict
            .as_ref()
            .and_then(|v| v.get("matched_prefix_records"))
            .map(|r| r.to_string())
            .unwrap_or_else(|| format!("{status:?}"));
        (matched, records)
    }

    /// Compares an observation and exit code with a reference.
    fn check(
        &mut self,
        label: &str,
        got: &Observation,
        code: i32,
        want: &Observation,
        want_code: i32,
    ) {
        let stdout = Self::bytes(&got.stdout) == Self::bytes(&want.stdout);
        let stderr = Self::bytes(&got.stderr) == Self::bytes(&want.stderr);
        let record = Self::bytes(&got.record) == Self::bytes(&want.record);
        let (log, records) = match (&got.log, &want.log) {
            (Some(a), Some(b)) => {
                let a_path = self.dir.join(format!("{label}.got.log"));
                let b_path = self.dir.join(format!("{label}.want.log"));
                let _ = std::fs::write(&a_path, Self::bytes(&Some(a.clone())));
                let _ = std::fs::write(&b_path, Self::bytes(&Some(b.clone())));
                self.log_diff(&a_path, &b_path, label)
            }
            _ => (false, "missing".into()),
        };
        let exit = code == want_code;
        let ok = stdout && stderr && record && log && exit;
        if !ok {
            self.failures += 1;
        }
        let mark = |b: bool| if b { "EQUAL" } else { "DIFFERS" };
        println!(
            "{label}: {} stdout {} stderr {} record {} exit {} ({code} vs {want_code}) log {} ({records})",
            if ok { "PASS" } else { "FAIL" },
            mark(stdout),
            mark(stderr),
            mark(record),
            mark(exit),
            mark(log)
        );
    }
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

fn run_kvm(options: &Options, gate: &mut Gate) -> Result<(), String> {
    let mut timelines = KvmTimelines::new(options.dir.clone()).map_err(|e| e.to_string())?;
    let tape = |name: &str, bytes: &[u8]| -> Result<String, String> {
        let path = options.dir.join(name);
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(format!("--choice-tape={}", path.display()))
    };
    let spec = |tape_arg: String| RunSpec {
        launcher: options.launcher.clone(),
        global_args: options.global.clone(),
        run_args: options.run.iter().cloned().chain([tape_arg]).collect(),
        guest: options.guest.clone(),
        dir: options.dir.clone(),
        timeout: Duration::from_secs(600),
        copy: options.copy,
        placement: options.placement.clone(),
    };
    println!(
        "policies: placement {}, copy {}",
        options.placement.describe(),
        options.copy.describe()
    );
    let base = spec(tape("tape-default", &[])?);

    // The reference: a fresh run, START copy resumed with its own source.
    let start = timelines.start(&base).map_err(|e| e.to_string())?;
    let mut attempt = 0;
    let reference = loop {
        let mut fresh = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
        let outcome = exited(
            timelines
                .resume(&mut fresh[0], None, StopAt::End)
                .map_err(|e| e.to_string())?,
            "reference",
        )?;
        if !overshot(&outcome) || attempt == OVERSHOOT_RETRIES {
            break outcome;
        }
        attempt += 1;
        gate.retried += 1;
        println!("reference: refused for a skid overshoot; run again ({attempt})");
    };
    println!(
        "reference: exit {} in {}",
        reference.code,
        ms(reference.wall)
    );

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
    let started = Instant::now();
    let checkpoint = timelines
        .capture(paused)
        .map_err(|(refusal, _)| refusal.to_string())?;
    let capture = started.elapsed();
    let started = Instant::now();
    let mut copies = timelines
        .fork(&checkpoint, options.copies)
        .map_err(|e| e.to_string())?;
    let fork = started.elapsed();
    println!(
        "template: paused at request {} ({}) after {}; capture {}; fork of {} copies {}",
        request.index,
        request.kind,
        ms(to_pause),
        ms(capture),
        options.copies,
        ms(fork)
    );

    // The futures: copy 0 the same; copy k answers as byte bytes[k-1] would.
    let answers: Vec<Option<u64>> = (0..options.copies)
        .map(|k| match k {
            0 => None,
            k => options.bytes.get(k - 1).map(|&b| tape_answer(&request, b)),
        })
        .collect();
    let work = copies
        .iter_mut()
        .zip(answers.iter())
        .map(|(copy, answer)| (copy, *answer, StopAt::End))
        .collect::<Vec<_>>();
    let started = Instant::now();
    let results = if options.parallel {
        timelines.resume_many(work)
    } else {
        work.into_iter()
            .map(|(copy, answer, until)| timelines.resume(copy, answer, until))
            .collect()
    };
    let all = started.elapsed();
    println!(
        "copies: {} resumed {} in {}",
        options.copies,
        if options.parallel {
            "at once"
        } else {
            "one after another"
        },
        ms(all)
    );
    let mut outcomes = Vec::new();
    for (k, result) in results.into_iter().enumerate() {
        let mut outcome = exited(result.map_err(|e| e.to_string())?, &format!("copy {k}"))?;
        println!("copy {k}: exit {} in {}", outcome.code, ms(outcome.wall));
        // A copy Hermit refused for a skid overshoot proves nothing: copy the
        // checkpoint again for the same future, a bounded number of times.
        let mut attempt = 0;
        while overshot(&outcome) && attempt < OVERSHOOT_RETRIES {
            attempt += 1;
            gate.retried += 1;
            let mut again = timelines.fork(&checkpoint, 1).map_err(|e| e.to_string())?;
            println!(
                "copy {k}: refused for a skid overshoot; copied again as timeline {} ({attempt})",
                again[0].timeline()
            );
            outcome = exited(
                timelines
                    .resume(&mut again[0], answers[k], StopAt::End)
                    .map_err(|e| e.to_string())?,
                &format!("copy {k} again"),
            )?;
            println!("copy {k}: exit {} in {}", outcome.code, ms(outcome.wall));
        }
        outcomes.push(outcome);
    }

    for (k, outcome) in outcomes.iter().enumerate() {
        let joined = checkpoint
            .observation()
            .join(&outcome.observation, &options.dir, &format!("joined-{k}"))
            .map_err(|e| e.to_string())?;
        let (want, want_code, label) = match (k, answers[k]) {
            (0, _) | (_, None) => (
                reference.observation.clone(),
                reference.code,
                format!("copy {k} same future"),
            ),
            (k, Some(_)) => {
                let b = options.bytes[k - 1];
                let mut bytes = vec![0u8; options.at as usize];
                bytes.push(b);
                let own = spec(tape(&format!("tape-{k}"), &bytes)?);
                let own_start = timelines.start(&own).map_err(|e| e.to_string())?;
                let mut attempt = 0;
                let fresh = loop {
                    let mut fresh = timelines.fork(&own_start, 1).map_err(|e| e.to_string())?;
                    let outcome = exited(
                        timelines
                            .resume(&mut fresh[0], None, StopAt::End)
                            .map_err(|e| e.to_string())?,
                        "reference",
                    )?;
                    if !overshot(&outcome) || attempt == OVERSHOOT_RETRIES {
                        break outcome;
                    }
                    attempt += 1;
                    gate.retried += 1;
                    println!(
                        "copy {k}'s fresh run: refused for a skid overshoot; run again ({attempt})"
                    );
                };
                (
                    fresh.observation,
                    fresh.code,
                    format!("copy {k} byte {b} at request {}", options.at),
                )
            }
        };
        let label = label.replace(' ', "-");
        gate.check(&label, &joined, outcome.code, &want, want_code);
        // Teeth: a different answer must show in the choice record, or the
        // copy did not take its own future.
        if k > 0 && answers[k].is_some() {
            let same = options.dir.join("joined-0.choices");
            let own = options.dir.join(format!("joined-{k}.choices"));
            if std::fs::read(&same).ok() == std::fs::read(&own).ok() {
                gate.failures += 1;
                println!("{label}: FAIL its record equals the same-future copy's");
            }
        }
    }
    // Release with a copy that was never resumed: it waits for a command,
    // and release must still end the template and it promptly (P6).
    let unresumed = timelines.fork(&checkpoint, 1).map_err(|e| e.to_string())?;
    let started = Instant::now();
    timelines.release(checkpoint);
    let release = started.elapsed();
    let prompt = release < Duration::from_secs(30);
    if !prompt {
        gate.failures += 1;
    }
    println!(
        "release with copy {} unresumed: {} in {}",
        unresumed[0].timeline(),
        if prompt { "PASS" } else { "FAIL" },
        ms(release)
    );
    Ok(())
}

fn run_fork_server(options: &Options, gate: &mut Gate) -> Result<(), String> {
    // Copy k's own source is tape k: tape 0 is empty (every request takes
    // its default, the seeded run); tape k > 0 holds `at` zero bytes and then
    // byte bytes[k-1], a different future from request `at` on. Each copy is
    // checked against a plain run on its tape. Paths on the two command
    // lines have equal lengths (tape-0 and tape-k, plain.* and serve.*):
    // Hermit's arguments are visible to the guest.
    let path = |name: &str| options.dir.join(name);
    for k in 0..options.copies {
        let mut bytes = Vec::new();
        if k > 0 {
            bytes = vec![0u8; options.at as usize];
            bytes.push(
                *options
                    .bytes
                    .get(k - 1)
                    .ok_or("bytes= needs one byte per copy after the first")?,
            );
        }
        std::fs::write(path(&format!("tape-{k}")), bytes).map_err(|e| e.to_string())?;
    }
    let (program, rest) = options.launcher.split_first().expect("launcher");
    let mut plain = Vec::new();
    for k in 0..options.copies {
        let started = Instant::now();
        let status = Command::new(program)
            .args(rest)
            .args(&options.global)
            .arg(format!("--log-file={}", path("plain.log").display()))
            .arg("run")
            .args(&options.run)
            .arg(format!(
                "--choice-tape={}",
                path(&format!("tape-{k}")).display()
            ))
            .arg(format!(
                "--choice-record={}",
                path("plain.choices").display()
            ))
            .arg("--")
            .args(&options.guest)
            .stdin(std::process::Stdio::null())
            .stdout(std::fs::File::create(path("plain.stdout")).map_err(|e| e.to_string())?)
            .stderr(std::fs::File::create(path("plain.stderr")).map_err(|e| e.to_string())?)
            .status()
            .map_err(|e| e.to_string())?;
        let code = status.code().unwrap_or(128);
        println!("plain {k}: exit {code} in {}", ms(started.elapsed()));
        let mut observation = Observation::default();
        for (output, slot) in [
            ("stdout", &mut observation.stdout),
            ("stderr", &mut observation.stderr),
            ("log", &mut observation.log),
            ("choices", &mut observation.record),
        ] {
            let own = path(&format!("plain-{k}.{output}"));
            std::fs::rename(path(&format!("plain.{output}")), &own).map_err(|e| e.to_string())?;
            *slot = Some(Part::whole(own));
        }
        plain.push((observation, code));
    }

    let mut timelines = ForkServerTimelines::new();
    let spec = RunSpec {
        launcher: options.launcher.clone(),
        global_args: options.global.clone(),
        run_args: options
            .run
            .iter()
            .cloned()
            .chain([
                format!("--choice-tape={}", path("tape-0").display()),
                format!("--choice-record={}", path("serve.choices").display()),
            ])
            .collect(),
        guest: options.guest.clone(),
        dir: options.dir.clone(),
        timeout: Duration::from_secs(600),
        copy: CopyPolicy::default(),
        placement: Placement::default(),
    };
    let started = Instant::now();
    let start = timelines.start(&spec).map_err(|e| e.to_string())?;
    println!("server: listening after {}", ms(started.elapsed()));
    let mut copies = timelines
        .fork(&start, options.copies)
        .map_err(|e| e.to_string())?;
    let mut records = Vec::new();
    for (k, copy) in copies.iter_mut().enumerate() {
        copy.set_tape(path(&format!("tape-{k}")));
        let outcome = exited(
            timelines
                .resume(copy, None, StopAt::End)
                .map_err(|e| e.to_string())?,
            &format!("copy {k}"),
        )?;
        println!("copy {k}: exit {} in {}", outcome.code, ms(outcome.wall));
        let (want, want_code) = &plain[k];
        gate.check(
            &format!("served-copy-{k}-tape-{k}"),
            &outcome.observation,
            outcome.code,
            want,
            *want_code,
        );
        records.push(Gate::bytes(&outcome.observation.record));
    }
    // Teeth: a different tape must show in the choice record.
    for (k, record) in records.iter().enumerate().skip(1) {
        if *record == records[0] {
            gate.failures += 1;
            println!("served-copy-{k}: FAIL its record equals copy 0's");
        }
    }
    // R4: an answer at START is refused by name.
    let mut extra = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
    match timelines.resume(&mut extra[0], Some(1), StopAt::End) {
        Err(refusal) => println!("answer at START: refused ({})", refusal.name),
        Ok(_) => {
            gate.failures += 1;
            println!("answer at START: FAIL, not refused");
        }
    }
    timelines.release(start);
    Ok(())
}

fn main() {
    let options = match options() {
        Ok(options) => options,
        Err(error) => {
            eprintln!("timelines_gate: {error}");
            std::process::exit(2);
        }
    };
    if let Err(error) = std::fs::create_dir_all(&options.dir) {
        eprintln!("timelines_gate: {}: {error}", options.dir.display());
        std::process::exit(2);
    }
    let mut gate = Gate {
        launcher: options.launcher.clone(),
        dir: options.dir.clone(),
        failures: 0,
        retried: 0,
    };
    let result = match options.mode.as_str() {
        "kvm" => run_kvm(&options, &mut gate),
        "fork-server" => run_fork_server(&options, &mut gate),
        other => Err(format!("unknown implementation {other:?}")),
    };
    if let Err(error) = result {
        eprintln!("timelines_gate: {error}");
        std::process::exit(2);
    }
    println!(
        "{}: {} failure(s); {} run(s) refused for a skid overshoot and run again",
        if gate.failures == 0 { "PASS" } else { "FAIL" },
        gate.failures,
        gate.retried
    );
    std::process::exit(if gate.failures == 0 { 0 } else { 1 });
}
