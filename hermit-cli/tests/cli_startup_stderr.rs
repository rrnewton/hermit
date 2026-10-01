/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Official owned-wrapper test only. No provider, service or guest is admitted.
use std::fs::File;
use std::io::Read;
use std::io::{self};
use std::mem::ManuallyDrop;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::process::ChildStderr;
use std::process::ChildStdout;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

#[path = "../../ci/cli-refusal-test-boundary.rs"]
mod boundary;

const ADAPTER: &[u8] = hermit_cli_refusal_fixture::EXECUTABLE;
const CAPTURE_LIMIT: usize = 65536;

#[derive(Default, Debug, Clone, Copy)]
struct Completion {
    terminal: bool,
    code: Option<i32>,
    stdout_eof: bool,
    stderr_eof: bool,
    forced: bool,
}
impl Completion {
    fn concluded(self) -> bool {
        self.terminal && self.stdout_eof && self.stderr_eof
    }
    fn natural_125(self) -> bool {
        self.concluded() && self.code == Some(125) && !self.forced
    }
}

fn witness(mode: &str, output: &[u8]) -> bool {
    if mode == "normal" {
        return output == b"startup-stderr-fixture mode=normal capacity=0 queued=0\n";
    }
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    let Some(values) = text.strip_prefix("startup-stderr-fixture mode=full capacity=") else {
        return false;
    };
    let Some((capacity, queued)) = values.split_once(" queued=") else {
        return false;
    };
    let Ok(capacity) = capacity.parse::<usize>() else {
        return false;
    };
    mode == "full"
        && capacity > 0
        && capacity <= 1024 * 1024
        && queued == format!("{capacity}\n")
        && text
            == format!("startup-stderr-fixture mode=full capacity={capacity} queued={capacity}\n")
}

fn official_cli(mode: &str) -> (PathBuf, File) {
    assert_eq!(
        std::env::var(boundary::CASE_ENV).as_deref(),
        Ok(mode),
        "official CLI case owner absent"
    );
    let fd: i32 = std::env::var(boundary::FD_ENV)
        .expect("official owned cgroup absent")
        .parse()
        .unwrap();
    assert!(fd >= 3);
    // The wrapper transfers this actual directory capability exactly once.
    let directory = unsafe { File::from_raw_fd(fd) };
    let meta = directory.metadata().unwrap();
    assert!(meta.is_dir());
    let membership = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    let relative = membership
        .strip_prefix("0::/")
        .unwrap()
        .strip_suffix('\n')
        .unwrap();
    assert!(!relative.contains('\n'));
    let current = std::fs::metadata(PathBuf::from("/sys/fs/cgroup").join(relative)).unwrap();
    assert_eq!((meta.dev(), meta.ino()), (current.dev(), current.ino()));
    let members = std::fs::read_to_string(format!("/proc/self/fd/{fd}/cgroup.procs")).unwrap();
    assert_eq!(members.trim(), std::process::id().to_string());
    for (name, expected) in [
        ("memory.max", "8589934592"),
        ("memory.swap.max", "0"),
        ("cpu.max", "200000 100000"),
    ] {
        assert_eq!(
            std::fs::read_to_string(format!("/proc/self/fd/{fd}/{name}"))
                .unwrap()
                .trim(),
            expected
        );
    }
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
        0
    );
    let cli =
        PathBuf::from(std::env::var(boundary::CLI_ENV).expect("verified official CLI absent"));
    assert!(cli.is_absolute() && cli.is_file());
    (cli, directory)
}

fn monotonic_ns() -> io::Result<u64> {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let time = unsafe { time.assume_init() };
    Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

// Runs only in Command's fork child. No allocation, locks, or new supervisor.
fn arm_original_deadline(deadline: u64) -> io::Result<()> {
    let remaining = deadline
        .checked_sub(monotonic_ns()?)
        .filter(|n| *n >= 1000)
        .ok_or_else(|| io::Error::from_raw_os_error(libc::ETIMEDOUT))?;
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGALRM, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut alarm: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut alarm);
        libc::sigaddset(&mut alarm, libc::SIGALRM);
        let timer = libc::itimerval {
            it_interval: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            it_value: libc::timeval {
                tv_sec: (remaining / 1_000_000_000) as _,
                tv_usec: ((remaining % 1_000_000_000) / 1000) as _,
            },
        };
        if libc::syscall(
            libc::SYS_setitimer,
            libc::ITIMER_REAL,
            &timer,
            std::ptr::null_mut::<libc::itimerval>(),
        ) != 0
            || libc::sigprocmask(libc::SIG_UNBLOCK, &alarm, std::ptr::null_mut()) != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn nonblocking(fd: i32) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn capture(reader: &mut impl Read, bytes: &mut Vec<u8>) -> io::Result<bool> {
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if bytes.len() + n > CAPTURE_LIMIT {
                    return Err(io::Error::other("stream cap"));
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

struct Owner {
    child: Child,
    stdout: ChildStdout,
    stderr: ChildStderr,
    _stdin_read: OwnedFd,
    _stdin_write: OwnedFd,
    _fixture: tempfile::TempDir,
}

fn run_case(mode: &str) {
    let (cli, _scope) = official_cli(mode);
    let started = Instant::now();
    let deadline = monotonic_ns().unwrap() + 10_000_000_000;
    let fixture = tempfile::tempdir().unwrap();
    let executable = fixture.path().join("accepted-startup-stderr-exec");
    std::fs::write(&executable, ADAPTER).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut ends = [-1; 2];
    assert_eq!(
        unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) },
        0
    );
    let read = unsafe { OwnedFd::from_raw_fd(ends[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(ends[1]) };
    let mut command = Command::new(executable);
    command
        .arg(mode)
        .arg(&cli)
        .stdin(Stdio::from(read.try_clone().unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(move || arm_original_deadline(deadline));
    }
    let mut child = command.spawn().expect("official refusal adapter spawn");
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    // On an unresolved failure, only the existing outside cgroup owner may
    // conclude cleanup. Never unwind these owners and report native success.
    let mut owner = ManuallyDrop::new(Owner {
        child,
        stdout,
        stderr,
        _stdin_read: read,
        _stdin_write: write,
        _fixture: fixture,
    });
    let mut state = Completion::default();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let observed = (|| -> io::Result<()> {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, owner.child.id(), 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        nonblocking(owner.stdout.as_raw_fd())?;
        nonblocking(owner.stderr.as_raw_fd())?;
        loop {
            if let Some(status) = owner.child.try_wait()? {
                state.terminal = true;
                state.code = status.code();
            }
            if !state.stdout_eof {
                state.stdout_eof = capture(&mut owner.stdout, &mut out)?;
            }
            if !state.stderr_eof {
                state.stderr_eof = capture(&mut owner.stderr, &mut err)?;
            }
            if state.concluded() {
                return Ok(());
            }
            if started.elapsed() >= Duration::from_secs(11) && !state.forced {
                state.forced = true;
                if unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        pidfd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            if started.elapsed() >= Duration::from_secs(boundary::WALL_SECONDS) {
                return Err(io::Error::other("original capture deadline"));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    if observed.is_err() {
        // Failure, never a success receipt. The official wrapper retains the
        // actual cgroup/PIDFD and kills/reaps any remaining attempt members.
        unsafe { libc::_exit(1) }
    }
    drop(ManuallyDrop::into_inner(owner));
    assert!(
        state.natural_125(),
        "required natural125: {state:?}; stdout={out:?}; stderr={err:?}"
    );
    assert!(witness(mode, &out), "actual fixture witness: {out:?}");
    assert_eq!(err, b"", "fixed refusal must have empty product stderr");
}

#[test]
fn full_stderr_refusal_exits_naturally() {
    run_case("full");
}
#[test]
fn normal_stderr_refusal_exits_naturally() {
    run_case("normal");
}

#[cfg(test)]
mod pure {
    use super::*;
    #[test]
    fn each_original_stream_and_terminal_are_required() {
        let complete = Completion {
            terminal: true,
            code: Some(125),
            stdout_eof: true,
            stderr_eof: true,
            forced: false,
        };
        assert!(complete.natural_125());
        assert!(
            !Completion {
                terminal: false,
                ..complete
            }
            .concluded()
        );
        assert!(
            !Completion {
                stdout_eof: false,
                ..complete
            }
            .concluded()
        );
        assert!(
            !Completion {
                stderr_eof: false,
                ..complete
            }
            .concluded()
        );
        assert!(
            !Completion {
                forced: true,
                ..complete
            }
            .natural_125()
        );
        for code in [None, Some(0), Some(122), Some(124), Some(143)] {
            assert!(!Completion { code, ..complete }.natural_125());
        }
    }
    #[test]
    fn exact_real_full_and_normal_witnesses() {
        assert!(witness(
            "full",
            b"startup-stderr-fixture mode=full capacity=8192 queued=8192\n"
        ));
        assert!(witness(
            "normal",
            b"startup-stderr-fixture mode=normal capacity=0 queued=0\n"
        ));
        for bytes in [
            b"".as_slice(),
            b"startup-stderr-fixture mode=full capacity=0 queued=0\n",
            b"startup-stderr-fixture mode=full capacity=8192 queued=8191\n",
            b"startup-stderr-fixture mode=full capacity=08192 queued=8192\n",
            b"startup-stderr-fixture mode=full capacity=8192 queued=8192\nextra",
        ] {
            assert!(!witness("full", bytes));
        }
        assert!(!witness(
            "normal",
            b"startup-stderr-fixture mode=full capacity=8192 queued=8192\n"
        ));
    }
    #[test]
    fn capture_requires_read_eof_and_rejects_excess() {
        let mut out = vec![];
        assert!(capture(&mut io::Cursor::new(b"hello"), &mut out).unwrap());
        assert_eq!(out, b"hello");
        assert!(
            capture(
                &mut io::Cursor::new(vec![0; CAPTURE_LIMIT + 1]),
                &mut vec![]
            )
            .is_err()
        );
        struct Pending;
        impl Read for Pending {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::WouldBlock.into())
            }
        }
        assert!(!capture(&mut Pending, &mut vec![]).unwrap());
    }
}
