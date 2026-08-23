/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression coverage for blocking `waitid` progress and Linux's
//! child-ready-before-interrupt precedence.
//!
//! The liveness defect is an unbounded polling loop. The watchdog therefore
//! lives in this host process, outside both the guest and every blocking pipe
//! read. A dedicated reader drains Hermit's stderr while this thread polls the
//! process and an independent wall-clock deadline.

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

const MAX_RETRIES: usize = 20_000;
const BACKSTOP: Duration = Duration::from_secs(60);
const MAX_DIAGNOSTIC_LINES: usize = 2_000;

static WAITID_GUEST: OnceLock<PathBuf> = OnceLock::new();

struct GuestRun {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    retries: usize,
}

enum StderrEvent {
    Line(String),
    Error(String),
    Eof,
}

fn waitid_guest() -> &'static Path {
    WAITID_GUEST.get_or_init(|| {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("hermit-cli should be inside the repository");
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("waitid-signal-interrupt");
        fs::create_dir_all(&build_root).expect("failed to create waitid guest build directory");

        // The guest must not live under /tmp: Hermit replaces guest /tmp with
        // an isolated directory, making a host /tmp fixture invisible.
        let guest = build_root.join("waitid_signal_interrupt");
        let source = repository.join("tests/c/waitid_signal_interrupt.c");
        let compile = Command::new("cc")
            .args(["-O2", "-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&guest)
            .output()
            .unwrap_or_else(|error| panic!("failed to compile the waitid guest: {error}"));
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
    // The group contains Hermit and every guest process it started. Killing
    // only the CLI can strand the deliberately nonterminating fixture child.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
}

fn run_bounded(args: &[&str], trace_retries: bool, verify_report: Option<&Path>) -> GuestRun {
    let build_root = waitid_guest()
        .parent()
        .expect("waitid guest should have a parent");
    let stdout_file = tempfile::Builder::new()
        .prefix("waitid-guest-")
        .tempfile_in(build_root)
        .expect("failed to create guest stdout file");
    let stdout_path = stdout_file.path().to_path_buf();
    let stdout_writer = stdout_file
        .reopen()
        .expect("failed to reopen guest stdout file");

    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    if trace_retries {
        command.env("RUST_LOG", "detcore::syscalls::threads=trace");
    } else {
        command.arg("--log=info");
    }
    command.args(["run", "--backend=ptrace", "--strict"]);
    if let Some(report) = verify_report {
        command
            .args(["--verify", "--verify-strict"])
            .arg("--verify-json")
            .arg(report);
    }
    command
        .arg("--")
        .arg(waitid_guest())
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
    let mut retries = 0usize;
    let mut stderr_lines = Vec::new();
    let mut stderr_eof = false;
    let mut status = None;
    let mut failure = None;

    while status.is_none() || !stderr_eof {
        while let Ok(event) = receive.try_recv() {
            match event {
                StderrEvent::Line(line) => {
                    if line.contains("Retry #") && line.contains("waitid") {
                        retries += 1;
                    }
                    if stderr_lines.len() < MAX_DIAGNOSTIC_LINES {
                        stderr_lines.push(line);
                    }
                }
                StderrEvent::Error(error) => {
                    failure = Some(format!("failed while draining hermit stderr: {error}"));
                    stderr_eof = true;
                }
                StderrEvent::Eof => stderr_eof = true,
            }
        }

        if retries > MAX_RETRIES {
            failure = Some(format!(
                "waitid retried more than {MAX_RETRIES} times without returning; a blocking \
                 waiter was left runnable instead of allowing logical time to reach its signal"
            ));
        }
        if started.elapsed() >= BACKSTOP {
            failure = Some(format!(
                "BACKSTOP: hermit did not finish within {}s after {retries} waitid retries; \
                 this deadline is enforced by the host test thread, independently of stderr \
                 activity",
                BACKSTOP.as_secs(),
            ));
        }
        if failure.is_some() {
            kill_process_group(&mut child);
            break;
        }
        if status.is_none() {
            status = child.try_wait().expect("failed to poll hermit");
        }
        if status.is_some() && stderr_eof {
            break;
        }
        match receive.recv_timeout(Duration::from_millis(10)) {
            Ok(StderrEvent::Line(line)) => {
                if line.contains("Retry #") && line.contains("waitid") {
                    retries += 1;
                }
                if stderr_lines.len() < MAX_DIAGNOSTIC_LINES {
                    stderr_lines.push(line);
                }
            }
            Ok(StderrEvent::Error(error)) => {
                failure = Some(format!("failed while draining hermit stderr: {error}"));
            }
            Ok(StderrEvent::Eof) => stderr_eof = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => stderr_eof = true,
        }
    }

    // Always reap the process here. Keeping this unconditional makes the
    // lifecycle obvious on timeout, normal exit, and every diagnostic path.
    let waited_status = child.wait().expect("failed to wait for hermit");
    status.get_or_insert(waited_status);
    reader.join().expect("stderr reader panicked");
    let stdout = fs::read_to_string(&stdout_path).expect("failed to read guest stdout");
    let stderr = stderr_lines.join("\n");
    if let Some(reason) = failure {
        panic!("{reason}\nguest stdout:\n{stdout}\nhermit stderr:\n{stderr}");
    }

    GuestRun {
        status: status.expect("hermit status should be collected"),
        stdout,
        stderr,
        retries,
    }
}

#[test]
fn waitid_interrupted_by_a_signal_returns_instead_of_spinning() {
    let run = run_bounded(&[], true, None);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-signal-interrupt rc=-1 errno=4 handler=1"),
        "waitid did not report EINTR with its handler run\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.stdout.contains("waitid-signal-interrupt-done"),
        "the guest did not reach its child cleanup\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= MAX_RETRIES,
        "waitid returned correctly but performed {} retries",
        run.retries,
    );
}

#[test]
fn waitid_honors_sa_restart_after_running_the_handler() {
    let run = run_bounded(&["--signal-restart"], true, None);
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-signal-restart rc=0 errno=0 handler=1 pid-match=1 code=1 status=17"),
        "waitid did not restart after the SA_RESTART handler\nguest stdout:\n{}",
        run.stdout,
    );
    assert!(
        run.retries <= MAX_RETRIES,
        "restarted waitid performed {} retries",
        run.retries,
    );
}

#[test]
fn waitid_ready_child_path_is_ptrace_l2() {
    let report = tempfile::NamedTempFile::new().expect("failed to create verify report");
    let run = run_bounded(&["--child-ready-wins"], false, Some(report.path()));
    assert!(
        run.status.success(),
        "hermit exited with {}\nguest stdout:\n{}\nhermit stderr:\n{}",
        run.status,
        run.stdout,
        run.stderr,
    );
    assert!(
        run.stdout
            .contains("waitid-ready-wins rc=0 errno=0 handler=1 pid-match=1 code=1 status=23"),
        "waitid did not report the ready child before delivering simultaneous SIGCHLD\n\
         guest stdout:\n{}",
        run.stdout,
    );

    let report: serde_json::Value = serde_json::from_slice(
        &fs::read(report.path()).expect("strict verification did not publish its report"),
    )
    .expect("strict verification report was not valid JSON");
    assert_eq!(report["verdict"], "matched", "verify report: {report}");
    assert_eq!(report["verified"], true, "verify report: {report}");
    assert_eq!(report["bitwise_parity"], true, "verify report: {report}");
    assert_eq!(
        report["comparison"]["strictness"], "canonical",
        "verify report: {report}"
    );
    assert_eq!(
        report["comparison"]["log_scope"], "info",
        "verify report: {report}"
    );
    assert!(
        report["compared_log_messages"]["left"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "verify report compared no INFO evidence: {report}"
    );
}
