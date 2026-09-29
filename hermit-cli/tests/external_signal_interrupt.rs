/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for signals interrupting a blocked futex wait
//! (https://github.com/rrnewton/hermit/issues/3146).
//!
//! Linux interrupts a blocking `FUTEX_WAIT` when a signal with a handler is
//! delivered. An untimed wait restarts under `SA_RESTART`; a timed wait
//! returns `EINTR` even under `SA_RESTART`. Each case below runs the guest in
//! `tests/c/external_signal_interrupt.c` under both the ptrace and LiteInst
//! backends and asserts the guest's single deterministic `RESULT` line.
//!
//! A signal that is ignored, blocked, or ignored by default must not end a
//! wait: `poll`, `epoll_wait`, `select`, and a timed futex wait then run to
//! their 300 ms deadline.
//!
//! Signal dispositions belong to the whole process, so a sibling can change
//! one while a waiter is parked. The disposition that counts is the one in
//! force when the signal arrives, not the one when the wait began: a signal
//! caught after the wait parked ends it near the 100 ms send, and one ignored
//! after the wait parked leaves it running to its original deadline. A
//! SIGCHLD from an exiting child ends a wait whose handler is installed,
//! including a child that dies by SIGKILL or by its only thread calling `exit`,
//! whose SIGCHLD comes from the kernel alone.
//!
//! Every cell with only Hermit-internal senders, on both backends, runs under
//! `--verify --verify-strict` and requires a matched strict report.
//!
//! The watchdog lives in this host process, as in
//! `waitid_signal_interrupt.rs`: a dedicated thread drains Hermit's stderr
//! while this thread polls the process, the guest's stdout file, and an
//! independent wall-clock deadline. A lost interruption is a hang, so each
//! run must print `RESULT` within `RESULT_BOUND` of `READY`.
//!
//! External cells send `SIGUSR1` from this host process, which is outside
//! Hermit's deterministic schedule. Every such trial runs the guest through a
//! symlink in a fresh directory, so the guest is found by its exact `argv[0]`.
//!
//! Not covered here:
//! - An external signal to a precise-mode futex waiter: with no in-guest waker
//!   Hermit's deadlock detector ends the run before the signal can arrive.
//! - External signals to blocked `wait4`/`waitid`. Those still do not observe
//!   a host-queued signal; that is a separate defect in the same issue.

#[path = "common/liteinst.rs"]
mod liteinst_runtime;

use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

const BACKENDS: [&str; 2] = ["ptrace", "liteinst"];
const WATCHDOG_BACKSTOP: Duration = Duration::from_secs(90);
/// Wall-clock bound from the guest's `READY` line to its `RESULT` line. Every
/// passing run measured on 2026-09-28 finished in under 3 s.
const RESULT_BOUND: Duration = Duration::from_secs(20);
/// Wall-clock delay between `READY` and the external signal, so the guest has
/// entered the blocking call. An early signal cannot make a broken Hermit pass:
/// the handler would run first and the wait would then block with no waker.
const EXTERNAL_SIGNAL_DELAY: Duration = Duration::from_millis(500);
const EXTERNAL_TRIALS: usize = 10;
const MAX_DIAGNOSTIC_LINES: usize = 2_000;

const EINTR_FUTEX: &str = "RESULT call=futex ret=-1 errno=EINTR handler=1";
/// Guest calls that wait for fd readiness.
const READINESS_CALLS: [&str; 4] = ["poll", "epoll", "select", "rawselect"];
/// The guest's timeout for a wait that a signal must not end.
const QUIET_TIMEOUT_MS: u64 = 300;
/// Virtual-time slack allowed past that deadline before the wait returns.
/// Every must-not-wake cell measured on 2026-09-29 returned at 300-302 ms. A
/// wait that a signal at 100 ms restarts with a fresh 300 ms timeout returns
/// near 400 ms, so the slack must stay well below 100 ms to catch it.
const QUIET_OVERSHOOT_MS: u64 = 50;
/// When the guest's in-Hermit senders send their signal, after the wait began.
const SIGNAL_DELAY_MS: u64 = 100;
/// Slack past `SIGNAL_DELAY_MS` for a wait the signal must end. Natively these
/// waits return at 100-101 ms (measured 2026-09-29). A wait the signal did not
/// end returns at its 300 ms timeout or later, far outside this window.
const WAKE_SLACK_MS: u64 = 100;
/// Lower bound on a child-exit wake. The `exit` child starts its 100 ms sleep
/// at `fork()`, before the parent prints `READY`, arms its wait and takes its
/// start stamp. Under Hermit each of those syscalls advances virtual time, so
/// the wake reads about 1 ms short of `SIGNAL_DELAY_MS` (99 ms measured on
/// 2026-09-29 for `epoll` on both backends). 90 ms still separates a wake by the
/// exit from an immediate return, and the upper bound is unchanged.
const EXIT_WAKE_FLOOR_MS: u64 = 90;
/// Strict-verified repetitions of each child-exit SIGCHLD cell. The kernel also
/// posts its own SIGCHLD for the exit at a host-timed moment, so a single
/// matched pair of runs is weak evidence that the result ignores it.
const SIGCHLD_TRIALS: usize = 3;

static GUEST: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Copy, Debug)]
enum FutexMode {
    Precise,
    Polling,
}

struct GuestRun {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    /// The strict verification report, for a cell run under `--verify`.
    verify_report: Option<serde_json::Value>,
}

impl GuestRun {
    fn result_line(&self) -> Option<&str> {
        self.stdout.lines().find(|line| line.starts_with("RESULT "))
    }

    fn describe(&self) -> String {
        format!(
            "status={:?}\nverify report: {:?}\nguest stdout:\n{}\nhermit stderr:\n{}",
            self.status, self.verify_report, self.stdout, self.stderr
        )
    }

    /// The `ELAPSED ms=` value a must-not-wake, disposition-change, or
    /// child-exit cell prints.
    fn elapsed_ms(&self) -> Option<u64> {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("ELAPSED ms="))
            .and_then(|value| value.parse().ok())
    }
}

fn build_root() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("external-signal-interrupt")
}

fn guest() -> &'static Path {
    GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let root = build_root();
        fs::create_dir_all(&root).expect("failed to create guest build directory");
        // The guest must not live under /tmp: Hermit replaces guest /tmp with
        // an isolated directory, making a host /tmp fixture invisible.
        let guest = root.join("external_signal_interrupt");
        let source = repository.join("tests/c/external_signal_interrupt.c");
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile the guest: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&compile.stderr),
        );
        guest
    })
}

fn kill_process_group(child: &mut std::process::Child) {
    // The group contains Hermit and every guest process it started.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

/// Every host pid whose `argv[0]` is exactly `argv0`.
fn pids_with_argv0(argv0: &Path) -> Vec<libc::pid_t> {
    let want = argv0.as_os_str().as_encoded_bytes();
    let mut pids = Vec::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if let Ok(cmdline) = fs::read(entry.path().join("cmdline"))
            && cmdline.split(|byte| *byte == 0).next() == Some(want)
        {
            pids.push(pid);
        }
    }
    pids
}

enum StderrEvent {
    Line(String),
    Error(String),
    Eof,
}

/// Run one cell. With `external`, send `SIGUSR1` to the guest process from
/// this host process `EXTERNAL_SIGNAL_DELAY` after it prints `READY`.
fn run_cell(backend: &str, mode: FutexMode, args: &[&str], external: bool) -> GuestRun {
    if backend == "liteinst" {
        liteinst_runtime::ensure_liteinst_runtime();
    }
    // Compiling the guest also creates the directory that holds every trial.
    let guest = guest();
    let trial = tempfile::Builder::new()
        .prefix("trial-")
        .tempdir_in(build_root())
        .expect("failed to create trial directory");
    let program = trial.path().join("esi");
    std::os::unix::fs::symlink(guest, &program).expect("failed to link the guest");
    let stdout_path = trial.path().join("stdout");
    let stdout_writer = fs::File::create(&stdout_path).expect("failed to create guest stdout");

    // Strict verification replays the guest, so it needs a sender inside Hermit.
    let verify_report = (!external).then(|| trial.path().join("verify.json"));
    let mut command = Command::new(liteinst_runtime::hermit_binary());
    if verify_report.is_some() {
        command.arg("--log=info");
    }
    command.args(["run", "--backend", backend, "--strict"]);
    if let FutexMode::Polling = mode {
        command.arg("--debug-futex-mode=polling");
    }
    if let Some(report) = &verify_report {
        command
            .args(["--verify", "--verify-strict", "--verify-json"])
            .arg(report);
    }
    command
        .arg("--")
        .arg(&program)
        .args(args)
        .process_group(0)
        .stdout(Stdio::from(stdout_writer))
        .stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("failed to spawn hermit: {error}"));
    let stderr = child.stderr.take().expect("stderr was piped");
    let (send, receive) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => {
                    if send.send(StderrEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = send.send(StderrEvent::Error(error.to_string()));
                    return;
                }
            }
        }
        let _ = send.send(StderrEvent::Eof);
    });

    let started = Instant::now();
    let mut lines = Vec::new();
    let mut truncated = false;
    let mut stderr_eof = false;
    let mut status = None;
    let mut ready_at = None;
    let mut signalled = !external;
    let mut failure = None;

    while status.is_none() || !stderr_eof {
        if status.is_none() {
            status = child.try_wait().expect("failed to poll hermit");
        }
        let stdout = fs::read_to_string(&stdout_path).unwrap_or_default();
        if ready_at.is_none() && stdout.lines().any(|line| line == "READY") {
            ready_at = Some(Instant::now());
        }
        let has_result = stdout.lines().any(|line| line.starts_with("RESULT "));
        if let Some(ready) = ready_at {
            if !signalled && ready.elapsed() >= EXTERNAL_SIGNAL_DELAY {
                let pids = pids_with_argv0(&program);
                if let [pid] = pids[..] {
                    // SAFETY: plain kill(2) on a pid read from /proc.
                    if unsafe { libc::kill(pid, libc::SIGUSR1) } != 0 {
                        failure = Some(format!(
                            "kill({pid}, SIGUSR1) failed: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                } else {
                    failure = Some(format!(
                        "expected exactly one guest process with argv[0] {}, found {pids:?}",
                        program.display()
                    ));
                }
                signalled = true;
            }
            if failure.is_none() && !has_result && ready.elapsed() >= RESULT_BOUND {
                failure = Some(format!(
                    "no RESULT line within {}s of READY (external={external}): the blocked call \
                     was not interrupted",
                    RESULT_BOUND.as_secs()
                ));
            }
        }
        if failure.is_none() && started.elapsed() >= WATCHDOG_BACKSTOP {
            failure = Some(format!(
                "watchdog deadline of {}s exceeded; process_exited={}, ready={}",
                WATCHDOG_BACKSTOP.as_secs(),
                status.is_some(),
                ready_at.is_some(),
            ));
        }
        if failure.is_some() {
            break;
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(StderrEvent::Line(line)) => {
                if lines.len() < MAX_DIAGNOSTIC_LINES {
                    lines.push(line);
                } else {
                    truncated = true;
                }
            }
            Ok(StderrEvent::Error(error)) => {
                failure = Some(format!("failed while draining hermit stderr: {error}"));
                break;
            }
            Ok(StderrEvent::Eof) | Err(mpsc::RecvTimeoutError::Disconnected) => stderr_eof = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    if failure.is_some() {
        kill_process_group(&mut child);
    }
    // Always reap the process here, on timeout, normal exit, and failure.
    let waited_status = child.wait().expect("failed to wait for hermit");
    status.get_or_insert(waited_status);
    reader.join().expect("stderr reader panicked");
    let stdout = fs::read_to_string(&stdout_path).expect("failed to read guest stdout");
    let mut stderr = lines.join("\n");
    if truncated {
        stderr.push_str(&format!(
            "\n[watchdog retained the first {MAX_DIAGNOSTIC_LINES} stderr lines]"
        ));
    }
    if let Some(reason) = failure {
        panic!(
            "{backend} {mode:?} {args:?}: {reason}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}"
        );
    }
    let verify_report = verify_report.map(|report| {
        let bytes = fs::read(&report).unwrap_or_else(|error| {
            panic!(
                "{backend} {mode:?} {args:?}: strict verification did not publish its report: \
                 {error}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}"
            )
        });
        serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!("{backend} {mode:?} {args:?}: invalid verification report: {error}")
        })
    });
    GuestRun {
        status: status.expect("hermit status should be collected"),
        stdout,
        stderr,
        verify_report,
    }
}

/// Require a matched strict verification report on a cell that ran under `--verify`.
fn assert_verified(backend: &str, mode: FutexMode, args: &[&str], run: &GuestRun) {
    let Some(report) = &run.verify_report else {
        return;
    };
    assert!(
        report["verdict"] == "matched"
            && report["verified"] == true
            && report["comparison"]["strictness"] == "canonical",
        "{backend} {mode:?} {args:?}: strict verification did not match\n{}",
        run.describe()
    );
}

/// Run a cell and require a clean exit, the expected `RESULT` line, and `DONE`.
fn assert_cell(backend: &str, mode: FutexMode, args: &[&str], external: bool, expected: &str) {
    let run = run_cell(backend, mode, args, external);
    assert!(
        run.status.success() && run.result_line() == Some(expected),
        "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    assert!(
        run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: guest did not finish\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
}

/// A signal that must not end the wait: the call returns its timeout result, the
/// handler never ran, and the call took its full 300 ms timeout.
fn assert_quiet_cell(backend: &str, mode: FutexMode, args: &[&str], expected: &str) {
    let run = run_cell(backend, mode, args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected)
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    let elapsed = run.elapsed_ms();
    assert!(
        elapsed.is_some_and(|ms| {
            (QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS).contains(&ms)
        }),
        "{backend} {mode:?} {args:?}: the wait took {elapsed:?} ms, not its \
         {QUIET_TIMEOUT_MS} ms timeout\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
}

/// A signal the wait must end: the call returns `expected`, and it returned near
/// the signal, well before any timeout.
fn assert_woken_cell(backend: &str, mode: FutexMode, args: &[&str], expected: &str) {
    let run = run_cell(backend, mode, args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected)
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: expected `{expected}`\n{}",
        run.describe()
    );
    let floor = if args.contains(&"exit") {
        EXIT_WAKE_FLOOR_MS
    } else {
        SIGNAL_DELAY_MS
    };
    let elapsed = run.elapsed_ms();
    assert!(
        elapsed.is_some_and(|ms| (floor..SIGNAL_DELAY_MS + WAKE_SLACK_MS).contains(&ms)),
        "{backend} {mode:?} {args:?}: the wait took {elapsed:?} ms, not the \
         {SIGNAL_DELAY_MS} ms until the signal\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
}

/// Fix A: a polling-mode futex wait must observe a signal that Hermit did not
/// send. Before the fix the retry loop never looked for one and spun until the
/// watchdog fired. Each backend is its own test so that each stays inside the
/// per-test wall and CPU bounds.
fn assert_polling_futex_wait_observes_external_signal(backend: &str) {
    for trial in 0..EXTERNAL_TRIALS {
        let run = run_cell(backend, FutexMode::Polling, &["futex", "external"], true);
        assert!(
            run.status.success() && run.result_line() == Some(EINTR_FUTEX),
            "{backend} trial {trial}/{EXTERNAL_TRIALS}: expected `{EINTR_FUTEX}`\n{}",
            run.describe()
        );
    }
}

#[test]
fn ptrace_polling_futex_wait_is_interrupted_by_an_external_signal() {
    assert_polling_futex_wait_observes_external_signal("ptrace");
}

#[test]
fn liteinst_polling_futex_wait_is_interrupted_by_an_external_signal() {
    assert_polling_futex_wait_observes_external_signal("liteinst");
}

/// Fix C: a precise-mode futex waiter woken for a signal must return EINTR, not
/// 0 as if a FUTEX_WAKE had arrived. The thread sender is Hermit-internal
/// (tgkill) and wakes the futex 100 ms later.
#[test]
fn precise_futex_wait_is_interrupted_by_a_sibling_thread_signal() {
    for backend in BACKENDS {
        assert_cell(
            backend,
            FutexMode::Precise,
            &["futex", "thread"],
            false,
            EINTR_FUTEX,
        );
    }
}

/// A signal from a guest process that stays alive. Before the fix a parked
/// precise waiter was never woken for it and the run hung.
#[test]
fn precise_futex_wait_is_interrupted_by_a_live_sibling_process_signal() {
    for backend in BACKENDS {
        assert_cell(
            backend,
            FutexMode::Precise,
            &["futex", "process"],
            false,
            EINTR_FUTEX,
        );
    }
}

/// SIGALRM from ITIMER_REAL, which Hermit's scheduler delivers itself.
#[test]
fn futex_wait_is_interrupted_by_a_timer_signal() {
    for backend in BACKENDS {
        for mode in [FutexMode::Precise, FutexMode::Polling] {
            assert_cell(backend, mode, &["futex", "timer"], false, EINTR_FUTEX);
        }
    }
}

/// Internal senders in polling mode: a sibling thread (tgkill) and a live
/// sibling process (kill). Before the fix the thread case returned EAGAIN once
/// the word changed and the process case hung.
#[test]
fn polling_futex_wait_is_interrupted_by_internal_signals() {
    for backend in BACKENDS {
        for sender in ["thread", "process"] {
            assert_cell(
                backend,
                FutexMode::Polling,
                &["futex", sender],
                false,
                EINTR_FUTEX,
            );
        }
    }
}

/// Linux returns EINTR from a timed FUTEX_WAIT even under SA_RESTART
/// (ERESTART_RESTARTBLOCK), so the timeout does not silently restart.
#[test]
fn timed_futex_wait_returns_eintr_under_sa_restart() {
    for backend in BACKENDS {
        for mode in [FutexMode::Precise, FutexMode::Polling] {
            for sender in ["thread", "process", "timer"] {
                assert_cell(
                    backend,
                    mode,
                    &["futex", sender, "restart", "timed"],
                    false,
                    EINTR_FUTEX,
                );
            }
        }
    }
}

/// Control: an untimed FUTEX_WAIT restarts under SA_RESTART. The handler runs
/// and the call never reports EINTR. The waker thread sets the word and wakes the
/// futex only after the handler has run, so the wait must have been interrupted.
/// Natively the restarted wait is woken by the thread's FUTEX_WAKE and returns
/// 0; precise mode reproduces that. Polling mode's deterministic schedule runs
/// the restarted wait after the thread has set the word, which Linux also
/// permits and reports as EAGAIN. Each mode's result is pinned.
#[test]
fn untimed_futex_wait_restarts_under_sa_restart() {
    for backend in BACKENDS {
        assert_cell(
            backend,
            FutexMode::Precise,
            &["futex", "thread", "restart"],
            false,
            "RESULT call=futex ret=0 errno=none handler=1",
        );
        assert_cell(
            backend,
            FutexMode::Polling,
            &["futex", "thread", "restart"],
            false,
            "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
        );
    }
}

/// `poll`, `epoll_wait`, glibc `select` (pselect6 with no mask), and the
/// `select` system call end with EINTR for a caught signal from a sibling
/// thread or a live sibling process, SA_RESTART or not, as Linux does.
fn assert_readiness_waits_are_interrupted(backend: &str) {
    for call in READINESS_CALLS {
        for sender in ["thread", "process"] {
            for restart in [None, Some("restart")] {
                let mut args = vec![call, sender];
                args.extend(restart);
                assert_cell(
                    backend,
                    FutexMode::Precise,
                    &args,
                    false,
                    &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
                );
            }
        }
    }
}

#[test]
fn ptrace_readiness_waits_are_interrupted_by_internal_signals() {
    assert_readiness_waits_are_interrupted("ptrace");
}

#[test]
fn liteinst_readiness_waits_are_interrupted_by_internal_signals() {
    assert_readiness_waits_are_interrupted("liteinst");
}

/// An ignored, blocked, or default-ignored signal does not end `poll`,
/// `epoll_wait`, or either `select`: each returns 0 at its 300 ms timeout.
/// Every one of these 48 cells is strict-verified, so they are split by backend
/// and sender to keep each test inside the per-test wall and CPU bounds.
fn assert_readiness_waits_are_not_ended(backend: &str, sender: &str) {
    for call in READINESS_CALLS {
        for quiet in ["ignored", "blocked", "winch"] {
            assert_quiet_cell(
                backend,
                FutexMode::Precise,
                &[call, sender, quiet],
                &format!("RESULT call={call} ret=0 errno=none handler=0"),
            );
        }
    }
}

#[test]
fn ptrace_readiness_waits_are_not_ended_by_non_interrupting_thread_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "thread");
}

#[test]
fn ptrace_readiness_waits_are_not_ended_by_non_interrupting_process_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "process");
}

#[test]
fn liteinst_readiness_waits_are_not_ended_by_non_interrupting_thread_signals() {
    assert_readiness_waits_are_not_ended("liteinst", "thread");
}

#[test]
fn liteinst_readiness_waits_are_not_ended_by_non_interrupting_process_signals() {
    assert_readiness_waits_are_not_ended("liteinst", "process");
}

/// An ignored, blocked, or default-ignored signal does not end a timed futex wait
/// in either mode: it returns ETIMEDOUT at its original 300 ms deadline.
fn assert_timed_futex_wait_is_not_ended(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for sender in ["thread", "process"] {
            for quiet in ["ignored", "blocked", "winch"] {
                assert_quiet_cell(
                    backend,
                    mode,
                    &["futex", sender, quiet],
                    "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
                );
            }
        }
    }
}

#[test]
fn ptrace_timed_futex_wait_is_not_ended_by_non_interrupting_signals() {
    assert_timed_futex_wait_is_not_ended("ptrace");
}

#[test]
fn liteinst_timed_futex_wait_is_not_ended_by_non_interrupting_signals() {
    assert_timed_futex_wait_is_not_ended("liteinst");
}

/// Positive control: `select` already observed an external signal before the
/// fix, on both backends.
#[test]
fn select_is_interrupted_by_an_external_signal() {
    for backend in BACKENDS {
        assert_cell(
            backend,
            FutexMode::Precise,
            &["select", "external"],
            true,
            "RESULT call=select ret=-1 errno=EINTR handler=1",
        );
    }
}

/// Positive control: `wait4` and `waitid` on a live child already returned
/// EINTR for a signal from that child before the fix.
#[test]
fn child_waits_are_interrupted_by_a_live_sibling_process_signal() {
    for backend in BACKENDS {
        for call in ["wait4", "waitid"] {
            assert_cell(
                backend,
                FutexMode::Precise,
                &[call, "process"],
                false,
                &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
            );
        }
    }
}

/// A signal whose disposition a sibling changes to caught while the waiter is
/// parked ends the wait with EINTR near the send, timed or not, as Linux does:
/// SIGUSR1 turned from SIG_IGN to a handler, and SIGCHLD turned from its
/// default to a handler before a child exits. Before
/// https://github.com/rrnewton/hermit/pull/3361 stopped filtering on the
/// disposition read when the wait began, the precise wait ran to its timeout
/// and the untimed one never ended.
fn assert_caught_after_parking_ends_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for flip in ["ign2caught", "chldlate"] {
            for timed in [None, Some("timed")] {
                let mut args = vec!["futex", "thread", flip];
                args.extend(timed);
                assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
            }
        }
    }
}

#[test]
fn ptrace_futex_wait_is_ended_by_a_signal_caught_after_it_parked() {
    assert_caught_after_parking_ends_futex_wait("ptrace");
}

#[test]
fn liteinst_futex_wait_is_ended_by_a_signal_caught_after_it_parked() {
    assert_caught_after_parking_ends_futex_wait("liteinst");
}

/// As `chldlate`, but the child dies without `exit_group`: by SIGKILL, or by its
/// only thread calling `exit`. Hermit's scheduler then sends no child-exit
/// SIGCHLD of its own, so the only SIGCHLD is the kernel's, and the scheduler
/// makes it eligible at the child's logical death. The wait must still end with
/// EINTR near the death, timed or not, under strict verification, as it does on
/// Linux.
fn assert_child_death_without_exit_group_ends_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for flip in ["chldkill", "chldthrexit"] {
            for timed in [None, Some("timed")] {
                let mut args = vec!["futex", "thread", flip];
                args.extend(timed);
                assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
            }
        }
    }
}

#[test]
fn ptrace_futex_wait_is_ended_by_the_sigchld_of_a_child_that_dies_without_exit_group() {
    assert_child_death_without_exit_group_ends_futex_wait("ptrace");
}

#[test]
fn liteinst_futex_wait_is_ended_by_the_sigchld_of_a_child_that_dies_without_exit_group() {
    assert_child_death_without_exit_group_ends_futex_wait("liteinst");
}

/// A signal whose disposition a sibling changes to ignored while the waiter is
/// parked does not end the wait: a timed wait returns ETIMEDOUT at its original
/// 300 ms deadline, not after a restart with a fresh timeout, and an untimed
/// wait is ended only by the sibling's FUTEX_WAKE 200 ms after the signal.
/// Precise mode reports that wake as 0; polling mode runs its next probe after
/// the sibling set the word and reports EAGAIN, as in
/// `untimed_futex_wait_restarts_under_sa_restart`.
fn assert_ignored_after_parking_leaves_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for flip in ["caught2ign", "chldign"] {
            assert_quiet_cell(
                backend,
                mode,
                &["futex", "thread", flip, "timed"],
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
            );
            let untimed = match mode {
                FutexMode::Precise => "RESULT call=futex ret=0 errno=none handler=0",
                FutexMode::Polling => "RESULT call=futex ret=-1 errno=EAGAIN handler=0",
            };
            assert_quiet_cell(backend, mode, &["futex", "thread", flip], untimed);
        }
    }
}

#[test]
fn ptrace_futex_wait_is_not_ended_by_a_signal_ignored_after_it_parked() {
    assert_ignored_after_parking_leaves_futex_wait("ptrace");
}

#[test]
fn liteinst_futex_wait_is_not_ended_by_a_signal_ignored_after_it_parked() {
    assert_ignored_after_parking_leaves_futex_wait("liteinst");
}

/// A caught SIGCHLD from a child that exits during the wait ends a timed or
/// untimed futex wait with EINTR near the exit. Hermit's scheduler delivers the
/// child-exit SIGCHLD at a deterministic point; the kernel's own SIGCHLD for
/// the same exit arrives at a host-timed moment and must not decide the result,
/// so each cell is strict-verified `SIGCHLD_TRIALS` times. Each backend and
/// futex mode is its own test, to stay inside the per-test wall and CPU bounds.
fn assert_child_exit_ends_futex_wait(backend: &str, mode: FutexMode) {
    for _ in 0..SIGCHLD_TRIALS {
        for timed in [None, Some("timed")] {
            let mut args = vec!["futex", "exit"];
            args.extend(timed);
            assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
        }
    }
}

#[test]
fn ptrace_precise_futex_wait_is_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_futex_wait("ptrace", FutexMode::Precise);
}

#[test]
fn ptrace_polling_futex_wait_is_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_futex_wait("ptrace", FutexMode::Polling);
}

#[test]
fn liteinst_precise_futex_wait_is_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_futex_wait("liteinst", FutexMode::Precise);
}

#[test]
fn liteinst_polling_futex_wait_is_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_futex_wait("liteinst", FutexMode::Polling);
}

/// The same child-exit SIGCHLD ends `poll`, `epoll_wait`, and both `select`s
/// with EINTR near the exit, each strict-verified `SIGCHLD_TRIALS` times.
fn assert_child_exit_ends_readiness_waits(backend: &str, calls: [&str; 2]) {
    for _ in 0..SIGCHLD_TRIALS {
        for call in calls {
            assert_woken_cell(
                backend,
                FutexMode::Precise,
                &[call, "exit"],
                &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
            );
        }
    }
}

#[test]
fn ptrace_poll_and_epoll_are_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_readiness_waits("ptrace", ["poll", "epoll"]);
}

#[test]
fn ptrace_selects_are_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_readiness_waits("ptrace", ["select", "rawselect"]);
}

#[test]
fn liteinst_poll_and_epoll_are_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_readiness_waits("liteinst", ["poll", "epoll"]);
}

#[test]
fn liteinst_selects_are_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_readiness_waits("liteinst", ["select", "rawselect"]);
}
