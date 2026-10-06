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
//! `tests/c/external_signal_interrupt.c` under the ptrace backend and asserts
//! the guest's single deterministic `RESULT` line.
//!
//! A signal that is ignored, blocked, or ignored by default must not end a
//! wait: `poll`, `epoll_wait`, `select`, and a timed futex wait then run to
//! their 300 ms deadline.
//!
//! Nor does a SIGTSTP, SIGTTIN, or SIGTTOU left at its default action end a
//! `poll` with a timeout or a timed `FUTEX_WAIT`, because Detcore restarts those
//! two calls with their relative timeout re-armed. In an orphaned process group
//! Linux discards such a signal and restarts the call with its original
//! deadline, so these calls still return at 300 ms, and the other waits restart
//! with their deadline kept or, for `epoll_wait`, return EINTR.
//!
//! Signal dispositions belong to the whole process, so a sibling can change
//! one while a waiter is parked. The disposition that counts is the one in
//! force when the signal arrives, not the one when the wait began: a signal
//! caught after the wait parked ends it near the 100 ms send, and one ignored
//! after the wait parked leaves it running to its original deadline.
//!
//! A SIGCHLD never ends a gated wait (a futex wait, `poll` or `epoll_wait`),
//! whoever sends it: Hermit holds a caught SIGCHLD until the call returns, and
//! the handler runs then, as on Hermit's main branch. Linux instead returns
//! EINTR when the SIGCHLD arrives. This gives up the SIGCHLD part of
//! https://github.com/rrnewton/hermit/issues/3146 until Reverie reports every
//! delivery it holds. The `held` cells assert it: a child's exit, including a
//! child that dies by SIGKILL or by its only thread calling `exit`, leaves the
//! wait running to its 300 ms deadline or its wakeup, and the handler has run
//! by the `RESULT` line. `select` and the `select` system call are not gated,
//! so a caught SIGCHLD still ends them with EINTR, as on Linux.
//!
//! A child's SIGCHLD is queued on its parent process and taken by one thread
//! that does not block it; Linux offers it first to the thread that forked the
//! child. The role cells give the waiter a sibling that does not block SIGCHLD
//! and check both the waiter's result and which thread's handler ran. A
//! runnable sibling that forks the child takes the signal, and the waiter keeps
//! waiting to its original deadline or its wakeup. A sibling that forks the
//! child and then parks in a wait of its own holds it until that wait returns.
//! A waiting main thread that forks the child while a sibling spins does not
//! take it: the sibling does, and the sibling's wake ends the wait.
//!
//! Every cell with only Hermit-internal senders runs under strict verification
//! (`--verify --verify-strict`) and requires a matched strict report.
//!
//! The watchdog lives in this host process, as in
//! `waitid_signal_interrupt.rs`: a dedicated thread drains Hermit's stderr
//! while this thread polls the process, the guest's stdout file, and an
//! independent wall-clock deadline. A lost interruption is a hang, so each
//! run must print `RESULT` within `RESULT_BOUND` of `READY`.
//!
//! External cells send `SIGUSR1` from this host process, which is outside
//! Hermit's deterministic schedule; the `usr2` cells add `SIGUSR2`, and the
//! external-SIGCHLD cells send a caught `SIGCHLD` instead, to a guest that never
//! had a child, reaped one, or has a live one. Linux ends `select` with EINTR
//! for it in all three. Every such trial runs the guest through a symlink in a
//! fresh directory, so the guest is found by its exact `argv[0]`.
//!
//! Not covered here:
//! - An external signal to a precise-mode futex waiter: with no in-guest waker
//!   Hermit's deadlock detector ends the run before the signal can arrive.
//! - External signals to blocked `wait4`/`waitid`. Those still do not observe
//!   a host-queued signal; that is a separate defect in the same issue.
//! - A caught SIGCHLD sent from outside Hermit to a gated wait. It is held until
//!   the call returns, like every SIGCHLD (above), where Linux returns EINTR at
//!   once; Hermit main did not end `poll` or `epoll_wait` for it either. Only
//!   the select waits, which are not gated, are asserted above.
//! - The `spin` role in polling mode. Between probes a polling waiter blocks
//!   every signal, so the running sibling takes the SIGCHLD that Linux gives the
//!   waiting thread that forked the child.
//! - LiteInst. LiteInst now runs only in-guest, and the in-guest run refuses
//!   `--verify` and a guest signal handler other than `SIG_DFL` or `SIG_IGN`
//!   (https://github.com/rrnewton/reverie/issues/243). Every cell here needs one
//!   or the other: each cell with only Hermit-internal senders runs under
//!   `--verify`, and each external cell catches the signal it is sent. The
//!   LiteInst cells are restoration targets
//!   (https://github.com/rrnewton/hermit/issues/3745).
//! - A default SIGTSTP, SIGTTIN, or SIGTTOU in a process group that is not
//!   orphaned. Linux stops the process when the signal arrives; Detcore lets a
//!   `poll` with a timeout or a timed `FUTEX_WAIT` run to its end first, and the
//!   process stops when the call returns, as on hermit main.

#[path = "common/hermit_binary.rs"]
mod hermit_binary;

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

const WATCHDOG_BACKSTOP: Duration = Duration::from_secs(90);
/// Wall-clock bound from the guest's `READY` line to its `RESULT` line: 20 s,
/// unchanged since this file was added. It catches a lost interruption, which
/// hangs; it is not a timing assertion, which the `ELAPSED` windows below make
/// on Hermit's virtual clock. Every passing run measured on 2026-09-28 finished
/// in under 3 s; the rest of the bound is margin for a loaded host.
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
/// Lower bound on a wake by a child's exit (`exit`), by the `tstp` cells'
/// sender, and by the `thread` and `process` senders of the restart cells. The
/// guest takes its start stamp before it starts any sender, so each sender's
/// 100 ms sleep begins after the stamp. Until round 7 of
/// https://github.com/rrnewton/hermit/pull/3361 the stamp came after the forks,
/// the thread creation, `READY` and the epoll setup, these wakes read about
/// 1 ms short of 100 ms (99 ms measured on 2026-09-29 and 2026-10-03), and this
/// floor was 90 ms.
const EXIT_WAKE_FLOOR_MS: u64 = 100;
/// Strict-verified repetitions of each child-exit SIGCHLD cell. The kernel also
/// posts its own SIGCHLD for the exit at a host-timed moment, so a single
/// matched pair of runs is weak evidence that the result ignores it.
const SIGCHLD_TRIALS: usize = 3;
/// A role cell's SIGCHLD handler ran once, on the sibling.
const HANDLED_BY_SIBLING: &str = "HANDLER main=0 sibling=1";
/// When the steal role's untimed wait ends: the sibling forks at 100 ms and
/// wakes the futex 100 ms after the handler ran. Measured at 202-205 ms on
/// 2026-09-29, on both backends and in both futex modes.
const STEAL_WAKE_MS: u64 = 2 * SIGNAL_DELAY_MS;
/// Upper bound on the spin role's wake. Natively the wait ends at the child's
/// death, about 100 ms in. Under Hermit the spinning sibling takes the SIGCHLD
/// and then wakes the futex: the wait ended at 456-457 ms for the `spin` and
/// `spinthrexit` deaths and at 254 ms for `spinkill` on 2026-10-05. Before
/// round 9 of https://github.com/rrnewton/hermit/pull/3361, when the SIGCHLD
/// ended the main thread's wait with EINTR, it ended at 450-652 ms on
/// 2026-09-29 and at 454-656 ms on 2026-10-05.
const SPIN_WAKE_BOUND_MS: u64 = 1_000;
/// When a wait that the signal interrupted near 100 ms and that Linux then
/// restarted ends: 100 ms after the signal, when the `thread` sender wakes the
/// futex or the `process` sender's child exits. A wait that returned at the
/// signal instead reads near 100 ms, below this window.
const RESTARTED_WAKE_MS: std::ops::Range<u64> =
    SIGNAL_DELAY_MS + EXIT_WAKE_FLOOR_MS..2 * SIGNAL_DELAY_MS + WAKE_SLACK_MS;
/// Upper bound on when the handler of a signal that interrupted a child wait
/// first ran: midway between the child's signal near 100 ms and its exit near
/// 200 ms. A handler that ran only after the wait returned reads near 200 ms.
const HANDLED_BEFORE_EXIT_MS: u64 = SIGNAL_DELAY_MS + SIGNAL_DELAY_MS / 2;
/// Trials in the guest's `sigsuspend creator` mode (`CREATOR_TRIALS` in the
/// guest).
const CREATOR_TRIALS: usize = 6;
/// Trials in the guest's `poll pdeathchld` mode (`PDEATH_TRIALS` in the guest).
const PDEATH_TRIALS: usize = 3;
/// Unverified strict runs of the `poll pdeathchld` cell; each must match.
const PDEATH_RUNS: usize = 3;
/// Trials in the guest's `racing` mode (`RACING_TRIALS` in the guest).
const RACING_TRIALS: usize = 6;
/// Part of the INFO line Hermit's scheduler logs when a signal is delivered to a
/// thread that sleeps outside its run queue, here the creator in
/// `rt_sigsuspend` (`[dtid N] signal S armed signaled background thread`, in
/// detcore/src/scheduler.rs).
const SIGNALED_BACKGROUND_ARMED: &str = "armed signaled background thread";
/// Part of the INFO line it logs when it later orders that thread's
/// continuation at a fixed point of the schedule (`[step2] Reschedule signaled
/// background dtid`).
const SIGNALED_BACKGROUND_RELEASED: &str = "Reschedule signaled background dtid";

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
    /// Hermit's INFO log, for a cell run with `Observe::InfoLog`.
    info_log: Option<String>,
}

/// How a cell is observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Observe {
    /// Strict verification for a cell with no external signal; for a cell with
    /// an external signal, one plain run.
    Default,
    /// One run, not verified, that writes Hermit's INFO log to a file and keeps
    /// it in `GuestRun::info_log`. A run under `--verify` does not print its INFO
    /// messages, so a test that counts a scheduler INFO line needs this run as
    /// well as the strict-verified one.
    InfoLog,
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

    /// The first guest stdout line that starts with `prefix`.
    fn line(&self, prefix: &str) -> Option<&str> {
        self.stdout.lines().find(|line| line.starts_with(prefix))
    }

    /// The `ms=` value of the fork role's `SIBLING` line.
    fn sibling_ms(&self) -> Option<u64> {
        self.line("SIBLING ")
            .and_then(|line| line.rsplit_once(" ms="))
            .and_then(|(_, value)| value.parse().ok())
    }

    /// The `HANDLED_AT ms=` value a `restart` child-wait cell prints: when the
    /// handler first ran, after the wait began, or -1 if it never ran.
    fn handled_at_ms(&self) -> Option<i64> {
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix("HANDLED_AT ms="))
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
        // Nextest runs every test in a process of its own, so each test
        // compiles the guest. Each compiles into a file of its own and renames
        // it into place: rename(2) replaces the name atomically, so no test
        // executes a binary that another test's compiler is still writing, and
        // a guest that is already running keeps the copy it started from.
        let partial = root.join(format!(
            "external_signal_interrupt.{}.partial",
            std::process::id()
        ));
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror", "-pthread"])
            .arg(&source)
            .arg("-o")
            .arg(&partial)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile the guest: {error}"));
        assert!(
            compile.status.success(),
            "failed to compile {}\nstderr:\n{}",
            source.display(),
            String::from_utf8_lossy(&compile.stderr),
        );
        fs::rename(&partial, &guest).unwrap_or_else(|error| {
            panic!(
                "failed to move the guest from {} to {}: {error}",
                partial.display(),
                guest.display()
            )
        });
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
    let signals: &[libc::c_int] = if external { &[libc::SIGUSR1] } else { &[] };
    run_cell_with_external_signals(backend, mode, args, signals)
}

/// Run one cell, sending each of `external_signals`, in order and back to back,
/// to the guest process from this host process `EXTERNAL_SIGNAL_DELAY` after it
/// prints `READY`. A cell with no external signal runs under strict
/// verification instead.
fn run_cell_with_external_signals(
    backend: &str,
    mode: FutexMode,
    args: &[&str],
    external_signals: &[libc::c_int],
) -> GuestRun {
    run_cell_observed(backend, mode, args, external_signals, Observe::Default)
}

/// Run one cell as `run_cell_with_external_signals` does, observed as `observe`
/// says.
fn run_cell_observed(
    backend: &str,
    mode: FutexMode,
    args: &[&str],
    external_signals: &[libc::c_int],
    observe: Observe,
) -> GuestRun {
    let external = !external_signals.is_empty();
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
    let verify_report =
        (!external && observe == Observe::Default).then(|| trial.path().join("verify.json"));
    let info_log = (observe == Observe::InfoLog).then(|| trial.path().join("info.log"));
    let mut command = Command::new(hermit_binary::hermit_binary());
    if verify_report.is_some() {
        command.arg("--log=info");
    }
    if let Some(log) = &info_log {
        command.arg("--log=info").arg("--log-file").arg(log);
    }
    // `--backend` is a global option and goes before the subcommand.
    command.args(["--backend", backend, "run", "--strict"]);
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
                    for &signal in external_signals {
                        // SAFETY: plain kill(2) on a pid read from /proc.
                        if unsafe { libc::kill(pid, signal) } != 0 {
                            failure = Some(format!(
                                "kill({pid}, {signal}) failed: {}",
                                std::io::Error::last_os_error()
                            ));
                            break;
                        }
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
    let info_log = info_log.map(|log| {
        fs::read_to_string(&log).unwrap_or_else(|error| {
            panic!(
                "{backend} {mode:?} {args:?}: Hermit did not write its INFO log: {error}\nguest \
                 stdout:\n{stdout}\nhermit stderr:\n{stderr}"
            )
        })
    });
    GuestRun {
        status: status.expect("hermit status should be collected"),
        stdout,
        stderr,
        verify_report,
        info_log,
    }
}

/// Why a strict verification report is not determinism evidence, or `None` when
/// it is. The two runs must have matched under the canonical comparison of their
/// INFO logs, the report must certify bitwise parity, and each run must have
/// supplied INFO messages to compare. A match alone is not enough: an empty
/// comparison also matches, and Hermit's verifier then reports
/// `bitwise_parity: false` (`verification_report` in
/// hermit-cli/src/bin/hermit/verify.rs).
fn strict_verification_gap(report: &serde_json::Value) -> Option<String> {
    let mut gaps = Vec::new();
    if report["verdict"] != "matched" {
        gaps.push(format!("verdict is {}, not \"matched\"", report["verdict"]));
    }
    if report["verified"] != true {
        gaps.push(format!("verified is {}, not true", report["verified"]));
    }
    let comparison = &report["comparison"];
    if comparison["strictness"] != "canonical" {
        gaps.push(format!(
            "comparison.strictness is {}, not \"canonical\"",
            comparison["strictness"]
        ));
    }
    if comparison["log_scope"] != "info" {
        gaps.push(format!(
            "comparison.log_scope is {}, not \"info\"",
            comparison["log_scope"]
        ));
    }
    if report["bitwise_parity"] != true {
        gaps.push(format!(
            "bitwise_parity is {}, not true",
            report["bitwise_parity"]
        ));
    }
    for side in ["left", "right"] {
        let count = &report["compared_log_messages"][side];
        if !count.as_u64().is_some_and(|count| count > 0) {
            gaps.push(format!(
                "compared_log_messages.{side} is {count}, not a positive count"
            ));
        }
    }
    (!gaps.is_empty()).then(|| gaps.join("; "))
}

/// Require determinism evidence from a cell that ran under `--verify`: a matched
/// canonical comparison of nonzero INFO messages with bitwise parity
/// (`strict_verification_gap`).
fn assert_verified(backend: &str, mode: FutexMode, args: &[&str], run: &GuestRun) {
    let Some(report) = &run.verify_report else {
        return;
    };
    if let Some(gap) = strict_verification_gap(report) {
        panic!(
            "{backend} {mode:?} {args:?}: strict verification is not determinism evidence: \
             {gap}\n{}",
            run.describe()
        );
    }
}

/// A change that `verification_report_with` makes to a matching report.
type ReportEdit = fn(&mut serde_json::Value);

/// A strict verification report with the fields `strict_verification_gap` reads,
/// as Hermit's verifier writes them for two runs that matched on 412 INFO
/// messages each, after `edit`.
fn verification_report_with(edit: ReportEdit) -> serde_json::Value {
    let mut report = serde_json::json!({
        "verified": true,
        "bitwise_parity": true,
        "verdict": "matched",
        "comparison": {"strictness": "canonical", "log_scope": "info"},
        "compared_log_messages": {"left": 412, "right": 412},
    });
    edit(&mut report);
    report
}

/// `assert_verified` accepts only a matched canonical INFO comparison with
/// bitwise parity and nonzero message counts on both sides. Every other report
/// below is refused, including the empty comparison that also matches.
#[test]
fn strict_verification_requires_bitwise_parity_over_compared_info_messages() {
    assert_eq!(
        strict_verification_gap(&verification_report_with(|_| {})),
        None
    );
    let refused: [(&str, ReportEdit); 10] = [
        ("an empty comparison", |report| {
            report["compared_log_messages"] = serde_json::json!({"left": 0, "right": 0});
        }),
        ("an empty right side", |report| {
            report["compared_log_messages"]["right"] = serde_json::json!(0);
        }),
        ("no message counts", |report| {
            report["compared_log_messages"] = serde_json::Value::Null;
        }),
        ("no bitwise parity", |report| {
            report["bitwise_parity"] = serde_json::json!(false);
        }),
        ("an absent bitwise parity", |report| {
            report
                .as_object_mut()
                .expect("the report is an object")
                .remove("bitwise_parity");
        }),
        ("a diverged verdict", |report| {
            report["verdict"] = serde_json::json!("diverged");
        }),
        ("an unverified report", |report| {
            report["verified"] = serde_json::json!(false);
        }),
        ("a stripped comparison", |report| {
            report["comparison"]["strictness"] = serde_json::json!("stripped");
        }),
        ("a deterministic-log comparison", |report| {
            report["comparison"]["log_scope"] = serde_json::json!("deterministic");
        }),
        ("no comparison", |report| {
            report["comparison"] = serde_json::Value::Null;
        }),
    ];
    for (case, edit) in refused {
        assert!(
            strict_verification_gap(&verification_report_with(edit)).is_some(),
            "{case} was accepted as determinism evidence"
        );
    }
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

/// A signal that must not end the wait: the call returns `expected`, its timeout
/// result or the result of a wakeup the guest schedules at 300 ms, and it took
/// those 300 ms. A must-not-wake cell expects `handler=0`; a `held` cell expects
/// `handler=1`, from the handler that ran once the call returned.
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
    let floor = if args.contains(&"exit") || args.contains(&"tstp") {
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

/// A role cell: the main thread's call returns `expected` within `elapsed` ms,
/// the SIGCHLD handler ran on the thread `handler` names, and the run exits
/// cleanly with `DONE` and a matched strict report.
fn assert_role_cell(
    backend: &str,
    mode: FutexMode,
    args: &[&str],
    expected: &str,
    handler: &str,
    elapsed: std::ops::Range<u64>,
) -> GuestRun {
    let run = run_cell(backend, mode, args, false);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected)
            && run.line("HANDLER ") == Some(handler)
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {mode:?} {args:?}: expected `{expected}` and `{handler}`\n{}",
        run.describe()
    );
    let ms = run.elapsed_ms();
    assert!(
        ms.is_some_and(|ms| elapsed.contains(&ms)),
        "{backend} {mode:?} {args:?}: the wait took {ms:?} ms, outside {elapsed:?}\n{}",
        run.describe()
    );
    assert_verified(backend, mode, args, &run);
    run
}

/// Fix A: a polling-mode futex wait must observe a signal that Hermit did not
/// send. Before the fix the retry loop never looked for one and spun until the
/// watchdog fired. The cell runs `EXTERNAL_TRIALS` trials, split into two
/// tests of half as many each (`trials`) so that each test stays inside the
/// per-test wall and CPU bounds with room to spare.
fn assert_polling_futex_wait_observes_external_signal(
    backend: &str,
    trials: std::ops::Range<usize>,
) {
    for trial in trials {
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
    assert_polling_futex_wait_observes_external_signal("ptrace", FIRST_HALF_OF_EXTERNAL_TRIALS);
}

#[test]
fn ptrace_polling_futex_wait_is_interrupted_by_an_external_signal_in_later_trials() {
    assert_polling_futex_wait_observes_external_signal("ptrace", SECOND_HALF_OF_EXTERNAL_TRIALS);
}

/// Fix C: a precise-mode futex waiter woken for a signal must return EINTR, not
/// 0 as if a FUTEX_WAKE had arrived. The thread sender is Hermit-internal
/// (tgkill) and wakes the futex 100 ms later.
#[test]
fn precise_futex_wait_is_interrupted_by_a_sibling_thread_signal() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["futex", "thread"],
        false,
        EINTR_FUTEX,
    );
}

/// A signal from a guest process that stays alive. Before the fix a parked
/// precise waiter was never woken for it and the run hung.
#[test]
fn precise_futex_wait_is_interrupted_by_a_live_sibling_process_signal() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["futex", "process"],
        false,
        EINTR_FUTEX,
    );
}

/// SIGALRM from ITIMER_REAL, which Hermit's scheduler delivers itself.
#[test]
fn futex_wait_is_interrupted_by_a_timer_signal() {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        assert_cell("ptrace", mode, &["futex", "timer"], false, EINTR_FUTEX);
    }
}

/// Internal senders in polling mode: a sibling thread (tgkill) and a live
/// sibling process (kill). Before the fix the thread case returned EAGAIN once
/// the word changed and the process case hung.
#[test]
fn polling_futex_wait_is_interrupted_by_internal_signals() {
    for sender in ["thread", "process"] {
        assert_cell(
            "ptrace",
            FutexMode::Polling,
            &["futex", sender],
            false,
            EINTR_FUTEX,
        );
    }
}

/// Linux returns EINTR from a timed FUTEX_WAIT even under SA_RESTART
/// (ERESTART_RESTARTBLOCK), so the timeout does not silently restart.
#[test]
fn timed_futex_wait_returns_eintr_under_sa_restart() {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for sender in ["thread", "process", "timer"] {
            assert_cell(
                "ptrace",
                mode,
                &["futex", sender, "restart", "timed"],
                false,
                EINTR_FUTEX,
            );
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
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["futex", "thread", "restart"],
        false,
        "RESULT call=futex ret=0 errno=none handler=1",
    );
    assert_cell(
        "ptrace",
        FutexMode::Polling,
        &["futex", "thread", "restart"],
        false,
        "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
    );
}

/// `poll`, `epoll_wait`, glibc `select` (pselect6 with no mask), and the
/// `select` system call end with EINTR for a caught signal from a sibling
/// thread or a live sibling process, SA_RESTART or not, as Linux does. The 16
/// cells are split by sender, the `thread` cells in one test and the `process`
/// cells in another, so that each test stays inside the per-test CPU bound with
/// room to spare.
fn assert_readiness_waits_are_interrupted(backend: &str, sender: &str) {
    for call in READINESS_CALLS {
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

#[test]
fn ptrace_readiness_waits_are_interrupted_by_internal_signals() {
    assert_readiness_waits_are_interrupted("ptrace", "thread");
}

#[test]
fn ptrace_readiness_waits_are_interrupted_by_internal_process_signals() {
    assert_readiness_waits_are_interrupted("ptrace", "process");
}

/// An ignored, blocked, or default-ignored signal does not end `poll`,
/// `epoll_wait`, or either `select`: each returns 0 at its 300 ms timeout.
/// Every one of these 24 cells is strict-verified, so they are split by sender
/// and pair of calls, `poll` and `epoll_wait` in one test and the two `select`s
/// in another, to keep each test inside the per-test wall and CPU bounds with
/// room to spare.
fn assert_readiness_waits_are_not_ended(backend: &str, sender: &str, calls: [&str; 2]) {
    for call in calls {
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

/// The two halves of `READINESS_CALLS` that `assert_readiness_waits_are_not_ended`
/// runs in separate tests.
const POLL_AND_EPOLL: [&str; 2] = ["poll", "epoll"];
const SELECTS: [&str; 2] = ["select", "rawselect"];

#[test]
fn ptrace_readiness_waits_are_not_ended_by_non_interrupting_thread_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "thread", POLL_AND_EPOLL);
}

#[test]
fn ptrace_selects_are_not_ended_by_non_interrupting_thread_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "thread", SELECTS);
}

#[test]
fn ptrace_readiness_waits_are_not_ended_by_non_interrupting_process_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "process", POLL_AND_EPOLL);
}

#[test]
fn ptrace_selects_are_not_ended_by_non_interrupting_process_signals() {
    assert_readiness_waits_are_not_ended("ptrace", "process", SELECTS);
}

/// `POLL_AND_EPOLL` and `SELECTS` together are exactly `READINESS_CALLS`, so
/// splitting the not-ended cells between them drops none.
#[test]
fn the_not_ended_call_pairs_cover_every_readiness_call() {
    let mut split: Vec<&str> = POLL_AND_EPOLL.iter().chain(&SELECTS).copied().collect();
    let mut all = READINESS_CALLS.to_vec();
    split.sort_unstable();
    all.sort_unstable();
    assert_eq!(split, all);
}

/// A call that does not wait reports its result although a caught signal races
/// it: a ready descriptor for `poll` and `ppoll`, a queued event for
/// `epoll_wait` and `epoll_pwait`, or an argument error, EINVAL for
/// `epoll_wait` with maxevents 0, EBADF for `epoll_wait` on a descriptor that is
/// not open, and EFAULT for `rt_sigtimedwait` with a set it cannot read. Linux
/// looks at pending signals only after those checks: do_poll returns a ready
/// count before it checks signal_pending, ep_poll hands over queued events
/// first, and do_epoll_wait and rt_sigtimedwait check their arguments first.
/// So in each of the guest's six trials, wherever the sibling's SIGUSR1 lands,
/// the call returns its result and the handler runs once; the cell is
/// strict-verified. Before round 7 of
/// https://github.com/rrnewton/hermit/pull/3361, Hermit's first probe of these
/// calls saw the pending signal first and returned EINTR in the trial where
/// the signal was already pending when the call began (`matched=5`).
fn assert_racing_call(backend: &str, call: &str) {
    assert_cell(
        backend,
        FutexMode::Precise,
        &[call, "racing"],
        false,
        &format!("RESULT call={call} role=racing trials={RACING_TRIALS} matched={RACING_TRIALS}"),
    );
}

#[test]
fn ptrace_poll_reports_a_ready_descriptor_before_a_racing_signal() {
    assert_racing_call("ptrace", "poll");
}

#[test]
fn ptrace_ppoll_reports_a_ready_descriptor_before_a_racing_signal() {
    assert_racing_call("ptrace", "ppoll");
}

#[test]
fn ptrace_epoll_wait_reports_a_queued_event_before_a_racing_signal() {
    assert_racing_call("ptrace", "epoll");
}

#[test]
fn ptrace_epoll_pwait_reports_a_queued_event_before_a_racing_signal() {
    assert_racing_call("ptrace", "epollpwait");
}

#[test]
fn ptrace_epoll_wait_reports_einval_for_zero_maxevents_before_a_racing_signal() {
    assert_racing_call("ptrace", "epollinval");
}

#[test]
fn ptrace_epoll_wait_reports_ebadf_for_a_closed_descriptor_before_a_racing_signal() {
    assert_racing_call("ptrace", "epollbadf");
}

#[test]
fn ptrace_rt_sigtimedwait_reports_efault_for_an_unreadable_set_before_a_racing_signal() {
    assert_racing_call("ptrace", "sigtimedwaitfault");
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

/// The waits whose restart re-arms a relative timeout keep their deadline when
/// a SIGTSTP left at SIG_DFL arrives in an orphaned process group (the guest's
/// `tstp` option). Linux discards that signal: the process does not stop, no
/// handler runs, and the kernel restarts the interrupted call with the end time
/// it saved, so `poll` and a timed `FUTEX_WAIT` return their timeout result at
/// 300 ms. Detcore restarts these two calls with their relative timeout
/// re-armed, so it leaves them waiting through a default SIGTSTP, SIGTTIN or
/// SIGTTOU, as hermit main does, instead of ending them. Ending them returned
/// near 400 ms (review of https://github.com/rrnewton/hermit/pull/3361 at
/// `cbb36408`, finding 4).
fn assert_discarded_default_stop_leaves_rearming_waits(backend: &str) {
    for sender in ["thread", "process"] {
        for mode in [FutexMode::Precise, FutexMode::Polling] {
            assert_quiet_cell(
                backend,
                mode,
                &["futex", sender, "tstp"],
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=0",
            );
        }
        assert_quiet_cell(
            backend,
            FutexMode::Precise,
            &["poll", sender, "tstp"],
            "RESULT call=poll ret=0 errno=none handler=0",
        );
    }
}

#[test]
fn ptrace_timed_futex_wait_and_poll_keep_their_deadline_through_a_discarded_default_stop() {
    assert_discarded_default_stop_leaves_rearming_waits("ptrace");
}

/// How many SIGCHLDs the `chldflood` cells send after their SIGUSR1.
const SIGCHLD_FLOOD: usize = 20_000;

/// `poll` and a timed polling `FUTEX_WAIT` keep their deadline while a flood of
/// default-ignored SIGCHLD from outside Hermit arrives as they start (the
/// guest's `chldflood` option). Linux neither ends nor restarts either call for
/// that signal, so both return their timeout result at 300 ms. The flood keeps
/// a SIGCHLD arriving after Detcore's `/proc` read and before its injection,
/// where the stop cannot be identified; that stop used to end the wait with a
/// restart code that, with no handler to run, restarted the call with its
/// relative timeout started again (round-8 High 1 on
/// https://github.com/rrnewton/hermit/pull/3361). The unit tests
/// `a_stop_that_cannot_be_identified_keeps_a_timed_wait_and_its_deadline` and
/// `a_timed_wait_keeps_its_deadline_past_a_held_sigchld` pin the decision and
/// are the tests that fail without it. This test also passed at round-7 head
/// 3032cd65 when measured: the unidentified stop falls in the call's first
/// turn, where a restart costs microseconds of virtual time, well inside the
/// 50 ms overshoot. It guards the deadline, and the liveness of a deadline
/// wait's unbounded retry, end to end.
fn assert_sigchld_flood_leaves_timed_waits_their_deadline(backend: &str) {
    let mut signals = vec![libc::SIGUSR1];
    signals.extend(std::iter::repeat_n(libc::SIGCHLD, SIGCHLD_FLOOD));
    let cells: [(&[&str], &str); 2] = [
        (
            &["poll", "external", "chldflood"],
            "RESULT call=poll ret=0 errno=none handler=1",
        ),
        (
            &["futex", "external", "timed", "chldflood"],
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
        ),
    ];
    for (args, expected) in cells {
        let run = run_cell_with_external_signals(backend, FutexMode::Polling, args, &signals);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected)
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?}: expected `{expected}`\n{}",
            run.describe()
        );
        let elapsed = run.elapsed_ms();
        assert!(
            elapsed.is_some_and(|ms| {
                (QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS).contains(&ms)
            }),
            "{backend} {args:?}: the wait took {elapsed:?} ms, not its \
             {QUIET_TIMEOUT_MS} ms timeout\n{}",
            run.describe()
        );
    }
}

#[test]
fn ptrace_timed_futex_wait_and_poll_keep_their_deadline_through_a_sigchld_flood() {
    assert_sigchld_flood_leaves_timed_waits_their_deadline("ptrace");
}

/// Control for the test above: the same discarded SIGTSTP still ends, or
/// restarts, every other wait as Linux does. glibc `select`, the `select`
/// system call, and `sem_timedwait` restart with their deadline kept (Detcore
/// writes the time left back for both selects, and `sem_timedwait` passes an
/// absolute deadline), so they return their timeout result at 300 ms.
/// `epoll_wait` is not restarted: Linux returns EINTR when the signal arrives,
/// although no handler runs.
fn assert_discarded_default_stop_is_handled_as_on_linux(backend: &str) {
    for (call, expected) in [
        ("select", "RESULT call=select ret=0 errno=none handler=0"),
        (
            "rawselect",
            "RESULT call=rawselect ret=0 errno=none handler=0",
        ),
        ("sem", "RESULT call=sem ret=-1 errno=ETIMEDOUT handler=0"),
    ] {
        assert_quiet_cell(
            backend,
            FutexMode::Precise,
            &[call, "thread", "tstp"],
            expected,
        );
    }
    for sender in ["thread", "process"] {
        assert_woken_cell(
            backend,
            FutexMode::Precise,
            &["epoll", sender, "tstp"],
            "RESULT call=epoll ret=-1 errno=EINTR handler=0",
        );
    }
}

#[test]
fn ptrace_other_waits_take_a_discarded_default_stop_as_on_linux() {
    assert_discarded_default_stop_is_handled_as_on_linux("ptrace");
}

/// Positive control: `select` already observed an external signal before the
/// fix, on both backends.
#[test]
fn select_is_interrupted_by_an_external_signal() {
    assert_cell(
        "ptrace",
        FutexMode::Precise,
        &["select", "external"],
        true,
        "RESULT call=select ret=-1 errno=EINTR handler=1",
    );
}

/// The guest options that catch SIGCHLD in a process that never had a child,
/// that reaped one, and that has a live one.
const EXTERNAL_SIGCHLD_OPTIONS: [&str; 3] = ["extchld", "extchldreaped", "extchldlive"];

/// A caught SIGCHLD sent with kill(2) from outside Hermit ends glibc's `select`,
/// which is `pselect6` with no mask, and the `select` system call with EINTR,
/// as on Linux, whether or not the process has had a child. Before select and
/// pselect6 stopped asking the scheduler whether a pending SIGCHLD may end the
/// wait, the scheduler, which only makes the SIGCHLD it can order eligible,
/// held this one in a process that had had a child, and the wait never ended
/// (https://github.com/rrnewton/hermit/issues/3146).
fn assert_external_sigchld_ends_selects(backend: &str) {
    for option in EXTERNAL_SIGCHLD_OPTIONS {
        for call in ["select", "rawselect"] {
            let args = [call, "external", option];
            let run = run_cell_with_external_signals(
                backend,
                FutexMode::Precise,
                &args,
                &[libc::SIGCHLD],
            );
            let expected = format!("RESULT call={call} ret=-1 errno=EINTR handler=1");
            assert!(
                run.status.success() && run.result_line() == Some(expected.as_str()),
                "{backend} {args:?}: expected `{expected}`\n{}",
                run.describe()
            );
            assert!(
                run.stdout.lines().any(|line| line == "DONE"),
                "{backend} {args:?}: guest did not finish\n{}",
                run.describe()
            );
        }
    }
}

#[test]
fn ptrace_selects_are_ended_by_an_external_sigchld_with_or_without_a_child() {
    assert_external_sigchld_ends_selects("ptrace");
}

/// Positive control: `wait4` and `waitid` on a live child already returned
/// EINTR for a signal from that child before the fix.
#[test]
fn child_waits_are_interrupted_by_a_live_sibling_process_signal() {
    for call in ["wait4", "waitid"] {
        assert_cell(
            "ptrace",
            FutexMode::Precise,
            &[call, "process"],
            false,
            &format!("RESULT call={call} ret=-1 errno=EINTR handler=1"),
        );
    }
}

/// A signal whose disposition a sibling changes to caught while the waiter is
/// parked ends the wait with EINTR near the send, timed or not, as Linux does:
/// SIGUSR1 turned from SIG_IGN to a handler. Before
/// https://github.com/rrnewton/hermit/pull/3361 stopped filtering on the
/// disposition read when the wait began, the precise wait ran to its timeout
/// and the untimed one never ended. The SIGCHLD half of this cell (`chldlate`)
/// moved to `assert_caught_sigchld_is_held_by_futex_wait`: a SIGCHLD no longer
/// ends a gated wait.
fn assert_caught_after_parking_ends_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for timed in [None, Some("timed")] {
            let mut args = vec!["futex", "thread", "ign2caught"];
            args.extend(timed);
            assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
        }
    }
}

#[test]
fn ptrace_futex_wait_is_ended_by_a_signal_caught_after_it_parked() {
    assert_caught_after_parking_ends_futex_wait("ptrace");
}

/// A SIGCHLD whose disposition a sibling changes to caught while the waiter is
/// parked, from a child that then dies by `exit_group` (`chldlate`), by SIGKILL
/// (`chldkill`), or by its only thread calling `exit` (`chldthrexit`), does not
/// end the futex wait: Hermit holds it until the call returns (the guest's
/// `held` option). A timed wait returns ETIMEDOUT at its 300 ms deadline, and
/// an untimed one is ended by the sibling's FUTEX_WAKE 200 ms after the signal,
/// which precise mode reports as 0 and polling mode as EAGAIN, as in
/// `assert_ignored_after_parking_leaves_futex_wait`. The handler has run by
/// the `RESULT` line, and each cell is strict-verified. Linux instead ends the
/// wait with EINTR near the child's death; Hermit main holds the SIGCHLD too.
/// Until round 9 of https://github.com/rrnewton/hermit/pull/3361 these cells
/// asserted that EINTR. Each futex mode is its own test, to stay inside the
/// per-test wall and CPU bounds.
fn assert_caught_sigchld_is_held_by_futex_wait(backend: &str, mode: FutexMode) {
    for flip in ["chldlate", "chldkill", "chldthrexit"] {
        assert_quiet_cell(
            backend,
            mode,
            &["futex", "thread", flip, "held", "timed"],
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
        );
        let untimed = match mode {
            FutexMode::Precise => "RESULT call=futex ret=0 errno=none handler=1",
            FutexMode::Polling => "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
        };
        assert_quiet_cell(backend, mode, &["futex", "thread", flip, "held"], untimed);
    }
}

#[test]
fn ptrace_precise_futex_wait_holds_a_sigchld_caught_after_it_parked_until_it_returns() {
    assert_caught_sigchld_is_held_by_futex_wait("ptrace", FutexMode::Precise);
}

#[test]
fn ptrace_polling_futex_wait_holds_a_sigchld_caught_after_it_parked_until_it_returns() {
    assert_caught_sigchld_is_held_by_futex_wait("ptrace", FutexMode::Polling);
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

/// A caught SIGCHLD from a child that exits during the wait does not end a
/// futex wait: Hermit holds it until the call returns. With the guest's `held`
/// option the wait has a 300 ms timeout, so it returns ETIMEDOUT at that
/// deadline, and the handler has run by the `RESULT` line. Linux instead ends
/// the wait with EINTR near the exit; Hermit main holds the SIGCHLD too. The
/// kernel posts its own SIGCHLD for the exit at a host-timed moment, which must
/// not decide the result, so each cell is strict-verified `SIGCHLD_TRIALS`
/// times. Until round 9 of https://github.com/rrnewton/hermit/pull/3361 these
/// cells asserted EINTR near the exit, for a timed and an untimed wait; with
/// `held` both waits have the same 300 ms timeout, so one cell covers them.
/// Each futex mode is its own test, to stay inside the per-test wall and CPU
/// bounds.
fn assert_child_exit_sigchld_is_held_by_futex_wait(backend: &str, mode: FutexMode) {
    for _ in 0..SIGCHLD_TRIALS {
        assert_quiet_cell(
            backend,
            mode,
            &["futex", "exit", "held"],
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
        );
    }
}

#[test]
fn ptrace_precise_futex_wait_holds_the_sigchld_of_an_exiting_child_until_it_returns() {
    assert_child_exit_sigchld_is_held_by_futex_wait("ptrace", FutexMode::Precise);
}

#[test]
fn ptrace_polling_futex_wait_holds_the_sigchld_of_an_exiting_child_until_it_returns() {
    assert_child_exit_sigchld_is_held_by_futex_wait("ptrace", FutexMode::Polling);
}

/// The same child-exit SIGCHLD does not end `poll` or `epoll_wait` either:
/// with `held` each returns 0 at its 300 ms timeout, and the handler has run by
/// the `RESULT` line, each strict-verified `SIGCHLD_TRIALS` times. Until round
/// 9 of https://github.com/rrnewton/hermit/pull/3361 these cells asserted EINTR
/// near the exit, as Linux returns.
fn assert_child_exit_sigchld_is_held_by_readiness_waits(backend: &str) {
    for _ in 0..SIGCHLD_TRIALS {
        for call in ["poll", "epoll"] {
            assert_quiet_cell(
                backend,
                FutexMode::Precise,
                &[call, "exit", "held"],
                &format!("RESULT call={call} ret=0 errno=none handler=1"),
            );
        }
    }
}

#[test]
fn ptrace_poll_and_epoll_hold_the_sigchld_of_an_exiting_child_until_they_return() {
    assert_child_exit_sigchld_is_held_by_readiness_waits("ptrace");
}

/// The same child-exit SIGCHLD ends both `select`s with EINTR near the exit,
/// as on Linux, each strict-verified `SIGCHLD_TRIALS` times: select and
/// pselect6 are not gated.
fn assert_child_exit_ends_selects(backend: &str) {
    for _ in 0..SIGCHLD_TRIALS {
        for call in ["select", "rawselect"] {
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
fn ptrace_selects_are_ended_by_a_caught_sigchld_from_an_exiting_child() {
    assert_child_exit_ends_selects("ptrace");
}

/// A child's SIGCHLD goes to the thread that forked it when that thread does
/// not block it and is running (Linux's `complete_signal`). Here a sibling forks
/// a child that dies at once and stays runnable until the handler has run, so
/// the main thread's futex wait is not interrupted. A timed wait returns
/// ETIMEDOUT at its original 300 ms deadline; an untimed one is ended by the
/// sibling's FUTEX_WAKE 100 ms after the handler ran, which precise mode reports
/// as 0 and polling mode as EAGAIN, as in
/// `untimed_futex_wait_restarts_under_sa_restart`. The child dies by
/// `exit_group`, by SIGKILL, and by its only thread calling `exit`. Each wait
/// runs from a cold and from a warm call site (the guest's `warm` option, which
/// matters to a backend that patches a call site at its first execution).
/// Timed and untimed waits are separate tests so that each test stays within
/// two thirds of the per-test CPU budget.
///
/// Before https://github.com/rrnewton/hermit/pull/3361 was fixed Hermit also
/// woke the main thread for the sibling's SIGCHLD in precise mode. The kernel
/// then restarted a timed wait with a fresh timeout, ending it near 400 ms, and
/// at LiteInst's patched site the kernel-internal errno 512 or 514 reached the
/// guest. Polling mode already left the wait alone; its cells guard that. The
/// ptrace precise untimed cell also passes without the fix, because there the
/// kernel restarts an untimed `FUTEX_WAIT` transparently.
fn assert_runnable_sibling_takes_the_sigchld(backend: &str, mode: FutexMode, timed: bool) {
    let (expected, elapsed) = match (timed, mode) {
        (true, _) => (
            "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
            QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS,
        ),
        (false, FutexMode::Precise) => (
            "RESULT call=futex ret=0 errno=none handler=1",
            STEAL_WAKE_MS..STEAL_WAKE_MS + QUIET_OVERSHOOT_MS,
        ),
        (false, FutexMode::Polling) => (
            "RESULT call=futex ret=-1 errno=EAGAIN handler=1",
            STEAL_WAKE_MS..STEAL_WAKE_MS + QUIET_OVERSHOOT_MS,
        ),
    };
    for death in ["stealgrp", "stealkill", "stealthrexit"] {
        for warm in [false, true] {
            let mut args = vec!["futex", "thread"];
            if timed {
                args.push("timed");
            }
            if warm {
                args.push("warm");
            }
            args.push(death);
            assert_role_cell(
                backend,
                mode,
                &args,
                expected,
                HANDLED_BY_SIBLING,
                elapsed.clone(),
            );
        }
    }
}

#[test]
fn ptrace_precise_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Precise, true);
}

#[test]
fn ptrace_precise_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Precise, false);
}

#[test]
fn ptrace_polling_timed_futex_wait_keeps_its_deadline_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Polling, true);
}

#[test]
fn ptrace_polling_untimed_futex_wait_keeps_waiting_when_a_runnable_sibling_takes_the_sigchld() {
    assert_runnable_sibling_takes_the_sigchld("ptrace", FutexMode::Polling, false);
}

/// As above, but the sibling that forks the child, which dies 100 ms later,
/// then parks in a 300 ms FUTEX_WAIT of its own. The SIGCHLD does not end the
/// sibling's wait: Hermit holds it until that call returns, so the sibling's
/// wait returns ETIMEDOUT at its 300 ms deadline and its handler runs then.
/// The main thread's timed wait also runs to ETIMEDOUT at its 300 ms deadline.
/// Linux instead ends the sibling's wait with EINTR near 100 ms, and until
/// round 9 of https://github.com/rrnewton/hermit/pull/3361 this cell asserted
/// that. Before https://github.com/rrnewton/hermit/pull/3361 was fixed, a child
/// that called `exit_group` had its SIGCHLD sent to the main thread instead,
/// ending the main thread's wait at 100 ms.
fn assert_parked_forker_holds_the_sigchld(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for death in ["forkgrp", "forkkill", "forkthrexit"] {
            let args = ["futex", "thread", "timed", death];
            let run = assert_role_cell(
                backend,
                mode,
                &args,
                "RESULT call=futex ret=-1 errno=ETIMEDOUT handler=1",
                HANDLED_BY_SIBLING,
                QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS,
            );
            let ms = run.sibling_ms();
            assert!(
                run.line("SIBLING ")
                    .is_some_and(|line| line.starts_with("SIBLING ret=-1 errno=ETIMEDOUT ms="))
                    && ms.is_some_and(|ms| {
                        (QUIET_TIMEOUT_MS..QUIET_TIMEOUT_MS + QUIET_OVERSHOOT_MS).contains(&ms)
                    }),
                "{backend} {mode:?} {args:?}: expected the sibling's wait to run to \
                 ETIMEDOUT at its {QUIET_TIMEOUT_MS} ms deadline, not end after {ms:?} ms\n{}",
                run.describe()
            );
        }
    }
}

#[test]
fn ptrace_futex_wait_of_the_thread_that_forked_the_child_holds_its_sigchld_until_it_returns() {
    assert_parked_forker_holds_the_sigchld("ptrace");
}

/// The thread that forked the child does not take its SIGCHLD while it waits,
/// although Linux offers the signal to that thread first. Here the main thread
/// forks a child that dies after 100 ms and waits with no timeout, while a
/// sibling that does not block SIGCHLD spins in user code, with no system
/// calls, until a handler has run, and then wakes the futex. The handler runs
/// once, on the sibling, and the sibling's wake ends the main thread's wait,
/// which returns 0 within `SPIN_WAKE_BOUND_MS` for every death: at 456-457 ms
/// when the child calls `exit_group` or its only thread calls `exit`, and at
/// 254 ms when it sends itself SIGKILL (measured on 2026-10-05). Linux instead
/// ends the main thread's wait with EINTR near the child's death and runs the
/// handler there. Until round 9 of https://github.com/rrnewton/hermit/pull/3361
/// this cell asserted that EINTR and the main thread's handler, for an untimed
/// and a timed (10 s) wait, in two tests. The sibling does not wake a timed
/// wait, which now runs to its 10 s timeout (ETIMEDOUT at 10003 ms, measured
/// on 2026-10-05), so only the untimed wait is tested. Hermit's main branch
/// behaved the same way before https://github.com/rrnewton/hermit/pull/3361:
/// the spinning sibling took the SIGCHLD there too. Precise mode only; see the
/// module documentation for polling mode.
fn assert_waiting_forker_leaves_the_sigchld_to_the_sibling(backend: &str) {
    for death in ["spin", "spinkill", "spinthrexit"] {
        assert_role_cell(
            backend,
            FutexMode::Precise,
            &["futex", "exit", death],
            "RESULT call=futex ret=0 errno=none handler=1",
            HANDLED_BY_SIBLING,
            EXIT_WAKE_FLOOR_MS..SPIN_WAKE_BOUND_MS,
        );
    }
}

#[test]
fn ptrace_precise_futex_wait_leaves_its_childs_sigchld_to_a_running_sibling() {
    assert_waiting_forker_leaves_the_sigchld_to_the_sibling("ptrace");
}

/// A thread other than the thread-group leader forks a child and waits for it in
/// `rt_sigsuspend` with SIGCHLD unblocked, while the leader blocks SIGCHLD and
/// waits in `pthread_join`. Linux sends the child's SIGCHLD to the thread that
/// forked it, so each of the guest's six trials ends with the creator's handler
/// running once and `sigsuspend` returning EINTR after the child's exit, and the
/// cell is strict-verified. Hermit's scheduler does not keep a thread in
/// `rt_sigsuspend` on its run queue; it delivers the signal by arming the
/// sleeper and ordering its continuation at a fixed point of the schedule. A
/// second run, with Hermit's INFO log written to a file, counts the arm and
/// release lines of that path: one each per trial. Before
/// https://github.com/rrnewton/hermit/pull/3361 round 7, Hermit panicked when the
/// child's exit sent its SIGCHLD to the sleeping creator ("should be parked",
/// exit status 125).
fn assert_nonleader_creator_takes_its_childs_sigchld_in_sigsuspend(backend: &str) {
    let args = ["sigsuspend", "creator"];
    let expected = format!(
        "RESULT call=sigsuspend role=creator trials={CREATOR_TRIALS} matched={CREATOR_TRIALS}"
    );
    assert_cell(backend, FutexMode::Precise, &args, false, &expected);
    let run = run_cell_observed(backend, FutexMode::Precise, &args, &[], Observe::InfoLog);
    assert!(
        run.status.success()
            && run.result_line() == Some(expected.as_str())
            && run.stdout.lines().any(|line| line == "DONE"),
        "{backend} {args:?} with an INFO log: expected `{expected}`\n{}",
        run.describe()
    );
    let log = run
        .info_log
        .as_deref()
        .expect("an InfoLog run keeps its log");
    let armed = log.matches(SIGNALED_BACKGROUND_ARMED).count();
    let released = log.matches(SIGNALED_BACKGROUND_RELEASED).count();
    assert!(
        armed == CREATOR_TRIALS && released == CREATOR_TRIALS,
        "{backend} {args:?}: expected {CREATOR_TRIALS} `{SIGNALED_BACKGROUND_ARMED}` and \
         {CREATOR_TRIALS} `{SIGNALED_BACKGROUND_RELEASED}` INFO lines, one per trial; found \
         {armed} and {released}\n{}",
        run.describe()
    );
}

#[test]
fn ptrace_nonleader_creator_takes_its_childs_sigchld_in_sigsuspend() {
    assert_nonleader_creator_takes_its_childs_sigchld_in_sigsuspend("ptrace");
}

/// A child that armed SIGCHLD as its parent-death signal (`PR_SET_PDEATHSIG`)
/// polls no descriptors for 300 ms while another guest process kills its parent,
/// which sleeps outside Hermit's run queue (the guest's `poll pdeathchld` mode).
/// The child has never had a child. Linux returns EINTR when the parent dies.
/// Under Hermit the parent exits physically before the scheduler deregisters it,
/// at an instant set by the host, so counting that SIGCHLD would end the poll at
/// a host-dependent turn. Hermit keeps the behaviour from before the external
/// signal work instead: in each trial the poll returns 0 after its full timeout
/// (the guest's 300 ms and 50 ms overshoot match `QUIET_TIMEOUT_MS` and
/// `QUIET_OVERSHOOT_MS`) and the handler runs once afterwards. Round 7 of
/// https://github.com/rrnewton/hermit/pull/3361 counted that SIGCHLD under its
/// childless-process exception (round-8 High 3) and returned EINTR. Round 9
/// removed that exception: no SIGCHLD ends a gated wait.
///
/// The cell runs `PDEATH_RUNS` times under `--strict` without `--verify`, and
/// every trial of every run must keep its deadline. It is not strict-verified:
/// killing a parked process from another guest is itself host-timed in Hermit.
/// The tracer retires the killed parent at a host-set point relative to the
/// killer's own `kill` completion and to the killer's SIGCHLD, so two runs'
/// INFO logs differ on the killer's side whatever the child does. That race is
/// the physical-death-before-logical-deregistration race the finding names; this
/// pull request does not close it, so it claims only the child's result here.
fn assert_parent_death_sigchld_keeps_a_childless_poll_to_its_deadline(backend: &str) {
    let args = ["poll", "pdeathchld"];
    let expected =
        format!("RESULT call=poll role=pdeathchld trials={PDEATH_TRIALS} matched={PDEATH_TRIALS}");
    for attempt in 0..PDEATH_RUNS {
        let run = run_cell_observed(backend, FutexMode::Precise, &args, &[], Observe::InfoLog);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected.as_str())
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?} run {attempt} of {PDEATH_RUNS}: expected `{expected}`\n{}",
            run.describe()
        );
    }
}

#[test]
fn ptrace_parent_death_sigchld_keeps_a_childless_poll_to_its_deadline() {
    assert_parent_death_sigchld_keeps_a_childless_poll_to_its_deadline("ptrace");
}

/// `wait4` and `waitid` restart under SA_RESTART, as Linux restarts them: the
/// waited child signals the parent near 100 ms and exits 100 ms later. The
/// handler runs near 100 ms, before `HANDLED_BEFORE_EXIT_MS`, the call does not
/// report EINTR, and the restarted wait returns the child near 200 ms.
/// Strict-verified.
fn assert_child_waits_restart_under_sa_restart(backend: &str) {
    for (call, expected) in [
        ("wait4", "RESULT call=wait4 ret=child errno=none handler=1"),
        ("waitid", "RESULT call=waitid ret=0 errno=none handler=1"),
    ] {
        let args = [call, "process", "restart"];
        let run = run_cell(backend, FutexMode::Precise, &args, false);
        assert!(
            run.status.success()
                && run.result_line() == Some(expected)
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?}: expected `{expected}`\n{}",
            run.describe()
        );
        let elapsed = run.elapsed_ms();
        assert!(
            elapsed.is_some_and(|ms| RESTARTED_WAKE_MS.contains(&ms)),
            "{backend} {args:?}: the wait took {elapsed:?} ms, outside \
             {RESTARTED_WAKE_MS:?}\n{}",
            run.describe()
        );
        let handled_at = run.handled_at_ms();
        assert!(
            handled_at.is_some_and(|ms| {
                (EXIT_WAKE_FLOOR_MS as i64..HANDLED_BEFORE_EXIT_MS as i64).contains(&ms)
            }),
            "{backend} {args:?}: the handler first ran at {handled_at:?} ms, not near the \
             child's signal at {SIGNAL_DELAY_MS} ms: the signal did not interrupt the wait\n{}",
            run.describe()
        );
        assert_verified(backend, FutexMode::Precise, &args, &run);
    }
}

#[test]
fn ptrace_child_waits_restart_under_sa_restart() {
    assert_child_waits_restart_under_sa_restart("ptrace");
}

/// A caught signal and a default-ignored SIGCHLD pending together end a futex
/// wait with EINTR near the signal, timed or not, and the handler runs, as on
/// Linux. The sibling reaps a child, whose SIGCHLD stays pending where only the
/// waiter can take it, and then sends SIGUSR1. Under ptrace the kernel queues
/// even a default-ignored signal and stops the guest for it, so both signals
/// reach the backend, and the backend holds one signal per thread.
/// Strict-verified.
fn assert_caught_signal_with_a_pending_sigchld_ends_futex_wait(backend: &str) {
    for mode in [FutexMode::Precise, FutexMode::Polling] {
        for timed in [false, true] {
            let mut args = vec!["futex", "thread", "chldpend"];
            if timed {
                args.push("timed");
            }
            assert_woken_cell(backend, mode, &args, EINTR_FUTEX);
        }
    }
}

#[test]
fn ptrace_futex_wait_is_ended_by_a_caught_signal_pending_with_a_default_ignored_sigchld() {
    assert_caught_signal_with_a_pending_sigchld_ends_futex_wait("ptrace");
}

/// Two caught signals sent together end a polling futex wait with EINTR, and
/// both handlers run, as on Linux: the harness sends SIGUSR1 and SIGUSR2 back to
/// back from outside Hermit, and the guest reports both handlers on its
/// `HANDLED` line. The backend holds one signal per thread, so a second signal
/// that stops the guest must not replace the first. External signals arrive at
/// host-timed moments, so the cell runs `EXTERNAL_TRIALS` trials, split into
/// two tests of half as many each (`trials`) so that each test stays inside the
/// per-test CPU bound with room to spare.
fn assert_two_external_signals_are_both_delivered(backend: &str, trials: std::ops::Range<usize>) {
    let args = ["futex", "external", "usr2"];
    for trial in trials {
        let run = run_cell_with_external_signals(
            backend,
            FutexMode::Polling,
            &args,
            &[libc::SIGUSR1, libc::SIGUSR2],
        );
        assert!(
            run.status.success()
                && run.result_line() == Some(EINTR_FUTEX)
                && run.line("HANDLED ") == Some("HANDLED usr1=1 usr2=1")
                && run.stdout.lines().any(|line| line == "DONE"),
            "{backend} {args:?} trial {trial}/{EXTERNAL_TRIALS}: expected `{EINTR_FUTEX}` and \
             `HANDLED usr1=1 usr2=1`\n{}",
            run.describe()
        );
    }
}

/// The first half of `EXTERNAL_TRIALS`; the second half is
/// `SECOND_HALF_OF_EXTERNAL_TRIALS`.
const FIRST_HALF_OF_EXTERNAL_TRIALS: std::ops::Range<usize> = 0..EXTERNAL_TRIALS / 2;
const SECOND_HALF_OF_EXTERNAL_TRIALS: std::ops::Range<usize> = EXTERNAL_TRIALS / 2..EXTERNAL_TRIALS;

#[test]
fn ptrace_polling_futex_wait_delivers_both_of_two_external_signals() {
    assert_two_external_signals_are_both_delivered("ptrace", FIRST_HALF_OF_EXTERNAL_TRIALS);
}

#[test]
fn ptrace_polling_futex_wait_delivers_both_of_two_external_signals_in_later_trials() {
    assert_two_external_signals_are_both_delivered("ptrace", SECOND_HALF_OF_EXTERNAL_TRIALS);
}
