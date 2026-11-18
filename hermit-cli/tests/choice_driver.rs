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

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::collections::BTreeMap;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
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
        "sched.slice" => {
            let scale = number("scale").max(1) as f64;
            (scale * slice_factor(i32::from(b) - 128)).round() as u64
        }
        "sched.priority" => lo + (hi - lo) * u64::from(b - 1) / 254,
        other => panic!("unknown choice kind {other}"),
    };
    v.clamp(lo, hi)
}

/// `2^(k/16)` as the help's slice decode computes it, bit-exactly on every
/// host: whole octaves are exact powers of two, and each sixteenth is a
/// product of repeated square roots of 2, lowest first (no `powf`, whose
/// precision varies by platform).
fn slice_factor(k: i32) -> f64 {
    let r2 = 2f64.sqrt();
    let roots = [r2.sqrt().sqrt().sqrt(), r2.sqrt().sqrt(), r2.sqrt(), r2];
    let mut factor = 1.0;
    for (i, root) in roots.iter().enumerate() {
        if k.rem_euclid(16) >> i & 1 == 1 {
            factor *= root;
        }
    }
    let step = if k < 0 { 0.5 } else { 2.0 };
    for _ in 0..k.div_euclid(16).unsigned_abs() {
        factor *= step;
    }
    factor
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
        Some(122),
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
