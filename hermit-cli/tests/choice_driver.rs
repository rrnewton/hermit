/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The experimental choice driver (`--choice-tape`, `--choice-controller`,
//! `--choice-record`): an external source answers the chaos scheduler's
//! choice points, and the same answers give the same run.
//!
//! Every run here uses `--base-env=minimal`. With the default base
//! environment, Hermit's own command line reaches the guest's environment
//! strings, so two runs that differ only in Hermit flags would differ in the
//! guest's stack layout. Under the minimal environment they do not, and runs
//! with and without the choice flags can be compared directly.
//!
//! The last cases test `tools/libvoidstar`, the guest library whose
//! `fuzz_get_random` reads one word per call from a path Hermit is to answer
//! with `guest.random` choices. They are native: they compile the library and
//! a probe with `cc` and run no Hermit, so they check the library's own
//! behaviour, its refusal without Hermit included.

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/test_skip.rs"]
mod test_skip;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

/// Chaos seed of every run.
const SEED: &str = "--sched-seed=3";

/// Wall backstop per run for a hung tracer. A healthy run of the guest takes
/// well under a second of guest work; Hermit start-up under validation load
/// can take seconds.
const TIMEOUT_SECONDS: u64 = 120;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The repository's chaos schedule-diversity guest, compiled once. Its
/// `contention` mode prints a trace that depends on the schedule.
fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let source = repository.join("tests/e2e/determinism-stress/thread_contention.c");
        let output = scratch("guest").join("thread_contention");
        let mut command = Command::new("cc");
        command
            .args([
                "-std=c11", "-O2", "-g", "-pthread", "-Wall", "-Wextra", "-Werror",
            ])
            .arg(&source)
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
        assert!(status.success(), "guest compilation failed: {command:?}");
        output
    })
}

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("choice-driver-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create the test directory");
    dir
}

/// The Hermit command for a chaos run of the guest in `dir`, with `extra` run
/// options, not yet run.
fn hermit_command(dir: &Path, extra: &[&str]) -> Command {
    chaos_command(dir, extra, &[guest(), Path::new("contention")])
}

/// The Hermit command for a chaos run of `program` (a path, then arguments)
/// in `dir`, with `extra` run options, not yet run.
fn chaos_command(dir: &Path, extra: &[&str], program: &[&Path]) -> Command {
    let mut command = Command::new("timeout");
    command
        .current_dir(dir)
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--cpu-profile=x86-64-v2",
            "--chaos",
            SEED,
        ])
        .arg(format!("--summary-json={}", dir.join("sum.json").display()))
        .args(extra)
        .arg("--")
        .args(program);
    hermit_test::configure_guest_execution(&mut command);
    command
}

/// Run the guest in `dir` with `extra` run options and require success.
fn run(dir: &Path, extra: &[&str]) -> Output {
    run_command(hermit_command(dir, extra))
}

/// Run `command` (a Hermit chaos run) and require success.
fn run_command(mut command: Command) -> Output {
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    assert!(
        output.status.success(),
        "run failed: {rendered}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

/// The schedule a run's `--summary-json` reports: scheduler turns, virtual
/// time and timeslice count.
fn schedule(dir: &Path) -> (u64, u64, u64) {
    let text = fs::read_to_string(dir.join("sum.json")).expect("the run wrote no summary");
    let summary: serde_json::Value = serde_json::from_str(&text).expect("summary is not JSON");
    let field = |value: &serde_json::Value| value.as_u64().expect("a number");
    (
        field(&summary["sched_turns"]),
        field(&summary["virttime_elapsed"]),
        field(&summary["timeslice_stats"]["count"]),
    )
}

/// The lines of a `--choice-record` file.
fn records(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("the run wrote no choice record")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a record line is not JSON"))
        .collect()
}

/// The value tape byte `b` selects for a request, as `hermit run --help`
/// documents it: 0 keeps the default; a slice byte picks the scale times
/// 2^((b-128)/16); a priority byte spreads 255 levels over the range.
fn decode(request: &serde_json::Value, b: u8) -> u64 {
    let number = |key: &str| request[key].as_u64().expect("a number");
    let (lo, hi) = (number("lo"), number("hi"));
    if b == 0 {
        return number("default");
    }
    let v = match request["kind"].as_str().expect("a kind") {
        "sched.slice" => slice(number("scale").max(1), b),
        "sched.priority" => lo + (hi - lo) * u64::from(b - 1) / 254,
        other => panic!("unknown choice kind {other}"),
    };
    v.clamp(lo, hi)
}

/// `scale * 2^((b-128)/16)` rounded half up, as the help documents the slice
/// decode, in the same fixed point: `2^(j/16)` for `j` in `0..16` with 60
/// fractional bits.
fn slice(scale: u64, b: u8) -> u64 {
    const POWERS: [u64; 16] = [
        0x1000_0000_0000_0000,
        0x10b5_586c_f989_0f63,
        0x1172_b83c_7d51_7add,
        0x1238_7a6e_7562_3867,
        0x1306_fe0a_31b7_152e,
        0x13de_a64c_1234_2236,
        0x14bf_dad5_362a_271d,
        0x15ab_07dd_4854_2959,
        0x16a0_9e66_7f3b_cc91,
        0x17a1_1473_eb01_86d8,
        0x18ac_e542_2aa0_db5c,
        0x19c4_9182_a3f0_901c,
        0x1ae8_9f99_5ad3_ad5f,
        0x1c19_9bdd_8552_9c22,
        0x1d58_18dc_fba4_8726,
        0x1ea4_afa2_a490_d986,
    ];
    let k = i32::from(b) - 128;
    let shift = (60 - k.div_euclid(16)) as u32;
    let product = u128::from(scale) * u128::from(POWERS[k.rem_euclid(16) as usize]);
    u64::try_from((product + (1u128 << (shift - 1))) >> shift).unwrap_or(u64::MAX)
}

/// A fixed tape that changes some choices and keeps others.
fn mixed_tape() -> Vec<u8> {
    (0..96u32)
        .map(|i| {
            if i.is_multiple_of(3) {
                0
            } else {
                (i * 37 + 11) as u8
            }
        })
        .collect()
}

#[test]
fn an_empty_tape_is_the_seeded_run() {
    let dir = scratch("empty");
    let plain = run(&dir, &[]);
    let plain_schedule = schedule(&dir);
    let tape = dir.join("empty.tape");
    fs::write(&tape, b"").unwrap();
    let record = dir.join("record.jsonl");
    let taped = run(
        &dir,
        &[
            &format!("--choice-tape={}", tape.display()),
            &format!("--choice-record={}", record.display()),
        ],
    );
    assert_eq!(
        plain.stdout, taped.stdout,
        "an empty tape changed the guest's output"
    );
    assert_eq!(
        plain_schedule,
        schedule(&dir),
        "an empty tape changed the schedule"
    );
    let lines = records(&record);
    let kinds = |kind: &str| lines.iter().filter(|line| line["kind"] == kind).count();
    assert!(
        kinds("sched.slice") > 0 && kinds("sched.priority") > 0,
        "{lines:?}"
    );
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["schema"], 1, "{line}");
        assert_eq!(line["i"], index as u64, "{line}");
        assert_eq!(line["v"], line["default"], "an empty tape answered {line}");
    }
}

#[test]
fn the_same_tape_gives_the_same_run_and_decodes_as_documented() {
    let dir = scratch("same");
    let tape = dir.join("mixed.tape");
    let bytes = mixed_tape();
    fs::write(&tape, &bytes).unwrap();
    let mut outputs = Vec::new();
    for name in ["first.jsonl", "second.jsonl"] {
        let record = dir.join(name);
        outputs.push(run(
            &dir,
            &[
                &format!("--choice-tape={}", tape.display()),
                &format!("--choice-record={}", record.display()),
            ],
        ));
    }
    let first = fs::read(dir.join("first.jsonl")).unwrap();
    assert_eq!(
        first,
        fs::read(dir.join("second.jsonl")).unwrap(),
        "records differ"
    );
    assert_eq!(
        outputs[0].stdout, outputs[1].stdout,
        "the same tape changed the output"
    );
    let lines = records(&dir.join("first.jsonl"));
    let mut changed = 0;
    for line in &lines {
        let index = line["i"].as_u64().unwrap() as usize;
        let b = bytes.get(index).copied().unwrap_or(0);
        assert_eq!(
            line["v"],
            decode(line, b),
            "byte {b} decoded wrongly in {line}"
        );
        changed += usize::from(line["v"] != line["default"]);
    }
    assert!(changed > 0, "the tape changed no choice: {lines:?}");
}

/// A controller listening on `c.sock` in a test directory, answering each
/// request with `answer`.
struct Controller {
    /// Keeps the directory's descriptor, through which the socket is named.
    directory: fs::File,
    thread: std::thread::JoinHandle<usize>,
}

impl Controller {
    fn start(
        dir: &Path,
        answer: impl Fn(&serde_json::Value) -> serde_json::Value + Send + 'static,
    ) -> Self {
        // The test directory's absolute path can exceed a Unix socket
        // address's 108 bytes, so bind through a descriptor for it. Hermit
        // gets `c.sock`, relative to the run's working directory, which is
        // that directory.
        let directory = fs::File::open(dir).expect("open the test directory");
        let listener =
            UnixListener::bind(Self::path(&directory)).expect("bind the controller socket");
        // Each Hermit attempt (the harness retries a skid overshoot) is one
        // connection; `finish` ends the loop with a connection that asks
        // nothing. The count is the last attempt's answers.
        let thread = std::thread::spawn(move || {
            let mut answered = 0;
            loop {
                let (stream, _) = listener.accept().expect("accept a connection");
                let mut writer = stream.try_clone().unwrap();
                let mut served = 0;
                for line in BufReader::new(stream).lines() {
                    let request: serde_json::Value =
                        serde_json::from_str(&line.unwrap()).expect("a request is not JSON");
                    assert_eq!(request["schema"], 1, "{request}");
                    assert_eq!(request["i"], served as u64, "{request}");
                    writeln!(writer, "{}", answer(&request)).unwrap();
                    served += 1;
                }
                if served == 0 {
                    return answered;
                }
                answered = served;
            }
        });
        Controller { directory, thread }
    }

    fn path(directory: &fs::File) -> String {
        format!("/proc/self/fd/{}/c.sock", directory.as_raw_fd())
    }

    /// Stop listening, and return how many requests the last run asked.
    fn finish(self) -> usize {
        drop(std::os::unix::net::UnixStream::connect(Self::path(
            &self.directory,
        )));
        self.thread.join().expect("the controller thread failed")
    }
}

#[test]
fn a_socket_controller_gives_the_tape_run() {
    let dir = scratch("socket");
    let tape = dir.join("mixed.tape");
    let bytes = mixed_tape();
    fs::write(&tape, &bytes).unwrap();
    let taped = dir.join("taped.jsonl");
    let taped_output = run(
        &dir,
        &[
            &format!("--choice-tape={}", tape.display()),
            &format!("--choice-record={}", taped.display()),
        ],
    );
    let controller = Controller::start(&dir, move |request| {
        let index = request["i"].as_u64().unwrap();
        let b = bytes.get(index as usize).copied().unwrap_or(0);
        if b == 0 {
            serde_json::json!({"i": index, "default": true})
        } else {
            serde_json::json!({"i": index, "v": decode(request, b)})
        }
    });
    let socketed = dir.join("socketed.jsonl");
    let socket_output = run(
        &dir,
        &[
            "--choice-controller=c.sock",
            &format!("--choice-record={}", socketed.display()),
        ],
    );
    assert!(controller.finish() > 0, "the controller answered nothing");
    assert_eq!(
        fs::read(&taped).unwrap(),
        fs::read(&socketed).unwrap(),
        "the controller's run recorded other choices than the tape's"
    );
    assert_eq!(taped_output.stdout, socket_output.stdout);
}

#[test]
fn a_controller_refusal_stops_the_run_at_that_choice() {
    let dir = scratch("refusing");
    let controller = Controller::start(&dir, |request| {
        let index = request["i"].as_u64().unwrap();
        if index < 3 {
            serde_json::json!({"i": index, "default": true})
        } else {
            serde_json::json!({"i": index, "refuse": "no answer here"})
        }
    });
    let record = dir.join("record.jsonl");
    stopped(
        &dir,
        &[
            "--choice-controller=c.sock",
            &format!("--choice-record={}", record.display()),
        ],
        "HERMIT_CHOICE_REFUSED: choice 3: controller refused: no answer here",
    );
    assert_eq!(controller.finish(), 4, "the run went on past the refusal");
    let lines = records(&record);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(
        lines[..3].iter().all(|line| line["v"] == line["default"]),
        "{lines:?}"
    );
    assert_eq!(lines[3]["i"], 3, "{lines:?}");
    assert_eq!(
        lines[3]["refused"], "controller refused: no answer here",
        "{lines:?}"
    );
    assert!(lines[3].get("v").is_none(), "{lines:?}");
}

#[test]
fn a_controller_socket_nobody_listens_on_stops_the_run() {
    let dir = scratch("stale");
    // A socket file left behind by a controller that has exited.
    let directory = fs::File::open(&dir).unwrap();
    drop(UnixListener::bind(Controller::path(&directory)).unwrap());
    stopped(
        &dir,
        &["--choice-controller=c.sock"],
        "HERMIT_CHOICE_REFUSED: --choice-controller c.sock: cannot connect",
    );
}

/// Run Hermit with `extra` options, which the choice driver must stop as a
/// policy refusal with `reason` on stderr, and return its output.
fn stopped(dir: &Path, extra: &[&str], reason: &str) -> Output {
    let mut command = hermit_command(dir, extra);
    let output = command.output().expect("failed to start Hermit");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(detcore_model::HERMIT_POLICY_REFUSAL_EXIT),
        "{command:?} was not a policy refusal\n{stderr}"
    );
    assert!(stderr.contains(reason), "{stderr}");
    assert!(stderr.contains("class=policy-refusal"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    output
}

#[test]
fn a_tape_run_passes_strict_verification() {
    let dir = scratch("verify");
    let tape = dir.join("mixed.tape");
    fs::write(&tape, mixed_tape()).unwrap();
    let verdict = dir.join("verify.json");
    run(
        &dir,
        &[
            "--strict",
            "--verify",
            "--verify-strict",
            &format!("--verify-json={}", verdict.display()),
            &format!("--choice-tape={}", tape.display()),
        ],
    );
    let verdict: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&verdict).unwrap()).unwrap();
    assert_eq!(verdict["verdict"], "matched", "{verdict}");
    assert_eq!(verdict["bitwise_parity"], true, "{verdict}");
    assert_eq!(
        verdict["comparison"]["strictness"], "canonical",
        "{verdict}"
    );
    for side in ["left", "right"] {
        assert!(
            verdict["compared_log_messages"][side]
                .as_u64()
                .is_some_and(|count| count > 0),
            "strict verification compared no {side} INFO messages: {verdict}"
        );
    }
}

/// Run Hermit with `extra` options that it must refuse before the guest
/// starts, and return its stderr.
fn refused(dir: &Path, extra: &[&str]) -> String {
    let mut command = hermit_command(dir, extra);
    let output = command.output().expect("failed to start Hermit");
    assert!(!output.status.success(), "{command:?} was not refused");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn unusable_choice_options_are_refused_before_the_run() {
    let dir = scratch("refusals");
    let tape = dir.join("t.tape");
    fs::write(&tape, b"").unwrap();
    let tape_arg = format!("--choice-tape={}", tape.display());
    let record_arg = format!("--choice-record={}", dir.join("r.jsonl").display());

    let stderr = refused(&dir, &["--choice-tape=missing.tape"]);
    assert!(stderr.contains("cannot read the tape"), "{stderr}");

    let stderr = refused(&dir, &["--choice-controller=nobody.sock"]);
    assert!(stderr.contains("no Unix socket is there"), "{stderr}");

    let stderr = refused(&dir, &[&tape_arg, "--choice-controller=c.sock"]);
    assert!(stderr.contains("cannot be used with"), "{stderr}");

    // --chaos-target-races sets a new thread's priority itself, so a choice
    // source could not steer it.
    for source in [tape_arg.as_str(), "--choice-controller=c.sock"] {
        let stderr = refused(&dir, &[source, "--chaos-target-races"]);
        assert!(
            stderr.contains("cannot be used with") && stderr.contains("--chaos-target-races"),
            "{source}: {stderr}"
        );
    }

    let stderr = refused(&dir, &[&record_arg]);
    assert!(stderr.contains("--choice-tape"), "{stderr}");

    let stderr = refused(&dir, &[&tape_arg, "--choice-record=/tmp/hidden.jsonl"]);
    assert!(
        stderr.contains("not visible inside the run container"),
        "{stderr}"
    );
}

#[test]
fn the_choice_options_need_chaos() {
    let dir = scratch("needs-chaos");
    let tape = dir.join("t.tape");
    fs::write(&tape, b"").unwrap();
    let output = Command::new("timeout")
        .args(["--kill-after=2s", "60s"])
        .arg(hermit_test::hermit_binary())
        .args(["run", "--base-env=minimal"])
        .arg(format!("--choice-tape={}", tape.display()))
        .arg("--")
        .arg("/bin/true")
        .output()
        .expect("failed to start Hermit");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--chaos"), "{stderr}");
}

/// A controller's reply is read strictly. An answer outside the request's
/// range, an answer to another index, and a reply that names two answers, or
/// one of the wrong type, each stop the run at that choice as a refusal
/// instead of being read one way.
#[test]
fn malformed_controller_replies_stop_the_run_as_refusals() {
    for (name, reply, reason) in [
        (
            "above",
            serde_json::json!({"i": 0, "v": u64::MAX}),
            "is outside [",
        ),
        (
            "index",
            serde_json::json!({"i": 1, "v": 1}),
            "controller answered another index",
        ),
        (
            "refuse-and-v",
            serde_json::json!({"i": 0, "refuse": 7, "v": 1}),
            "must carry exactly one of",
        ),
        (
            "default-and-v",
            serde_json::json!({"i": 0, "default": true, "v": 1}),
            "must carry exactly one of",
        ),
        (
            "default-false",
            serde_json::json!({"i": 0, "default": false}),
            "\"default\" is not true",
        ),
        (
            "refuse-number",
            serde_json::json!({"i": 0, "refuse": 7}),
            "\"refuse\" is not a string",
        ),
        (
            "v-negative",
            serde_json::json!({"i": 0, "v": -1}),
            "\"v\" is not an unsigned integer",
        ),
        (
            "capture",
            serde_json::json!({"i": 0, "v": 1, "capture": true}),
            "carries \"capture\", which this protocol revision does not support",
        ),
    ] {
        let dir = scratch(&format!("malformed-{name}"));
        let controller = Controller::start(&dir, move |_| reply.clone());
        let output = stopped(
            &dir,
            &["--choice-controller=c.sock"],
            "HERMIT_CHOICE_REFUSED: choice 0: ",
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(reason), "{name}: {stderr}");
        assert_eq!(controller.finish(), 1, "{name}: the run went on");
    }
}

/// Without the preemption timer (turned off here by name, as a host without
/// usable performance counters turns it off) Detcore draws no timeslices, so a
/// choice source is asked for priorities only, and Hermit says so by name.
/// This case needs no PMU; the cases above that count or steer slices run in
/// the PMU validation step.
#[test]
fn without_a_timer_there_are_only_priority_choices_and_a_warning() {
    let dir = scratch("no-timer");
    let tape = dir.join("empty.tape");
    fs::write(&tape, b"").unwrap();
    let record = dir.join("r.jsonl");
    let output = run(
        &dir,
        &[
            "--max-timeslice=disabled",
            &format!("--choice-tape={}", tape.display()),
            &format!("--choice-record={}", record.display()),
        ],
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "WARNING: --choice-tape: --max-timeslice is off (no usable preemption timer, or \
             --max-timeslice=disabled), so this run has no timeslice choices, only priority \
             choices."
        ),
        "{stderr}"
    );
    let lines = records(&record);
    assert!(!lines.is_empty(), "no choices at all");
    assert!(
        lines.iter().all(|line| line["kind"] == "sched.priority"),
        "a timeslice choice without the timer: {lines:?}"
    );
    // The first request is the root's own starting priority, drawn when it
    // registers itself, before its start grant: the order of the seeded draws.
    assert_eq!(lines[0]["ctx"]["site"], "child_priority", "{}", lines[0]);
    assert_eq!(lines[0]["ctx"]["dtid"], 3, "{}", lines[0]);
}

/// Two threads that each yield [`YIELDS`] times.
const YIELDING_GUEST: &str = r#"
#include <pthread.h>
#include <sched.h>
#include <stdio.h>

static void *yielder(void *arg) {
    (void)arg;
    for (int i = 0; i < 5; i++) sched_yield();
    return 0;
}

int main(void) {
    pthread_t threads[2];
    for (int i = 0; i < 2; i++) {
        if (pthread_create(&threads[i], 0, yielder, 0) != 0) return 2;
    }
    for (int i = 0; i < 2; i++) pthread_join(threads[i], 0);
    puts("yielded");
    return 0;
}
"#;

/// The `sched_yield` calls each thread of [`YIELDING_GUEST`] makes: its
/// loop bound.
const YIELDS: usize = 5;

static YIELDING: OnceLock<PathBuf> = OnceLock::new();

/// [`YIELDING_GUEST`], compiled once.
fn yielding_guest() -> &'static Path {
    YIELDING.get_or_init(|| {
        let dir = scratch("yielding-guest");
        let source = dir.join("yielding.c");
        fs::write(&source, YIELDING_GUEST).unwrap();
        let output = dir.join("yielding");
        let mut command = Command::new("cc");
        command
            .args(["-std=c11", "-O2", "-pthread", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
        assert!(status.success(), "guest compilation failed: {command:?}");
        output
    })
}

/// A new thread's draws are choice points, as the root's are. Without the
/// timer, each `sched_yield` under `--chaos` redraws the caller's priority,
/// so each of the guest's two threads makes one `priority` choice per yield,
/// asked in its own name; default answers leave the run as the seeded one.
/// This case needs no PMU.
#[test]
fn every_thread_asks_for_its_own_draws() {
    let dir = scratch("yielding");
    let tape = dir.join("empty.tape");
    fs::write(&tape, b"").unwrap();
    let program = [yielding_guest()];
    let plain = run_command(chaos_command(&dir, &["--max-timeslice=disabled"], &program));
    let plain_schedule = schedule(&dir);

    let record = dir.join("r.jsonl");
    let defaults = run_command(chaos_command(
        &dir,
        &[
            "--max-timeslice=disabled",
            &format!("--choice-tape={}", tape.display()),
            &format!("--choice-record={}", record.display()),
        ],
        &program,
    ));
    assert_eq!(
        defaults.stdout, plain.stdout,
        "default answers changed stdout"
    );
    assert_eq!(
        schedule(&dir),
        plain_schedule,
        "default answers changed the run"
    );
    let mut redraws: BTreeMap<u64, usize> = BTreeMap::new();
    for line in records(&record) {
        if line["ctx"]["site"] == "priority" {
            let dtid = line["ctx"]["dtid"].as_u64().expect("a dtid");
            *redraws.entry(dtid).or_insert(0) += 1;
        }
    }
    assert_eq!(
        redraws.values().copied().collect::<Vec<_>>(),
        vec![YIELDS, YIELDS],
        "one priority choice per yield, from each of the two threads: {redraws:?}"
    );
    assert!(
        !redraws.contains_key(&3),
        "the root does not yield: {redraws:?}"
    );
}

/// A guest that vforks three children while a sibling thread runs, then
/// forks two: each child, and the sibling, starts at a priority the chaos
/// scheduler draws. A vfork child execs this program as `child`, which yields
/// between its writes, so whether the sibling runs in between depends on the
/// child's priority.
const FORKING_GUEST: &str = r#"
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile int done;

static void *sibling(void *arg) {
    (void)arg;
    for (unsigned long n = 1; !done; n++) {
        if ((n & 0xfff) == 0) (void)write(1, "s", 1);
    }
    return 0;
}

static void reap(pid_t child) {
    int status;
    if (child < 0 || waitpid(child, &status, 0) != child) _exit(3);
}

int main(int argc, char **argv) {
    (void)argv;
    if (argc > 1) {
        for (int k = 0; k < 4; k++) {
            (void)write(1, "c", 1);
            sched_yield();
        }
        return 0;
    }
    pthread_t thread;
    if (pthread_create(&thread, 0, sibling, 0) != 0) return 2;
    for (int i = 0; i < 3; i++) {
        pid_t child = vfork();
        if (child == 0) {
            execl("/proc/self/exe", "forking", "child", (char *)0);
            _exit(4);
        }
        reap(child);
    }
    for (int i = 0; i < 2; i++) {
        pid_t child = fork();
        if (child == 0) {
            (void)write(1, "f", 1);
            _exit(0);
        }
        reap(child);
    }
    done = 1;
    pthread_join(thread, 0);
    (void)write(1, "\n", 1);
    return 0;
}
"#;

static FORKING: OnceLock<PathBuf> = OnceLock::new();

/// [`FORKING_GUEST`], compiled once.
fn forking_guest() -> &'static Path {
    FORKING.get_or_init(|| {
        let dir = scratch("forking-guest");
        let source = dir.join("forking.c");
        fs::write(&source, FORKING_GUEST).unwrap();
        let output = dir.join("forking");
        let mut command = Command::new("cc");
        command
            .args(["-std=c11", "-O2", "-pthread", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
        assert!(status.success(), "guest compilation failed: {command:?}");
        output
    })
}

/// How many record lines name each priority site.
fn priority_sites(records: &[serde_json::Value]) -> BTreeMap<String, usize> {
    let mut sites = BTreeMap::new();
    for line in records {
        if line["kind"] == "sched.priority" {
            let site = line["ctx"]["site"].as_str().expect("a site").to_owned();
            *sites.entry(site).or_insert(0) += 1;
        }
    }
    sites
}

/// Every new thread's starting priority is one choice point, a vfork child's
/// included, and its answer is the priority the child starts at. The draw a
/// vfork parent makes only to keep its stream in step with ordinary clone asks
/// nothing.
#[test]
fn each_new_thread_or_process_is_one_priority_choice() {
    let dir = scratch("forking");
    let tape = dir.join("empty.tape");
    fs::write(&tape, b"").unwrap();
    let program = [forking_guest()];
    let plain = run_command(chaos_command(&dir, &[], &program));
    let plain_schedule = schedule(&dir);

    let record = dir.join("defaults.jsonl");
    let defaults = run_command(chaos_command(
        &dir,
        &[
            &format!("--choice-tape={}", tape.display()),
            &format!("--choice-record={}", record.display()),
        ],
        &program,
    ));
    assert_eq!(
        defaults.stdout, plain.stdout,
        "default answers changed stdout"
    );
    assert_eq!(
        schedule(&dir),
        plain_schedule,
        "default answers changed the run"
    );
    let lines = records(&record);
    let sites = priority_sites(&lines);
    assert_eq!(
        sites.get("vfork_child_priority"),
        Some(&3),
        "each vfork child should be one choice: {sites:?}"
    );
    // Ordinary clones only: those any run makes as it starts (counted from
    // /bin/true under the same options), then the sibling thread and the two
    // fork children. No draw of the vfork path asks.
    let baseline_record = dir.join("baseline.jsonl");
    run_command(chaos_command(
        &dir,
        &[
            &format!("--choice-tape={}", tape.display()),
            &format!("--choice-record={}", baseline_record.display()),
        ],
        &[Path::new("/bin/true")],
    ));
    let baseline = priority_sites(&records(&baseline_record));
    assert_eq!(baseline.get("vfork_child_priority"), None, "{baseline:?}");
    let startup = baseline.get("child_priority").copied().unwrap_or(0);
    assert_eq!(
        sites.get("child_priority"),
        Some(&(startup + 3)),
        "ordinary clones only ({startup} at startup): {sites:?}"
    );

    // Answer the first vfork child's choice with the lowest and then the
    // highest priority, keeping every earlier default (so its index stays
    // put): the answer is the priority applied, so the two runs differ.
    let first = lines
        .iter()
        .position(|line| line["ctx"]["site"] == "vfork_child_priority")
        .unwrap();
    let mut outcomes = Vec::new();
    for byte in [1u8, 255] {
        let mut bytes = vec![0u8; first + 1];
        bytes[first] = byte;
        let steered = dir.join(format!("steer-{byte}.tape"));
        fs::write(&steered, &bytes).unwrap();
        let record = dir.join(format!("steer-{byte}.jsonl"));
        let output = run_command(chaos_command(
            &dir,
            &[
                &format!("--choice-tape={}", steered.display()),
                &format!("--choice-record={}", record.display()),
            ],
            &program,
        ));
        let lines = records(&record);
        assert_eq!(lines[first]["ctx"]["site"], "vfork_child_priority");
        assert_eq!(lines[first]["v"], decode(&lines[first], byte));
        outcomes.push((output.stdout, schedule(&dir)));
    }
    assert_ne!(
        outcomes[0], outcomes[1],
        "the first vfork child's priority changed nothing: the answer is not applied"
    );
}

/// The names the public Antithesis stub exports (antithesis_stub
/// `libvoidstar.c` at commit 00659dd02d9d), which `tools/libvoidstar` must
/// export exactly.
const VOIDSTAR_EXPORTS: [&str; 14] = [
    "__sanitizer_cov_pcs_init",
    "__sanitizer_cov_trace_pc_guard",
    "__sanitizer_cov_trace_pc_guard_init",
    "__sanitizer_cov_trace_pc_guard_internal",
    "coverage_lease_generation_addr",
    "fuzz_flush",
    "fuzz_get_random",
    "fuzz_json_data",
    "fuzz_json_data_unfilterable",
    "init_coverage_module",
    "instrumentation_max_abi_version",
    "instrumentation_request_abi_version",
    "notify_coverage",
    "notify_coverage_v2",
];

/// The path `fuzz_get_random` reads. It exists on no filesystem; Hermit is to
/// intercept it.
const GUEST_RANDOM_PATH: &str = "/dev/hermit/guest-random";

/// The variable naming the directory that receives SDK records.
const OUTPUT_DIR_VARIABLE: &str = "ANTITHESIS_OUTPUT_DIR";

static VOIDSTAR: OnceLock<PathBuf> = OnceLock::new();

/// `tools/libvoidstar/libvoidstar.c`, compiled once with the command its
/// README documents, `cc -O2 -shared -fPIC`, plus warnings as errors.
fn voidstar_library() -> &'static Path {
    VOIDSTAR.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let output = scratch("voidstar-library").join("libvoidstar.so");
        let mut command = Command::new("cc");
        command
            .args(["-O2", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
            .arg(repository.join("tools/libvoidstar/libvoidstar.c"))
            .arg("-o")
            .arg(&output);
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
        assert!(
            status.success(),
            "libvoidstar compilation failed: {command:?}"
        );
        output
    })
}

/// A program that loads the library named by its first argument with
/// `dlopen`, as the SDKs do, finds each export it calls by name, and
/// exercises the group its second argument names: `abi`, `coverage`,
/// `random` or `json`. It prints what the library returned.
const VOIDSTAR_PROBE: &str = r##"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <inttypes.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/prctl.h>
#include <unistd.h>

static void *library;

static void *symbol(const char *name) {
    void *address = dlsym(library, name);
    if (address == NULL) {
        fprintf(stderr, "probe: no %s: %s\n", name, dlerror());
        _exit(2);
    }
    return address;
}

#define FN(type, name) ((type)symbol(#name))

static int abi(void) {
    uint64_t (*max)(void) = FN(uint64_t (*)(void), instrumentation_max_abi_version);
    uint64_t (*request)(uint64_t) =
        FN(uint64_t (*)(uint64_t), instrumentation_request_abi_version);
    printf("max %" PRIu64 "\n", max());
    const uint64_t asks[] = {0, 1, 3, UINT64_MAX, 0, 3};
    for (size_t i = 0; i < sizeof asks / sizeof asks[0]; i++) {
        printf("request %" PRIu64 " granted %" PRIu64 "\n", asks[i], request(asks[i]));
    }
    printf("max %" PRIu64 "\n", max());
    return 0;
}

static int coverage(void) {
    uint32_t guards[4] = {0};
    const uintptr_t pcs[2] = {1, 2};
    FN(void (*)(uint32_t *, uint32_t *), __sanitizer_cov_trace_pc_guard_init)(guards, guards + 4);
    FN(void (*)(uint32_t *), __sanitizer_cov_trace_pc_guard)(&guards[0]);
    FN(void (*)(uint32_t *, uint64_t), __sanitizer_cov_trace_pc_guard_internal)(&guards[1], 7);
    FN(void (*)(const uintptr_t *, const uintptr_t *), __sanitizer_cov_pcs_init)(pcs, pcs + 2);
    printf("guards %u %u %u %u\n", guards[0], guards[1], guards[2], guards[3]);
    printf("init_coverage_module %zu\n",
           FN(size_t (*)(size_t, const char *), init_coverage_module)(4, "probe"));
    printf("notify_coverage %d\n", (int)FN(bool (*)(size_t), notify_coverage)(5));
    printf("notify_coverage_v2 %" PRIu64 "\n",
           FN(uint64_t (*)(size_t, uint64_t), notify_coverage_v2)(5, 3));
    const uint64_t *(*lease)(void) =
        FN(const uint64_t *(*)(void), coverage_lease_generation_addr);
    const uint64_t *first = lease();
    printf("lease %s %" PRIu64 "\n", first != NULL && lease() == first ? "stable" : "moved",
           first != NULL ? *first : UINT64_MAX);
    return 0;
}

static int random_word(void) {
    /* An abort here is the expected outcome: leave no core file. */
    prctl(PR_SET_DUMPABLE, 0, 0, 0, 0);
    uint64_t word = FN(uint64_t (*)(void), fuzz_get_random)();
    printf("word %" PRIu64 "\n", word);
    return 0;
}

static int json(void) {
    void (*data)(const char *, size_t) = FN(void (*)(const char *, size_t), fuzz_json_data);
    void (*unfilterable)(const char *, size_t) =
        FN(void (*)(const char *, size_t), fuzz_json_data_unfilterable);
    data("{\"a\":1}", 7);
    unfilterable("{\"b\":2}\n", 8);
    data("{\"c\":3}, bytes past the length", 7);
    FN(void (*)(void), fuzz_flush)();
    puts("returned");
    return 0;
}

int main(int argc, char **argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: probe LIBRARY abi|coverage|random|json\n");
        return 2;
    }
    library = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (library == NULL) {
        fprintf(stderr, "probe: %s\n", dlerror());
        return 2;
    }
    if (strcmp(argv[2], "abi") == 0) return abi();
    if (strcmp(argv[2], "coverage") == 0) return coverage();
    if (strcmp(argv[2], "random") == 0) return random_word();
    if (strcmp(argv[2], "json") == 0) return json();
    fprintf(stderr, "probe: unknown mode %s\n", argv[2]);
    return 2;
}
"##;

static VOIDSTAR_PROBE_PROGRAM: OnceLock<PathBuf> = OnceLock::new();

/// [`VOIDSTAR_PROBE`], compiled once.
fn voidstar_probe() -> &'static Path {
    VOIDSTAR_PROBE_PROGRAM.get_or_init(|| {
        let dir = scratch("voidstar-probe");
        let source = dir.join("probe.c");
        fs::write(&source, VOIDSTAR_PROBE).unwrap();
        let output = dir.join("probe");
        let mut command = Command::new("cc");
        command
            .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&output)
            .arg("-ldl");
        let status = command
            .status()
            .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
        assert!(status.success(), "probe compilation failed: {command:?}");
        output
    })
}

/// Run the probe on the library natively, in `dir`, exercising `mode`, with
/// `ANTITHESIS_OUTPUT_DIR` set to `output_dir` or unset.
fn probe(dir: &Path, mode: &str, output_dir: Option<&Path>) -> Output {
    let mut command = Command::new(voidstar_probe());
    command.current_dir(dir).arg(voidstar_library()).arg(mode);
    match output_dir {
        Some(output_dir) => command.env(OUTPUT_DIR_VARIABLE, output_dir),
        None => command.env_remove(OUTPUT_DIR_VARIABLE),
    };
    command
        .output()
        .unwrap_or_else(|error| panic!("failed to run the probe: {error}"))
}

/// [`probe`], requiring success; its stdout and stderr.
fn probed(dir: &Path, mode: &str, output_dir: Option<&Path>) -> (String, String) {
    let output = probe(dir, mode, output_dir);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "probe {mode} failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    (stdout, stderr)
}

/// The library's dynamic symbol table defines exactly the stub's 14 names,
/// and `nm -D --defined-only`, where the host has `nm`, lists the same.
#[test]
fn the_voidstar_library_exports_exactly_the_stubs_fourteen_names() {
    use goblin::elf::section_header::SHN_UNDEF;
    use goblin::elf::sym::STB_LOCAL;

    let library = voidstar_library();
    let expected: BTreeSet<&str> = VOIDSTAR_EXPORTS.into_iter().collect();
    let bytes = fs::read(library).unwrap();
    let elf = goblin::elf::Elf::parse(&bytes).expect("libvoidstar.so is not ELF");
    let defined: BTreeSet<&str> = elf
        .dynsyms
        .iter()
        .filter(|symbol| symbol.st_bind() != STB_LOCAL && symbol.st_shndx != SHN_UNDEF as usize)
        .map(|symbol| elf.dynstrtab.get_at(symbol.st_name).expect("a symbol name"))
        .collect();
    assert_eq!(defined, expected, "the dynamic symbol table's exports");

    match Command::new("nm")
        .args(["-D", "--defined-only"])
        .arg(library)
        .output()
    {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "nm failed: {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            let listed: BTreeSet<&str> = text
                .lines()
                .filter_map(|line| line.split_whitespace().nth(2))
                .collect();
            assert_eq!(listed, expected, "nm -D --defined-only lists:\n{text}");
        }
        Err(error) => eprintln!("no nm ({error}): only the dynamic symbol table was read"),
    }
}

/// The library offers ABI version 1 only: the maximum is 1, every request
/// (0, 1, 3, u64::MAX, then 0 and 3 again) is granted 1, and the maximum stays
/// 1 after them.
#[test]
fn the_voidstar_library_offers_and_grants_abi_version_1_only() {
    let dir = scratch("voidstar-abi");
    let (stdout, stderr) = probed(&dir, "abi", None);
    let mut expected = String::from("max 1\n");
    for request in [0, 1, 3, u64::MAX, 0, 3] {
        expected.push_str(&format!("request {request} granted 1\n"));
    }
    expected.push_str("max 1\n");
    assert_eq!(stdout, expected);
    assert_eq!(stderr, "", "the ABI functions said something");
}

/// The coverage entry points record nothing and return the stub's inert
/// values: the guards stay zero, `init_coverage_module` returns 2^32,
/// `notify_coverage` false, `notify_coverage_v2` two 20-bit fields of ones,
/// and `coverage_lease_generation_addr` the same address on every call,
/// holding 0.
#[test]
fn the_voidstar_coverage_entry_points_return_the_stubs_inert_values() {
    let dir = scratch("voidstar-coverage");
    let (stdout, stderr) = probed(&dir, "coverage", None);
    let expected = format!(
        "guards 0 0 0 0\n\
         init_coverage_module {}\n\
         notify_coverage 0\n\
         notify_coverage_v2 {}\n\
         lease stable 0\n",
        1u64 << 32,
        (0xFFFFFu64 << 20) | 0xFFFFF,
    );
    assert_eq!(stdout, expected);
    assert_eq!(stderr, "", "the coverage functions said something");
}

/// Without Hermit's interception the path does not exist, so
/// `fuzz_get_random` returns no word: the process ends by SIGABRT, and its
/// stderr is one line naming the path. No other randomness stands in.
#[test]
fn without_hermit_fuzz_get_random_aborts_and_names_its_path() {
    let test = "without_hermit_fuzz_get_random_aborts_and_names_its_path";
    if fs::symlink_metadata(GUEST_RANDOM_PATH).is_ok() {
        test_skip::record_test_skip(
            test,
            &format!("{GUEST_RANDOM_PATH} exists on this host, so the library's open can succeed"),
        );
        return;
    }
    let dir = scratch("voidstar-random");
    let output = probe(&dir, "random", None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGABRT),
        "fuzz_get_random did not abort: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    assert_eq!(stdout, "", "a word was returned");
    let prefix = format!("hermit libvoidstar: {GUEST_RANDOM_PATH}: ");
    assert!(
        stderr.starts_with(&prefix) && stderr.ends_with('\n') && stderr.lines().count() == 1,
        "stderr is not one line naming {GUEST_RANDOM_PATH}: {stderr:?}"
    );
}

/// With `ANTITHESIS_OUTPUT_DIR` set, each `fuzz_json_data` or
/// `fuzz_json_data_unfilterable` record is appended to `sdk.jsonl` there as
/// one line: its bytes up to the given length, unchanged, plus a newline only
/// if it has none, after what the file already held. Without the variable
/// the records are dropped, and stderr says so exactly once.
#[test]
fn fuzz_json_data_appends_one_line_per_record_or_says_once_that_it_drops_them() {
    let dir = scratch("voidstar-json");
    let output_dir = dir.join("output");
    fs::create_dir(&output_dir).unwrap();
    let sdk = output_dir.join("sdk.jsonl");
    fs::write(&sdk, "{\"before\":0}\n").unwrap();
    let (stdout, stderr) = probed(&dir, "json", Some(&output_dir));
    assert_eq!(stdout, "returned\n");
    assert_eq!(stderr, "", "a record was not appended");
    assert_eq!(
        fs::read_to_string(&sdk).unwrap(),
        "{\"before\":0}\n{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n"
    );

    let dropping = scratch("voidstar-json-unset");
    let (stdout, stderr) = probed(&dropping, "json", None);
    assert_eq!(stdout, "returned\n");
    assert_eq!(
        stderr, "hermit libvoidstar: ANTITHESIS_OUTPUT_DIR unset; SDK records are dropped\n",
        "three dropped records, said once"
    );
    assert!(
        fs::read_dir(&dropping).unwrap().next().is_none(),
        "a dropped record left a file in the working directory"
    );
}
