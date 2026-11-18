/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (muse-rrmode lane): seek-equivalence gate for the
//! rr-equivalent replay mode (ai_docs/plans/2026-11-18-rr-replay-mode-design.md).
//!
//! ```text
//! replay_seek_gate dir=DIR launcher=CMD global=ARGS run=ARGS
//!     checkpoints=I1:I2:... seeks=T1:T2:... [backup=1] [teeth=wrong] -- GUEST...
//! ```
//!
//! One recording (the RunSpec, choice tape included) is replayed three ways
//! on KvmTimelines: straight to the end (the oracle), forward with a kept
//! checkpoint at each index in `checkpoints=`, and, per target in `seeks=`
//! (served largest first, checkpoints past the target released after each
//! seek, as rr discards them), a seek: fork from the nearest kept
//! checkpoint at or before the target, replay forward to the target, check
//! the landed pending request against a straight run paused at the same
//! index, then replay to the end. Every seeked run's joined observation
//! (prefix at its checkpoint, then its own segments) must equal the
//! oracle's on stdout, choice record, exit status and canonical INFO log.
//! With `backup=1` the seek copy is a CopyPolicy::BACKUP dormant handle
//! forked during the forward pass and materialized at the seek. With
//! `teeth=wrong` one extra seek deliberately restores from the first
//! checkpoint after its target; that seek MUST fail its check, or the gate
//! itself fails. Prints one line per phase and per check; exits 1 on any
//! failure. Timings are printed for the keep() pricing, not gated.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

use hermit::timelines::Batch;
use hermit::timelines::Checkpoint as _;
use hermit::timelines::CopyPolicy;
use hermit::timelines::Cut;
use hermit::timelines::Observation;
use hermit::timelines::Part;
use hermit::timelines::PendingRequest;
use hermit::timelines::RunOutcome;
use hermit::timelines::RunSpec;
use hermit::timelines::Stop;
use hermit::timelines::StopAt;
use hermit::timelines::Timelines;
use hermit::timelines::kvm::KvmTimelines;

type Cp = <KvmTimelines as Timelines>::Checkpoint;
type Tl = <KvmTimelines as Timelines>::Timeline;
type Paused = <KvmTimelines as Timelines>::PausedRun;

struct Options {
    dir: PathBuf,
    launcher: Vec<String>,
    global: Vec<String>,
    run: Vec<String>,
    checkpoints: Vec<u64>,
    seeks: Vec<u64>,
    backup: bool,
    teeth: bool,
    guest: Vec<String>,
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn indices(text: &str, what: &str) -> Result<Vec<u64>, String> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    text.split(':')
        .map(|v| v.parse().map_err(|e| format!("{what}: {e}")))
        .collect()
}

fn options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        dir: PathBuf::new(),
        launcher: Vec::new(),
        global: Vec::new(),
        run: Vec::new(),
        checkpoints: Vec::new(),
        seeks: Vec::new(),
        backup: false,
        teeth: false,
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
            "checkpoints" => options.checkpoints = indices(value, "checkpoints")?,
            "seeks" => options.seeks = indices(value, "seeks")?,
            "backup" => options.backup = value == "1",
            "teeth" => options.teeth = value == "wrong",
            other => return Err(format!("unknown key {other:?}")),
        }
    }
    options.guest = args.collect();
    if options.guest.is_empty() || options.launcher.is_empty() || options.dir.as_os_str().is_empty()
    {
        return Err("dir=, launcher= and a guest after -- are required".into());
    }
    if options.checkpoints.windows(2).any(|w| w[0] >= w[1]) {
        return Err("checkpoints= must be strictly increasing".into());
    }
    Ok(options)
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
}

fn same_request(a: &PendingRequest, b: &PendingRequest) -> bool {
    a.index == b.index
        && a.kind == b.kind
        && a.lo == b.lo
        && a.hi == b.hi
        && a.default == b.default
        && a.scale == b.scale
}

/// Observation comparison, as in timelines_gate: stdout and choice record
/// byte for byte, exit status, canonical INFO log through hermit log-diff.
struct Checker {
    launcher: Vec<String>,
    dir: PathBuf,
    failures: usize,
}

impl Checker {
    fn bytes(part: &Option<Part>) -> Vec<u8> {
        part.as_ref()
            .map(|p| p.read().unwrap_or_default())
            .unwrap_or_default()
    }

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

    /// Returns true when got equals want; counts a failure when not and
    /// `count` is set (the teeth case expects inequality uncounted here).
    fn check(
        &mut self,
        label: &str,
        got: &Observation,
        code: i32,
        want: &Observation,
        want_code: i32,
        count: bool,
    ) -> bool {
        let stdout = Self::bytes(&got.stdout) == Self::bytes(&want.stdout);
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
        let ok = stdout && record && log && exit;
        if !ok && count {
            self.failures += 1;
        }
        let mark = |b: bool| if b { "EQUAL" } else { "DIFFERS" };
        println!(
            "{label}: {} stdout {} record {} exit {} ({code} vs {want_code}) log {} ({records})",
            if ok { "PASS" } else { "FAIL" },
            mark(stdout),
            mark(record),
            mark(exit),
            mark(log)
        );
        ok
    }
}

fn exited(stop: Stop<Paused>, what: &str) -> Result<RunOutcome, String> {
    match stop {
        Stop::Exited(outcome) => Ok(outcome),
        Stop::Paused(_) => Err(format!(
            "{what}: paused where it should have run to the end"
        )),
    }
}

fn paused_at(stop: Stop<Paused>, what: &str) -> Result<Paused, String> {
    match stop {
        Stop::Paused(paused) => Ok(paused),
        Stop::Exited(outcome) => Err(format!(
            "{what}: the run ended (exit {}) before its pause index",
            outcome.code
        )),
    }
}

/// One kept checkpoint: the template, the fully joined observation prefix up
/// to its cut, and (with backup=1) a dormant copy forked from it during the
/// forward pass, materialized only if a seek resumes it.
struct Kept {
    index: u64,
    checkpoint: Option<Cp>,
    prefix: Observation,
    dormant: Option<Tl>,
}

fn run() -> Result<i32, String> {
    let options = options()?;
    std::fs::create_dir_all(&options.dir).map_err(|e| e.to_string())?;
    let mut timelines = KvmTimelines::new(options.dir.clone()).map_err(|e| e.to_string())?;
    let mut checker = Checker {
        launcher: options.launcher.clone(),
        dir: options.dir.clone(),
        failures: 0,
    };

    // The recording: one RunSpec for every replay below. The empty tape
    // makes every choice answer its recorded default, as in timelines_gate.
    let tape_path = options.dir.join("tape-default");
    std::fs::write(&tape_path, []).map_err(|e| e.to_string())?;
    let spec = RunSpec {
        launcher: options.launcher.clone(),
        global_args: options.global.clone(),
        run_args: options
            .run
            .iter()
            .cloned()
            .chain([format!("--choice-tape={}", tape_path.display())])
            .collect(),
        guest: options.guest.clone(),
        dir: options.dir.clone(),
        timeout: Duration::from_secs(600),
        copy: CopyPolicy::default(),
        placement: Default::default(),
    };
    let start = timelines.start(&spec).map_err(|e| e.to_string())?;

    // The oracle: a straight replay to the end.
    let mut fresh = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
    let reference = exited(
        timelines
            .resume(&mut fresh[0], None, StopAt::End)
            .map_err(|e| e.to_string())?,
        "reference",
    )?;
    timelines.discard(fresh.pop().expect("reference copy"));
    println!(
        "reference: exit {} in {}",
        reference.code,
        ms(reference.wall)
    );

    // Expected pending requests at each distinct seek target, from straight
    // runs paused there (captured and released at once; observation unused).
    let mut targets: Vec<u64> = options.seeks.clone();
    targets.sort_unstable();
    targets.dedup();
    let mut expected: Vec<(u64, PendingRequest)> = Vec::new();
    for &t in &targets {
        let mut probe = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
        let paused = paused_at(
            timelines
                .resume(&mut probe[0], None, StopAt::Index(t))
                .map_err(|e| e.to_string())?,
            &format!("expected-request probe at {t}"),
        )?;
        expected.push((t, paused.request().clone()));
        let probe_cp = timelines
            .capture(paused)
            .map_err(|(refusal, _)| refusal.to_string())?;
        timelines.release(probe_cp);
        timelines.discard(probe.pop().expect("probe copy"));
    }

    // Forward checkpointed pass: pause at each cadence index, keep the
    // template, continue on one forked copy. Prefixes fold by join.
    let mut kept: Vec<Kept> = Vec::new();
    let mut live = timelines.fork(&start, 1).map_err(|e| e.to_string())?;
    let mut live = live.pop().expect("continuation");
    let mut prefix = Observation::default();
    for (k, &c) in options.checkpoints.iter().enumerate() {
        let started = Instant::now();
        let paused = paused_at(
            timelines
                .resume(&mut live, None, StopAt::Index(c))
                .map_err(|e| e.to_string())?,
            &format!("forward pass to checkpoint {c}"),
        )?;
        let to_pause = started.elapsed();
        let request = paused.request().clone();
        let started = Instant::now();
        let checkpoint = timelines
            .capture(paused)
            .map_err(|(refusal, _)| refusal.to_string())?;
        let capture = started.elapsed();
        prefix = prefix
            .join(
                checkpoint.observation(),
                &options.dir,
                &format!("prefix-{c}"),
            )
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        let dormant = if options.backup {
            let mut d = timelines
                .fork_batch(
                    &checkpoint,
                    1,
                    &Batch {
                        copy: Some(CopyPolicy::BACKUP),
                        placement: None,
                    },
                )
                .map_err(|e| e.to_string())?;
            Some(d.pop().expect("dormant copy"))
        } else {
            None
        };
        let mut next = timelines.fork(&checkpoint, 1).map_err(|e| e.to_string())?;
        let fork = started.elapsed();
        println!(
            "checkpoint[{k}] at request {} ({}): forward {}; capture {}; fork {}",
            request.index,
            request.kind,
            ms(to_pause),
            ms(capture),
            ms(fork)
        );
        kept.push(Kept {
            index: c,
            checkpoint: Some(checkpoint),
            prefix: prefix.clone(),
            dormant,
        });
        live = next.pop().expect("continuation");
    }
    let tail = exited(
        timelines
            .resume(&mut live, None, StopAt::End)
            .map_err(|e| e.to_string())?,
        "forward pass tail",
    )?;
    timelines.discard(live);
    let forward_joined = prefix
        .join(&tail.observation, &options.dir, "forward-joined")
        .map_err(|e| e.to_string())?;
    checker.check(
        "forward-checkpointed",
        &forward_joined,
        tail.code,
        &reference.observation,
        reference.code,
        true,
    );

    // Seeks, largest target first. After each seek, checkpoints past the
    // target are released (rr discards them); smaller seeks keep theirs.
    let mut descending: Vec<u64> = targets.clone();
    descending.reverse();
    for &t in &descending {
        let want_request = &expected
            .iter()
            .find(|(index, _)| *index == t)
            .expect("expected request")
            .1;
        // Nearest kept checkpoint at or before t, else START.
        let base = kept
            .iter()
            .rposition(|k| k.index <= t && k.checkpoint.is_some());
        let (base_index, base_prefix) = match base {
            Some(i) => (kept[i].index, kept[i].prefix.clone()),
            None => (0, Observation::default()),
        };
        let started = Instant::now();
        let mut copy = match base {
            Some(i) if kept[i].dormant.is_some() => kept[i].dormant.take().expect("dormant copy"),
            Some(i) => timelines
                .fork(kept[i].checkpoint.as_ref().expect("kept checkpoint"), 1)
                .map_err(|e| e.to_string())?
                .pop()
                .expect("seek copy"),
            None => timelines
                .fork(&start, 1)
                .map_err(|e| e.to_string())?
                .pop()
                .expect("seek copy"),
        };
        // A copy forked from a checkpoint starts paused at that
        // checkpoint's own request, so when t IS the checkpoint index the
        // landing is the fork itself: verify it against the checkpoint's
        // cut and run on. Resuming to Index(t) would answer t and run
        // past it. Otherwise replay forward and pause at t.
        let (joined, outcome_code) = if let Some(base_i) = base.filter(|&i| kept[i].index == t) {
            let cut_request = match kept[base_i]
                .checkpoint
                .as_ref()
                .expect("kept checkpoint")
                .cut()
            {
                Cut::Pending(request) => request.clone(),
                Cut::Start => return Err(format!("seek to {t}: base checkpoint is START")),
            };
            let landed = same_request(&cut_request, want_request);
            if !landed {
                checker.failures += 1;
            }
            println!(
                "seek-{t}: from checkpoint {base_index} (exact); cut request {} ({}) {}; restore {}",
                cut_request.index,
                cut_request.kind,
                if landed {
                    "MATCHES straight"
                } else {
                    "DIFFERS FROM straight"
                },
                ms(started.elapsed())
            );
            let outcome = exited(
                timelines
                    .resume(&mut copy, None, StopAt::End)
                    .map_err(|e| e.to_string())?,
                &format!("seek {t} onward"),
            )?;
            timelines.discard(copy);
            let joined = base_prefix
                .join(
                    &outcome.observation,
                    &options.dir,
                    &format!("seek-{t}-joined"),
                )
                .map_err(|e| e.to_string())?;
            (joined, outcome.code)
        } else {
            let paused = paused_at(
                timelines
                    .resume(&mut copy, None, StopAt::Index(t))
                    .map_err(|e| e.to_string())?,
                &format!("seek to {t}"),
            )?;
            let to_target = started.elapsed();
            let landed = same_request(paused.request(), want_request);
            if !landed {
                checker.failures += 1;
            }
            println!(
                "seek-{t}: from checkpoint {base_index}; landed request {} ({}) {}; restore+forward {}",
                paused.request().index,
                paused.request().kind,
                if landed {
                    "MATCHES straight"
                } else {
                    "DIFFERS FROM straight"
                },
                ms(to_target)
            );
            // Continue from the landing: capture there (a real session
            // stops here too), fork, replay to the end.
            let at_target = timelines
                .capture(paused)
                .map_err(|(refusal, _)| refusal.to_string())?;
            timelines.discard(copy);
            let prefix_t = base_prefix
                .join(
                    at_target.observation(),
                    &options.dir,
                    &format!("seek-{t}-prefix"),
                )
                .map_err(|e| e.to_string())?;
            let started = Instant::now();
            let mut onward = timelines.fork(&at_target, 1).map_err(|e| e.to_string())?;
            let outcome = exited(
                timelines
                    .resume(&mut onward[0], None, StopAt::End)
                    .map_err(|e| e.to_string())?,
                &format!("seek {t} onward"),
            )?;
            println!("seek-{t}: onward to end in {}", ms(started.elapsed()));
            timelines.discard(onward.pop().expect("onward copy"));
            timelines.release(at_target);
            let joined = prefix_t
                .join(
                    &outcome.observation,
                    &options.dir,
                    &format!("seek-{t}-joined"),
                )
                .map_err(|e| e.to_string())?;
            (joined, outcome.code)
        };
        checker.check(
            &format!("seek-{t}"),
            &joined,
            outcome_code,
            &reference.observation,
            reference.code,
            true,
        );
        // Discard checkpoints after t (and their dormant copies).
        for k in kept.iter_mut().filter(|k| k.index > t) {
            if let Some(dormant) = k.dormant.take() {
                timelines.discard(dormant);
            }
            if let Some(cp) = k.checkpoint.take() {
                timelines.release(cp);
            }
        }
    }

    // Teeth: a seeked copy answered wrongly at its target must NOT equal
    // the oracle. Uses the smallest seek target and its nearest surviving
    // checkpoint (START if seeks released the rest).
    if options.teeth {
        match targets.first() {
            None => {
                checker.failures += 1;
                println!("teeth-wrong: FAIL no seek target to corrupt");
            }
            Some(&t) => {
                let want = &expected
                    .iter()
                    .find(|(index, _)| *index == t)
                    .expect("expected request")
                    .1;
                let wrong = if want.default > want.lo {
                    Some(want.default - 1)
                } else if want.default < want.hi {
                    Some(want.default + 1)
                } else {
                    None
                };
                match wrong {
                    None => {
                        checker.failures += 1;
                        println!("teeth-wrong: FAIL request at {t} has a single legal answer");
                    }
                    Some(value) => {
                        let base = kept
                            .iter()
                            .rposition(|k| k.index <= t && k.checkpoint.is_some());
                        let (base_prefix, mut copy) = match base {
                            Some(i) => (
                                kept[i].prefix.clone(),
                                timelines
                                    .fork(kept[i].checkpoint.as_ref().expect("kept checkpoint"), 1)
                                    .map_err(|e| e.to_string())?
                                    .pop()
                                    .expect("teeth copy"),
                            ),
                            None => (
                                Observation::default(),
                                timelines
                                    .fork(&start, 1)
                                    .map_err(|e| e.to_string())?
                                    .pop()
                                    .expect("teeth copy"),
                            ),
                        };
                        let base_index = base.map(|i| kept[i].index).unwrap_or(0);
                        let (joined, outcome) = if base.is_some() && base_index == t {
                            // Already paused at t: answer it wrongly and run on.
                            let outcome = exited(
                                timelines
                                    .resume(&mut copy, Some(value), StopAt::End)
                                    .map_err(|e| e.to_string())?,
                                "teeth onward",
                            )?;
                            timelines.discard(copy);
                            let joined = base_prefix
                                .join(&outcome.observation, &options.dir, "teeth-joined")
                                .map_err(|e| e.to_string())?;
                            (joined, outcome)
                        } else {
                            let paused = paused_at(
                                timelines
                                    .resume(&mut copy, None, StopAt::Index(t))
                                    .map_err(|e| e.to_string())?,
                                "teeth seek",
                            )?;
                            let at_target = timelines
                                .capture(paused)
                                .map_err(|(refusal, _)| refusal.to_string())?;
                            timelines.discard(copy);
                            let prefix_t = base_prefix
                                .join(at_target.observation(), &options.dir, "teeth-prefix")
                                .map_err(|e| e.to_string())?;
                            let mut onward =
                                timelines.fork(&at_target, 1).map_err(|e| e.to_string())?;
                            let outcome = exited(
                                timelines
                                    .resume(&mut onward[0], Some(value), StopAt::End)
                                    .map_err(|e| e.to_string())?,
                                "teeth onward",
                            )?;
                            timelines.discard(onward.pop().expect("teeth onward copy"));
                            timelines.release(at_target);
                            let joined = prefix_t
                                .join(&outcome.observation, &options.dir, "teeth-joined")
                                .map_err(|e| e.to_string())?;
                            (joined, outcome)
                        };
                        let equal = checker.check(
                            "teeth-wrong-answer",
                            &joined,
                            outcome.code,
                            &reference.observation,
                            reference.code,
                            false,
                        );
                        if equal {
                            checker.failures += 1;
                            println!(
                                "teeth-wrong: FAIL wrong answer at {t} still equals the oracle"
                            );
                        } else {
                            println!(
                                "teeth-wrong: bites (wrong answer at {t} differs, as required)"
                            );
                        }
                    }
                }
            }
        }
    }

    for k in kept.iter_mut() {
        if let Some(dormant) = k.dormant.take() {
            timelines.discard(dormant);
        }
        if let Some(cp) = k.checkpoint.take() {
            timelines.release(cp);
        }
    }
    println!(
        "{}: {} failure(s)",
        if checker.failures == 0 {
            "PASS"
        } else {
            "FAIL"
        },
        checker.failures
    );
    Ok(i32::from(checker.failures != 0))
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("replay_seek_gate: {error}");
            std::process::exit(2);
        }
    }
}
