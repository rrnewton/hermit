/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Running a unit test in a process of its own.

use std::process::Command;
use std::process::Stdio;

/// Set in the copy of the test binary that [`rerun_alone`] starts.
const COPY: &str = "HERMIT_TEST_RERUN_ALONE";

/// Whether this process is a copy that [`rerun_alone`] started.
fn in_copy() -> bool {
    std::env::var_os(COPY).is_some()
}

/// Runs the calling test again, alone, in a fresh copy of this test binary.
///
/// Returns true in the calling process once the copy has run the test and
/// passed; the test then has nothing more to do. Returns false in the copy,
/// which goes on to run the test's body.
///
/// Every test that clones a container through `reverie::process::Container`
/// (directly or through Hermit's launch path) calls this first. That clone is
/// a raw `clone(2)`, which runs no `pthread_atfork` handler, so the child
/// starts with every lock that any other thread held at that instant, glibc's
/// malloc arena locks among them, and with no thread left to release them:
/// its next `malloc` or `fork` waits forever. `Container::run` therefore
/// requires a process with no other threads. Plain `cargo test` runs tests as
/// threads of one process, where such a test hung whenever a test beside it
/// held a malloc lock at the wrong moment, and its stuck child kept other
/// tests' pipes open, which hung them too. The copy holds only this test and
/// libtest's main thread, which waits for it: the arrangement nextest gives
/// every test.
///
/// A test that sets per-process state that can be set only once, such as
/// the skid margin of Reverie's PMU configuration, calls it too. Beside other
/// tests, whichever of them set the state first decided what the rest saw.
///
/// The test is found by its thread's name, which libtest sets to the test's
/// path, so this must be called on the test's own thread.
pub(crate) fn rerun_alone() -> bool {
    if in_copy() {
        return false;
    }
    let current = std::thread::current();
    let name = current
        .name()
        .expect("libtest names a test's thread after the test");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(COPY, "1")
        .stdin(Stdio::null())
        .output()
        .expect("start a copy of the test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{name} failed, or did not run, in a process of its own ({}):\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io;
    use std::io::Read;
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;
    use std::os::fd::OwnedFd;
    use std::sync::mpsc;
    use std::time::Duration;
    use std::time::Instant;

    use reverie::process::Container;

    use super::*;

    /// How long a forking container child may take before it counts as stuck.
    /// One that can fork finishes within milliseconds.
    const FORK_BUDGET: Duration = Duration::from_secs(10);

    /// A thread blocked inside glibc's `malloc_stats` while it holds the main
    /// malloc arena lock: `malloc_stats` writes to standard error under that
    /// lock, and standard error is a full pipe that nobody reads. Dropping it
    /// drains the pipe, which lets the thread finish and release the lock,
    /// and restores standard error.
    struct MallocLockHolder {
        reader: File,
        saved_stderr: OwnedFd,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MallocLockHolder {
        fn hold() -> Self {
            let (reader, writer) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).unwrap();
            let writer = File::from(writer);
            set_nonblocking(&writer, true);
            while nix::unistd::write(&writer, &[0; 4096]).is_ok() {}
            set_nonblocking(&writer, false);
            let saved_stderr = io::stderr().as_fd().try_clone_to_owned().unwrap();
            // SAFETY: both descriptors are open; standard error is replaced,
            // not closed, and restored on drop.
            assert_ne!(unsafe { libc::dup2(writer.as_raw_fd(), 2) }, -1);
            drop(writer);
            let (tid_sender, tid) = mpsc::channel();
            let thread = std::thread::spawn(move || {
                tid_sender.send(nix::unistd::gettid()).unwrap();
                // SAFETY: malloc_stats takes no arguments.
                unsafe { libc::malloc_stats() };
            });
            let tid = tid.recv().unwrap();
            // `write(2, ...)` from inside malloc_stats, with the lock held.
            let deadline = Instant::now() + FORK_BUDGET;
            while !std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall"))
                .is_ok_and(|syscall| syscall.starts_with("1 0x2 "))
            {
                assert!(Instant::now() < deadline, "malloc_stats never blocked");
                std::thread::sleep(Duration::from_millis(1));
            }
            Self {
                reader: File::from(reader),
                saved_stderr,
                thread: Some(thread),
            }
        }
    }

    impl Drop for MallocLockHolder {
        fn drop(&mut self) {
            set_nonblocking(&self.reader, true);
            let thread = self.thread.take().unwrap();
            let mut buffer = [0; 4096];
            while !thread.is_finished() {
                if let Err(error) = self.reader.read(&mut buffer) {
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{error}");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            thread.join().unwrap();
            // SAFETY: both descriptors are open.
            unsafe { libc::dup2(self.saved_stderr.as_raw_fd(), 2) };
        }
    }

    fn set_nonblocking(file: &File, nonblocking: bool) {
        use nix::fcntl::FcntlArg;
        use nix::fcntl::OFlag;
        let mut flags =
            OFlag::from_bits_truncate(nix::fcntl::fcntl(file, FcntlArg::F_GETFL).unwrap());
        flags.set(OFlag::O_NONBLOCK, nonblocking);
        nix::fcntl::fcntl(file, FcntlArg::F_SETFL(flags)).unwrap();
    }

    /// This process's children, from the parent field of every process's
    /// `/proc/<pid>/stat`.
    fn children() -> Vec<nix::unistd::Pid> {
        let this = nix::unistd::getpid().as_raw();
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                    // The parent PID is the second field after the
                    // parenthesized command name, which may contain spaces.
                    stat.rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse().ok())
                        == Some(this)
                })
            })
            .map(nix::unistd::Pid::from_raw)
            .collect()
    }

    /// Clones a container whose child forks, and fails, after killing it,
    /// if the child cannot fork within the budget.
    fn assert_a_container_child_can_fork() {
        let (done, finished) = mpsc::channel();
        let cloner = std::thread::spawn(move || {
            let forked = Container::new().run(|| {
                // SAFETY: the child only exits.
                match unsafe { libc::fork() } {
                    0 => unsafe { libc::_exit(0) },
                    -1 => false,
                    child => {
                        nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(child), None).is_ok()
                    }
                }
            });
            done.send(()).unwrap();
            forked
        });
        let stuck = finished.recv_timeout(FORK_BUDGET).is_err();
        if stuck {
            // The stuck container child is this process's only child.
            for child in children() {
                let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
            }
        }
        let forked = cloner.join().unwrap();
        assert!(
            !stuck,
            "the container child could not fork within {FORK_BUDGET:?}: a lock another \
             thread held when the container was cloned reached it"
        );
        assert!(matches!(forked, Ok(true)), "{forked:?}");
    }

    /// A container cloned by a test that `rerun_alone` moved into a process
    /// of its own forks even while another test, in the process the test was
    /// started in, holds glibc's main malloc arena lock: the lock cannot
    /// reach that process. A container cloned in the holder's own process
    /// instead starts with the lock held and no owner to release it, and its
    /// `fork` waits forever. That is how `cargo test` hung, and why
    /// serializing only the container tests would not help: any test that
    /// allocates can be the one holding the lock.
    #[test]
    fn a_test_rerun_alone_is_out_of_reach_of_a_malloc_lock_another_test_holds() {
        const HOLDER: &str = "HERMIT_TEST_MALLOC_LOCK_HOLDER";
        if std::env::var_os(HOLDER).is_none() {
            // Standard error is about to be taken over, which only a process
            // running nothing else can afford.
            let name = std::thread::current().name().unwrap().to_owned();
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--test-threads=1"])
                .env(HOLDER, "1")
                .stdin(Stdio::null())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "the lock-holding process failed or ran no test ({}):\n{stdout}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _held = (!in_copy()).then(MallocLockHolder::hold);
        if rerun_alone() {
            return;
        }
        assert_a_container_child_can_fork();
    }
}
