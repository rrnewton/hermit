/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! PROTOTYPE (fuzzpoc lane, M2): a fork server inside Hermit, so that one
//! launch serves many runs.
//!
//! With `HERMIT_FORK_SERVER=<socket path>` in its environment, `hermit run`
//! does its whole setup once (argument handling and validation, CPU-profile
//! admission, PMU configuration and validation, program and output checks),
//! then, where it would start the run's container, listens on that Unix socket
//! instead. For each request it forks; the child takes the request's stdin,
//! stdout and stderr files, renews the per-process handles a run owns (the
//! pidfd its container inits watch, the stderr deadline origin), and continues
//! into the run, so each run pays only for its container and guest. The parent
//! waits for the child and reports its status. The variable is removed at
//! entry, before anything else reads the environment ([`capture`]), so a run
//! sees the environment a plain invocation sees. Per-run inputs are files the
//! command line already names (a `--choice-tape`, the stdin file), rewritten by
//! the client between requests; everything else is fixed when the server
//! starts.
//!
//! Only an ordinary ptrace run is served; `hermit run` refuses the other modes
//! and any per-invocation output that cannot describe many runs (see
//! [`refuse_unserved`]).
//!
//! The server is launched once under `bin/safehermit`, whose deadline and
//! byte cap bound the whole process tree; an execution then costs a fork
//! instead of a launch.
//!
//! Protocol, one line each way per run:
//!   request  `RUN <stdin-path> <stdout-path> <stderr-path> <timeout-ms>`
//!   reply    `STATUS <code> <elapsed-us>`  (code: exit status, or 128 + signal;
//!            a run past its timeout is killed with its process group and
//!            reported as `TIMEOUT <elapsed-us>`)
//!   request  `QUIT` ends the server.

use std::ffi::CString;
use std::ffi::OsString;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// The environment variable that turns `hermit` into a fork server.
pub const ENV: &str = "HERMIT_FORK_SERVER";

/// The socket path [`ENV`] named at entry, if it was set.
static REQUESTED: OnceLock<OsString> = OnceLock::new();

/// Take [`ENV`] out of the environment at entry, while the process is still
/// single-threaded, and remember its socket path for [`serve_here`].
pub fn capture() {
    let Some(path) = std::env::var_os(ENV) else {
        return;
    };
    let threads = thread_count();
    if threads != 1 {
        eprintln!(
            "hermit fork server: refusing, {threads} threads exist at entry (fork needs one)"
        );
        std::process::exit(125);
    }
    // SAFETY: the process is single-threaded (checked above), so nothing
    // reads the environment concurrently.
    unsafe { std::env::remove_var(ENV) };
    let _ = REQUESTED.set(path);
}

/// Whether this invocation is to serve runs.
pub fn requested() -> bool {
    REQUESTED.get().is_some()
}

/// Refuse a served invocation that names any of `unserved`: a mode that never
/// reaches [`serve_here`], or an output made once per invocation, which cannot
/// describe many runs.
pub fn refuse_unserved(unserved: &[(&str, bool)]) -> anyhow::Result<()> {
    let named: Vec<&str> = unserved
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| *name)
        .collect();
    anyhow::ensure!(
        named.is_empty(),
        "{ENV} serves only an ordinary ptrace run; this invocation has {}",
        named.join(", ")
    );
    Ok(())
}

/// Become a fork server if [`capture`] found [`ENV`]. Called where the run is
/// about to start, after its setup. Returns only in a forked child, which then
/// continues into the run; the server itself exits.
pub fn serve_here() {
    let Some(path) = REQUESTED.get() else {
        return;
    };
    let threads = thread_count();
    if threads != 1 {
        eprintln!(
            "hermit fork server: refusing, {threads} threads exist after setup (fork needs one)"
        );
        std::process::exit(125);
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap_or_else(|error| {
        eprintln!(
            "hermit fork server: cannot listen on {}: {error}",
            std::path::Path::new(path).display()
        );
        std::process::exit(125);
    });
    eprintln!(
        "hermit fork server: listening on {}",
        std::path::Path::new(path).display()
    );
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Served::Child = serve_connection(stream) {
            drop(listener);
            return;
        }
    }
    std::process::exit(0);
}

enum Served {
    /// This process is a forked child that must continue as a run.
    Child,
    /// The connection ended; accept the next one.
    Closed,
}

fn serve_connection(stream: UnixStream) -> Served {
    let Ok(read_half) = stream.try_clone() else {
        return Served::Closed;
    };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Served::Closed,
            Ok(_) => {}
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let reply = match words.as_slice() {
            ["RUN", stdin, stdout, stderr, timeout_ms] => {
                let timeout = Duration::from_millis(timeout_ms.parse().unwrap_or(0));
                match fork_run(stdin, stdout, stderr, timeout) {
                    Forked::Child => return Served::Child,
                    Forked::Parent(reply) => reply,
                }
            }
            ["QUIT"] => std::process::exit(0),
            _ => format!("ERR bad request: {}", line.trim()),
        };
        if writeln!(writer, "{reply}").is_err() {
            return Served::Closed;
        }
    }
}

enum Forked {
    Child,
    Parent(String),
}

fn fork_run(stdin: &str, stdout: &str, stderr: &str, timeout: Duration) -> Forked {
    let (Ok(stdin), Ok(stdout), Ok(stderr)) = (
        CString::new(stdin),
        CString::new(stdout),
        CString::new(stderr),
    ) else {
        return Forked::Parent("ERR path contains a NUL byte".into());
    };
    let start = Instant::now();
    // SAFETY: the server is single-threaded, so the child may run arbitrary
    // code after fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Forked::Parent(format!("ERR fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: plain descriptor calls on paths owned by this frame.
        unsafe {
            libc::setpgid(0, 0);
            redirect(&stdin, libc::O_RDONLY, libc::STDIN_FILENO);
            let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
            redirect(&stdout, flags, libc::STDOUT_FILENO);
            redirect(&stderr, flags, libc::STDERR_FILENO);
        }
        super::replace_startup_stdin();
        super::container::reopen_parent_pidfd_after_fork();
        detcore::util::reset_shared_stderr_deadline_origin();
        return Forked::Child;
    }
    // Wait on a pidfd, not by polling waitpid: on a loaded host each wait4
    // takes the kernel's task-list lock, and a polling parent competes with
    // its own child for CPU (measured: a quarter of the server's samples).
    // SAFETY: pidfd_open on this process's own child.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    let mut killed = false;
    if pidfd >= 0 {
        loop {
            let left = if timeout.is_zero() {
                -1
            } else {
                timeout
                    .saturating_sub(start.elapsed())
                    .as_millis()
                    .min(i32::MAX as u128) as i32
            };
            let mut ready = libc::pollfd {
                fd: pidfd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            let n = unsafe { libc::poll(&mut ready, 1, left) };
            if n > 0 {
                break;
            }
            if n == 0 && !killed {
                // SAFETY: signals the child's own process group, made above.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                killed = true;
            }
        }
        // SAFETY: closes the descriptor opened above.
        unsafe { libc::close(pidfd) };
    }
    let mut status = 0;
    // SAFETY: `pid` is this process's child; `status` is an out-parameter.
    while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return Forked::Parent(format!("ERR waitpid: {}", std::io::Error::last_os_error()));
        }
    }
    let elapsed = start.elapsed().as_micros();
    if killed {
        return Forked::Parent(format!("TIMEOUT {elapsed}"));
    }
    let code = if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    };
    Forked::Parent(format!("STATUS {code} {elapsed}"))
}

/// Open `path` with `flags` onto descriptor `target`, or exit the child.
unsafe fn redirect(path: &CString, flags: libc::c_int, target: libc::c_int) {
    // SAFETY: `path` is NUL-terminated; descriptors are this process's.
    unsafe {
        let fd = libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, 0o644);
        if fd < 0 || libc::dup2(fd, target) < 0 {
            libc::_exit(126);
        }
        libc::close(fd);
    }
}

fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |tasks| tasks.count())
}
