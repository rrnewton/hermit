// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the LICENSE file.

//! Local process-group, wall, and complete-stream capture. Aggregate resource
//! admission belongs to the enclosing runner and is not inferred here.

use std::fs;
use std::io;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub wall: Duration,
    pub stdout: u64,
    pub stderr: u64,
}

#[derive(Debug)]
pub struct Receipt {
    pub status: Option<ExitStatus>,
    pub stdout_read: u64,
    pub stdout_written: u64,
    pub stderr_read: u64,
    pub stderr_written: u64,
    pub stdout_eof: bool,
    pub stderr_eof: bool,
    pub reaped: bool,
    pub cleanup_complete: bool,
    pub elapsed: Duration,
    pub failure: Option<String>,
}

impl Receipt {
    pub fn complete(&self) -> bool {
        self.failure.is_none()
            && self.status.is_some()
            && self.stdout_eof
            && self.stderr_eof
            && self.reaped
            && self.cleanup_complete
            && self.stdout_read == self.stdout_written
            && self.stderr_read == self.stderr_written
    }
}

struct ProcessGroup {
    child: Child,
    reaped: bool,
}

impl ProcessGroup {
    fn terminate(&mut self) -> (Option<ExitStatus>, Result<(), String>) {
        if self.reaped {
            // Reaping releases the numeric PID/process-group identity. Do not
            // signal it after ownership has been released.
            return (None, Ok(()));
        }
        // The child was started in its own fresh group. Its unreaped PID pins
        // that group identity while cleanup is attempted.
        let kill_error = if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } < 0 {
            let error = io::Error::last_os_error();
            (error.raw_os_error() != Some(libc::ESRCH))
                .then(|| format!("kill owned group: {error}"))
        } else {
            None
        };
        match self.child.wait() {
            Ok(status) => {
                self.reaped = true;
                (Some(status), kill_error.map_or(Ok(()), Err))
            }
            Err(error) => (
                None,
                Err(format!(
                    "{}wait owned child: {error}",
                    kill_error.map_or_else(String::new, |error| format!("{error}; "))
                )),
            ),
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if !self.reaped {
            // Keep the former KVM helper's cleanup on unwinding. Ordinary
            // failures explicitly record the result before reaching Drop.
            let _ = self.terminate();
        }
    }
}

struct CapturedStream<R> {
    pipe: R,
    output: fs::File,
    name: &'static str,
    limit: u64,
    read: u64,
    written: u64,
    eof: bool,
}

impl<R: Read + AsRawFd> CapturedStream<R> {
    fn new(pipe: R, directory: &Path, name: &'static str, limit: u64) -> Result<Self, String> {
        // This flag applies only to the parent's owned read end. It does not
        // alter the child's separate write end or guest-visible stdio state.
        let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(format!(
                "read {name} capture flags: {}",
                io::Error::last_os_error()
            ));
        }
        if unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(format!(
                "set {name} capture flags: {}",
                io::Error::last_os_error()
            ));
        }
        let output = fs::File::create(directory.join(name))
            .map_err(|error| format!("create {name} capture: {error}"))?;
        Ok(Self {
            pipe,
            output,
            name,
            limit,
            read: 0,
            written: 0,
            eof: false,
        })
    }

    fn drain(&mut self) -> Result<(), String> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0; 8192];
        // Preserve bounded work per poll so continuously writing output cannot
        // starve the other stream, retained-log callback, or wall check.
        for _ in 0..8 {
            let count = match self.pipe.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(format!("read {}: {error}", self.name)),
            };
            self.read = self
                .read
                .checked_add(count as u64)
                .ok_or_else(|| format!("{} byte count overflow", self.name))?;
            let keep = count.min((self.limit - self.written) as usize);
            self.output
                .write_all(&buffer[..keep])
                .map_err(|error| format!("retain {}: {error}", self.name))?;
            self.written += keep as u64;
            if keep != count {
                return Err(format!(
                    "{} exceeded its {}-byte output bound",
                    self.name, self.limit
                ));
            }
        }
        Ok(())
    }

    fn check_complete(&self) -> Result<(), String> {
        let actual = self
            .output
            .metadata()
            .map_err(|error| format!("{} retained metadata: {error}", self.name))?
            .len();
        if !self.eof || self.read != self.written || actual != self.written {
            return Err(format!(
                "incomplete {}: eof={} read={} retained={} actual={actual}",
                self.name, self.eof, self.read, self.written,
            ));
        }
        Ok(())
    }
}

/// Capture one explicitly supplied command. This does not guess an executor,
/// inspect environment markers, or provide aggregate CPU/memory/task limits.
pub fn run(
    command: &mut Command,
    directory: &Path,
    limits: Limits,
    mut poll: impl FnMut(),
) -> Receipt {
    let start = Instant::now();
    let mut receipt = Receipt {
        status: None,
        stdout_read: 0,
        stdout_written: 0,
        stderr_read: 0,
        stderr_written: 0,
        stdout_eof: false,
        stderr_eof: false,
        reaped: false,
        cleanup_complete: false,
        elapsed: Duration::ZERO,
        failure: None,
    };
    if limits.wall.is_zero() || limits.stdout == 0 || limits.stderr == 0 {
        receipt.failure = Some("wall and stream bounds must be positive".to_owned());
        return receipt;
    }
    if let Err(error) = fs::create_dir_all(directory) {
        receipt.failure = Some(format!("create retained command directory: {error}"));
        return receipt;
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // The limits apply to streams, not RLIMIT_FSIZE. KVM requires a large memfd.
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            receipt.failure = Some(format!("start bounded command: {error}"));
            receipt.elapsed = start.elapsed();
            return receipt;
        }
    };
    let mut group = ProcessGroup {
        child,
        reaped: false,
    };
    let mut stdout = None;
    let mut stderr = None;
    let mut known_status = None;
    let outcome = (|| -> Result<ExitStatus, String> {
        stdout = Some(CapturedStream::new(
            group
                .child
                .stdout
                .take()
                .ok_or("missing owned stdout pipe")?,
            directory,
            "stdout",
            limits.stdout,
        )?);
        stderr = Some(CapturedStream::new(
            group
                .child
                .stderr
                .take()
                .ok_or("missing owned stderr pipe")?,
            directory,
            "stderr",
            limits.stderr,
        )?);
        let stdout = stdout.as_mut().ok_or("missing stdout capture")?;
        let stderr = stderr.as_mut().ok_or("missing stderr capture")?;
        loop {
            stdout.drain()?;
            stderr.drain()?;
            poll();
            // Retain the unreaped identity until both writers close. A child
            // exit with a descendant writer still open does not complete.
            if stdout.eof
                && stderr.eof
                && let Some(status) = group
                    .child
                    .try_wait()
                    .map_err(|error| format!("poll owned child: {error}"))?
            {
                group.reaped = true;
                known_status = Some(status);
                stdout.check_complete()?;
                stderr.check_complete()?;
                if start.elapsed() >= limits.wall {
                    return Err(format!(
                        "{}-second wall limit exceeded",
                        limits.wall.as_secs_f64()
                    ));
                }
                return Ok(status);
            }
            if start.elapsed() >= limits.wall {
                return Err(format!(
                    "{}-second wall limit exceeded",
                    limits.wall.as_secs_f64()
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    if let Some(stdout) = stdout {
        receipt.stdout_read = stdout.read;
        receipt.stdout_written = stdout.written;
        receipt.stdout_eof = stdout.eof;
    }
    if let Some(stderr) = stderr {
        receipt.stderr_read = stderr.read;
        receipt.stderr_written = stderr.written;
        receipt.stderr_eof = stderr.eof;
    }
    match outcome {
        Ok(status) => {
            receipt.status = Some(status);
            receipt.cleanup_complete = true;
        }
        Err(error) => {
            let (status, cleanup) = group.terminate();
            receipt.status = status.or(known_status);
            receipt.cleanup_complete = cleanup.is_ok();
            receipt.failure = Some(match cleanup {
                Ok(()) => error,
                Err(cleanup) => format!("{error}; {cleanup}"),
            });
        }
    }
    receipt.reaped = group.reaped;
    receipt.elapsed = start.elapsed();
    receipt
}
