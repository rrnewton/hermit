/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! End-to-end tests of Hermit's guest-random adapter (plan step S4, diff B),
//! written before the adapter exists. With the adapter on, Detcore is to
//! answer each 8-byte read of `/dev/hermit/guest-random` with one
//! `guest.random` choice: a word from a per-thread stream seeded by
//! `--rng-seed`, or one a choice controller picks. Without it the path exists
//! nowhere and its open fails with ENOENT.
//!
//! Every case but the R4 case is ignored until diff B lands, and diff B
//! removes every `#[ignore]`. The adapter option's name is open until diff
//! B's review settles it, so every case names it through [`ADAPTER_OPTION`].
//!
//! The guest is `tests/c/guest_random_words.c`, which reads the path directly
//! or through `tools/libvoidstar`, and prints each word as `word TID VALUE`.
//! Runs use `--base-env=minimal`, as `choice_driver.rs` explains, and no
//! `--chaos`: a choice source is allowed without `--chaos` when only guest
//! kinds are wired, and then no scheduling choice is consumed or recorded.
//!
//! Left out: cases that answer words from a `--choice-tape`. How a tape for a
//! run with guest kinds states which kinds it holds, and its refusal when they
//! do not match the run's wiring, belong to diff A (the kind), so those cases
//! come with that format.

#[path = "common/hermit_binary.rs"]
mod hermit_test;
#[path = "common/test_skip.rs"]
mod test_skip;

use std::collections::BTreeSet;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::sync::OnceLock;

/// The `hermit run` option that turns the adapter on. Its name is open until
/// diff B's review settles it.
const ADAPTER_OPTION: &str = "--guest-random";

/// The path the adapter answers. It exists on no filesystem.
const GUEST_RANDOM_PATH: &str = "/dev/hermit/guest-random";

/// `--rng-seed` of every run but the one that changes it.
const SEED: u64 = 11;

/// The option that makes a run ask the controller a test starts in its
/// directory.
const CONTROLLER: &str = "--choice-controller=c.sock";

/// Where the cases that load `tools/libvoidstar` bind it in the guest: under
/// the guest's private `/tmp`, where host mode lets a bind create its target.
const LIBRARY_IN_GUEST: &str = "/tmp/libvoidstar.so";

/// Wall backstop per run for a hung tracer. A healthy run of the guest takes
/// well under a second of guest work; Hermit start-up under validation load
/// can take seconds.
const TIMEOUT_SECONDS: u64 = 120;

fn repository() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository")
}

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("guest-random-adapter-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create the test directory");
    dir
}

fn compile(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to run cc: {error}"));
    assert!(status.success(), "compilation failed: {command:?}");
}

static GUEST: OnceLock<PathBuf> = OnceLock::new();

/// `tests/c/guest_random_words.c`, compiled once.
fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let output = scratch("guest").join("guest_random_words");
        compile(
            Command::new("cc")
                .args(["-std=c11", "-O2", "-g", "-Wall", "-Wextra", "-Werror"])
                .arg(repository().join("tests/c/guest_random_words.c"))
                .arg("-o")
                .arg(&output)
                .arg("-ldl"),
        );
        output
    })
}

static VOIDSTAR: OnceLock<PathBuf> = OnceLock::new();

/// `tools/libvoidstar/libvoidstar.c`, compiled once with the command its
/// README documents, `cc -O2 -shared -fPIC`, plus warnings as errors.
fn voidstar_library() -> &'static Path {
    VOIDSTAR.get_or_init(|| {
        let output = scratch("voidstar-library").join("libvoidstar.so");
        compile(
            Command::new("cc")
                .args(["-O2", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror"])
                .arg(repository().join("tools/libvoidstar/libvoidstar.c"))
                .arg("-o")
                .arg(&output),
        );
        output
    })
}

/// The read-only bind that puts the library at [`LIBRARY_IN_GUEST`].
fn library_bind() -> String {
    format!(
        "--bind={}:{LIBRARY_IN_GUEST}:ro",
        voidstar_library().display()
    )
}

/// The Hermit command for a run of the guest in `dir`, with `global` options
/// before `run`, `--rng-seed=seed`, `extra` run options and the guest's
/// arguments `args`, not yet run.
fn hermit_command(
    dir: &Path,
    global: &[&str],
    seed: u64,
    extra: &[&str],
    args: &[&str],
) -> Command {
    let mut command = Command::new("timeout");
    command
        .current_dir(dir)
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(global)
        .args([
            "run",
            "--base-env=minimal",
            "--no-virtualize-cpuid",
            "--cpu-profile=x86-64-v2",
        ])
        .arg(format!("--rng-seed={seed}"))
        .arg(format!("--summary-json={}", dir.join("sum.json").display()))
        .args(extra)
        .arg("--")
        .arg(guest())
        .args(args);
    hermit_test::configure_guest_execution(&mut command);
    command
}

/// Run the guest in `dir` with `--rng-seed=seed`, `extra` run options and
/// the guest's arguments `args`, and require success.
fn run(dir: &Path, seed: u64, extra: &[&str], args: &[&str]) -> Output {
    run_command(hermit_command(dir, &[], seed, extra, args))
}

/// Run `command` (a Hermit run) and require success.
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

/// Run `command`, which Hermit must refuse by name before the guest starts,
/// and return its stderr. The refusal names [`ADAPTER_OPTION`] and each of
/// `names`; an unknown-option error from the argument parser is not a
/// refusal.
fn refused_by_name(mut command: Command, names: &[&str]) -> String {
    let rendered = format!("{command:?}");
    let output = hermit_test::hermit_output(&rendered, &mut command);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !output.status.success(),
        "{rendered} was not refused\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(stdout, "", "the guest ran: {rendered}\nstderr:\n{stderr}");
    assert!(
        !stderr.contains("unexpected argument"),
        "{ADAPTER_OPTION} is not an option of {rendered}:\n{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    for name in std::iter::once(ADAPTER_OPTION).chain(names.iter().copied()) {
        assert!(
            stderr.contains(name),
            "the refusal does not name {name}: {rendered}\n{stderr}"
        );
    }
    stderr
}

/// The `hermit record start` command for a recording of the guest into
/// `data_dir`, with `extra` options and the guest's arguments `args`.
fn record_command(data_dir: &Path, extra: &[&str], args: &[&str]) -> Command {
    let mut command = Command::new("timeout");
    command
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(["record", "start", "--record-timeout=30"])
        .arg(format!("--data-dir={}", data_dir.display()))
        .args(extra)
        .arg("--")
        .arg(guest())
        .args(args);
    command
}

/// Record a named skip, and return true, where the path exists on this host:
/// there its open can succeed without the adapter.
fn skipped_where_the_path_exists(test: &str) -> bool {
    if fs::symlink_metadata(GUEST_RANDOM_PATH).is_err() {
        return false;
    }
    test_skip::record_test_skip(
        test,
        &format!(
            "{GUEST_RANDOM_PATH} exists on this host, so its open can succeed without the adapter"
        ),
    );
    true
}

/// One `word TID VALUE` line of the guest's stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Word {
    tid: u64,
    value: u64,
}

/// The words the guest printed, in order.
fn words(stdout: &[u8]) -> Vec<Word> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("word "))
        .map(|rest| {
            let (tid, value) = rest
                .split_once(' ')
                .unwrap_or_else(|| panic!("a word line without a value: {rest}"));
            Word {
                tid: tid.parse().expect("a thread id"),
                value: value.parse().expect("an unsigned 64-bit word"),
            }
        })
        .collect()
}

fn values(words: &[Word]) -> Vec<u64> {
    words.iter().map(|word| word.value).collect()
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

/// The `--choice-record` option for a record at `path`.
fn choice_record(path: &Path) -> String {
    format!("--choice-record={}", path.display())
}

/// The lines of a `--choice-record` file.
fn records(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .expect("the run wrote no choice record")
        .lines()
        .map(|line| serde_json::from_str(line).expect("a record line is not JSON"))
        .collect()
}

/// The JSON text of `key`'s value in the one-line object `line`, as written:
/// an integer must be written exactly, never as a float.
fn raw_value<'a>(line: &'a str, key: &str) -> &'a str {
    let marker = format!("\"{key}\":");
    let start = line
        .find(&marker)
        .unwrap_or_else(|| panic!("no {key} in {line}"))
        + marker.len();
    let rest = &line[start..];
    &rest[..rest.find([',', '}']).unwrap_or(rest.len())]
}

/// Require `request` to be a schema-1 `guest.random` request: the whole
/// 64-bit range, no scale, asked at the `fuzz_get_random` site by a thread.
fn assert_guest_random_request(request: &serde_json::Value) {
    assert_eq!(request["schema"], 1, "{request}");
    assert_eq!(request["kind"], "guest.random", "{request}");
    assert_eq!(request["lo"], 0, "{request}");
    assert_eq!(request["hi"], u64::MAX, "{request}");
    assert_eq!(request["scale"], 0, "{request}");
    assert!(request["default"].is_u64(), "{request}");
    assert_eq!(request["ctx"]["site"], "fuzz_get_random", "{request}");
    assert!(request["ctx"]["dtid"].is_u64(), "{request}");
}

/// Require each record line to be the request the controller was asked,
/// plus the answer `"v"`, and return the answers.
fn answers(records: &[serde_json::Value], asked: &[serde_json::Value]) -> Vec<u64> {
    assert_eq!(
        records.len(),
        asked.len(),
        "the record and the controller disagree on the requests:\n{records:?}\n{asked:?}"
    );
    records
        .iter()
        .zip(asked)
        .map(|(line, request)| {
            let mut without = line.clone();
            let v = without
                .as_object_mut()
                .expect("a record line is an object")
                .remove("v")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(|| panic!("no unsigned 64-bit v in {line}"));
            assert_eq!(&without, request, "a record line is not its request plus v");
            v
        })
        .collect()
}

/// A controller listening on `c.sock` in a test directory, answering each
/// request with `answer`: `choice_driver.rs`'s, keeping the requests.
struct Controller {
    /// Keeps the directory's descriptor, through which the socket is named.
    directory: fs::File,
    thread: std::thread::JoinHandle<Vec<serde_json::Value>>,
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
        // nothing. The requests kept are the last attempt's.
        let thread = std::thread::spawn(move || {
            let mut asked = Vec::new();
            loop {
                let (stream, _) = listener.accept().expect("accept a connection");
                let mut writer = stream.try_clone().unwrap();
                let mut requests = Vec::new();
                for line in BufReader::new(stream).lines() {
                    let request: serde_json::Value =
                        serde_json::from_str(&line.unwrap()).expect("a request is not JSON");
                    assert_eq!(request["schema"], 1, "{request}");
                    assert_eq!(request["i"], requests.len() as u64, "{request}");
                    writeln!(writer, "{}", answer(&request)).unwrap();
                    requests.push(request);
                }
                if requests.is_empty() {
                    return asked;
                }
                asked = requests;
            }
        });
        Controller { directory, thread }
    }

    /// A controller that answers every request with its default.
    fn defaults(dir: &Path) -> Self {
        Self::start(
            dir,
            |request| serde_json::json!({"i": request["i"], "default": true}),
        )
    }

    fn path(directory: &fs::File) -> String {
        format!("/proc/self/fd/{}/c.sock", directory.as_raw_fd())
    }

    /// Stop listening, and return the requests the last run asked.
    fn finish(self) -> Vec<serde_json::Value> {
        drop(UnixStream::connect(Self::path(&self.directory)));
        self.thread.join().expect("the controller thread failed")
    }
}

/// R4: without the option nothing is intercepted. The guest's open of the
/// path fails with ENOENT, as it does natively, and Hermit says nothing about
/// the adapter. (R4's other half, that such a run's schedule is
/// byte-identical to the base revision's, compares two Hermit builds: diff
/// B's evidence compares this case's `--summary-json` at the base and at the
/// head.)
#[test]
fn without_the_option_the_path_does_not_exist() {
    if skipped_where_the_path_exists("without_the_option_the_path_does_not_exist") {
        return;
    }
    let dir = scratch("without-option");
    let native = Command::new(guest())
        .arg("absent")
        .current_dir(&dir)
        .output()
        .expect("failed to run the guest natively");
    assert!(native.status.success(), "{native:?}");
    assert_eq!(String::from_utf8_lossy(&native.stdout), "open: ENOENT\n");

    let output = run(&dir, SEED, &[], &["absent"]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&native.stdout),
        "the open's result under Hermit is not Linux's"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for name in [GUEST_RANDOM_PATH, "guest.random", ADAPTER_OPTION] {
        assert!(
            !stderr.contains(name),
            "a run without the option mentions {name}:\n{stderr}"
        );
    }
}

/// The same seed gives the same words, which are a stream rather than one
/// value repeated, all read by the root thread; another `--rng-seed` gives
/// other words.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn the_same_seed_gives_the_same_words_and_another_seed_others() {
    let dir = scratch("seeds");
    let args = ["words", "4"];
    let first = run(&dir, SEED, &[ADAPTER_OPTION], &args);
    let second = run(&dir, SEED, &[ADAPTER_OPTION], &args);
    assert_eq!(
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&second.stdout),
        "the same seed gave other words"
    );
    let first = words(&first.stdout);
    assert_eq!(first.len(), 4, "{first:?}");
    assert!(
        first.iter().all(|word| word.tid == first[0].tid),
        "{first:?}"
    );
    let distinct: BTreeSet<u64> = values(&first).into_iter().collect();
    assert_eq!(distinct.len(), 4, "the words repeat: {first:?}");

    let other = words(&run(&dir, SEED + 1, &[ADAPTER_OPTION], &args).stdout);
    assert_eq!(other.len(), 4, "{other:?}");
    for (a, b) in first.iter().zip(&other) {
        assert_ne!(
            a.value,
            b.value,
            "--rng-seed={SEED} and --rng-seed={} gave the same word: {first:?} {other:?}",
            SEED + 1
        );
    }
}

/// A controller's words reach the guest exactly, the extremes included: it
/// answers the first request with 0, the second with 2^64-1, and the third
/// with its default. The guest prints each, and the record carries each as
/// an exact JSON integer.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn a_controller_picks_zero_and_the_largest_word_exactly() {
    let dir = scratch("extremes");
    let controller = Controller::start(&dir, |request| {
        match request["i"].as_u64().expect("an index") {
            0 => serde_json::json!({"i": 0, "v": 0}),
            1 => serde_json::json!({"i": 1, "v": u64::MAX}),
            index => serde_json::json!({"i": index, "default": true}),
        }
    });
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let output = run(
        &dir,
        SEED,
        &[ADAPTER_OPTION, CONTROLLER, &record_arg],
        &["words", "3"],
    );
    let asked = controller.finish();
    assert_eq!(asked.len(), 3, "{asked:?}");
    asked.iter().for_each(assert_guest_random_request);
    let tid = asked[0]["ctx"]["dtid"].as_u64().unwrap();
    let default = asked[2]["default"].as_u64().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("word {tid} 0\nword {tid} 18446744073709551615\nword {tid} {default}\n"),
    );

    assert_eq!(answers(&records(&record), &asked), [0, u64::MAX, default]);
    let text = fs::read_to_string(&record).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(raw_value(lines[0], "v"), "0", "{}", lines[0]);
    assert_eq!(
        raw_value(lines[1], "v"),
        "18446744073709551615",
        "{}",
        lines[1]
    );
    assert_eq!(
        raw_value(lines[2], "v"),
        default.to_string(),
        "{}",
        lines[2]
    );
}

/// As eventfd's read: a read of fewer than 8 bytes fails with EINVAL and asks
/// nothing; a read of 16 takes one word, returns 8 and leaves the rest of the
/// buffer alone.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn a_short_read_is_einval_and_asks_nothing_and_a_long_one_takes_one_word() {
    let dir = scratch("sizes");
    let controller = Controller::defaults(&dir);
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let output = run(
        &dir,
        SEED,
        &[ADAPTER_OPTION, CONTROLLER, &record_arg],
        &["sizes"],
    );
    let asked = controller.finish();
    let v = answers(&records(&record), &asked);
    assert_eq!(v.len(), 1, "one request, the 16-byte read's: {asked:?}");
    assert_guest_random_request(&asked[0]);
    let tid = &asked[0]["ctx"]["dtid"];
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "read 4: EINVAL\nread 16: 8\nword {tid} {}\ntail: untouched\n",
            v[0]
        ),
    );
}

/// The descriptor is read-only and not seekable, and always readable: pread
/// fails with ESPIPE, write with EBADF, an open for writing with EACCES, a
/// nonblocking read-only open succeeds, and poll reports POLLIN. None of these
/// asks for a word; the one read after them does.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn pread_write_write_opens_and_poll_follow_the_rules_and_ask_nothing() {
    let dir = scratch("rules");
    let controller = Controller::defaults(&dir);
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let output = run(
        &dir,
        SEED,
        &[ADAPTER_OPTION, CONTROLLER, &record_arg],
        &["rules"],
    );
    let asked = controller.finish();
    let v = answers(&records(&record), &asked);
    assert_eq!(v.len(), 1, "one request, the final read's: {asked:?}");
    assert_guest_random_request(&asked[0]);
    let tid = &asked[0]["ctx"]["dtid"];
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "pread: ESPIPE\n\
             write: EBADF\n\
             open O_WRONLY: EACCES\n\
             open O_RDWR: EACCES\n\
             open O_RDONLY|O_NONBLOCK: OK\n\
             poll: 1 POLLIN\n\
             word {tid} {}\n",
            v[0]
        ),
    );
}

/// A forked child reads through the descriptor it inherited but draws from a
/// stream of its own; the fork leaves the parent's stream where it was; and
/// each request names its reader in `ctx.dtid`, the thread id the reader
/// sees.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn a_forked_child_draws_its_own_stream_and_each_request_names_its_reader() {
    let dir = scratch("fork");
    let plain = words(&run(&dir, SEED, &[ADAPTER_OPTION], &["words", "3"]).stdout);
    let forked = words(&run(&dir, SEED, &[ADAPTER_OPTION], &["fork", "1", "2"]).stdout);
    assert_eq!(plain.len(), 3, "{plain:?}");
    assert_eq!(forked.len(), 5, "{forked:?}");
    // In reading order: the parent's first word, the child's two (the parent
    // waits for the child), the parent's last two.
    let (parent, child) = (forked[0].tid, forked[1].tid);
    assert_ne!(parent, child, "{forked:?}");
    assert!(
        forked[1..3].iter().all(|word| word.tid == child),
        "{forked:?}"
    );
    let parents = [forked[0], forked[3], forked[4]];
    assert_eq!(
        parents.as_slice(),
        plain.as_slice(),
        "the fork moved the parent's stream"
    );
    let children: BTreeSet<u64> = values(&forked[1..3]).into_iter().collect();
    assert_eq!(children.len(), 2, "{forked:?}");
    assert!(
        parents.iter().all(|word| !children.contains(&word.value)),
        "the child drew its parent's words: {forked:?}"
    );

    let controller = Controller::defaults(&dir);
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let controlled = words(
        &run(
            &dir,
            SEED,
            &[ADAPTER_OPTION, CONTROLLER, &record_arg],
            &["fork", "1", "2"],
        )
        .stdout,
    );
    let asked = controller.finish();
    let v = answers(&records(&record), &asked);
    assert_eq!(controlled.len(), asked.len(), "{controlled:?}\n{asked:?}");
    for ((word, request), value) in controlled.iter().zip(&asked).zip(&v) {
        assert_guest_random_request(request);
        assert_eq!(
            request["ctx"]["dtid"], word.tid,
            "the request does not name its reader {word:?}: {request}"
        );
        assert_eq!(*value, word.value, "{request}");
    }
}

/// An exec keeps the thread, and so its stream: the words read after the
/// exec continue those read before it, and each request names that thread.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn an_exec_continues_the_threads_stream() {
    let dir = scratch("exec");
    let plain = run(&dir, SEED, &[ADAPTER_OPTION], &["words", "4"]);
    let execed = run(&dir, SEED, &[ADAPTER_OPTION], &["exec", "2", "2"]);
    assert_eq!(words(&plain.stdout).len(), 4);
    assert_eq!(
        String::from_utf8_lossy(&execed.stdout),
        String::from_utf8_lossy(&plain.stdout),
        "the exec restarted or moved the thread's stream"
    );

    let controller = Controller::defaults(&dir);
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let controlled = words(
        &run(
            &dir,
            SEED,
            &[ADAPTER_OPTION, CONTROLLER, &record_arg],
            &["exec", "2", "2"],
        )
        .stdout,
    );
    let asked = controller.finish();
    let v = answers(&records(&record), &asked);
    assert_eq!(controlled.len(), 4, "{controlled:?}");
    assert_eq!(v, values(&controlled));
    for (word, request) in controlled.iter().zip(&asked) {
        assert_guest_random_request(request);
        assert_eq!(request["ctx"]["dtid"], word.tid, "{request}");
        assert_eq!(word.tid, controlled[0].tid, "{controlled:?}");
    }
}

/// R5: a controller that answers every request with its default reproduces
/// the run without one: the same stdout, exit status and schedule. Each
/// request's default is the seeded word the guest printed, and each answer
/// is that default.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn a_controller_answering_every_default_gives_the_seeded_run() {
    let dir = scratch("defaults");
    let seeded = run(&dir, SEED, &[ADAPTER_OPTION], &["words", "4"]);
    let seeded_schedule = schedule(&dir);
    let printed = values(&words(&seeded.stdout));
    assert_eq!(printed.len(), 4, "{printed:?}");

    let controller = Controller::defaults(&dir);
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let controlled = run(
        &dir,
        SEED,
        &[ADAPTER_OPTION, CONTROLLER, &record_arg],
        &["words", "4"],
    );
    let asked = controller.finish();
    assert_eq!(
        String::from_utf8_lossy(&controlled.stdout),
        String::from_utf8_lossy(&seeded.stdout),
        "default answers changed the words"
    );
    assert_eq!(controlled.status.code(), seeded.status.code());
    assert_eq!(
        schedule(&dir),
        seeded_schedule,
        "default answers changed the schedule"
    );
    asked.iter().for_each(assert_guest_random_request);
    let defaults: Vec<u64> = asked
        .iter()
        .map(|request| request["default"].as_u64().unwrap())
        .collect();
    assert_eq!(defaults, printed, "a default is not the seeded word");
    assert_eq!(answers(&records(&record), &asked), printed);
}

/// The option and the words leave getrandom's bytes as they were: the
/// adapter's stream is a stream of its own.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn getrandom_is_unchanged_by_the_option_and_the_words() {
    if skipped_where_the_path_exists("getrandom_is_unchanged_by_the_option_and_the_words") {
        return;
    }
    let dir = scratch("getrandom");
    let without =
        String::from_utf8_lossy(&run(&dir, SEED, &[], &["getrandom"]).stdout).into_owned();
    let with = String::from_utf8_lossy(&run(&dir, SEED, &[ADAPTER_OPTION], &["getrandom"]).stdout)
        .into_owned();
    let without: Vec<&str> = without.lines().collect();
    let with: Vec<&str> = with.lines().collect();
    assert_eq!(without.len(), 3, "{without:?}");
    assert_eq!(without[1], "open: ENOENT", "{without:?}");
    assert_eq!(with.len(), 3, "{with:?}");
    assert!(with[1].starts_with("word "), "{with:?}");
    assert!(
        without[0].starts_with("getrandom ") && without[2].starts_with("getrandom "),
        "{without:?}"
    );
    assert_ne!(without[0], without[2], "{without:?}");
    assert_eq!(
        [with[0], with[2]],
        [without[0], without[2]],
        "the option or a word moved getrandom's stream"
    );
}

/// Without `--chaos` a controller is asked only for words. A run with a
/// fork, which under `--chaos` would also ask for the child's starting
/// priority, asks one `guest.random` request per word and nothing else, and
/// records only those.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn without_chaos_a_controller_is_asked_only_for_words() {
    let dir = scratch("no-chaos");
    let controller = Controller::defaults(&dir);
    let record = dir.join("record.jsonl");
    let record_arg = choice_record(&record);
    let output = run(
        &dir,
        SEED,
        &[ADAPTER_OPTION, CONTROLLER, &record_arg],
        &["fork", "1", "2"],
    );
    let asked = controller.finish();
    let lines = records(&record);
    assert_eq!(words(&output.stdout).len(), 5);
    assert_eq!(
        asked.len(),
        5,
        "one request per word, and no other: {asked:?}"
    );
    assert_eq!(lines.len(), 5, "{lines:?}");
    for line in asked.iter().chain(&lines) {
        assert_eq!(
            line["kind"], "guest.random",
            "a scheduling choice without --chaos: {line}"
        );
        assert_eq!(line["ctx"]["site"], "fuzz_get_random", "{line}");
    }
}

/// Off ptrace the option is refused by name before the guest starts, as the
/// choice options are, whether or not the backend could run on this host.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn the_option_is_refused_by_name_off_ptrace() {
    let dir = scratch("backends");
    for backend in ["liteinst", "in-guest-trap", "kvm"] {
        let global = format!("--backend={backend}");
        let command = hermit_command(&dir, &[&global], SEED, &[ADAPTER_OPTION], &["words", "1"]);
        // TODO(S4 diff B): pin the refusal's exact text once diff B settles
        // it; the choice options' wording would give "--guest-random has been
        // tested only on the ptrace backend; backend `kvm` is refused, use
        // --backend=ptrace".
        refused_by_name(command, &[backend]);
    }
}

/// Under record and under replay the option is refused by name: a recording
/// asked for with it, and the replay, with it, of a recording made without
/// it.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn the_option_is_refused_by_name_under_record_and_replay() {
    let dir = scratch("record-replay");
    let refused = dir.join("refused");
    fs::create_dir(&refused).unwrap();
    // TODO(S4 diff B): pin the exact refusal texts once diff B settles them,
    // and where on the replay command line the option is accepted to be
    // refused.
    refused_by_name(
        record_command(&refused, &[ADAPTER_OPTION], &["words", "1"]),
        &["record"],
    );

    let recording = dir.join("recording");
    fs::create_dir(&recording).unwrap();
    run_command(record_command(&recording, &[], &["absent"]));
    let mut replay = Command::new("timeout");
    replay
        .arg("--kill-after=2s")
        .arg(format!("{TIMEOUT_SECONDS}s"))
        .arg(hermit_test::hermit_binary())
        .args(["replay", "--autopilot"])
        .arg(format!("--data-dir={}", recording.display()))
        .arg(ADAPTER_OPTION);
    refused_by_name(replay, &["replay"]);
}

/// L2 for a seeded run with the option: strict verification compares the two
/// runs under the canonical comparator and finds bitwise parity, having
/// compared INFO messages on both sides.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn a_seeded_run_with_the_option_passes_strict_verification() {
    let dir = scratch("verify");
    let verdict = dir.join("verify.json");
    let verdict_arg = format!("--verify-json={}", verdict.display());
    run(
        &dir,
        SEED,
        &[
            ADAPTER_OPTION,
            "--strict",
            "--verify",
            "--verify-strict",
            &verdict_arg,
        ],
        &["words", "4"],
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

/// `tools/libvoidstar`, bound read-only into the guest and loaded with
/// dlopen, returns from `fuzz_get_random` the words the guest reads directly
/// for the same seed.
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn libvoidstar_returns_the_words_of_direct_reads() {
    let dir = scratch("library");
    let direct = words(&run(&dir, SEED, &[ADAPTER_OPTION], &["words", "4"]).stdout);
    assert_eq!(direct.len(), 4, "{direct:?}");
    let bind = library_bind();
    let loaded = words(
        &run(
            &dir,
            SEED,
            &[ADAPTER_OPTION, &bind],
            &["dlopen", LIBRARY_IN_GUEST, "4"],
        )
        .stdout,
    );
    assert_eq!(loaded, direct);
}

/// The descriptor-reuse gap. A program closes every descriptor from 3 up,
/// the library's included, and opens a regular file, which takes back the
/// lowest number. `fuzz_get_random` still returns the adapter's next word,
/// never the file's bytes, and leaves the file's offset alone, while the
/// program's own descriptor reads the file. Diff C's library keeps its first
/// descriptor and so reads the file; diff B closes the gap (by opening the
/// path per call, or by checking the descriptor's identity).
#[test]
#[ignore = "S4 diff B: the guest-random transport is not implemented yet"]
fn libvoidstar_never_reads_a_recycled_descriptor() {
    let dir = scratch("reuse");
    let file = dir.join("sixteen-bytes");
    fs::write(&file, b"ABCDEFGHIJKLMNOP").unwrap();
    let direct = words(&run(&dir, SEED, &[ADAPTER_OPTION], &["words", "2"]).stdout);
    assert_eq!(direct.len(), 2, "{direct:?}");
    let bind = library_bind();
    let output = run(
        &dir,
        SEED,
        &[ADAPTER_OPTION, &bind],
        &[
            "reuse",
            LIBRARY_IN_GUEST,
            file.to_str().expect("a UTF-8 path"),
        ],
    );
    let file_word = u64::from_le_bytes(*b"ABCDEFGH");
    assert!(
        words(&output.stdout)
            .iter()
            .all(|word| word.value != file_word),
        "fuzz_get_random returned the recycled file's bytes:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let (first, second) = (direct[0], direct[1]);
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "word {} {}\n\
             file descriptor reused\n\
             file bytes 4142434445464748\n\
             word {} {}\n\
             file offset 0\n",
            first.tid, first.value, second.tid, second.value
        ),
    );
}
