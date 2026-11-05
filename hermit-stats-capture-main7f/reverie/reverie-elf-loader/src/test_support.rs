/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Test-only bounded children and atomic fixture directories. The crate's
//! unit tests include this as `crate::test_support`; the integration harness
//! includes the same file, so both use one timeout and cleanup implementation.

use std::fs::File;
use std::fs::{self};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub const CLEANUP_TIMEOUT: Duration = Duration::from_millis(250);

/// Atomic directory creation supplies invocation identity even when separate
/// test binaries or PID namespaces reuse the same PID and sequence numbers.
pub fn fixture_dir_in(parent: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(parent).unwrap();
    loop {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{name}-{}-{sequence}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("allocate fixture directory {path:?}: {error}"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum CleanupOutcome {
    Reaped,
    Deadline,
    Error(String),
}

fn poll_until_deadline<T>(
    timeout: Duration,
    mut try_reap: impl FnMut() -> io::Result<Option<T>>,
) -> io::Result<Option<T>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = try_reap()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(
            Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Cleanup never waits synchronously for a killed task to leave an
/// uninterruptible syscall. Even the asynchronous reaper stops at its own
/// deadline, releasing its owned handles before publishing the result.
/// `try_reap` must be a nonblocking observation such as try_wait/WNOHANG.
pub fn bounded_async_cleanup(
    context: String,
    timeout: Duration,
    try_reap: impl FnMut() -> io::Result<bool> + Send + 'static,
) -> mpsc::Receiver<CleanupOutcome> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut try_reap = try_reap;
        let outcome =
            match poll_until_deadline(timeout, || try_reap().map(|reaped| reaped.then_some(()))) {
                Ok(Some(())) => CleanupOutcome::Reaped,
                Ok(None) => CleanupOutcome::Deadline,
                Err(error) => CleanupOutcome::Error(error.to_string()),
            };
        // A Child's drop closes owned pipes/handles without a blocking wait.
        drop(try_reap);
        if outcome != CleanupOutcome::Reaped {
            eprintln!("LB bounded asynchronous cleanup: {context}: {outcome:?}");
        }
        let _ = sender.send(outcome);
    });
    receiver
}

/// This test boundary also permits simulating a child which remains stuck
/// after SIGKILL. Every observation and termination operation is nonblocking;
/// there is deliberately no blocking wait/reap operation in the interface.
pub trait MonitoredProcess: Send + 'static {
    fn id(&self) -> u32;
    fn try_reap(&mut self) -> io::Result<Option<std::process::ExitStatus>>;
    fn terminate(&mut self) -> io::Result<()>;
}

pub fn monitor_process(
    mut process: impl MonitoredProcess,
    context: &str,
    timeout: Duration,
) -> std::process::ExitStatus {
    match poll_until_deadline(timeout, || process.try_reap()).unwrap() {
        Some(status) => status,
        None => {
            let pid = process.id();
            let kill = process.terminate();
            let _cleanup = bounded_async_cleanup(
                format!("{context}, pid={pid}"),
                CLEANUP_TIMEOUT,
                move || process.try_reap().map(|status| status.is_some()),
            );
            panic!(
                "LB child exceeded {timeout:?}: {context}, pid={pid}, kill={kill:?}; asynchronous cleanup bounded to {CLEANUP_TIMEOUT:?}"
            );
        }
    }
}

struct CommandChild(std::process::Child);

impl MonitoredProcess for CommandChild {
    fn id(&self) -> u32 {
        self.0.id()
    }

    fn try_reap(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.0.try_wait()
    }

    fn terminate(&mut self) -> io::Result<()> {
        // SAFETY: run_monitored_with_timeout established a new process group
        // containing only this fixture and its descendants.
        unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
        self.0.kill()
    }
}

pub fn run_monitored_with_timeout(
    mut command: Command,
    context: &str,
    timeout: Duration,
) -> std::process::ExitStatus {
    command.process_group(0);
    monitor_process(CommandChild(command.spawn().unwrap()), context, timeout)
}

/// File capture avoids pipe reads after a successful child exits while a
/// descendant still holds stdout/stderr open. Every invocation owns its files.
pub fn run_monitored_output(
    mut command: Command,
    context: &str,
    timeout: Duration,
    directory: &Path,
) -> Output {
    let output = fixture_dir_in(directory, "native-output");
    let stdout = output.join("stdout");
    let stderr = output.join("stderr");
    command
        .stdout(Stdio::from(File::create(&stdout).unwrap()))
        .stderr(Stdio::from(File::create(&stderr).unwrap()));
    let status = run_monitored_with_timeout(command, context, timeout);
    Output {
        status,
        stdout: fs::read(stdout).unwrap(),
        stderr: fs::read(stderr).unwrap(),
    }
}
