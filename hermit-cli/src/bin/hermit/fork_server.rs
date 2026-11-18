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
//!   request  `RUN stdin=<path> stdout=<path> stderr=<path> [timeout_ms=<n>]
//!            [record=<path>] [tape=<path>] [controller=<path>]
//!            [summary=<path>] [log=<path>]`
//!            or the positional form
//!            `RUN <stdin-path> <stdout-path> <stderr-path> <timeout-ms>`.
//!            `record`, `tape`, `controller` and `summary` replace the path
//!            the server's command line gave `--choice-record`,
//!            `--choice-tape`, `--choice-controller` and `--summary-json`, and
//!            `log` the `--log-file`; each is refused (`ERR ...`) when the
//!            command line named no such path, so a run keeps the
//!            configuration the server validated. So copies running from one
//!            server need not share any path.
//!   reply    `STATUS <code> <elapsed-us>`  (code: exit status, or 128 + signal;
//!            a run past its timeout is killed with its process group and
//!            reported as `TIMEOUT <elapsed-us>`), or `ERR <reason>` for a
//!            request that started no run.
//!   request  `QUIT` ends the server.

use std::ffi::CString;
use std::ffi::OsString;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::RawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
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

/// Which per-RUN keys a server accepts: a key may replace only a path the
/// server's command line already names.
pub struct Replaceable {
    pub record: bool,
    pub tape: bool,
    pub controller: bool,
    pub summary: bool,
    /// The descriptor `--log-file` was opened on, which each run's tracing
    /// duplicates; `log=` puts the run's own file there.
    pub log_fd: Option<RawFd>,
}

/// A served run's own paths, from its RUN request.
#[derive(Debug, Default, PartialEq)]
pub struct RunPaths {
    pub record: Option<PathBuf>,
    pub tape: Option<PathBuf>,
    pub controller: Option<PathBuf>,
    pub summary: Option<PathBuf>,
}

/// Become a fork server if [`capture`] found [`ENV`]. Called where the run is
/// about to start, after its setup. Returns only in a forked child, with that
/// run's own paths, which then continues into the run; the server itself
/// exits. Without [`ENV`] it returns no paths at once.
pub fn serve_here(replaceable: &Replaceable) -> RunPaths {
    let Some(path) = REQUESTED.get() else {
        return RunPaths::default();
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
        if let Served::Child(paths) = serve_connection(stream, replaceable) {
            drop(listener);
            return paths;
        }
    }
    std::process::exit(0);
}

enum Served {
    /// This process is a forked child that must continue as a run.
    Child(RunPaths),
    /// The connection ended; accept the next one.
    Closed,
}

/// One parsed RUN request.
#[derive(Debug, PartialEq)]
struct Request {
    stdin: CString,
    stdout: CString,
    stderr: CString,
    timeout: Duration,
    log: Option<CString>,
    paths: RunPaths,
}

/// Parse the words after `RUN`: four positional words, or `key=value` words.
fn parse_run(words: &[&str], replaceable: &Replaceable) -> Result<Request, String> {
    fn c_path(key: &str, value: &str) -> Result<CString, String> {
        if value.is_empty() {
            return Err(format!("{key}= is empty"));
        }
        CString::new(value).map_err(|_| format!("{key}= contains a NUL byte"))
    }
    fn timeout(value: &str) -> Result<Duration, String> {
        value
            .parse()
            .map(Duration::from_millis)
            .map_err(|_| format!("timeout_ms={value} is not a number of milliseconds"))
    }
    if words.len() == 4 && words.iter().all(|word| !word.contains('=')) {
        return Ok(Request {
            stdin: c_path("stdin", words[0])?,
            stdout: c_path("stdout", words[1])?,
            stderr: c_path("stderr", words[2])?,
            timeout: timeout(words[3])?,
            log: None,
            paths: RunPaths::default(),
        });
    }
    let mut values = std::collections::BTreeMap::new();
    for word in words {
        let (key, value) = word
            .split_once('=')
            .ok_or_else(|| format!("{word} is not key=value"))?;
        if values.insert(key, value).is_some() {
            return Err(format!("{key}= is given twice"));
        }
    }
    let mut take = |key: &str| values.remove(key);
    let required = |key: &str, value: Option<&str>| {
        value
            .ok_or_else(|| format!("{key}= is required"))
            .and_then(|value| c_path(key, value))
    };
    let stdin = required("stdin", take("stdin"))?;
    let stdout = required("stdout", take("stdout"))?;
    let stderr = required("stderr", take("stderr"))?;
    let timeout = take("timeout_ms").map_or(Ok(Duration::ZERO), timeout)?;
    let replace = |key: &str, flag: &str, allowed: bool, value: Option<&str>| match value {
        None => Ok(None),
        Some(_) if !allowed => Err(format!("{key}= needs {flag} on the server's command line")),
        Some(value) => c_path(key, value).map(|_| Some(PathBuf::from(value))),
    };
    let paths = RunPaths {
        record: replace(
            "record",
            "--choice-record",
            replaceable.record,
            take("record"),
        )?,
        tape: replace("tape", "--choice-tape", replaceable.tape, take("tape"))?,
        controller: replace(
            "controller",
            "--choice-controller",
            replaceable.controller,
            take("controller"),
        )?,
        summary: replace(
            "summary",
            "--summary-json",
            replaceable.summary,
            take("summary"),
        )?,
    };
    let log = match take("log") {
        None => None,
        Some(_) if replaceable.log_fd.is_none() => {
            return Err("log= needs --log-file on the server's command line".into());
        }
        Some(value) => Some(c_path("log", value)?),
    };
    if let Some(key) = values.keys().next() {
        return Err(format!("unknown key {key}="));
    }
    Ok(Request {
        stdin,
        stdout,
        stderr,
        timeout,
        log,
        paths,
    })
}

fn serve_connection(stream: UnixStream, replaceable: &Replaceable) -> Served {
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
            ["RUN", rest @ ..] => match parse_run(rest, replaceable) {
                Ok(request) => match fork_run(request, replaceable.log_fd) {
                    Forked::Child(paths) => return Served::Child(paths),
                    Forked::Parent(reply) => reply,
                },
                Err(reason) => format!("ERR {reason}"),
            },
            ["QUIT"] => std::process::exit(0),
            _ => format!("ERR bad request: {}", line.trim()),
        };
        if writeln!(writer, "{reply}").is_err() {
            return Served::Closed;
        }
    }
}

enum Forked {
    Child(RunPaths),
    Parent(String),
}

fn fork_run(request: Request, log_fd: Option<RawFd>) -> Forked {
    let Request {
        stdin,
        stdout,
        stderr,
        timeout,
        log,
        paths,
    } = request;
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
            if let (Some(log), Some(log_fd)) = (&log, log_fd) {
                // Close-on-exec like the --log-file descriptor it replaces,
                // so the guest never inherits it.
                let fd = libc::open(log.as_ptr(), flags | libc::O_CLOEXEC, 0o644);
                if fd < 0 || libc::dup3(fd, log_fd, libc::O_CLOEXEC) < 0 {
                    libc::_exit(126);
                }
                libc::close(fd);
            }
        }
        super::replace_startup_stdin();
        super::container::reopen_parent_pidfd_after_fork();
        detcore::util::reset_shared_stderr_deadline_origin();
        return Forked::Child(paths);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Replaceable {
        Replaceable {
            record: true,
            tape: true,
            controller: true,
            summary: true,
            log_fd: Some(9),
        }
    }

    fn none() -> Replaceable {
        Replaceable {
            record: false,
            tape: false,
            controller: false,
            summary: false,
            log_fd: None,
        }
    }

    #[test]
    fn the_positional_form_still_parses() {
        let request = parse_run(&["in", "out", "err", "250"], &none()).unwrap();
        assert_eq!(request.stdin.as_bytes(), b"in");
        assert_eq!(request.stderr.as_bytes(), b"err");
        assert_eq!(request.timeout, Duration::from_millis(250));
        assert_eq!(request.paths, RunPaths::default());
        assert_eq!(request.log, None);
    }

    #[test]
    fn key_value_words_name_every_per_run_path() {
        let words = [
            "stdin=i",
            "stdout=o",
            "stderr=e",
            "timeout_ms=5",
            "record=r.jsonl",
            "tape=t.bin",
            "summary=s.json",
            "log=l.txt",
        ];
        let request = parse_run(&words, &all()).unwrap();
        assert_eq!(request.timeout, Duration::from_millis(5));
        assert_eq!(request.paths.record, Some(PathBuf::from("r.jsonl")));
        assert_eq!(request.paths.tape, Some(PathBuf::from("t.bin")));
        assert_eq!(request.paths.summary, Some(PathBuf::from("s.json")));
        assert_eq!(request.paths.controller, None);
        assert_eq!(request.log.unwrap().as_bytes(), b"l.txt");
    }

    #[test]
    fn a_key_the_command_line_did_not_name_is_refused() {
        let words = ["stdin=i", "stdout=o", "stderr=e", "record=r"];
        assert_eq!(
            parse_run(&words, &none()).unwrap_err(),
            "record= needs --choice-record on the server's command line"
        );
        let words = ["stdin=i", "stdout=o", "stderr=e", "log=l"];
        assert_eq!(
            parse_run(&words, &none()).unwrap_err(),
            "log= needs --log-file on the server's command line"
        );
    }

    #[test]
    fn malformed_requests_are_refused_by_name() {
        let refuse = |words: &[&str]| parse_run(words, &all()).unwrap_err();
        assert_eq!(refuse(&["stdout=o", "stderr=e"]), "stdin= is required");
        assert_eq!(
            refuse(&["stdin=i", "stdin=j", "stdout=o", "stderr=e"]),
            "stdin= is given twice"
        );
        assert_eq!(
            refuse(&["stdin=i", "stdout=o", "stderr=e", "color=red"]),
            "unknown key color="
        );
        assert_eq!(
            refuse(&["stdin=i", "stdout=o", "stderr=e", "timeout_ms=soon"]),
            "timeout_ms=soon is not a number of milliseconds"
        );
        assert_eq!(
            refuse(&["stdin=", "stdout=o", "stderr=e"]),
            "stdin= is empty"
        );
    }
}
