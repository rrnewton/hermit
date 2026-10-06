/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Observes guest processes' physical exits through pidfds, for backends whose
//! processes leave the host some time after their scheduler-granted exit.
//!
//! Detcore holds only abstract lifetime state; how a process's exit is observed
//! on the host belongs to the backend (owner directive 082). A backend hands
//! this watcher one pidfd per guest process (from `pidfd_open`, or a socket's
//! `SO_PEERPIDFD`) together with the process id Detcore knows it by. The
//! watcher calls the backend's callback with that id exactly once, after the
//! exit is complete as the guest's parent can see it; a backend wires the
//! callback to `GlobalState::complete_physical_process_exit`.
//!
//! The pidfd alone is not enough. `exit_notify` runs under
//! `write_lock_irq(&tasklist_lock)`: it sets the zombie state, then
//! `do_notify_parent` wakes pidfd waiters and only then queues SIGCHLD to the
//! parent. So when the pidfd becomes readable, the watcher calls
//! `waitid(P_PIDFD, pidfd, WEXITED | WNOHANG | WNOWAIT)`. While the process is
//! unreaped, the kernel takes `read_lock(&tasklist_lock)` for that call
//! (`__do_wait`), which cannot be granted until `exit_notify`'s write section,
//! and with it the SIGCHLD queueing, has finished. A non-parent gets `ECHILD`
//! and nothing is consumed; `WNOWAIT` leaves a parent's zombie in place. If
//! the process was already reaped, `release_task` ran after that section
//! anyway. Either way, a parent probing its pending signals or children sees
//! the exit in every run once it is reported.
//!
//! The watcher never waits on or reaps a process: the guest's own parent, or
//! the backend's launcher for the root, stays the only reaper.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;
use std::time::Instant;

/// Why [`PhysicalExitWatch::drain`] returned without every watched process gone.
#[derive(Debug)]
pub enum DrainError {
    /// Some watched processes had not exited when the timeout expired; their ids.
    TimedOut(Vec<i32>),
    /// The watcher thread failed, so exits are no longer observed.
    Watcher(io::Error),
}

struct Entry {
    pidfd: OwnedFd,
    raw_pid: i32,
}

/// Returns once `entry`'s exit has been published to its parent; see the
/// module documentation for why this `waitid` call is the synchronization.
fn await_publication(entry: &Entry) -> io::Result<()> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        let rc = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                entry.pidfd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // Not our child, or already reaped: both after publication.
            Some(libc::ECHILD) => return Ok(()),
            Some(libc::EINTR) => continue,
            _ => {
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "waitid on the pidfd of exited process {}: {error}",
                        entry.raw_pid
                    ),
                ));
            }
        }
    }
}

#[derive(Default)]
struct State {
    /// Processes whose exit has not been observed yet.
    live: Vec<Entry>,
    /// Processes whose exit was observed but whose callback has not returned.
    delivering: Vec<i32>,
    /// Set by `Drop`; the thread exits at its next wakeup.
    shutdown: bool,
    /// The watcher thread's terminal error, if it stopped observing.
    failure: Option<io::Error>,
}

struct Shared {
    state: Mutex<State>,
    /// Notified whenever a callback returns or the watcher fails.
    changed: Condvar,
    /// Wakes the watcher thread's poll after `live` grows or on shutdown.
    wake: OwnedFd,
}

impl Shared {
    fn wake_thread(&self) {
        let one: u64 = 1;
        // An eventfd write only fails if the counter would overflow, which
        // still leaves the descriptor readable, so the wakeup is not lost.
        let _ = unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                std::mem::size_of::<u64>(),
            )
        };
    }
}

/// One watcher thread over a set of pidfds. See the module documentation.
pub struct PhysicalExitWatch {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl PhysicalExitWatch {
    /// Starts the watcher thread. `on_exit` is called on that thread, once per
    /// watched process, with the id given to [`PhysicalExitWatch::watch`].
    pub fn new(on_exit: impl Fn(i32) + Send + 'static) -> io::Result<Self> {
        let wake = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake < 0 {
            return Err(io::Error::last_os_error());
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            wake: unsafe { OwnedFd::from_raw_fd(wake) },
        });
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("physical-exit-watch".to_string())
            .spawn(move || {
                // A panicking callback must not lose reports silently: record
                // it as the watcher's terminal failure, which `drain` returns
                // and `watch` refuses on.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    watch_loop(&thread_shared, &on_exit)
                }));
                let failure = match outcome {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(error),
                    Err(_) => Some(io::Error::other("the physical-exit callback panicked")),
                };
                if let Some(error) = failure {
                    let mut state = thread_shared.state.lock().unwrap();
                    state.failure = Some(error);
                    drop(state);
                    thread_shared.changed.notify_all();
                }
            })?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Watches one process. `pidfd` must refer to it; `raw_pid` is the id the
    /// callback reports. A process that has already exited is reported at once.
    pub fn watch(&self, pidfd: OwnedFd, raw_pid: i32) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if let Some(failure) = &state.failure {
            return Err(io::Error::new(failure.kind(), failure.to_string()));
        }
        state.live.push(Entry { pidfd, raw_pid });
        drop(state);
        self.shared.wake_thread();
        Ok(())
    }

    /// Sends `SIGKILL` to every watched process that has not exited yet. Used
    /// when a run has failed, so that no guest outlives it; the exits that
    /// follow are reported as usual. `SIGKILL` also ends a stopped process.
    /// Every process is attempted; the first error other than `ESRCH` (the
    /// process already exited, so its report is on the way) is returned.
    pub fn fail_and_kill_all(&self) -> io::Result<()> {
        let state = self.shared.state.lock().unwrap();
        let mut first_error = None;
        for entry in &state.live {
            let rc = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    entry.pidfd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            if rc < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) && first_error.is_none() {
                    first_error = Some(io::Error::new(
                        error.kind(),
                        format!("SIGKILL to watched process {}: {error}", entry.raw_pid),
                    ));
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Waits until every watched process has exited and its callback has
    /// returned, or until `timeout` expires.
    pub fn drain(&self, timeout: Duration) -> Result<(), DrainError> {
        // An unrepresentable deadline (for example Duration::MAX) waits forever.
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some(failure) = &state.failure {
                return Err(DrainError::Watcher(io::Error::new(
                    failure.kind(),
                    failure.to_string(),
                )));
            }
            if state.live.is_empty() && state.delivering.is_empty() {
                return Ok(());
            }
            let Some(deadline) = deadline else {
                state = self.shared.changed.wait(state).unwrap();
                continue;
            };
            let now = Instant::now();
            if now >= deadline {
                let mut pending: Vec<i32> = state.live.iter().map(|entry| entry.raw_pid).collect();
                pending.extend(&state.delivering);
                return Err(DrainError::TimedOut(pending));
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap()
                .0;
        }
    }
}

impl Drop for PhysicalExitWatch {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().shutdown = true;
        self.shared.wake_thread();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch_loop(shared: &Shared, on_exit: &impl Fn(i32)) -> io::Result<()> {
    loop {
        // Snapshot the descriptors to poll. Entries are only removed by this
        // thread, so each index stays valid until the removal below.
        let mut fds = vec![libc::pollfd {
            fd: shared.wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        {
            let state = shared.state.lock().unwrap();
            if state.shutdown {
                return Ok(());
            }
            fds.extend(state.live.iter().map(|entry| libc::pollfd {
                fd: entry.pidfd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }));
        }
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[0].revents != 0 {
            let mut counter: u64 = 0;
            let _ = unsafe {
                libc::read(
                    shared.wake.as_raw_fd(),
                    &mut counter as *mut u64 as *mut libc::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
        }
        let exited: Vec<libc::c_int> = fds[1..]
            .iter()
            .filter(|fd| fd.revents != 0)
            .map(|fd| fd.fd)
            .collect();
        if exited.is_empty() {
            continue;
        }
        // POLLIN is the exit; POLLHUP/POLLERR/POLLNVAL on a pidfd also mean
        // the process can no longer be observed alive, so all end the entry.
        let reported: Vec<Entry> = {
            let mut state = shared.state.lock().unwrap();
            let (reported, live): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut state.live)
                .into_iter()
                .partition(|entry| exited.contains(&entry.pidfd.as_raw_fd()));
            state.live = live;
            state
                .delivering
                .extend(reported.iter().map(|entry| entry.raw_pid));
            reported
        };
        // Report outside the lock: the callback takes the scheduler's lock.
        // A process stays in `delivering` until its callback has returned, so
        // `drain` cannot succeed before the report has been delivered.
        for entry in reported {
            await_publication(&entry)?;
            let raw_pid = entry.raw_pid;
            on_exit(raw_pid);
            let mut state = shared.state.lock().unwrap();
            if let Some(index) = state.delivering.iter().position(|&pid| pid == raw_pid) {
                state.delivering.remove(index);
            }
            drop(state);
            shared.changed.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Child;
    use std::process::Command;
    use std::sync::mpsc;

    use super::*;

    fn pidfd_open(pid: u32) -> OwnedFd {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
        assert!(fd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        unsafe { OwnedFd::from_raw_fd(fd as i32) }
    }

    /// Kills and reaps its child however the test ends. `Child::kill` does not
    /// signal a child that was already reaped, so a reused pid is never hit.
    struct Reaped(Child);

    impl Reaped {
        fn spawn(command: &mut Command) -> Self {
            Self(command.spawn().unwrap())
        }
        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// The same guard for a raw `fork` child; `reap` disarms it.
    struct ForkedChild(Option<libc::pid_t>);

    impl ForkedChild {
        fn reap(&mut self) -> libc::c_int {
            let pid = self.0.take().unwrap();
            let mut status = 0;
            assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
            status
        }
    }

    impl Drop for ForkedChild {
        fn drop(&mut self) {
            if let Some(pid) = self.0 {
                let mut status = 0;
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, &mut status, 0);
                }
            }
        }
    }

    #[test]
    fn exiting_child_is_reported_once_with_its_pid() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut child = Reaped::spawn(Command::new("sh").args(["-c", "exit 3"]));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        watch.drain(Duration::from_secs(30)).unwrap();
        // The watcher never reaps: the exit status is still the parent's to collect.
        assert_eq!(child.0.wait().unwrap().code(), Some(3));
        drop(watch);
        assert!(
            receiver.try_recv().is_err(),
            "the exit was reported more than once"
        );
    }

    #[test]
    fn drain_honours_its_timeout() {
        let watch = PhysicalExitWatch::new(|_| {}).unwrap();
        let mut child = Reaped::spawn(Command::new("sleep").arg("60"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        let start = Instant::now();
        match watch.drain(Duration::from_millis(200)) {
            Err(DrainError::TimedOut(live)) => assert_eq!(live, [pid as i32]),
            other => panic!("expected a timeout, got {other:?}"),
        }
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(200),
            "returned early: {waited:?}"
        );
        assert!(waited < Duration::from_secs(20), "overran: {waited:?}");
        child.0.kill().unwrap();
        assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
    }

    #[test]
    fn drain_without_processes_accepts_an_unbounded_timeout() {
        let watch = PhysicalExitWatch::new(|_| {}).unwrap();
        watch.drain(Duration::MAX).unwrap();
    }

    #[test]
    fn drain_waits_for_the_callback_to_return() {
        // The process has exited, but the report has not been delivered until
        // the callback returns; drain must not succeed in between.
        let (started_sender, started) = mpsc::channel();
        let (release, release_receiver) = mpsc::channel::<()>();
        let watch = PhysicalExitWatch::new(move |pid| {
            started_sender.send(pid).unwrap();
            let _ = release_receiver.recv();
        })
        .unwrap();
        // Rebound after `watch`, so it is dropped first if an assertion below
        // fails: the paused callback then returns and `watch` can join.
        let release = release;
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        assert_eq!(
            started.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        match watch.drain(Duration::from_millis(200)) {
            Err(DrainError::TimedOut(pending)) => assert_eq!(pending, [pid as i32]),
            other => panic!("drain returned before the callback did: {other:?}"),
        }
        release.send(()).unwrap();
        watch.drain(Duration::from_secs(30)).unwrap();
        child.0.wait().unwrap();
    }

    #[test]
    fn a_panicking_callback_fails_the_watcher() {
        let watch = PhysicalExitWatch::new(|_| panic!("callback failure under test")).unwrap();
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        watch.watch(pidfd_open(pid), pid as i32).unwrap();
        match watch.drain(Duration::from_secs(30)) {
            Err(DrainError::Watcher(_)) => {}
            other => panic!("expected the watcher's failure, got {other:?}"),
        }
        child.0.wait().unwrap();
        let other = Reaped::spawn(Command::new("sleep").arg("60"));
        assert!(
            watch
                .watch(pidfd_open(other.pid()), other.pid() as i32)
                .is_err(),
            "a failed watcher accepted a new process"
        );
    }

    #[test]
    fn kill_all_ends_every_watched_process() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut children: Vec<Reaped> = (0..3)
            .map(|_| Reaped::spawn(Command::new("sleep").arg("60")))
            .collect();
        // A stopped process is ended too: stop one and wait until it is stopped.
        let stopped = children[0].pid();
        assert_eq!(
            unsafe { libc::kill(stopped as libc::pid_t, libc::SIGSTOP) },
            0
        );
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                stopped,
                &mut info,
                libc::WSTOPPED | libc::WNOWAIT,
            )
        };
        assert_eq!(rc, 0, "waitid: {}", io::Error::last_os_error());
        assert_eq!(info.si_code, libc::CLD_STOPPED);
        for child in &children {
            watch
                .watch(pidfd_open(child.pid()), child.pid() as i32)
                .unwrap();
        }
        watch.fail_and_kill_all().unwrap();
        watch.drain(Duration::from_secs(30)).unwrap();
        let mut reported: Vec<i32> = receiver.try_iter().collect();
        reported.sort_unstable();
        let mut expected: Vec<i32> = children.iter().map(|child| child.pid() as i32).collect();
        expected.sort_unstable();
        assert_eq!(reported, expected);
        for child in &mut children {
            assert_eq!(child.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        }
    }

    #[test]
    fn already_exited_process_is_reported_at_once() {
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        let mut child = Reaped::spawn(&mut Command::new("true"));
        let pid = child.pid();
        let pidfd = pidfd_open(pid);
        // Wait for the exit without reaping, so the pidfd is readable before it is watched.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc =
            unsafe { libc::waitid(libc::P_PID, pid, &mut info, libc::WEXITED | libc::WNOWAIT) };
        assert_eq!(rc, 0, "waitid: {}", io::Error::last_os_error());
        watch.watch(pidfd, pid as i32).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(30)).unwrap(),
            pid as i32
        );
        child.0.wait().unwrap();
    }

    /// Set in a test's own fresh process; see [`run_in_fresh_process`].
    const INNER_RUN: &str = "HERMIT_PHYSICAL_EXIT_WATCH_INNER_RUN";

    /// Re-runs the test `name` alone in a fresh process of this test binary and
    /// requires it to pass. Used where the property needs a process no other
    /// test shares: under a threaded harness another test can fork a child
    /// that inherits this test's descriptors, or a harness thread can take a
    /// signal. With `block_sigchld`, the fresh process starts with SIGCHLD
    /// blocked in every thread (a blocked mask survives exec, and `pre_exec`
    /// runs after std resets the child's mask).
    fn run_in_fresh_process(name: &str, block_sigchld: bool) {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(INNER_RUN, "1");
        if block_sigchld {
            unsafe {
                command.pre_exec(|| {
                    let set = sigchld_set();
                    if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "inner run failed:\n{stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("1 passed"),
            "inner run did not execute the test:\n{stdout}\n{stderr}"
        );
    }

    fn sigchld_set() -> libc::sigset_t {
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGCHLD);
        }
        set
    }

    #[test]
    fn sigchld_is_queued_before_the_exit_is_reported() {
        // The kernel wakes pidfd waiters before it queues SIGCHLD to the
        // parent. The callback must find SIGCHLD already pending, observed
        // without any call that takes tasklist_lock itself. That needs every
        // thread of the process to block SIGCHLD, so nothing can dequeue it,
        // which this test process cannot arrange for its harness threads. So
        // the test re-runs itself in a fresh process whose initial mask blocks
        // SIGCHLD (a blocked mask survives exec, and pre_exec runs after std
        // resets the child's mask).
        if std::env::var_os(INNER_RUN).is_some() {
            return sigchld_inner();
        }
        run_in_fresh_process(
            "physical_exit_watch::tests::sigchld_is_queued_before_the_exit_is_reported",
            true,
        );
    }

    fn sigchld_inner() {
        let chld = sigchld_set();
        let mut mask: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut mask) };
        assert_eq!(
            unsafe { libc::sigismember(&mask, libc::SIGCHLD) },
            1,
            "the inner process must start with SIGCHLD blocked"
        );
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| {
            let mut pending: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe { libc::sigpending(&mut pending) };
            let queued = unsafe { libc::sigismember(&pending, libc::SIGCHLD) } == 1;
            sender.send((pid, queued)).unwrap();
        })
        .unwrap();
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        for _ in 0..50 {
            let mut child = Reaped::spawn(&mut Command::new("true"));
            let pid = child.pid();
            watch.watch(pidfd_open(pid), pid as i32).unwrap();
            let (reported, queued) = receiver.recv_timeout(Duration::from_secs(30)).unwrap();
            assert_eq!(reported, pid as i32);
            assert!(queued, "exit of {pid} reported before SIGCHLD was queued");
            child.0.wait().unwrap();
            // Consume this child's SIGCHLD before the next child.
            let signal = unsafe { libc::sigtimedwait(&chld, std::ptr::null_mut(), &zero) };
            assert_eq!(signal, libc::SIGCHLD);
        }
    }

    #[test]
    fn descriptors_are_closed_before_the_pidfd_is_readable() {
        // The property the scheduler relies on: once the exit is reported, a
        // pipe whose only writer was the exited process reads EOF. Run in a
        // fresh process, so no other test's fork can inherit the writer.
        if std::env::var_os(INNER_RUN).is_none() {
            return run_in_fresh_process(
                "physical_exit_watch::tests::descriptors_are_closed_before_the_pidfd_is_readable",
                false,
            );
        }
        let mut pipe = [0; 2];
        assert_eq!(
            unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read_end = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
        let write_end = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // Child: only async-signal-safe calls. Keep the write end, exit.
            unsafe {
                libc::close(read_end.as_raw_fd());
                libc::_exit(0)
            };
        }
        let mut child = ForkedChild(Some(pid));
        drop(write_end);
        let (sender, receiver) = mpsc::channel();
        let watch = PhysicalExitWatch::new(move |pid| sender.send(pid).unwrap()).unwrap();
        watch.watch(pidfd_open(pid as u32), pid).unwrap();
        assert_eq!(receiver.recv_timeout(Duration::from_secs(30)).unwrap(), pid);
        let mut byte = 0u8;
        let flags = unsafe { libc::fcntl(read_end.as_raw_fd(), libc::F_GETFL) };
        unsafe {
            libc::fcntl(
                read_end.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        };
        let n = unsafe {
            libc::read(
                read_end.as_raw_fd(),
                &mut byte as *mut u8 as *mut libc::c_void,
                1,
            )
        };
        assert_eq!(
            n,
            0,
            "expected EOF, got {n} ({})",
            io::Error::last_os_error()
        );
        child.reap();
    }
}
