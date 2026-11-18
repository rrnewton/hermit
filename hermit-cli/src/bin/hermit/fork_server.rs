/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Experimental: a fork server inside `hermit run`, so that one launch serves
//! many runs of the same command line (for a fuzzer or another search tool
//! that runs a guest thousands of times).
//!
//! With `HERMIT_FORK_SERVER=<socket path>` in its environment, `hermit run`
//! does its whole setup once (argument handling and validation, CPU-profile
//! admission, PMU configuration and validation, program and output checks),
//! then, where it would start the run's container, listens on that Unix socket
//! instead. For each request it forks; the child takes the request's stdin,
//! stdout and stderr files, renews the per-process handles a run owns (the
//! pidfd its container inits watch, the stderr deadline origin), and continues
//! into the run, so each run pays only for its container and guest. (The
//! startup stdin reservation needs no renewal: only `--verify`, DBT and KVM
//! take it, at setup, and the server refuses all three.) The server waits for
//! the child and reports its status. The variable is removed at entry, before
//! anything else reads the environment ([`capture`]), so a run sees the
//! environment a plain invocation sees. Per-run inputs are files the command line already names (a
//! `--choice-tape`, the stdin file), rewritten by the client between requests;
//! everything else is fixed when the server starts.
//!
//! Contract:
//! - Promises: a served run is the run the same command line would make as a
//!   plain invocation started at the server's setup, with the request's stdin,
//!   stdout and stderr: same guest output, exit status and Detcore outputs.
//!   Runs are served one at a time: a run is reaped before the next is
//!   forked, and a run past its timeout is killed with its process group
//!   first. A run's container inits watch the run's own process (a pidfd the
//!   child renews), so they end with it.
//! - May assume: the inputs the setup read (the host, the program, the files
//!   the command line names other than per-run inputs) do not change while it
//!   serves, as for one long run.
//! - Refuses: every mode but an ordinary ptrace run, and any per-invocation
//!   output that cannot describe many runs ([`refuse_unserved`]), as a policy
//!   refusal before it listens. Also a socket path that names anything but a
//!   socket nobody listens on ([`claim_socket_path`]): a stale socket is
//!   replaced, but no other file is ever removed. A kernel without
//!   `pidfd_open` (before Linux 5.3), on which it could not bound a run.
//!
//! Protocol, one line each way per run, over a connection to the socket:
//! - `RUN <stdin-path> <stdout-path> <stderr-path> <timeout-ms>
//!   [coverage=<path>]` forks a run. The stdout and stderr files are created
//!   or truncated. A timeout of 0 means none.
//! - Coverage (experimental): with `HERMIT_FORK_SERVER_COVERAGE=<absolute
//!   path>` at the server's start, naming the file the guest's coverage
//!   runtime writes (a path its command line already gives it), that file is
//!   removed before every run, so a run never inherits another's. A RUN with
//!   `coverage=<path>` then has it renamed to `<path>` after the run is
//!   reaped (when its process group and container are gone, so no writer is
//!   left), and its reply ends with ` coverage=<bytes>`, ` coverage=missing`
//!   (the run wrote none), ` coverage=invalid` (not a regular file) or
//!   ` coverage=unarchived` (the rename failed, for example across file
//!   systems; the file is left for the next reset).
//! - The reply is `STATUS <code> <elapsed-us>` (the run's exit status, or 128
//!   plus the signal that ended it), `TIMEOUT <elapsed-us>` for a run killed
//!   with its process group at its timeout, or `ERR <reason>` for a request
//!   that started no run (malformed, or a file that cannot be opened) or whose
//!   run could not be watched and was killed at once.
//! - `QUIT` ends the server.
//!
//! The server is meant to be launched once under a bound such as
//! `bin/safehermit`, whose deadline and byte cap then cover every run.

use std::ffi::CString;
use std::ffi::OsString;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// The environment variable that turns `hermit run` into a fork server.
pub const ENV: &str = "HERMIT_FORK_SERVER";

/// The socket path [`ENV`] named at entry, if it was set.
static REQUESTED: OnceLock<OsString> = OnceLock::new();

/// The environment variable naming the guest's coverage file (experimental).
pub const COVERAGE_ENV: &str = "HERMIT_FORK_SERVER_COVERAGE";

/// The coverage file [`COVERAGE_ENV`] named at entry, if it was set.
static COVERAGE: OnceLock<std::path::PathBuf> = OnceLock::new();

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
    let coverage = std::env::var_os(COVERAGE_ENV);
    // SAFETY: the process is single-threaded (checked above), so nothing
    // reads the environment concurrently.
    unsafe {
        std::env::remove_var(ENV);
        std::env::remove_var(COVERAGE_ENV);
    }
    let _ = REQUESTED.set(path);
    if let Some(coverage) = coverage {
        let _ = COVERAGE.set(coverage.into());
    }
}

/// Whether this invocation is to serve runs.
pub fn requested() -> bool {
    REQUESTED.get().is_some()
}

/// Refuse a served invocation that names any of `unserved`, as a policy
/// refusal: a mode that never reaches [`serve_here`], or an output made once
/// per invocation, which cannot describe many runs.
pub fn refuse_unserved(unserved: &[(&str, bool)]) -> anyhow::Result<()> {
    let named: Vec<&str> = unserved
        .iter()
        .filter(|(_, present)| *present)
        .map(|(name, _)| *name)
        .collect();
    if named.is_empty() {
        return Ok(());
    }
    Err(
        anyhow::Error::new(super::container::PolicyRefusal).context(format!(
            "{ENV} serves only an ordinary ptrace run; this invocation has {}",
            named.join(", ")
        )),
    )
}

/// Become a fork server if [`capture`] found [`ENV`]. Called where the run is
/// about to start, after its setup. Returns `Ok` only in a forked child, which
/// then continues into the run; the server itself exits. Returns a policy
/// refusal for a socket path it will not take ([`claim_socket_path`]).
pub fn serve_here() -> anyhow::Result<()> {
    let Some(path) = REQUESTED.get() else {
        return Ok(());
    };
    let path = Path::new(path);
    let threads = thread_count();
    if threads != 1 {
        eprintln!(
            "hermit fork server: refusing, {threads} threads exist after setup (fork needs one)"
        );
        std::process::exit(125);
    }
    // Each run's timeout is kept by polling a pidfd on it (`fork_run`).
    // SAFETY: `pidfd_open` takes a pid and flags and touches no caller memory.
    let own = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
    if own < 0 {
        eprintln!(
            "hermit fork server: refusing, pidfd_open is unavailable ({}), so runs could not be \
             bounded",
            std::io::Error::last_os_error()
        );
        std::process::exit(125);
    }
    // SAFETY: closes the descriptor just opened.
    unsafe { libc::close(own as libc::c_int) };
    claim_socket_path(path)?;
    if let Some(coverage) = COVERAGE.get() {
        check_coverage_path(coverage)?;
    }
    let listener = UnixListener::bind(path).unwrap_or_else(|error| {
        eprintln!(
            "hermit fork server: cannot listen on {}: {error}",
            path.display()
        );
        std::process::exit(125);
    });
    eprintln!("hermit fork server: listening on {}", path.display());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Served::Child = serve_connection(stream) {
            drop(listener);
            return Ok(());
        }
    }
    std::process::exit(0);
}

/// Make `path` free for the listener. A socket nobody listens on is left by
/// an earlier server and is removed. Anything else is refused, so that a
/// mistyped path never costs a file and a live server keeps its socket.
fn claim_socket_path(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let refuse = |why: String| -> anyhow::Result<()> {
        Err(anyhow::Error::new(super::container::PolicyRefusal)
            .context(format!("{ENV}={}: {why}", path.display())))
    };
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => refuse(format!("cannot inspect it: {error}")),
        Ok(metadata) if !metadata.file_type().is_socket() => {
            refuse("it names an existing file that is not a socket; it is left as it is".into())
        }
        Ok(_) if UnixStream::connect(path).is_ok() => refuse("another server listens on it".into()),
        Ok(_) => std::fs::remove_file(path)
            .or_else(|error| refuse(format!("cannot remove the stale socket: {error}"))),
    }
}

/// Refuse a coverage file the server could not reset safely: a relative path
/// (the guest and the server could resolve it differently), or an existing
/// file that is not a regular file.
fn check_coverage_path(coverage: &Path) -> anyhow::Result<()> {
    let refuse = |why: &str| -> anyhow::Result<()> {
        Err(anyhow::Error::new(super::container::PolicyRefusal)
            .context(format!("{COVERAGE_ENV}={}: {why}", coverage.display())))
    };
    if !coverage.is_absolute() {
        return refuse("name the coverage file by an absolute path");
    }
    match std::fs::symlink_metadata(coverage) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            refuse("it names an existing file that is not a regular file; it is left as it is")
        }
        _ => Ok(()),
    }
}

/// Remove the coverage file before a run, so the run starts with none. Only a
/// regular file is removed; anything else answers the request with `ERR`.
fn reset_coverage(coverage: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(coverage) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot inspect the coverage file {}: {error}",
            coverage.display()
        )),
        Ok(metadata) if !metadata.file_type().is_file() => Err(format!(
            "the coverage file {} is not a regular file; it is left as it is",
            coverage.display()
        )),
        Ok(_) => std::fs::remove_file(coverage).map_err(|error| {
            format!(
                "cannot reset the coverage file {}: {error}",
                coverage.display()
            )
        }),
    }
}

/// Before a run: refuse `coverage=` on a server with no coverage file, and
/// reset the coverage file, if the server has one, for every run.
fn prepare_coverage(request: &Request) -> Result<(), String> {
    match (COVERAGE.get(), &request.coverage) {
        (None, Some(_)) => Err(format!(
            "coverage= needs {COVERAGE_ENV} at the server's start, naming the file the guest's \
             coverage runtime writes"
        )),
        (None, None) => Ok(()),
        (Some(coverage), _) => reset_coverage(coverage),
    }
}

/// The reply value for a reaped run's coverage file, renamed to `dest`: its
/// length in bytes, or why there is none. The run's writers are gone (its
/// process group was reaped and its container ends with it), so the length
/// read here is the file's final length.
fn coverage_field(coverage: &Path, dest: &Path) -> String {
    match std::fs::symlink_metadata(coverage) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "missing".into(),
        Err(_) => "unarchived".into(),
        Ok(metadata) if !metadata.file_type().is_file() => "invalid".into(),
        Ok(metadata) => match std::fs::rename(coverage, dest) {
            Ok(()) => metadata.len().to_string(),
            Err(_) => "unarchived".into(),
        },
    }
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
            ["QUIT"] => std::process::exit(0),
            ["RUN", ..] => match parse_run(&words[1..])
                .and_then(|request| prepare_coverage(&request).map(|()| request))
                .and_then(|request| request.open())
            {
                Ok(mut opened) => {
                    let dest = opened.coverage.take();
                    match fork_run(opened) {
                        Forked::Child => return Served::Child,
                        Forked::Parent(reply) => match (dest, COVERAGE.get()) {
                            (Some(dest), Some(coverage)) if !reply.starts_with("ERR") => {
                                let dest = Path::new(std::ffi::OsStr::from_bytes(dest.as_bytes()));
                                format!("{reply} coverage={}", coverage_field(coverage, dest))
                            }
                            _ => reply,
                        },
                    }
                }
                Err(reason) => format!("ERR {reason}"),
            },
            _ => format!("ERR unknown request: {}", line.trim()),
        };
        if writeln!(writer, "{reply}").is_err() {
            return Served::Closed;
        }
    }
}

/// One `RUN` request.
#[derive(Debug, PartialEq)]
struct Request {
    stdin: CString,
    stdout: CString,
    stderr: CString,
    /// The run's wall-clock bound; zero for none.
    timeout: Duration,
    /// Where the run's coverage file goes after it (`coverage=`).
    coverage: Option<CString>,
}

/// Parse the words after `RUN`: four positional words, then `key=value`
/// fields, of which only `coverage=` exists.
fn parse_run(words: &[&str]) -> Result<Request, String> {
    let positional = words.len().min(4);
    let [stdin, stdout, stderr, timeout_ms] = &words[..positional] else {
        return Err(format!(
            "RUN takes <stdin-path> <stdout-path> <stderr-path> <timeout-ms> \
             [coverage=<path>], got {} words",
            words.len()
        ));
    };
    let mut coverage = None;
    for field in &words[positional..] {
        match field.split_once('=') {
            Some(("coverage", path)) if coverage.is_none() && !path.is_empty() => {
                coverage = Some(path)
            }
            Some(("coverage", _)) => {
                return Err(format!(
                    "RUN takes one coverage=<path> with a path, got {field:?}"
                ));
            }
            _ => return Err(format!("RUN does not take {field:?}")),
        }
    }
    let path = |role: &str, path: &str| {
        CString::new(path).map_err(|_| format!("the {role} path contains a NUL byte"))
    };
    let timeout_ms: u64 = timeout_ms
        .parse()
        .map_err(|_| format!("the timeout {timeout_ms:?} is not a whole number of milliseconds"))?;
    Ok(Request {
        stdin: path("stdin", stdin)?,
        stdout: path("stdout", stdout)?,
        stderr: path("stderr", stderr)?,
        timeout: Duration::from_millis(timeout_ms),
        coverage: coverage.map(|dest| path("coverage", dest)).transpose()?,
    })
}

/// A request whose files are open, so that a file that cannot be opened is
/// an `ERR` reply rather than a run that fails in a way a guest could too.
struct Opened {
    stdin: OwnedFd,
    stdout: OwnedFd,
    stderr: OwnedFd,
    timeout: Duration,
    coverage: Option<CString>,
}

impl Request {
    fn open(self) -> Result<Opened, String> {
        let write = libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
        Ok(Opened {
            stdin: open("stdin", &self.stdin, libc::O_RDONLY)?,
            stdout: open("stdout", &self.stdout, write)?,
            stderr: open("stderr", &self.stderr, write)?,
            timeout: self.timeout,
            coverage: self.coverage,
        })
    }
}

/// Open `path` with `flags` (close-on-exec), naming `role` in the error.
fn open(role: &str, path: &CString, flags: libc::c_int) -> Result<OwnedFd, String> {
    use std::os::fd::FromRawFd;
    // SAFETY: `path` is NUL-terminated.
    let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC, 0o644) };
    if fd < 0 {
        return Err(format!(
            "cannot open the {role} file {}: {}",
            path.to_string_lossy(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `fd` was just opened and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

enum Forked {
    Child,
    Parent(String),
}

fn fork_run(opened: Opened) -> Forked {
    let start = Instant::now();
    // SAFETY: the server is single-threaded, so the child may run arbitrary
    // code after fork.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Forked::Parent(format!("ERR fork: {}", std::io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: plain descriptor calls on descriptors this frame owns. dup2
        // onto 0-2 clears close-on-exec on the copies, as an ordinary
        // invocation's standard streams have it clear.
        unsafe {
            libc::setpgid(0, 0);
            for (fd, target) in [
                (&opened.stdin, libc::STDIN_FILENO),
                (&opened.stdout, libc::STDOUT_FILENO),
                (&opened.stderr, libc::STDERR_FILENO),
            ] {
                if libc::dup2(fd.as_raw_fd(), target) < 0 {
                    libc::_exit(125);
                }
            }
        }
        drop(opened);
        super::container::reopen_parent_pidfd_after_fork();
        detcore::util::reset_shared_stderr_deadline_origin();
        return Forked::Child;
    }
    // The child makes its own process group too; doing it here as well means
    // the group exists before any kill below, whichever of the two runs first.
    // SAFETY: `pid` is this process's child, which has not exec'd.
    unsafe { libc::setpgid(pid, pid) };
    let timeout = opened.timeout;
    drop(opened);
    // Wait on a pidfd, not by polling waitpid: on a loaded host each wait4
    // takes the kernel's task-list lock, and a polling parent competes with
    // its own child for CPU (measured: a quarter of the server's samples).
    // SAFETY: pidfd_open on this process's own child.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
    if pidfd < 0 {
        // No run goes unbounded: one that cannot be watched is ended at once.
        let error = std::io::Error::last_os_error();
        kill_run(pid);
        let _ = reap(pid);
        return Forked::Parent(format!(
            "ERR cannot watch the run, so it was killed: pidfd_open: {error}"
        ));
    }
    let mut killed = false;
    let mut failed = None;
    loop {
        let left = if timeout.is_zero() || killed {
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
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            kill_run(pid);
            failed = Some(error);
            break;
        }
        if !killed {
            kill_run(pid);
            killed = true;
        }
    }
    // SAFETY: closes the descriptor opened above.
    unsafe { libc::close(pidfd) };
    let status = match reap(pid) {
        Ok(status) => status,
        Err(error) => return Forked::Parent(format!("ERR waitpid: {error}")),
    };
    if let Some(error) = failed {
        return Forked::Parent(format!(
            "ERR cannot watch the run, so it was killed: poll: {error}"
        ));
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

/// Kill a run with its process group, made by `fork_run` before any wait.
fn kill_run(pid: libc::pid_t) {
    // SAFETY: signals the child's own process group.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
}

/// Wait for the child `pid` to end and return its wait status.
fn reap(pid: libc::pid_t) -> std::io::Result<libc::c_int> {
    let mut status = 0;
    loop {
        // SAFETY: `pid` is this process's child; `status` is an out-parameter.
        if unsafe { libc::waitpid(pid, &mut status, 0) } >= 0 {
            return Ok(status);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

fn thread_count() -> usize {
    std::fs::read_dir("/proc/self/task").map_or(0, |tasks| tasks.count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_request_names_three_files_and_a_timeout() {
        let request = parse_run(&["in", "out", "err", "2500"]).unwrap();
        assert_eq!(
            request,
            Request {
                stdin: CString::new("in").unwrap(),
                stdout: CString::new("out").unwrap(),
                stderr: CString::new("err").unwrap(),
                timeout: Duration::from_millis(2500),
                coverage: None,
            }
        );
        assert_eq!(
            parse_run(&["in", "out", "err", "0", "coverage=/c/1"])
                .unwrap()
                .coverage,
            Some(CString::new("/c/1").unwrap())
        );
        assert!(
            parse_run(&["in", "out", "err", "0"])
                .unwrap()
                .timeout
                .is_zero()
        );
    }

    #[test]
    fn a_malformed_run_request_is_refused_by_name() {
        let error = parse_run(&["in", "out", "err"]).unwrap_err();
        assert!(error.contains("got 3 words"), "{error}");
        let error = parse_run(&["in", "out", "err", "10", "extra"]).unwrap_err();
        assert!(error.contains("RUN does not take \"extra\""), "{error}");
        let error = parse_run(&["in", "out", "err", "10", "tape=t"]).unwrap_err();
        assert!(error.contains("RUN does not take \"tape=t\""), "{error}");
        for fields in [&["coverage=a", "coverage=b"][..], &["coverage="][..]] {
            let mut words = vec!["in", "out", "err", "10"];
            words.extend_from_slice(fields);
            let error = parse_run(&words).unwrap_err();
            assert!(error.contains("one coverage=<path>"), "{fields:?}: {error}");
        }
        for timeout in ["-1", "1.5", "soon", ""] {
            let error = parse_run(&["in", "out", "err", timeout]).unwrap_err();
            assert!(error.contains("not a whole number"), "{timeout:?}: {error}");
        }
    }

    #[test]
    fn a_file_that_cannot_be_opened_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing").join("in");
        let request = parse_run(&[missing.to_str().unwrap(), "/dev/null", "/dev/null", "0"]);
        let error = request.unwrap().open().err().unwrap();
        assert!(error.starts_with("cannot open the stdin file"), "{error}");
        let out = dir.path().join("out");
        let opened = parse_run(&["/dev/null", out.to_str().unwrap(), "/dev/null", "0"])
            .unwrap()
            .open();
        assert!(opened.is_ok());
        assert!(out.exists(), "the stdout file is created");
    }

    /// The coverage file is reset before a run, only when it is a regular
    /// file, and archived after it with its length, or named missing.
    #[test]
    fn a_coverage_file_is_reset_then_archived_with_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let coverage = dir.path().join("cov");
        let dest = dir.path().join("kept");
        reset_coverage(&coverage).unwrap();
        std::fs::write(&coverage, b"stale").unwrap();
        reset_coverage(&coverage).unwrap();
        assert!(!coverage.exists(), "the stale file is removed");
        assert_eq!(coverage_field(&coverage, &dest), "missing");
        std::fs::write(&coverage, b"0123456789").unwrap();
        assert_eq!(coverage_field(&coverage, &dest), "10");
        assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
        assert!(!coverage.exists(), "the file moved");
        std::fs::create_dir(&coverage).unwrap();
        let error = reset_coverage(&coverage).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        assert!(coverage.is_dir(), "a directory is never removed");
        assert_eq!(coverage_field(&coverage, &dest), "invalid");
    }

    #[test]
    fn a_relative_or_irregular_coverage_path_is_refused() {
        let error = check_coverage_path(Path::new("cov")).err().unwrap();
        assert!(format!("{error:#}").contains("absolute path"), "{error:#}");
        let dir = tempfile::tempdir().unwrap();
        let error = check_coverage_path(dir.path()).err().unwrap();
        assert!(error.is::<super::super::container::PolicyRefusal>());
        check_coverage_path(&dir.path().join("absent")).unwrap();
    }

    #[test]
    fn served_refusals_are_policy_refusals() {
        refuse_unserved(&[("--verify", false)]).unwrap();
        let error = refuse_unserved(&[("--verify", true), ("--fatal-core-dir", true)])
            .err()
            .unwrap();
        assert!(error.is::<super::super::container::PolicyRefusal>());
        let text = format!("{error:#}");
        assert!(
            text.contains("this invocation has --verify, --fatal-core-dir"),
            "{text}"
        );
    }
}
