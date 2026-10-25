/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Stopping `hermit analyze` on SIGTERM, SIGINT or SIGHUP.
//!
//! At their default action these signals kill analyze where it stands. The
//! trial running dies with it (its container init has a parent-death
//! signal), but analyze's workspace and the trial's own temporary
//! directories stay behind. While [`StopSignals`] is held, a stop signal
//! instead ends the trial running ([`signal_children`]), a trial whose
//! container init is forked after it exits at once
//! (`container::exit_if_stop_requested`), and every later trial refuses to
//! start ([`check`]), so analyze unwinds, removes what it created, and then
//! dies of the same signal (`AnalyzeOpts::main`).

use std::fmt;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;

use anyhow::Context as _;

use crate::container;

/// The signals a supervisor, a shell or a terminal sends to stop a process.
const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP];

/// The process that installed [`on_stop_signal`]. A process forked from it
/// inherits the handler until it installs its own.
static HOLDER: AtomicI32 = AtomicI32::new(0);

/// How many children analyze may already have when it installs the handler
/// and still pass signals on.
const INHERITED_CAPACITY: usize = 64;

/// The children analyze already had when it installed the handler, 0 in the
/// unused slots. A process keeps its children across `exec`, so these are
/// processes whatever ran analyze started (`job & exec hermit analyze ...`,
/// or the `tee` behind `exec > >(tee log)`), not analyze's to stop. Nothing
/// reaps them, so their PIDs cannot be reused while analyze runs.
static INHERITED: [AtomicI32; INHERITED_CAPACITY] =
    [const { AtomicI32::new(0) }; INHERITED_CAPACITY];

/// A stop signal arrived: analyze starts no further trial and reads no
/// result of the trial it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped(pub reverie::Signal);

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "hermit analyze received {}", self.0)
    }
}

impl std::error::Error for Stopped {}

/// `Err` once a stop signal has arrived.
pub fn check() -> Result<(), Stopped> {
    stopped(container::stop_requested())
}

fn stopped(recorded: Option<libc::c_int>) -> Result<(), Stopped> {
    match recorded {
        None => Ok(()),
        // Only the three stop signals are recorded.
        Some(raw) => Err(Stopped(
            reverie::Signal::try_from(raw).unwrap_or(reverie::Signal::SIGTERM),
        )),
    }
}

/// Analyze's handler for every stop signal.
///
/// In analyze, it records the first signal (`container::request_stop`) and
/// passes every one on to the children analyze started: the container init
/// of the trial running, whose own handler exits 128 + signo and so takes the
/// trial's namespace down. A later signal reaches a trial that started after
/// an earlier one.
///
/// In a process analyze forked that has not installed its own handler yet,
/// which is a trial's container init during Reverie's container setup, it
/// exits 128 + signo, as that init's handler will
/// (`ContainerInitStopSignalBridge` does the same while analyze holds no
/// handler).
extern "C" fn on_stop_signal(signal: libc::c_int) {
    // SAFETY: getpid(2) and _exit(2) are async-signal-safe.
    let pid = unsafe { libc::getpid() };
    if pid != HOLDER.load(Ordering::Relaxed) {
        unsafe { libc::_exit(128 + signal) }
    }
    // SAFETY: errno is this thread's; restoring it keeps the code this
    // handler interrupted from reading a value the handler left.
    let errno = unsafe { *libc::__errno_location() };
    container::request_stop(signal);
    signal_children(pid, signal, inherited);
    unsafe { *libc::__errno_location() = errno };
}

/// Whether `pid` is a child analyze had when it installed the handler.
fn inherited(pid: libc::pid_t) -> bool {
    INHERITED
        .iter()
        .any(|slot| slot.load(Ordering::Relaxed) == pid)
}

/// Holds [`on_stop_signal`] installed; dropping it restores the default
/// action.
#[must_use = "the stop handlers are removed when this guard is dropped"]
pub struct StopSignals {
    /// Which entries of [`STOP_SIGNALS`] this guard installed.
    installed: [bool; STOP_SIGNALS.len()],
    /// The exact handler value installed, compared on drop.
    handler: libc::sighandler_t,
}

impl StopSignals {
    /// Install the handler for every stop signal still at its default
    /// action. One that is ignored (SIGINT in a shell's background job,
    /// SIGHUP under `nohup`) or handled stays as it is, and so does every
    /// signal of a process that is PID 1: the container init of a trial sees
    /// itself as PID 1 too, so the handler could not tell the two apart.
    /// Nothing is installed either when the handler could not find the run's
    /// container init ([`record_inherited_children`]). Such an analyze dies
    /// of the signal, or ignores it, as before.
    ///
    /// Call it while analyze runs no other thread: the query and the install
    /// are two steps.
    pub fn install() -> anyhow::Result<Self> {
        let handler: extern "C" fn(libc::c_int) = on_stop_signal;
        let mut guard = Self {
            installed: [false; STOP_SIGNALS.len()],
            handler: handler as libc::sighandler_t,
        };
        // SAFETY: getpid(2) has no preconditions.
        let pid = unsafe { libc::getpid() };
        if pid == 1 {
            return Ok(guard);
        }
        // Before any disposition changes: a handler that recorded a stop but
        // could not end the run in progress would leave that run going.
        if !record_inherited_children(pid) {
            return Ok(guard);
        }
        HOLDER.store(pid, Ordering::Relaxed);
        // SAFETY: an all-zero sigaction is a valid value, and sigemptyset(3)
        // and sigaddset(3) only write the mask of this local.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = guard.handler;
        // Interrupted system calls resume: the trial's wait ends when its
        // init exits, not with EINTR.
        action.sa_flags = libc::SA_RESTART;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            for signal in STOP_SIGNALS {
                libc::sigaddset(&mut action.sa_mask, signal);
            }
        }
        for (index, signal) in STOP_SIGNALS.into_iter().enumerate() {
            if current_action(signal)?.sa_sigaction != libc::SIG_DFL {
                continue;
            }
            // SAFETY: the handler performs only async-signal-safe calls, and
            // `action` outlives the call.
            if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } == -1 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot install analyze's signal {signal} handler"));
            }
            guard.installed[index] = true;
        }
        Ok(guard)
    }
}

impl Drop for StopSignals {
    fn drop(&mut self) {
        for (index, signal) in STOP_SIGNALS.into_iter().enumerate() {
            // Undo only this guard's own handler: anything else was installed
            // after it on purpose.
            if !self.installed[index]
                || !current_action(signal).is_ok_and(|current| current.sa_sigaction == self.handler)
            {
                continue;
            }
            // SAFETY: an all-zero sigaction is SIG_DFL with no flags.
            let default: libc::sigaction = unsafe { std::mem::zeroed() };
            unsafe { libc::sigaction(signal, &default, std::ptr::null_mut()) };
        }
    }
}

/// Record analyze's present children in [`INHERITED`], and say whether the
/// handler can find the children analyze starts and pass signals on to
/// them: not when `/proc` belongs to another PID namespace (the
/// `/proc/self` link names the reader's PID as that `/proc` numbers it,
/// and `kill` takes PIDs of analyze's own), nor when there are more
/// children than [`INHERITED`] holds.
fn record_inherited_children(pid: libc::pid_t) -> bool {
    let own_proc = std::fs::read_link("/proc/self")
        .ok()
        .and_then(|link| link.to_str()?.parse::<libc::pid_t>().ok())
        == Some(pid);
    if !own_proc {
        return false;
    }
    for slot in &INHERITED {
        slot.store(0, Ordering::Relaxed);
    }
    let mut recorded = 0;
    let mut fits = true;
    for_each_child(pid, |child| match INHERITED.get(recorded) {
        Some(slot) => {
            slot.store(child, Ordering::Relaxed);
            recorded += 1;
        }
        None => fits = false,
    });
    fits
}

/// The action installed for `signal`, read without changing it.
fn current_action(signal: libc::c_int) -> anyhow::Result<libc::sigaction> {
    // SAFETY: a null new action makes sigaction(2) a query, and `current` is
    // read only on success.
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) } == -1 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot read the signal {signal} disposition"));
    }
    Ok(current)
}

/// Send `signal` to every child of `parent` that `skip` does not name;
/// returns how many it reached. Async-signal-safe, as [`for_each_child`].
///
/// Analyze reaps its children on its one thread, which the handler
/// interrupts, so a child found here cannot be reaped, and its PID reused,
/// before the signal is sent.
pub(super) fn signal_children(
    parent: libc::pid_t,
    signal: libc::c_int,
    skip: impl Fn(libc::pid_t) -> bool,
) -> usize {
    let mut reached = 0;
    for_each_child(parent, |child| {
        // SAFETY: kill(2) has no memory arguments.
        if !skip(child) && unsafe { libc::kill(child, signal) } == 0 {
            reached += 1;
        }
    });
    reached
}

/// Call `visit` with the PID of every child of `parent`.
///
/// Reverie gives the trial's container init's PID out only once the trial
/// has ended, so a signal handler finds the init here: the processes whose
/// `/proc/<pid>/stat` names `parent` as their parent (`do_task_stat`,
/// <https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git/tree/fs/proc/array.c?h=v6.9#n534>).
/// `/proc/<pid>/task/<tid>/children` would be shorter, but kernels built
/// without CONFIG_PROC_CHILDREN (which comes with checkpoint/restore) leave it
/// out.
///
/// Async-signal-safe: system calls on stack buffers, no allocation, no lock,
/// no panic.
fn for_each_child(parent: libc::pid_t, mut visit: impl FnMut(libc::pid_t)) {
    // SAFETY: open(2) on a NUL-terminated literal.
    let proc_dir = unsafe {
        libc::open(
            c"/proc".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if proc_dir < 0 {
        return;
    }
    let reclen_at = std::mem::offset_of!(libc::dirent64, d_reclen);
    let name_at = std::mem::offset_of!(libc::dirent64, d_name);
    let mut entries = [0u8; 4096];
    loop {
        // SAFETY: getdents64(2) writes at most `entries.len()` bytes into it.
        let filled = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                proc_dir,
                entries.as_mut_ptr(),
                entries.len(),
            )
        };
        let Some(filled) = usize::try_from(filled).ok().filter(|&filled| filled > 0) else {
            break;
        };
        let mut records = entries.get(..filled).unwrap_or_default();
        while let Some(length) = records
            .get(reclen_at..reclen_at + 2)
            .map(|bytes| usize::from(u16::from_ne_bytes([bytes[0], bytes[1]])))
            .filter(|&length| length > name_at)
        {
            let Some(record) = records.get(..length) else {
                break;
            };
            let name = &record[name_at..];
            let name = &name[..name
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(name.len())];
            if let Some(pid) = parse_pid(name)
                && parent_of(proc_dir, name) == Some(parent)
            {
                visit(pid);
            }
            records = &records[length..];
        }
    }
    // SAFETY: `proc_dir` is the descriptor opened above.
    unsafe { libc::close(proc_dir) };
}

/// The parent of the process named `pid` (its decimal digits) in the open
/// `/proc` directory `proc_dir`.
fn parent_of(proc_dir: libc::c_int, pid: &[u8]) -> Option<libc::pid_t> {
    const STAT: &[u8] = b"/stat\0";
    let mut path = [0u8; 32];
    path.get_mut(..pid.len())?.copy_from_slice(pid);
    path.get_mut(pid.len()..pid.len() + STAT.len())?
        .copy_from_slice(STAT);
    // SAFETY: `path` is NUL-terminated within its length.
    let file = unsafe {
        libc::openat(
            proc_dir,
            path.as_ptr().cast(),
            libc::O_RDONLY | libc::O_CLOEXEC,
        )
    };
    if file < 0 {
        return None;
    }
    let mut stat = [0u8; 512];
    // SAFETY: read(2) writes at most `stat.len()` bytes into it, and `file`
    // is the descriptor opened above.
    let read = unsafe { libc::read(file, stat.as_mut_ptr().cast(), stat.len()) };
    unsafe { libc::close(file) };
    stat_parent(stat.get(..usize::try_from(read).ok()?)?)
}

/// The parent PID in a `/proc/<pid>/stat` line: the field after the state,
/// which follows the command name. The name is in parentheses and may itself
/// hold spaces and parentheses, so the fields are counted from its last
/// closing one.
fn stat_parent(stat: &[u8]) -> Option<libc::pid_t> {
    let name_end = stat.iter().rposition(|&byte| byte == b')')?;
    let mut fields = stat[name_end + 1..]
        .split(|&byte| byte == b' ')
        .filter(|field| !field.is_empty());
    fields.next()?;
    parse_pid(fields.next()?)
}

/// A PID written in decimal digits, and nothing else.
fn parse_pid(digits: &[u8]) -> Option<libc::pid_t> {
    if digits.is_empty() {
        return None;
    }
    let zero: libc::pid_t = 0;
    digits.iter().try_fold(zero, |pid, &digit| {
        if digit.is_ascii_digit() {
            pid.checked_mul(10)?
                .checked_add(libc::pid_t::from(digit - b'0'))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt as _;
    use std::process::Command;
    use std::time::Duration;
    use std::time::Instant;

    use super::*;

    /// The parent field is read after the command name's last closing
    /// parenthesis, whatever the name holds.
    #[test]
    fn the_parent_is_the_field_after_the_state() {
        assert_eq!(stat_parent(b"12 (sh) S 1 12 12 0 -1"), Some(1));
        assert_eq!(stat_parent(b"345 (a) b (c)) R 77 345"), Some(77));
        assert_eq!(stat_parent(b"345 (a) b (c)) R"), None);
        assert_eq!(stat_parent(b"345 (no close S 77"), None);
        assert_eq!(parse_pid(b"self"), None);
        assert_eq!(parse_pid(b""), None);
        assert_eq!(parse_pid(b"99999999999"), None);
    }

    /// A stop reads as unset until one is recorded, and then as that signal.
    #[test]
    fn the_first_stop_signal_is_the_one_reported() {
        assert_eq!(stopped(None), Ok(()));
        assert_eq!(
            stopped(Some(libc::SIGHUP)),
            Err(Stopped(reverie::Signal::SIGHUP))
        );
        assert_eq!(
            Stopped(reverie::Signal::SIGINT).to_string(),
            "hermit analyze received SIGINT"
        );
    }

    /// The children of a process are found without
    /// `/proc/<pid>/task/<tid>/children`, and only they are signalled, unless
    /// skipped: here a shell's `sleep`, so the shell's `wait` returns
    /// 128 + SIGTERM. A process without children reaches none.
    #[test]
    fn a_stop_signal_reaches_the_children_and_only_them() {
        let mut childless = Command::new("sleep").arg("60").spawn().unwrap();
        assert_eq!(
            signal_children(childless.id() as libc::pid_t, libc::SIGTERM, |_| false),
            0
        );
        childless.kill().unwrap();
        childless.wait().unwrap();

        let mut shell = Command::new("sh")
            .args(["-c", "sleep 60 & wait $!"])
            .spawn()
            .unwrap();
        let parent = shell.id() as libc::pid_t;
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut children = Vec::new();
        while children.is_empty() {
            assert!(Instant::now() < deadline, "the shell never started sleep");
            std::thread::sleep(Duration::from_millis(10));
            for_each_child(parent, |child| children.push(child));
        }
        assert_eq!(
            signal_children(parent, libc::SIGTERM, |child| children.contains(&child)),
            0
        );
        assert_eq!(signal_children(parent, libc::SIGTERM, |_| false), 1);
        let status = shell.wait().unwrap();
        assert_eq!(status.code(), Some(128 + libc::SIGTERM), "{status:?}");
    }

    /// The handler itself, in a child process of its own (this test binary,
    /// running this test alone), so that the dispositions it changes cannot
    /// reach the test harness, and no other test's thread holds a lock when
    /// it forks. With more inherited children than it can record, nothing is
    /// installed. Otherwise it takes SIGTERM and SIGHUP but leaves an ignored
    /// SIGINT ignored. The first signal is the one recorded, and each is
    /// passed on to a child started after the install but not to one the
    /// process already had (as a shell's job is after `exec hermit
    /// analyze`). A process forked afterwards exits 128 + SIGTERM when it
    /// arms the container-init guards, as a trial's container init does
    /// before it starts its guest. Dropped, it restores the default actions
    /// it replaced.
    #[test]
    fn the_handler_stops_what_analyze_started_and_nothing_else() {
        const CHILD: &str = "HERMIT_ANALYZE_STOP_HANDLER_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let name = format!(
                "{}::the_handler_stops_what_analyze_started_and_nothing_else",
                module_path!().split_once("::").unwrap().1
            );
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child ran no test: {output:?}"
            );
            return;
        }
        let action = |signal| current_action(signal).unwrap().sa_sigaction;
        // SAFETY: SIG_IGN installs no handler.
        unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
        let mut crowd: Vec<_> = (0..=INHERITED_CAPACITY)
            .map(|_| Command::new("sleep").arg("60").spawn().unwrap())
            .collect();
        let unarmed = StopSignals::install().unwrap();
        assert_eq!(unarmed.installed, [false; STOP_SIGNALS.len()]);
        assert_eq!(action(libc::SIGTERM), libc::SIG_DFL);
        assert_eq!(action(libc::SIGHUP), libc::SIG_DFL);
        drop(unarmed);
        for mut child in crowd.drain(..) {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        let mut before = Command::new("sleep").arg("60").spawn().unwrap();
        let guard = StopSignals::install().unwrap();
        assert_eq!(action(libc::SIGINT), libc::SIG_IGN);
        assert_eq!(action(libc::SIGTERM), guard.handler);
        assert_eq!(action(libc::SIGHUP), guard.handler);
        let mut after = Command::new("sleep").arg("60").spawn().unwrap();
        // SAFETY: raise(3) runs the handler on this thread before it returns.
        unsafe {
            libc::raise(libc::SIGTERM);
            libc::raise(libc::SIGHUP);
        }
        let status = after.wait().unwrap();
        let left_alone = before.try_wait().unwrap();
        let _ = before.kill();
        let _ = before.wait();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status:?}");
        assert_eq!(left_alone, None, "the child the process had before");
        assert_eq!(check(), Err(Stopped(reverie::Signal::SIGTERM)));
        // SAFETY: the harness's other thread waits for this test and holds
        // no lock; the child calls only async-signal-safe functions.
        let forked = unsafe { libc::fork() };
        if forked == 0 {
            let _ = container::arm_container_init_guards();
            unsafe { libc::_exit(0) }
        }
        let mut raw = 0;
        // SAFETY: `raw` outlives the call; `forked` is this process's child.
        assert_eq!(unsafe { libc::waitpid(forked, &mut raw, 0) }, forked);
        assert!(
            libc::WIFEXITED(raw) && libc::WEXITSTATUS(raw) == 128 + libc::SIGTERM,
            "the process forked after the stop: status {raw:#x}"
        );
        drop(guard);
        assert_eq!(action(libc::SIGTERM), libc::SIG_DFL);
        assert_eq!(action(libc::SIGHUP), libc::SIG_DFL);
        assert_eq!(action(libc::SIGINT), libc::SIG_IGN);
    }
}
