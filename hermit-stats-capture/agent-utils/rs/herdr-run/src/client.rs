//! Typed access to Herdr's command-line socket API.
//!
//! Herdr itself owns protocol negotiation, so this module invokes its JSON-producing CLI rather
//! than duplicating the socket wire format.  Production calls deliberately ignore the caller's
//! `PATH` and `HOME`: the executable is selected from fixed install locations rooted in the current
//! account's database home, and that same home is the only environment value passed to
//! `systemd-run`.
//!
//! Every executable and the account home are resolved lazily, at the first call that needs them, in
//! one fixed order (Herdr, then `systemd-run`, then the account home), so a subcommand that never
//! calls Herdr never depends on its installation, and both editions report the same missing piece
//! first.
//!
//! This is a safety rail, not a same-UID security boundary.  A normal user installation is
//! necessarily writable by that user, and a same-UID process can usually reach Herdr's socket (and
//! often the user systemd bus) directly.  Enforceable isolation therefore also requires a broker
//! and sandbox policy that deny those raw control channels.

use std::ffi::{CStr, CString, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::error::{HerdrRunError, Result};
use crate::textfmt::{display, os_error_text, quote, strict_json, strip_space};

/// Transient user-systemd unit used for a server started by `herdr-run`.
pub const SERVER_UNIT: &str = "herdr-run-server";

/// Tokio runtime workers for an auto-started server.
///
/// Herdr is a static musl binary, and musl's mallocng has one global allocator lock; tokio's
/// default of one worker per core turns that lock into a convoy on a many-core host. Measured on a
/// 316-core host with 260 panes: uncapped, the server sat above 1000% CPU with about 98% of cycles
/// in the kernel's futex spinlock and every control call timing out; capped at 16, the same
/// session served `pane list` in 0.03 s. This bounds only the runtime workers; per-pane blocking
/// threads still scale with the pane count.
pub const SERVER_WORKER_THREADS: usize = 16;

/// Fixed locations searched for `systemd-run`, in order.
pub const SYSTEMD_RUN_CANDIDATES: [&str; 2] = ["/usr/bin/systemd-run", "/bin/systemd-run"];

const SERVER_ATTEMPTS: usize = 30;
const SERVER_DELAY: Duration = Duration::from_millis(200);
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

// Linux `pid_t` is signed.  Parsing into i64 first lets us issue one clear protocol error for
// zero, negative, and otherwise representable values beyond the shared process-ID range.
const MAX_PROCESS_ID: i64 = i32::MAX as i64;

pub(crate) struct BoundedOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// A process's exit status as one number: its exit code, or `-N` when signal `N` killed it.
#[must_use]
pub fn exit_status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .unwrap_or_else(|| status.signal().map_or(1, |signal| -signal))
}

/// Capture a subprocess while enforcing a wall-clock bound and draining both pipes concurrently.
///
/// The child starts a new session (`setsid`): it has no controlling terminal, so a helper that
/// tries to prompt on `/dev/tty` fails at once instead of stopping in the background until the
/// bound expires, and killing the session's process group on timeout also reaches descendants
/// that inherited the captured pipes.
///
/// The bound covers the PIPES as well as the child: a command whose own process has exited while
/// a descendant still holds stdout or stderr open has not finished, and at the deadline the whole
/// process group is killed exactly as for a child that is still running. The child is reaped only
/// once both pipes have closed, so its process-group ID cannot be reused by an unrelated group
/// before that kill. This is how the other edition's `communicate(timeout=...)` behaves.
///
/// The child also starts with SIGXFSZ at its default action, as the other edition's children do:
/// this process ignores SIGXFSZ (see [`crate::cli::prepare_process`]), and an ignored disposition
/// would otherwise be inherited across `exec`.
///
/// The program is executed by [`ExecPlan`], not by the standard library's `execvp`, so a file the
/// kernel cannot execute -- a script without a `#!` line, say -- fails with `Exec format error
/// (os error 8)` exactly as it does in the other edition, instead of being handed to `/bin/sh`.
/// Its environment is built by [`child_environment`], the rule both editions share.
///
/// Every signal this process was started with ignored is ignored again in the child, except
/// SIGPIPE, SIGXFSZ, and SIGCHLD (see [`signals_kept_ignored`]). The C library takes signal 33 for
/// its own use as soon as this process starts a thread, and a handler is reset to the default
/// action across `exec`; without this, every child started after the first reader thread got
/// signal 33 at its default action where the other edition's children keep it ignored.
pub(crate) fn bounded_output(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<BoundedOutput> {
    let kept_ignored = signals_kept_ignored(INHERITED_IGNORED_SIGNALS.load(Ordering::Relaxed));
    bounded_output_keeping(command, timeout, kept_ignored)
}

/// [`bounded_output`], with the signals the child gets ignored again given as a mask.
fn bounded_output_keeping(
    command: &mut Command,
    timeout: Duration,
    kept_ignored: u64,
) -> io::Result<BoundedOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let plan = ExecPlan::new(command)?;
    // SAFETY: `setsid`, `signal`, `rt_sigaction`, and `execve` are async-signal-safe, and neither
    // `ignore_signals` nor `ExecPlan::exec` makes any other call or allocates: everything they
    // pass was built before the fork.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::signal(libc::SIGXFSZ, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            ignore_signals(kept_ignored);
            Err(plan.exec())
        });
    }
    let mut child = command.spawn()?;
    let child_pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("child stdout pipe was not captured"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("child stderr pipe was not captured"))?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let deadline = Instant::now().checked_add(timeout);
    let expired = || deadline.is_some_and(|value| Instant::now() >= value);
    let kill_and_fail = |child: &mut std::process::Child| {
        // The child leads a fresh session and process group. Killing the group also closes pipes
        // inherited by descendants, so reader threads cannot keep this path blocked.
        // SAFETY: `kill` has no pointer arguments; the group is the unreaped child's own.
        let _ = unsafe { libc::kill(-(child_pid as i32), libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("control command timed out after {}s", timeout.as_secs()),
        )
    };
    // Both pipes to end of file first, with the child left unreaped.
    while !(stdout_reader.is_finished() && stderr_reader.is_finished()) {
        if expired() {
            let error = kill_and_fail(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(10));
    }
    // Then the exit status, within the same deadline.
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if expired() {
            let error = kill_and_fail(&mut child);
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| io::Error::other("stdout reader thread panicked"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| io::Error::other("stderr reader thread panicked"))??;
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

/// This process's environment as `NAME=value` entries, in its own order.
///
/// Read through the standard library, which skips an entry with no `=` after its first byte and
/// splits the others at the first `=` after their first byte. Joined back, every entry it keeps is
/// the entry as it was, and every entry it skips (no `=` at all, or only a leading one) is one the
/// shared rule drops anyway, so [`child_environment`] makes of these exactly what it makes of the
/// entries themselves. This process never changes its own environment.
fn environment_entries() -> Vec<Vec<u8>> {
    std::env::vars_os()
        .map(|(name, value)| {
            let mut entry = name.into_vec();
            entry.push(b'=');
            entry.extend_from_slice(&value.into_vec());
            entry
        })
        .collect()
}

/// The environment a process herdr-run starts receives, by the rule both editions share.
///
/// * Each entry is split at its first `=`: the name is what precedes it and the value what
///   follows, so a value may hold `=` and a name never does.
/// * An entry with no `=`, or with nothing before its first `=`, is dropped. That includes `=A=1`,
///   which the C library's `getenv` and the standard library's environment reader take for a
///   variable named `=A`: its name here is empty, so no child receives it.
/// * Of two entries with one name, the first is kept, where it stood.
/// * The order is otherwise the order of `entries`.
/// * Then each change is applied in turn: a value replaces the variable's value where it stands
///   (`HOME` for a Herdr call keeps its place), or is added at the end when the variable is
///   absent; `None` removes the variable (`PATH` for a Herdr call).
///
/// Applying the rule to its own result changes nothing.
pub(crate) fn child_environment<E: AsRef<[u8]>>(
    entries: impl IntoIterator<Item = E>,
    changes: &[(&[u8], Option<&[u8]>)],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut variables: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let entry = entry.as_ref();
        let Some(split) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (name, value) = (&entry[..split], &entry[split + 1..]);
        if !name.is_empty() && seen.insert(name.to_vec()) {
            variables.push((name.to_vec(), value.to_vec()));
        }
    }
    for (name, replacement) in changes {
        let position = variables.iter().position(|(known, _)| known == name);
        match (replacement, position) {
            (Some(value), Some(index)) => variables[index].1 = value.to_vec(),
            (Some(value), None) => variables.push((name.to_vec(), value.to_vec())),
            (None, Some(index)) => {
                variables.remove(index);
            }
            (None, None) => {}
        }
    }
    variables
}

/// Signals this process was started with ignored, as a mask (bit `N - 1` for signal `N`).
///
/// Recorded once by [`record_inherited_ignored_signals`], before any thread exists, and read by
/// every child started afterwards. Zero until then, so a test that drives [`crate::cli::main`]
/// in-process restores nothing.
static INHERITED_IGNORED_SIGNALS: AtomicU64 = AtomicU64::new(0);

/// The highest signal number on Linux.
const LAST_SIGNAL: libc::c_int = 64;

/// Which signals ignored at start-up a child gets ignored again, as a mask.
///
/// Every one of them, except the three every child of either edition gets at its default action:
/// SIGPIPE and SIGXFSZ, which both runtimes ignore for themselves and reset for their children,
/// and SIGCHLD, which herdr-run resets for itself at start-up. SIGKILL and SIGSTOP can never be
/// ignored.
#[must_use]
pub(crate) fn signals_kept_ignored(ignored_at_start: u64) -> u64 {
    let reset = [
        libc::SIGPIPE,
        libc::SIGXFSZ,
        libc::SIGCHLD,
        libc::SIGKILL,
        libc::SIGSTOP,
    ]
    .iter()
    .fold(0_u64, |mask, signal| mask | signal_bit(*signal));
    ignored_at_start & !reset
}

fn signal_bit(signal: libc::c_int) -> u64 {
    1_u64 << (signal - 1)
}

/// Record which signals this process was started with ignored; called before any thread exists.
///
/// The C library refuses to report or change the signals it keeps for itself (32 and 33 on glibc),
/// so they are read with the `rt_sigaction` system call directly.
pub(crate) fn record_inherited_ignored_signals() {
    let mut mask = 0_u64;
    for signal in 1..=LAST_SIGNAL {
        if disposition_is_ignored(signal) {
            mask |= signal_bit(signal);
        }
    }
    INHERITED_IGNORED_SIGNALS.store(mask, Ordering::Relaxed);
}

/// The kernel's `struct sigaction` on the architectures whose layout this declares.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
#[repr(C)]
struct KernelSigaction {
    handler: libc::sighandler_t,
    flags: libc::c_ulong,
    restorer: libc::sighandler_t,
    mask: u64,
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn disposition_is_ignored(signal: libc::c_int) -> bool {
    let mut current = KernelSigaction {
        handler: libc::SIG_DFL,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    // SAFETY: a query: no new action, and `current` is a valid kernel `struct sigaction` whose
    // signal set is the eight bytes the size argument names.
    let status = unsafe {
        libc::syscall(
            libc::SYS_rt_sigaction,
            signal,
            std::ptr::null::<KernelSigaction>(),
            &mut current as *mut KernelSigaction,
            std::mem::size_of::<u64>(),
        )
    };
    status == 0 && current.handler == libc::SIG_IGN
}

/// Elsewhere through the C library, which cannot see the signals it keeps for itself.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn disposition_is_ignored(signal: libc::c_int) -> bool {
    // SAFETY: a query through a zeroed, valid `sigaction`.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

/// Ignore every signal in `mask`; called in the child between `fork` and `exec`.
///
/// Async-signal-safe: one `rt_sigaction` (or `sigaction`) per signal and nothing else. A signal
/// that cannot be ignored is left as it is.
fn ignore_signals(mask: u64) {
    for signal in 1..=LAST_SIGNAL {
        if mask & signal_bit(signal) != 0 {
            ignore_signal(signal);
        }
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn ignore_signal(signal: libc::c_int) {
    let action = KernelSigaction {
        handler: libc::SIG_IGN,
        flags: 0,
        restorer: 0,
        mask: 0,
    };
    // SAFETY: `action` is a valid kernel `struct sigaction` with an eight-byte signal set; the
    // old action is not asked for.
    unsafe {
        libc::syscall(
            libc::SYS_rt_sigaction,
            signal,
            &action as *const KernelSigaction,
            std::ptr::null_mut::<KernelSigaction>(),
            std::mem::size_of::<u64>(),
        );
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn ignore_signal(signal: libc::c_int) {
    // SAFETY: `signal` with a constant disposition has no pointer arguments.
    unsafe {
        libc::signal(signal, libc::SIG_IGN);
    }
}

/// The search path a child gets when its environment names none, as in the other edition.
const DEFAULT_SEARCH_PATH: &[u8] = b"/bin:/usr/bin";

/// Everything one `execve` attempt needs, built before the fork so the child allocates nothing.
///
/// The rule is the other edition's (`subprocess` on POSIX), not the C library's `execvp`: a
/// program holding a `/` is executed as named; a bare name is tried in each directory of the
/// child's `PATH` (`/bin:/usr/bin` when it has none, an empty entry meaning the bare name), the
/// first that executes wins, and when none does the error reported is the first one that was not
/// "no such file" or "not a directory", else the last. Above all, an `Exec format error` is
/// reported as it is, never retried through `/bin/sh` the way `execvp` retries it.
///
/// The environment is [`child_environment`] applied to this process's environment, with the
/// command's explicit changes (`Command::env`, `Command::env_remove`) as the changes, in the order
/// `Command::get_envs` lists them. `Command::env_clear` cannot be honoured, because the standard
/// library gives no stable way to see that it was called; this crate's `clippy.toml` refuses any
/// call to it, so it cannot be used by accident.
struct ExecPlan {
    candidates: Vec<CString>,
    _arguments: Vec<CString>,
    argv: Vec<*const libc::c_char>,
    _variables: Vec<CString>,
    envp: Vec<*const libc::c_char>,
}

// SAFETY: the raw pointers point into the `CString`s owned by the same value, which are never
// mutated or dropped while it lives; the plan is only read, by the forked child.
unsafe impl Send for ExecPlan {}
// SAFETY: as above; nothing is ever written through the pointers.
unsafe impl Sync for ExecPlan {}

impl ExecPlan {
    fn new(command: &Command) -> io::Result<Self> {
        use std::os::unix::ffi::OsStrExt;

        let nul = || io::Error::new(io::ErrorKind::InvalidInput, "argument contains a NUL byte");
        let text = |bytes: &[u8]| CString::new(bytes).map_err(|_| nul());
        let program = command.get_program().as_bytes();
        let changes = command
            .get_envs()
            .map(|(name, value)| (name.as_bytes(), value.map(OsStrExt::as_bytes)))
            .collect::<Vec<_>>();
        let variables = child_environment(environment_entries(), &changes);
        let candidates = if program.contains(&b'/') {
            vec![text(program)?]
        } else {
            let search = variables
                .iter()
                .find(|(name, _)| name == b"PATH")
                .map_or(DEFAULT_SEARCH_PATH, |(_, value)| value.as_slice());
            search
                .split(|byte| *byte == b':')
                .map(|directory| {
                    let mut path = directory.to_vec();
                    if !path.is_empty() && !path.ends_with(b"/") {
                        path.push(b'/');
                    }
                    path.extend_from_slice(program);
                    text(&path)
                })
                .collect::<io::Result<Vec<_>>>()?
        };
        let arguments = std::iter::once(program)
            .chain(command.get_args().map(OsStrExt::as_bytes))
            .map(text)
            .collect::<io::Result<Vec<_>>>()?;
        let variables = variables
            .into_iter()
            .map(|(mut name, value)| {
                name.push(b'=');
                name.extend_from_slice(&value);
                text(&name)
            })
            .collect::<io::Result<Vec<_>>>()?;
        let pointers = |strings: &[CString]| {
            strings
                .iter()
                .map(|string| string.as_ptr())
                .chain(std::iter::once(std::ptr::null()))
                .collect::<Vec<_>>()
        };
        Ok(Self {
            argv: pointers(&arguments),
            envp: pointers(&variables),
            candidates,
            _arguments: arguments,
            _variables: variables,
        })
    }

    /// Try each candidate in turn; return only when none could be executed.
    ///
    /// Async-signal-safe: `execve`, `errno`, and arithmetic only.
    fn exec(&self) -> io::Error {
        let mut first = 0;
        let mut last = libc::ENOENT;
        for candidate in &self.candidates {
            // SAFETY: every pointer is a live NUL-terminated string, and both arrays end in null.
            unsafe {
                libc::execve(candidate.as_ptr(), self.argv.as_ptr(), self.envp.as_ptr());
            }
            last = io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::ENOENT);
            if first == 0 && last != libc::ENOENT && last != libc::ENOTDIR {
                first = last;
            }
        }
        io::Error::from_raw_os_error(if first == 0 { last } else { first })
    }
}

/// One terminal pane and its owning tab and workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pane {
    /// Herdr pane identifier.
    pub pane_id: String,
    /// Herdr tab identifier.
    pub tab_id: String,
    /// Herdr workspace identifier.
    pub workspace_id: String,
}

/// A pane's foreground-process state as reported by Herdr.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessInfo {
    /// Herdr pane identifier to which the reading belongs.
    pub pane_id: String,
    /// PID of the pane's interactive shell.
    pub shell_pid: i64,
    /// Foreground process-group ID associated with the terminal.
    pub foreground_pgid: i64,
    /// `(pid, name, command line)` for each foreground process.
    pub foreground: Vec<(i64, String, String)>,
}

/// Captured result of one external command invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    /// Exit code, or `-N` when signal `N` killed the process.
    pub status: i32,
    /// Captured standard output decoded as UTF-8 with replacement.
    pub stdout: String,
    /// Captured standard error decoded as UTF-8 with replacement.
    pub stderr: String,
}

/// Injectable external-command transport used by [`HerdrClient`].
pub trait CommandRunner: Send + Sync {
    /// Run exactly `argv` with `HOME` set to `home`, bounded by `timeout`.
    ///
    /// Test transports may ignore `home` and `timeout` because they execute in-process.
    fn run(&self, argv: &[String], home: &str, timeout: Duration) -> io::Result<CommandOutput>;
}

/// Real [`CommandRunner`] backed by [`std::process::Command`].
///
/// It removes caller-controlled `PATH` and replaces `HOME` with the account-database value. All
/// programs it receives from [`HerdrClient`] are absolute, so executable search is unnecessary.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl SystemCommandRunner {
    fn command(argv: &[String], home: &str) -> io::Result<Command> {
        if argv.iter().any(|argument| argument.contains('\0')) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "argument contains a NUL byte",
            ));
        }
        let (program, arguments) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command argv"))?;
        let mut command = Command::new(program);
        command.args(arguments).env_remove("PATH").env("HOME", home);
        Ok(command)
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, argv: &[String], home: &str, timeout: Duration) -> io::Result<CommandOutput> {
        let output = bounded_output(&mut Self::command(argv, home)?, timeout)?;
        Ok(CommandOutput {
            status: exit_status_code(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Injectable delay mechanism used while polling server startup.
pub trait Sleeper: Send + Sync {
    /// Pause for `duration`.
    fn sleep(&self, duration: Duration);
}

/// Real [`Sleeper`] backed by [`std::thread::sleep`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Operations needed by session resolution, readiness checks, and command execution.
///
/// The trait keeps those layers independently testable without spawning Herdr.  The production
/// implementation is [`HerdrClient`].
pub trait HerdrApi {
    /// Ensure a compatible server is running; return whether this call started it.
    fn ensure_server(&self) -> Result<bool>;
    /// Report whether a server is running, WITHOUT starting one.
    ///
    /// Separate from [`HerdrApi::ensure_server`] because a read-only caller must be able to say
    /// "no server" without becoming the reason there is one.
    fn server_running(&self) -> bool;
    /// Resolve a workspace label, refusing ambiguous duplicate labels.
    fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>>;
    /// Create a workspace and return `(workspace ID, root tab ID, root pane ID)`.
    fn create_workspace(&self, label: &str, cwd: &str) -> Result<(String, String, String)>;
    /// Resolve a tab label inside a workspace, refusing ambiguous duplicate labels.
    fn tab_id_for_label(&self, workspace_id: &str, label: &str) -> Result<Option<String>>;
    /// Create a tab without changing the user's focused tab.
    fn create_tab(&self, workspace_id: &str, label: &str, cwd: &str) -> Result<String>;
    /// Rename a tab.
    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()>;
    /// Close a tab and every pane in it.
    ///
    /// Defaults to a refusal so a client that never closes tabs need not implement it.
    fn close_tab(&self, tab_id: &str) -> Result<()> {
        Err(HerdrRunError::unavailable(format!(
            "this Herdr client cannot close tab {tab_id}"
        )))
    }
    /// List panes, optionally restricted to a workspace.
    fn panes(&self, workspace_id: Option<&str>) -> Result<Vec<Pane>>;
    /// Report whether a pane ID is currently live.
    fn pane_exists(&self, pane_id: &str) -> bool;
    /// Read live foreground-process information for a pane.
    fn process_info(&self, pane_id: &str) -> Result<ProcessInfo>;
    /// Read rendered pane text.
    fn read(&self, pane_id: &str, source: &str, lines: Option<usize>) -> Result<String>;
    /// Type and submit a command in a pane.
    fn run(&self, pane_id: &str, command: &str) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Broker {
    Direct,
    SystemdRun,
}

impl Broker {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "direct" => Ok(Self::Direct),
            "systemd-run" => Ok(Self::SystemdRun),
            other => Err(HerdrRunError::config(format!(
                "broker must be 'direct' or 'systemd-run', got {}",
                quote(other)
            ))),
        }
    }
}

/// Production command-level client for a Herdr session.
pub struct HerdrClient {
    configured_herdr_bin: PathBuf,
    herdr_bin: OnceLock<String>,
    systemd_run_bin: OnceLock<String>,
    fixed_account_home: Option<String>,
    broker: Broker,
    runner: Arc<dyn CommandRunner>,
    sleeper: Arc<dyn Sleeper>,
}

impl fmt::Debug for HerdrClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HerdrClient")
            .field("configured_herdr_bin", &self.configured_herdr_bin)
            .field("herdr_bin", &self.herdr_bin.get())
            .field("systemd_run_bin", &self.systemd_run_bin.get())
            .field("broker", &self.broker)
            .finish_non_exhaustive()
    }
}

impl HerdrClient {
    /// Construct a production client without consulting caller-controlled `HOME` or `PATH`.
    ///
    /// Nothing is resolved here; see the module documentation.
    pub fn new(broker: &str) -> Result<Self> {
        Self::with_executable(broker, Path::new("herdr"))
    }

    /// Construct a production client with one explicitly configured Herdr executable.
    ///
    /// A bare filename is searched only in the fixed trusted install locations. A path is
    /// canonicalized directly. Either form must resolve to an executable regular file that is not
    /// group/world writable; caller-controlled `PATH` is never searched.
    pub fn with_executable(broker: &str, configured: &Path) -> Result<Self> {
        Ok(Self {
            configured_herdr_bin: configured.to_path_buf(),
            herdr_bin: OnceLock::new(),
            systemd_run_bin: OnceLock::new(),
            fixed_account_home: None,
            broker: Broker::parse(broker)?,
            runner: Arc::new(SystemCommandRunner),
            sleeper: Arc::new(ThreadSleeper),
        })
    }

    #[cfg(test)]
    fn from_parts(
        broker: &str,
        herdr_bin: PathBuf,
        systemd_run_bin: PathBuf,
        account_home: PathBuf,
        runner: Arc<dyn CommandRunner>,
        sleeper: Arc<dyn Sleeper>,
    ) -> Result<Self> {
        let herdr_bin = utf8_absolute(&herdr_bin, "Herdr executable")?;
        let systemd_run_bin = utf8_absolute(&systemd_run_bin, "systemd-run executable")?;
        let account_home = utf8_absolute(&account_home, "account home")?;
        Ok(Self {
            configured_herdr_bin: PathBuf::from(&herdr_bin),
            herdr_bin: OnceLock::from(herdr_bin),
            systemd_run_bin: OnceLock::from(systemd_run_bin),
            fixed_account_home: Some(account_home),
            broker: Broker::parse(broker)?,
            runner,
            sleeper,
        })
    }

    /// Resolve every executable this client would need, without invoking any of them.
    ///
    /// `status` calls this because it must distinguish "Herdr is not installed" from "Herdr is
    /// installed and no server is running". Herdr is checked first, then `systemd-run`.
    pub fn preflight(&self) -> Result<()> {
        self.herdr_executable()?;
        self.systemd_run_executable()?;
        Ok(())
    }

    fn account_home(&self) -> Result<String> {
        if let Some(home) = &self.fixed_account_home {
            return Ok(home.clone());
        }
        utf8_absolute(&current_account_home()?, "account home")
    }

    fn herdr_executable(&self) -> Result<String> {
        if let Some(resolved) = self.herdr_bin.get() {
            return Ok(resolved.clone());
        }
        let resolved = if self.configured_herdr_bin.components().count() > 1
            || self.configured_herdr_bin.is_absolute()
        {
            resolve_fixed_executable("Herdr", std::slice::from_ref(&self.configured_herdr_bin))?
        } else {
            let home = PathBuf::from(self.account_home()?);
            resolve_configured_herdr_executable(&self.configured_herdr_bin, &home)?
        };
        let resolved = utf8_absolute(&resolved, "Herdr executable")?;
        // Concurrent first callers may both resolve. Whichever stores first becomes the stable
        // executable for this client; every caller returns that same cached winner.
        let _ = self.herdr_bin.set(resolved);
        Ok(self
            .herdr_bin
            .get()
            .expect("a successful Herdr resolution must populate the cache")
            .clone())
    }

    fn systemd_run_executable(&self) -> Result<String> {
        if let Some(resolved) = self.systemd_run_bin.get() {
            return Ok(resolved.clone());
        }
        let candidates = SYSTEMD_RUN_CANDIDATES.map(PathBuf::from);
        let resolved = resolve_fixed_executable("systemd-run", &candidates)?;
        let resolved = utf8_absolute(&resolved, "systemd-run executable")?;
        let _ = self.systemd_run_bin.set(resolved);
        Ok(self
            .systemd_run_bin
            .get()
            .expect("a successful systemd-run resolution must populate the cache")
            .clone())
    }

    /// Ensure a compatible server is available, using production polling limits.
    pub fn ensure_server(&self) -> Result<bool> {
        self.ensure_server_with(SERVER_ATTEMPTS, SERVER_DELAY)
    }

    /// Ensure a server is available with explicit polling controls.
    ///
    /// This is primarily useful to deterministic test harnesses and embedders with their own
    /// latency budget.  The first readiness check happens before any server launch, and the server
    /// is always launched through `systemd-run`, whatever the broker.
    pub fn ensure_server_with(&self, attempts: usize, delay: Duration) -> Result<bool> {
        if self.server_running() {
            return Ok(false);
        }

        let herdr_bin = self.herdr_executable()?;
        let systemd_run = self.systemd_run_executable()?;
        let home = self.account_home()?;
        let launch = vec![
            systemd_run,
            "--user".to_owned(),
            "--collect".to_owned(),
            "--unit".to_owned(),
            SERVER_UNIT.to_owned(),
            "--description".to_owned(),
            "herdr-run Herdr server (outside the agent sandbox)".to_owned(),
            "--setenv".to_owned(),
            format!("HOME={home}"),
            "--setenv".to_owned(),
            format!("TOKIO_WORKER_THREADS={SERVER_WORKER_THREADS}"),
            herdr_bin,
            "server".to_owned(),
        ];
        let completed = self
            .runner
            .run(&launch, &home, CONTROL_TIMEOUT)
            .map_err(|error| {
                HerdrRunError::unavailable(format!(
                    "cannot start the Herdr server: {}",
                    os_error_text(&error)
                ))
            })?;
        let combined = format!("{}{}", completed.stdout, completed.stderr);
        if completed.status != 0 && !combined.contains("already exists") {
            return Err(HerdrRunError::unavailable(format!(
                "cannot start the Herdr server: {}",
                detail(&completed)
            )));
        }

        for _ in 0..attempts {
            self.sleeper.sleep(delay);
            if self.server_running() {
                return Ok(true);
            }
        }
        Err(HerdrRunError::unavailable(format!(
            "the Herdr server did not become ready after {attempts} attempts; check 'systemctl --user status {SERVER_UNIT}'"
        )))
    }

    /// Resolve a workspace label, refusing ambiguous duplicate labels.
    pub fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
        let result = self.call(&strings(&["workspace", "list"]), "workspace list")?;
        let values = value_array(result.get("workspaces"), "workspace list.workspaces")?;
        let entries = object_entries(values, "workspace list entry")?;
        unique_label_id(&entries, "workspace_id", label, "workspace")
    }

    /// Create a labelled workspace without changing focus.
    pub fn create_workspace(&self, label: &str, cwd: &str) -> Result<(String, String, String)> {
        let result = self.call(
            &strings(&[
                "workspace",
                "create",
                "--label",
                label,
                "--cwd",
                cwd,
                "--no-focus",
            ]),
            &format!("workspace create {}", quote(label)),
        )?;
        let workspace = required_object(&result, "workspace", "workspace create")?;
        let tab = required_object(&result, "tab", "workspace create")?;
        let pane = required_object(&result, "root_pane", "workspace create")?;
        Ok((
            required_string(workspace, "workspace_id", "workspace create")?,
            required_string(tab, "tab_id", "workspace create")?,
            required_string(pane, "pane_id", "workspace create")?,
        ))
    }

    /// Resolve a tab label inside `workspace_id`, refusing ambiguous duplicates.
    pub fn tab_id_for_label(&self, workspace_id: &str, label: &str) -> Result<Option<String>> {
        let result = self.call(
            &strings(&["tab", "list", "--workspace", workspace_id]),
            "tab list",
        )?;
        let tabs = value_array(result.get("tabs"), "tab list.tabs")?;
        let entries = object_entries(tabs, "tab list entry")?;
        unique_label_id(&entries, "tab_id", label, "tab")
    }

    /// Create a labelled tab without changing focus and return its ID.
    pub fn create_tab(&self, workspace_id: &str, label: &str, cwd: &str) -> Result<String> {
        let result = self.call(
            &strings(&[
                "tab",
                "create",
                "--workspace",
                workspace_id,
                "--label",
                label,
                "--cwd",
                cwd,
                "--no-focus",
            ]),
            &format!("tab create {}", quote(label)),
        )?;
        let tab = match result.get("tab") {
            Some(value) => value.as_object().ok_or_else(|| {
                HerdrRunError::unavailable("tab create: \"tab\" is not an object")
            })?,
            None => &result,
        };
        required_string(tab, "tab_id", "tab create")
    }

    /// Rename `tab_id` to `label`.
    pub fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        self.call(
            &strings(&["tab", "rename", tab_id, label]),
            &format!("tab rename {tab_id}"),
        )?;
        Ok(())
    }

    /// Close one explicitly owned tab, without closing its shared workspace.
    pub fn close_tab(&self, tab_id: &str) -> Result<()> {
        self.call_ok(
            &strings(&["tab", "close", tab_id]),
            &format!("tab close {tab_id}"),
        )
    }

    /// List all panes, optionally restricted to `workspace_id`.
    ///
    /// Every entry is validated before any is checked against the requested workspace.
    pub fn panes(&self, workspace_id: Option<&str>) -> Result<Vec<Pane>> {
        let mut args = strings(&["pane", "list"]);
        if let Some(workspace_id) = workspace_id {
            args.extend(strings(&["--workspace", workspace_id]));
        }
        let result = self.call(&args, "pane list")?;
        let values = value_array(result.get("panes"), "pane list.panes")?;
        let entries = object_entries(values, "pane list entry")?;
        let panes = entries
            .iter()
            .map(|pane| {
                Ok(Pane {
                    pane_id: required_string(pane, "pane_id", "pane list entry")?,
                    tab_id: required_string(pane, "tab_id", "pane list entry")?,
                    workspace_id: required_string(pane, "workspace_id", "pane list entry")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(expected) = workspace_id {
            if let Some(pane) = panes.iter().find(|pane| pane.workspace_id != expected) {
                return Err(HerdrRunError::unavailable(format!(
                    "pane list: returned pane {} from workspace {}, expected {}",
                    quote(&pane.pane_id),
                    quote(&pane.workspace_id),
                    quote(expected)
                )));
            }
        }
        Ok(panes)
    }

    /// Report whether a pane ID is currently live.
    #[must_use]
    pub fn pane_exists(&self, pane_id: &str) -> bool {
        self.call(
            &strings(&["pane", "get", pane_id]),
            &format!("pane get {pane_id}"),
        )
        .is_ok()
    }

    /// Read foreground-process information for `pane_id`.
    pub fn process_info(&self, pane_id: &str) -> Result<ProcessInfo> {
        let result = self.call(
            &strings(&["pane", "process-info", "--pane", pane_id]),
            &format!("pane process-info {pane_id}"),
        )?;
        let info = required_object(&result, "process_info", "pane process-info")?;
        let processes = value_array(info.get("foreground_processes"), "foreground_processes")?;
        let foreground = object_entries(processes, "foreground process")?
            .iter()
            .map(|process| {
                Ok((
                    required_process_id(process, "pid", "foreground process")?,
                    optional_string(process, "name", "foreground process")?.unwrap_or_default(),
                    optional_string(process, "cmdline", "foreground process")?.unwrap_or_default(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let parsed = ProcessInfo {
            pane_id: required_string(info, "pane_id", "pane process-info")?,
            shell_pid: required_process_id(info, "shell_pid", "pane process-info")?,
            foreground_pgid: required_process_id(
                info,
                "foreground_process_group_id",
                "pane process-info",
            )?,
            foreground,
        };
        if parsed.pane_id != pane_id {
            return Err(HerdrRunError::unavailable(format!(
                "pane process-info: returned pane {}, expected {}",
                quote(&parsed.pane_id),
                quote(pane_id)
            )));
        }
        Ok(parsed)
    }

    /// Read rendered text from `pane_id`.
    pub fn read(&self, pane_id: &str, source: &str, lines: Option<usize>) -> Result<String> {
        let mut args = strings(&["pane", "read", pane_id, "--source", source]);
        if let Some(lines) = lines {
            args.extend(["--lines".to_owned(), lines.to_string()]);
        }
        let completed = self.invoke(&args)?;
        if completed.status != 0 {
            return Err(HerdrRunError::unavailable(format!(
                "pane read {pane_id}: {}",
                stderr_detail(&completed)
            )));
        }
        Ok(completed.stdout)
    }

    /// Type and submit `command` in `pane_id`.
    pub fn run(&self, pane_id: &str, command: &str) -> Result<()> {
        self.call_ok(
            &strings(&["pane", "run", pane_id, command]),
            &format!("pane run {pane_id}"),
        )
    }

    /// Report whether a Herdr server is currently running, starting nothing.
    pub fn server_running(&self) -> bool {
        let completed = match self.invoke(&strings(&["status", "--json"])) {
            Ok(completed) if completed.status == 0 => completed,
            _ => return false,
        };
        let Some(document) = strict_json(&completed.stdout) else {
            return false;
        };
        document
            .as_object()
            .and_then(|object| object.get("server"))
            .and_then(Value::as_object)
            .and_then(|server| server.get("running"))
            .and_then(Value::as_bool)
            == Some(true)
    }

    fn invoke(&self, args: &[String]) -> Result<CommandOutput> {
        let mut command = Vec::with_capacity(args.len() + 1);
        command.push(self.herdr_executable()?);
        command.extend_from_slice(args);
        if self.broker == Broker::SystemdRun {
            let systemd_run = self.systemd_run_executable()?;
            let home = self.account_home()?;
            let mut wrapped = vec![
                systemd_run,
                "--user".to_owned(),
                "--wait".to_owned(),
                "--pipe".to_owned(),
                "--collect".to_owned(),
                "--quiet".to_owned(),
                "--setenv".to_owned(),
                format!("HOME={home}"),
            ];
            wrapped.extend(command);
            command = wrapped;
        }
        let home = self.account_home()?;
        self.runner
            .run(&command, &home, CONTROL_TIMEOUT)
            .map_err(|error| {
                HerdrRunError::unavailable(format!(
                    "cannot invoke Herdr: {}",
                    os_error_text(&error)
                ))
            })
    }

    fn call(&self, args: &[String], purpose: &str) -> Result<Map<String, Value>> {
        let completed = self.invoke(args)?;
        if completed.status != 0 {
            return Err(HerdrRunError::unavailable(format!(
                "{purpose}: {}",
                detail(&completed)
            )));
        }
        let document = strict_json(&completed.stdout).ok_or_else(|| {
            let preview: String = strip_space(&completed.stdout).chars().take(200).collect();
            HerdrRunError::unavailable(format!(
                "{purpose}: herdr returned non-JSON output: {}",
                quote(&preview)
            ))
        })?;
        let envelope = document.as_object().ok_or_else(|| {
            HerdrRunError::unavailable(format!("{purpose}: Herdr response is not an object"))
        })?;
        envelope
            .get("result")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| {
                HerdrRunError::unavailable(format!(
                    "{purpose}: Herdr response has no result object"
                ))
            })
    }

    fn call_ok(&self, args: &[String], purpose: &str) -> Result<()> {
        let completed = self.invoke(args)?;
        if completed.status == 0 {
            Ok(())
        } else {
            Err(HerdrRunError::unavailable(format!(
                "{purpose}: {}",
                detail(&completed)
            )))
        }
    }
}

impl HerdrApi for HerdrClient {
    fn ensure_server(&self) -> Result<bool> {
        HerdrClient::ensure_server(self)
    }

    fn server_running(&self) -> bool {
        HerdrClient::server_running(self)
    }

    fn workspace_id_for_label(&self, label: &str) -> Result<Option<String>> {
        HerdrClient::workspace_id_for_label(self, label)
    }

    fn create_workspace(&self, label: &str, cwd: &str) -> Result<(String, String, String)> {
        HerdrClient::create_workspace(self, label, cwd)
    }

    fn tab_id_for_label(&self, workspace_id: &str, label: &str) -> Result<Option<String>> {
        HerdrClient::tab_id_for_label(self, workspace_id, label)
    }

    fn create_tab(&self, workspace_id: &str, label: &str, cwd: &str) -> Result<String> {
        HerdrClient::create_tab(self, workspace_id, label, cwd)
    }

    fn rename_tab(&self, tab_id: &str, label: &str) -> Result<()> {
        HerdrClient::rename_tab(self, tab_id, label)
    }

    fn close_tab(&self, tab_id: &str) -> Result<()> {
        HerdrClient::close_tab(self, tab_id)
    }

    fn panes(&self, workspace_id: Option<&str>) -> Result<Vec<Pane>> {
        HerdrClient::panes(self, workspace_id)
    }

    fn pane_exists(&self, pane_id: &str) -> bool {
        HerdrClient::pane_exists(self, pane_id)
    }

    fn process_info(&self, pane_id: &str) -> Result<ProcessInfo> {
        HerdrClient::process_info(self, pane_id)
    }

    fn read(&self, pane_id: &str, source: &str, lines: Option<usize>) -> Result<String> {
        HerdrClient::read(self, pane_id, source, lines)
    }

    fn run(&self, pane_id: &str, command: &str) -> Result<()> {
        HerdrClient::run(self, pane_id, command)
    }
}

/// Return the current account's home directory from the account database.
pub(crate) fn current_account_home() -> Result<PathBuf> {
    // SAFETY: `getuid` has no preconditions. `getpwuid_r` writes only into the supplied `passwd`
    // and byte buffer; pointers are inspected only after a zero return and non-null result, and
    // the C string is copied before either backing allocation is dropped. This small FFI wrapper
    // is necessary because Rust's standard library has no account-database lookup API.
    let uid = unsafe { libc::getuid() };
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16_384)
    } else {
        16_384
    };
    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        let code = unsafe {
            libc::getpwuid_r(
                uid,
                record.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if code == libc::ERANGE {
            size = size.saturating_mul(2);
            if size > 16 * 1024 * 1024 {
                return Err(HerdrRunError::unavailable(
                    "cannot resolve current account home: account record is too large",
                ));
            }
            continue;
        }
        if code != 0 {
            return Err(HerdrRunError::unavailable(format!(
                "cannot resolve current account home: {}",
                io::Error::from_raw_os_error(code)
            )));
        }
        if result.is_null() {
            return Err(HerdrRunError::unavailable(
                "cannot resolve current account home: no account record",
            ));
        }
        let record = unsafe { record.assume_init() };
        if record.pw_dir.is_null() {
            return Err(HerdrRunError::unavailable(
                "the current account has no home directory",
            ));
        }
        let bytes = unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes();
        if bytes.is_empty() {
            return Err(HerdrRunError::unavailable(
                "the current account has no home directory",
            ));
        }
        return Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())));
    }
}

/// Resolve a configured Herdr executable: a path directly, a bare name in the fixed locations.
fn resolve_configured_herdr_executable(configured: &Path, home: &Path) -> Result<PathBuf> {
    if configured.components().count() > 1 || configured.is_absolute() {
        return resolve_fixed_executable("Herdr", &[configured.to_path_buf()]);
    }
    let name = configured.as_os_str();
    resolve_fixed_executable(
        "Herdr",
        &[
            Path::new("/usr/local/bin").join(name),
            Path::new("/usr/bin").join(name),
            home.join(".local/bin").join(name),
            home.join("bin").join(name),
            home.join(".cargo/bin").join(name),
        ],
    )
}

fn resolve_fixed_executable(name: &str, candidates: &[PathBuf]) -> Result<PathBuf> {
    for candidate in candidates {
        let Ok(metadata) = fs::metadata(candidate) else {
            continue;
        };
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }
        let canonical = fs::canonicalize(candidate).map_err(|error| {
            HerdrRunError::unavailable(format!(
                "cannot canonicalize {name} executable {}: {}",
                candidate.display(),
                os_error_text(&error)
            ))
        })?;
        let metadata = fs::metadata(&canonical).map_err(|error| {
            HerdrRunError::unavailable(format!(
                "cannot inspect {name} executable {}: {}",
                canonical.display(),
                os_error_text(&error)
            ))
        })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }
        if metadata.permissions().mode() & 0o022 != 0 {
            return Err(HerdrRunError::unavailable(format!(
                "refusing group/world-writable {name} executable outside the jail: {}",
                canonical.display()
            )));
        }
        return Ok(canonical);
    }
    let searched = candidates
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(HerdrRunError::unavailable(format!(
        "{name} executable not found in fixed install locations: {searched}"
    )))
}

fn utf8_absolute(path: &Path, what: &str) -> Result<String> {
    if !path.is_absolute() {
        return Err(HerdrRunError::unavailable(format!(
            "{what} must be absolute: {}",
            path.display()
        )));
    }
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| HerdrRunError::unavailable(format!("{what} is not valid UTF-8")))
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// A failed call's detail: trimmed stderr, else trimmed stdout, else `exit N`.
///
/// Shown with [`display`], so what Herdr wrote is quoted when it holds a line break, a terminal
/// control, or an invisible character: it ends up in a herdr-run message, and nothing reaches a
/// terminal raw from one of those.
fn detail(output: &CommandOutput) -> String {
    let stderr = strip_space(&output.stderr);
    let detail = if stderr.is_empty() {
        strip_space(&output.stdout)
    } else {
        stderr
    };
    if detail.is_empty() {
        format!("exit {}", output.status)
    } else {
        display(detail)
    }
}

/// A failed `pane read`'s detail: trimmed stderr, else `exit N`, shown as [`detail`] shows it.
fn stderr_detail(output: &CommandOutput) -> String {
    let detail = strip_space(&output.stderr);
    if detail.is_empty() {
        format!("exit {}", output.status)
    } else {
        display(detail)
    }
}

fn value_array<'a>(value: Option<&'a Value>, what: &str) -> Result<&'a [Value]> {
    match value {
        Some(Value::Array(values)) => Ok(values),
        None | Some(_) => Err(HerdrRunError::unavailable(format!(
            "{what}: expected an array"
        ))),
    }
}

fn object_entries(values: &[Value], what: &str) -> Result<Vec<Map<String, Value>>> {
    values
        .iter()
        .map(|value| {
            value
                .as_object()
                .cloned()
                .ok_or_else(|| HerdrRunError::unavailable(format!("{what}: expected an object")))
        })
        .collect()
}

fn required_object<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a Map<String, Value>> {
    object.get(key).and_then(Value::as_object).ok_or_else(|| {
        HerdrRunError::unavailable(format!("{what}: {} is not an object", quote(key)))
    })
}

fn required_string(object: &Map<String, Value>, key: &str, what: &str) -> Result<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            HerdrRunError::unavailable(format!("{what}: {} is not a string", quote(key)))
        })
}

fn optional_string(object: &Map<String, Value>, key: &str, what: &str) -> Result<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(HerdrRunError::unavailable(format!(
            "{what}: {} is not a string",
            quote(key)
        ))),
    }
}

fn required_integer(object: &Map<String, Value>, key: &str, what: &str) -> Result<i64> {
    object.get(key).and_then(Value::as_i64).ok_or_else(|| {
        HerdrRunError::unavailable(format!("{what}: {} is not an integer", quote(key)))
    })
}

fn required_process_id(object: &Map<String, Value>, key: &str, what: &str) -> Result<i64> {
    let value = required_integer(object, key, what)?;
    if !(1..=MAX_PROCESS_ID).contains(&value) {
        return Err(HerdrRunError::unavailable(format!(
            "{what}: {} is outside the positive Linux pid_t range",
            quote(key)
        )));
    }
    Ok(value)
}

fn unique_label_id(
    entries: &[Map<String, Value>],
    id_key: &str,
    label: &str,
    kind: &str,
) -> Result<Option<String>> {
    let mut matches = Vec::new();
    for entry in entries {
        if optional_string(entry, "label", &format!("{kind} list entry"))?.as_deref() == Some(label)
        {
            matches.push(required_string(
                entry,
                id_key,
                &format!("{kind} list entry"),
            )?);
        }
    }
    if matches.len() > 1 {
        return Err(HerdrRunError::unavailable(format!(
            "{kind} label {} is ambiguous: {} matching IDs",
            quote(label),
            matches.len()
        )));
    }
    Ok(matches.into_iter().next())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use super::*;

    /// The direct child exiting is not the command finishing while a descendant holds its output:
    /// the deadline covers the pipes, and the whole group is killed at it, descendant included.
    #[test]
    fn the_deadline_covers_a_descendant_holding_the_pipes() {
        let directory = std::env::temp_dir().join(format!(
            "herdr-run-pipe-holder-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        fs::create_dir_all(&directory).unwrap();
        let pid_path = directory.join("holder.pid");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("/usr/bin/sleep 60 & printf '%s' \"$!\" > \"$1\"; exit 0")
            .arg("pipe-holder")
            .arg(&pid_path);
        let started = Instant::now();
        let error = bounded_output(&mut command, Duration::from_millis(500))
            .err()
            .expect("a held pipe must time out");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        let holder: i32 = fs::read_to_string(&pid_path).unwrap().parse().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            // Gone, or a zombie waiting for its reaper: either way it is no longer running.
            let finished =
                fs::read_to_string(format!("/proc/{holder}/stat")).map_or(true, |stat| {
                    stat.rsplit_once(") ")
                        .is_some_and(|(_, rest)| rest.starts_with('Z'))
                });
            if finished {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the descendant {holder} is still running"
            );
            thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    /// A child starts with SIGXFSZ at its default action even when this process ignores it.
    #[test]
    fn a_child_gets_the_default_sigxfsz_action() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("grep '^SigIgn:' /proc/self/status");
        let output = bounded_output(&mut command, Duration::from_secs(10)).unwrap();
        let line = String::from_utf8(output.stdout).unwrap();
        let mask =
            u64::from_str_radix(line.trim().trim_start_matches("SigIgn:").trim(), 16).unwrap();
        assert_eq!(mask & (1 << (libc::SIGXFSZ - 1)), 0, "{line}");
    }

    fn child_cases() -> Value {
        serde_json::from_str(include_str!("../testdata/child_cases.json")).expect("child cases")
    }

    /// An item of `child_cases.json`: a string read as UTF-8, or a list of byte values.
    fn corpus_bytes(value: &Value) -> Vec<u8> {
        match value {
            Value::String(text) => text.as_bytes().to_vec(),
            Value::Array(bytes) => bytes
                .iter()
                .map(|byte| u8::try_from(byte.as_u64().expect("a byte")).expect("a byte"))
                .collect(),
            other => panic!("not an environment item: {other}"),
        }
    }

    fn joined(variables: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
        variables
            .iter()
            .map(|(name, value)| [name.as_slice(), b"=", value.as_slice()].concat())
            .collect()
    }

    /// The environment rule shared with the other edition, row by row; and applying it to its own
    /// result changes nothing.
    #[test]
    fn golden_child_environment() {
        let document = child_cases();
        for row in document["environment"].as_array().expect("rows") {
            let entries = row[0]
                .as_array()
                .unwrap()
                .iter()
                .map(corpus_bytes)
                .collect::<Vec<_>>();
            let owned = row[1]
                .as_array()
                .unwrap()
                .iter()
                .map(|change| {
                    let value = (!change[1].is_null()).then(|| corpus_bytes(&change[1]));
                    (corpus_bytes(&change[0]), value)
                })
                .collect::<Vec<_>>();
            let changes = owned
                .iter()
                .map(|(name, value)| (name.as_slice(), value.as_deref()))
                .collect::<Vec<_>>();
            let expected = row[2]
                .as_array()
                .unwrap()
                .iter()
                .map(corpus_bytes)
                .collect::<Vec<_>>();
            let built = child_environment(&entries, &changes);
            assert_eq!(joined(&built), expected, "{row}");
            assert_eq!(child_environment(joined(&built), &[]), built, "{row}");
        }
    }

    /// A child's environment is this process's own under the rule: a variable the command sets
    /// keeps its place, one it removes is gone, and nothing else moves.
    #[test]
    fn a_child_receives_the_environment_the_rule_builds() {
        let mut command = Command::new("/bin/cat");
        command
            .arg("/proc/self/environ")
            .env("HOME", "/herdr-run/replaced")
            .env_remove("PATH");
        let output = bounded_output(&mut command, Duration::from_secs(10)).unwrap();
        let mut received = output.stdout;
        assert_eq!(received.pop(), Some(0), "the environment ends in a NUL");
        let received = received
            .split(|byte| *byte == 0)
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        let expected = child_environment(
            environment_entries(),
            &[
                (b"HOME".as_slice(), Some(b"/herdr-run/replaced".as_slice())),
                (b"PATH".as_slice(), None),
            ],
        );
        assert_eq!(received, joined(&expected));
    }

    fn signal_mask(row: &Value) -> u64 {
        row.as_array()
            .expect("signal numbers")
            .iter()
            .map(|signal| libc::c_int::try_from(signal.as_i64().unwrap()).unwrap())
            .fold(0, |mask, signal| mask | signal_bit(signal))
    }

    /// Which signals ignored at start-up a child gets ignored again, row by row.
    #[test]
    fn golden_child_signals() {
        let document = child_cases();
        for row in document["signals"].as_array().expect("rows") {
            assert_eq!(
                signals_kept_ignored(signal_mask(&row[0])),
                signal_mask(&row[1]),
                "{row}"
            );
        }
    }

    fn child_ignored_mask(kept_ignored: u64) -> u64 {
        let mut command = Command::new("/bin/cat");
        command.arg("/proc/self/status");
        let output =
            bounded_output_keeping(&mut command, Duration::from_secs(10), kept_ignored).unwrap();
        let status = String::from_utf8(output.stdout).unwrap();
        let line = status
            .lines()
            .find(|line| line.starts_with("SigIgn:"))
            .expect("a SigIgn line");
        u64::from_str_radix(line.trim_start_matches("SigIgn:").trim(), 16).unwrap()
    }

    /// A signal recorded as ignored at start-up is ignored again in the child, signal 33
    /// included, whatever this process holds it with by then: the C library took 33 for itself
    /// when this test process started its threads.
    #[test]
    fn a_child_gets_the_recorded_ignored_signals_back() {
        let wanted = signal_bit(libc::SIGHUP) | signal_bit(33) | signal_bit(libc::SIGUSR2);
        assert_eq!(child_ignored_mask(wanted) & wanted, wanted);
        // Nothing recorded, nothing added: the child holds 33 as this process hands it on.
        if !disposition_is_ignored(33) {
            assert_eq!(child_ignored_mask(0) & signal_bit(33), 0);
        }
    }

    /// The query sees what the runtime set: the standard library ignores SIGPIPE for itself.
    #[test]
    fn dispositions_are_read_from_the_kernel() {
        assert!(disposition_is_ignored(libc::SIGPIPE));
        assert!(!disposition_is_ignored(libc::SIGTERM));
    }

    fn scratch_directory(label: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "herdr-run-{label}-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn executable(path: &Path, text: &str) {
        fs::write(path, text).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A script without `#!` is not executable, and is reported so: the C library's `execvp`
    /// would have run it with `/bin/sh` instead, which the other edition never does.
    #[test]
    fn a_file_the_kernel_cannot_execute_is_an_exec_format_error() {
        let directory = scratch_directory("noexec");
        let script = directory.join("no-shebang");
        executable(&script, "touch \"$0.ran\"\n");
        let error = bounded_output(&mut Command::new(&script), Duration::from_secs(10))
            .err()
            .expect("a script without #! must not run");
        assert_eq!(error.to_string(), "Exec format error (os error 8)");
        assert!(!directory.join("no-shebang.ran").exists());
        // Found through PATH it is the same error. A later directory is still tried, as the other
        // edition tries it, and one that holds a runnable file wins.
        let mut command = Command::new("no-shebang");
        command.env("PATH", format!("/nonexistent-dir:{}", directory.display()));
        let error = bounded_output(&mut command, Duration::from_secs(10))
            .err()
            .expect("the first error that is not ENOENT is reported");
        assert_eq!(error.to_string(), "Exec format error (os error 8)");
        let later = scratch_directory("noexec-later");
        executable(&later.join("no-shebang"), "#!/bin/sh\nexit 7\n");
        let mut command = Command::new("no-shebang");
        command.env(
            "PATH",
            format!("{}:{}", directory.display(), later.display()),
        );
        let output = bounded_output(&mut command, Duration::from_secs(10)).unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert!(!directory.join("no-shebang.ran").exists());
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(later).unwrap();
    }

    /// A bare name is searched in the child's PATH, skipping directories that lack it, and the
    /// child gets the environment the command describes.
    #[test]
    fn a_bare_name_is_found_in_the_child_path_with_the_commanded_environment() {
        let empty = scratch_directory("path-empty");
        let found = scratch_directory("path-found");
        executable(
            &found.join("probe-tool"),
            "#!/bin/sh\nprintf '%s|%s|%s' \"$0\" \"$HERDR_RUN_EXEC_PLAN\" \"${HERDR_RUN_GONE-unset}\"\n",
        );
        let mut command = Command::new("probe-tool");
        command
            .env(
                "PATH",
                format!("/nonexistent-dir:{}:{}", empty.display(), found.display()),
            )
            .env("HERDR_RUN_EXEC_PLAN", "set")
            .env_remove("HERDR_RUN_GONE");
        let output = bounded_output(&mut command, Duration::from_secs(10)).unwrap();
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{}/probe-tool|set|unset", found.display())
        );
        let mut missing = Command::new("probe-tool");
        missing.env("PATH", empty.display().to_string());
        let error = bounded_output(&mut missing, Duration::from_secs(10))
            .err()
            .expect("absent everywhere");
        assert_eq!(error.to_string(), "No such file or directory (os error 2)");
        fs::remove_dir_all(empty).unwrap();
        fs::remove_dir_all(found).unwrap();
    }

    static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn protocol_value(
        client: &HerdrClient,
        method: &str,
        args: &[String],
        attempts: Option<usize>,
    ) -> Result<Value> {
        Ok(match method {
            "server_running" => serde_json::json!(client.server_running()),
            "ensure_server" => serde_json::json!(
                client.ensure_server_with(attempts.expect("attempts"), Duration::ZERO)?
            ),
            "workspace_id_for_label" => serde_json::json!(client.workspace_id_for_label(&args[0])?),
            "tab_id_for_label" => serde_json::json!(client.tab_id_for_label(&args[0], &args[1])?),
            "panes" => {
                let panes = client.panes(args.first().map(String::as_str))?;
                let shown = panes
                    .iter()
                    .map(|pane| serde_json::json!([pane.pane_id, pane.tab_id, pane.workspace_id]))
                    .collect::<Vec<_>>();
                Value::Array(shown)
            }
            "process_info" => {
                let info = client.process_info(&args[0])?;
                let foreground = info
                    .foreground
                    .iter()
                    .map(|(pid, name, cmdline)| serde_json::json!([pid, name, cmdline]))
                    .collect::<Vec<_>>();
                serde_json::json!([
                    info.pane_id,
                    info.shell_pid,
                    info.foreground_pgid,
                    foreground
                ])
            }
            "create_workspace" => {
                let (workspace, tab, pane) = client.create_workspace(&args[0], &args[1])?;
                serde_json::json!([workspace, tab, pane])
            }
            "create_tab" => serde_json::json!(client.create_tab(&args[0], &args[1], &args[2])?),
            "read" => serde_json::json!(client.read(&args[0], "recent-unwrapped", None)?),
            "run" => {
                client.run(&args[0], &args[1])?;
                Value::Null
            }
            "pane_exists" => serde_json::json!(client.pane_exists(&args[0])),
            "rename_tab" => {
                client.rename_tab(&args[0], &args[1])?;
                Value::Null
            }
            "close_tab" => {
                client.close_tab(&args[0])?;
                Value::Null
            }
            other => panic!("unknown protocol method {other}"),
        })
    }

    /// Herdr protocol calls shared with the other edition: argv sent, value or error returned.
    #[test]
    fn golden_protocol() {
        let document: Value = serde_json::from_str(include_str!("../testdata/protocol_cases.json"))
            .expect("golden protocol cases");
        assert_eq!(document["herdr"], "/opt/herdr/bin/herdr");
        for case in document["cases"].as_array().expect("case list") {
            let outputs = case["responses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|response| {
                    output(
                        i32::try_from(response[0].as_i64().unwrap()).unwrap(),
                        response[1].as_str().unwrap(),
                        response[2].as_str().unwrap(),
                    )
                })
                .collect();
            let runner = FakeRunner::with_outputs(outputs);
            let client = client(case["broker"].as_str().expect("broker"), runner.clone());
            let attempts = case["args"]
                .get(0)
                .and_then(Value::as_u64)
                .map(|count| usize::try_from(count).unwrap());
            let args = case["args"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|argument| argument.as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            let method = case["method"].as_str().unwrap();
            match (
                protocol_value(&client, method, &args, attempts),
                case.get("error"),
            ) {
                (Ok(value), None) => assert_eq!(value, case["ok"], "{case}"),
                (Err(error), Some(expected)) => assert_eq!(error.to_string(), *expected, "{case}"),
                (got, _) => panic!("{case}: got {got:?}"),
            }
            assert_eq!(serde_json::json!(runner.calls()), case["calls"], "{case}");
            assert!(
                runner.outputs.lock().expect("outputs lock").is_empty(),
                "{case}"
            );
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<Vec<String>>>,
        outputs: Mutex<VecDeque<io::Result<CommandOutput>>>,
    }

    impl FakeRunner {
        fn with_outputs(outputs: Vec<CommandOutput>) -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                outputs: Mutex::new(outputs.into_iter().map(Ok).collect()),
            })
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(
            &self,
            argv: &[String],
            _home: &str,
            _timeout: Duration,
        ) -> io::Result<CommandOutput> {
            self.calls.lock().expect("calls lock").push(argv.to_vec());
            self.outputs
                .lock()
                .expect("outputs lock")
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected command: {argv:?}"))
        }
    }

    #[derive(Default)]
    struct NoSleep;

    impl Sleeper for NoSleep {
        fn sleep(&self, _duration: Duration) {}
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    fn client(broker: &str, runner: Arc<dyn CommandRunner>) -> HerdrClient {
        HerdrClient::from_parts(
            broker,
            PathBuf::from("/opt/herdr/bin/herdr"),
            PathBuf::from("/usr/bin/systemd-run"),
            PathBuf::from("/home/account"),
            runner,
            Arc::new(NoSleep),
        )
        .expect("client")
    }

    #[test]
    fn real_runner_removes_path_pins_account_home_and_refuses_nul() {
        let command =
            SystemCommandRunner::command(&strings(&["/usr/bin/true"]), "/home/account").unwrap();
        let environment = command.get_envs().collect::<Vec<_>>();
        assert!(environment
            .iter()
            .any(|(key, value)| *key == "PATH" && value.is_none()));
        assert!(environment.iter().any(|(key, value)| {
            *key == "HOME" && value.is_some_and(|value| value == "/home/account")
        }));
        let error =
            SystemCommandRunner::command(&strings(&["/bin/echo", "a\0b"]), "/h").unwrap_err();
        assert_eq!(error.to_string(), "argument contains a NUL byte");
    }

    #[test]
    fn real_runner_kills_a_hung_control_command_at_its_bound() {
        let error = SystemCommandRunner
            .run(
                &strings(&["/bin/sh", "-c", "while :; do :; done"]),
                "/home/account",
                Duration::from_millis(30),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "control command timed out after 0s");
    }

    #[test]
    fn a_signal_killed_child_reports_minus_the_signal_and_leads_its_own_session() {
        let output = SystemCommandRunner
            .run(
                &strings(&["/bin/sh", "-c", "kill -9 $$"]),
                "/home/account",
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(output.status, -9);
        let output = SystemCommandRunner
            .run(
                &strings(&["/bin/sh", "-c", "ps -o sid= -p $$; echo $$"]),
                "/home/account",
                Duration::from_secs(10),
            )
            .unwrap();
        let values: Vec<&str> = output.stdout.split_whitespace().collect();
        assert_eq!(values.len(), 2, "{output:?}");
        assert_eq!(values[0], values[1], "the child must lead its own session");
    }

    #[test]
    fn herdr_resolution_uses_fixed_home_candidates_and_rejects_loose_mode() {
        let root = std::env::temp_dir().join(format!(
            "herdr-client-resolution-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let candidate = root.join(".local/bin/herdr");
        fs::create_dir_all(candidate.parent().expect("candidate parent")).unwrap();
        fs::write(&candidate, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&candidate).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&candidate, permissions).unwrap();
        assert_eq!(
            resolve_configured_herdr_executable(Path::new("herdr"), &root).unwrap(),
            fs::canonicalize(&candidate).unwrap()
        );

        let mut permissions = fs::metadata(&candidate).unwrap().permissions();
        permissions.set_mode(0o720);
        fs::set_permissions(&candidate, permissions).unwrap();
        let error = resolve_configured_herdr_executable(Path::new("herdr"), &root).unwrap_err();
        assert!(error.to_string().contains("group/world-writable"));

        fs::remove_file(&candidate).unwrap();
        let cargo_candidate = root.join(".cargo/bin/herdr");
        fs::create_dir_all(cargo_candidate.parent().expect("cargo candidate parent")).unwrap();
        fs::write(&cargo_candidate, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&cargo_candidate).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&cargo_candidate, permissions).unwrap();
        assert_eq!(
            resolve_configured_herdr_executable(Path::new("herdr"), &root).unwrap(),
            fs::canonicalize(&cargo_candidate).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_herdr_is_deferred_until_preflight_or_a_live_query() {
        let missing = std::env::temp_dir().join(format!(
            "missing-herdr-{}-{}",
            std::process::id(),
            TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let client = HerdrClient::with_executable("direct", &missing)
            .expect("constructing a query-free client must not resolve Herdr");

        assert!(client.herdr_bin.get().is_none());
        let preflight_error = client.preflight().unwrap_err();
        assert_eq!(preflight_error.kind(), crate::error::ErrorKind::Unavailable);
        assert!(preflight_error
            .to_string()
            .contains(&missing.display().to_string()));
        assert!(
            client.herdr_bin.get().is_none(),
            "failed resolution is not cached"
        );

        let query_error = client.workspace_id_for_label("wanted").unwrap_err();
        assert_eq!(query_error.kind(), crate::error::ErrorKind::Unavailable);
        assert!(query_error
            .to_string()
            .contains(&missing.display().to_string()));
    }

    #[test]
    fn systemd_broker_uses_absolute_paths_and_only_account_home() {
        let runner =
            FakeRunner::with_outputs(vec![output(0, r#"{"result":{"workspaces":[]}}"#, "")]);
        let client = client("systemd-run", runner.clone());
        assert_eq!(client.workspace_id_for_label("wanted").unwrap(), None);
        assert_eq!(
            runner.calls()[0],
            strings(&[
                "/usr/bin/systemd-run",
                "--user",
                "--wait",
                "--pipe",
                "--collect",
                "--quiet",
                "--setenv",
                "HOME=/home/account",
                "/opt/herdr/bin/herdr",
                "workspace",
                "list",
            ])
        );
        assert!(!runner.calls()[0].iter().any(|arg| arg.starts_with("PATH=")));
    }

    #[test]
    fn server_launch_is_always_outside_and_tolerates_unit_collision() {
        let runner = FakeRunner::with_outputs(vec![
            output(1, "", "not running"),
            output(1, "", "Unit herdr-run-server already exists"),
            output(0, r#"{"server":{"running":true}}"#, ""),
        ]);
        let client = client("direct", runner.clone());
        assert!(client.ensure_server_with(1, Duration::ZERO).unwrap());
        let calls = runner.calls();
        assert_eq!(
            calls[0],
            strings(&["/opt/herdr/bin/herdr", "status", "--json"])
        );
        assert_eq!(calls[1][0], "/usr/bin/systemd-run");
        assert_eq!(calls[1][8], "HOME=/home/account");
        assert_eq!(calls[1][9], "--setenv");
        assert_eq!(calls[1][10], "TOKIO_WORKER_THREADS=16");
        assert_eq!(calls[1][11], "/opt/herdr/bin/herdr");
        assert_eq!(calls[1][12], "server");
        assert!(!calls[1].iter().any(|arg| arg.starts_with("PATH=")));
    }

    #[test]
    fn duplicate_workspace_labels_fail_closed() {
        let runner = FakeRunner::with_outputs(vec![output(
            0,
            r#"{"result":{"workspaces":[{"workspace_id":"w1","label":"x"},{"workspace_id":"w2","label":"x"}]}}"#,
            "",
        )]);
        let error = client("direct", runner)
            .workspace_id_for_label("x")
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn missing_required_protocol_list_fails_closed() {
        let runner = FakeRunner::with_outputs(vec![output(0, r#"{"result":{}}"#, "")]);
        let error = client("direct", runner)
            .workspace_id_for_label("x")
            .unwrap_err();
        assert!(error.to_string().contains("expected an array"));
    }

    #[test]
    fn process_info_refuses_a_response_for_another_pane() {
        let runner = FakeRunner::with_outputs(vec![output(
            0,
            r#"{"result":{"process_info":{"pane_id":"other","shell_pid":7,"foreground_process_group_id":7,"foreground_processes":[]}}}"#,
            "",
        )]);
        let error = client("direct", runner).process_info("wanted").unwrap_err();
        assert!(error.to_string().contains("expected \"wanted\""));
    }

    #[test]
    fn process_info_refuses_out_of_range_process_ids() {
        for field in ["shell_pid", "foreground_process_group_id", "foreground_pid"] {
            for value in [-1_i64, 0, MAX_PROCESS_ID + 1] {
                let mut info = serde_json::json!({
                    "pane_id": "p",
                    "shell_pid": 7,
                    "foreground_process_group_id": 7,
                    "foreground_processes": [{"pid": 7, "name": "sh", "cmdline": "sh"}],
                });
                if field == "foreground_pid" {
                    info["foreground_processes"][0]["pid"] = Value::from(value);
                } else {
                    info[field] = Value::from(value);
                }
                let response = serde_json::json!({"result": {"process_info": info}}).to_string();
                let runner = FakeRunner::with_outputs(vec![output(0, &response, "")]);
                let error = client("direct", runner).process_info("p").unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("outside the positive Linux pid_t range"),
                    "field={field} value={value}: {error}"
                );
            }
        }
    }

    #[test]
    fn process_info_refuses_boolean_process_ids() {
        for field in ["shell_pid", "foreground_process_group_id", "foreground_pid"] {
            let mut info = serde_json::json!({
                "pane_id": "p",
                "shell_pid": 7,
                "foreground_process_group_id": 7,
                "foreground_processes": [{"pid": 7, "name": "sh", "cmdline": "sh"}],
            });
            if field == "foreground_pid" {
                info["foreground_processes"][0]["pid"] = Value::Bool(true);
            } else {
                info[field] = Value::Bool(true);
            }
            let response = serde_json::json!({"result": {"process_info": info}}).to_string();
            let runner = FakeRunner::with_outputs(vec![output(0, &response, "")]);
            let error = client("direct", runner).process_info("p").unwrap_err();
            assert!(
                error.to_string().contains("not an integer"),
                "field={field}: {error}"
            );
        }
    }

    #[test]
    fn embedded_nul_identifier_is_typed_unavailable() {
        struct NulRejectingRunner;

        impl CommandRunner for NulRejectingRunner {
            fn run(
                &self,
                argv: &[String],
                _home: &str,
                _timeout: Duration,
            ) -> io::Result<CommandOutput> {
                assert!(argv.iter().any(|argument| argument.contains('\0')));
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "nul byte found in provided data",
                ))
            }
        }

        let error = client("direct", Arc::new(NulRejectingRunner))
            .read("bad\0pane", "recent-unwrapped", None)
            .unwrap_err();
        assert!(error.to_string().contains("cannot invoke Herdr"));
    }

    #[test]
    fn filtered_pane_list_refuses_another_workspace() {
        let runner = FakeRunner::with_outputs(vec![output(
            0,
            r#"{"result":{"panes":[{"pane_id":"p1","tab_id":"t1","workspace_id":"other"}]}}"#,
            "",
        )]);
        let error = client("direct", runner).panes(Some("wanted")).unwrap_err();
        assert!(error.to_string().contains("expected \"wanted\""));
    }

    #[test]
    fn create_commands_never_steal_focus_and_narrow_ids() {
        let runner = FakeRunner::with_outputs(vec![
            output(
                0,
                r#"{"result":{"workspace":{"workspace_id":"w"},"tab":{"tab_id":"t"},"root_pane":{"pane_id":"p"}}}"#,
                "",
            ),
            output(0, r#"{"result":{"tab":{"tab_id":"t2"}}}"#, ""),
        ]);
        let client = client("direct", runner.clone());
        assert_eq!(
            client.create_workspace("project", "/work").unwrap(),
            ("w".to_owned(), "t".to_owned(), "p".to_owned())
        );
        assert_eq!(client.create_tab("w", "agent", "/work").unwrap(), "t2");
        let calls = runner.calls();
        assert_eq!(calls[0].last().map(String::as_str), Some("--no-focus"));
        assert_eq!(calls[1].last().map(String::as_str), Some("--no-focus"));
    }

    #[test]
    fn malformed_json_and_wrong_integer_types_are_unavailable() {
        let malformed = FakeRunner::with_outputs(vec![output(0, "not-json", "")]);
        let error = client("direct", malformed)
            .workspace_id_for_label("x")
            .unwrap_err();
        assert!(error.to_string().contains("non-JSON"));

        let wrong_type = FakeRunner::with_outputs(vec![output(
            0,
            r#"{"result":{"process_info":{"pane_id":"p","shell_pid":"7","foreground_process_group_id":7,"foreground_processes":[]}}}"#,
            "",
        )]);
        let error = client("direct", wrong_type).process_info("p").unwrap_err();
        assert!(error.to_string().contains("not an integer"));
    }

    #[test]
    fn pane_read_is_plain_text_but_run_success_requires_no_json() {
        let runner =
            FakeRunner::with_outputs(vec![output(0, "$ trailing text\n", ""), output(0, "", "")]);
        let client = client("direct", runner.clone());
        assert_eq!(
            client.read("p", "recent-unwrapped", Some(4)).unwrap(),
            "$ trailing text\n"
        );
        client.run("p", "git status").unwrap();
        assert_eq!(
            runner.calls()[0],
            strings(&[
                "/opt/herdr/bin/herdr",
                "pane",
                "read",
                "p",
                "--source",
                "recent-unwrapped",
                "--lines",
                "4",
            ])
        );
    }
}
