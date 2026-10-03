// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the repository LICENSE file.

//! Per-cell bounds inside the official nextest owner's exact attempt cgroup.
//! The outside wrapper retains CPU accounting, descendant reaping, and removal.

use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Read;
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

#[path = "../../../ci/network-test-boundary.rs"]
mod protocol;

const WALL: Duration = Duration::from_secs(30);
const LOG_BYTES: usize = 4 * 1024 * 1024;
static BOUNDARY: OnceLock<Option<Boundary>> = OnceLock::new();

struct Boundary {
    directory: File,
    kill: File,
    cause: File,
    case: String,
    device: u64,
    inode: u64,
}

fn control(directory: &File, name: &std::ffi::CStr, flags: i32) -> File {
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    assert!(
        fd >= 0,
        "open cgroup control {name:?}: {}",
        io::Error::last_os_error()
    );
    unsafe { File::from_raw_fd(fd) }
}

fn current_directory() -> PathBuf {
    let text = fs::read_to_string("/proc/self/cgroup").expect("current cgroup membership");
    let mut paths = text.lines().filter_map(|line| line.strip_prefix("0::"));
    let relative = Path::new(
        paths
            .next()
            .expect("unified cgroup membership")
            .trim_start_matches('/'),
    );
    assert!(paths.next().is_none(), "multiple unified memberships");
    assert!(
        relative
            .components()
            .all(|part| matches!(part, Component::Normal(_))),
        "invalid current cgroup path"
    );
    Path::new("/sys/fs/cgroup").join(relative)
}

impl Boundary {
    fn verify(&self) {
        let held = self.directory.metadata().expect("held cgroup identity");
        let current = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(current_directory())
            .expect("open actual current cgroup");
        let actual = current.metadata().expect("current cgroup identity");
        assert_eq!((held.dev(), held.ino()), (self.device, self.inode));
        assert_eq!(
            (actual.dev(), actual.ino()),
            (self.device, self.inode),
            "test escaped its owned attempt cgroup"
        );
        for (name, expected) in [
            (c"memory.max", protocol::MEMORY_BYTES.to_string()),
            (c"memory.swap.max", "0".into()),
        ] {
            let mut text = String::new();
            control(&self.directory, name, libc::O_RDONLY)
                .take(64)
                .read_to_string(&mut text)
                .unwrap();
            assert_eq!(
                text.trim(),
                expected,
                "official network memory control changed"
            );
        }
    }
}

/// Called before workload preparation or any process launch. The descriptor
/// inherited for this exact test becomes CLOEXEC immediately, never a guest FD.
pub(super) fn initialize(case: &str) {
    let installed = BOUNDARY.get_or_init(|| {
        let inherited = std::env::var_os(protocol::FD_ENV);
        let granted = std::env::var_os(protocol::CASE_ENV);
        let cause_fd = std::env::var_os(protocol::CAUSE_FD_ENV);
        assert!(
            std::env::var_os(protocol::REQUEST_ENV).is_none(),
            "unconsumed official boundary request"
        );
        match (inherited, granted) {
            (None, None) => {
                assert!(cause_fd.is_none(), "orphan network cause capability");
                assert!(
                    std::env::var_os(super::record_workloads::REQUIRED_ENV).is_none(),
                    "official network test requires its declared attempt cgroup capability"
                );
                None
            }
            (Some(raw), Some(granted)) => {
                let fd: i32 = raw
                    .to_str()
                    .expect("cgroup fd text")
                    .parse()
                    .expect("cgroup fd number");
                assert!(fd >= 3, "cgroup fd overlaps stdio");
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                assert!(
                    flags >= 0
                        && unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
                    "could not make cgroup capability CLOEXEC"
                );
                let directory = unsafe { File::from_raw_fd(fd) };
                let cause_fd: i32 = cause_fd
                    .expect("network cause capability")
                    .to_str()
                    .expect("cause fd text")
                    .parse()
                    .expect("cause fd number");
                assert!(cause_fd >= 3 && cause_fd != fd);
                let cause_flags = unsafe { libc::fcntl(cause_fd, libc::F_GETFD) };
                assert!(
                    cause_flags >= 0
                        && unsafe {
                            libc::fcntl(cause_fd, libc::F_SETFD, cause_flags | libc::FD_CLOEXEC)
                        } == 0
                );
                let cause = unsafe { File::from_raw_fd(cause_fd) };
                let status = unsafe { libc::fcntl(cause_fd, libc::F_GETFL) };
                assert!(status >= 0 && status & libc::O_NONBLOCK != 0);
                assert!(
                    cause.metadata().unwrap().file_type().is_fifo(),
                    "cause capability must be a nonblocking pipe"
                );
                let prepared_cli = PathBuf::from(std::env::var_os(protocol::CLI_ENV).expect("verified prepared network CLI"));
                assert_eq!(prepared_cli, fs::canonicalize(env!("CARGO_BIN_EXE_hermit")).expect("actual embedded Cargo CLI"),
                    "compiled CARGO_BIN_EXE_hermit differs from the source-bound prepared runtime artifact");
                let granted = granted.into_string().expect("boundary case text");
                assert_eq!(granted, case, "wrong test received network capability");
                let name = protocol::CASES
                    .iter()
                    .find(|(key, _)| *key == case)
                    .expect("declared network case")
                    .1;
                assert!(protocol::authorize(
                    case,
                    "hermit",
                    "hermit::record_replay",
                    name
                ));
                assert!(
                    std::env::var_os(super::record_workloads::PREPARED_ENV).is_some(),
                    "official network test requires the complete prepared workload contract"
                );
                let mut filesystem = unsafe { std::mem::zeroed::<libc::statfs>() };
                assert_eq!(unsafe { libc::fstatfs(fd, &mut filesystem) }, 0);
                assert_eq!(
                    filesystem.f_type, 0x6367_7270,
                    "capability is not a cgroup v2 directory"
                );
                let identity = directory.metadata().expect("cgroup identity");
                assert!(identity.is_dir());
                let kill = control(&directory, c"cgroup.kill", libc::O_WRONLY);
                let boundary = Boundary {
                    directory,
                    kill,
                    cause,
                    case: granted,
                    device: identity.dev(),
                    inode: identity.ino(),
                };
                boundary.verify();
                Some(boundary)
            }
            _ => panic!("partial official network capability"),
        }
    });
    if let Some(boundary) = installed {
        assert_eq!(boundary.case, case);
    }
    // Explicit Hermit file logs retain their own independent 1-GiB guard.
    if let Some(limit) = std::env::var_os("HERMIT_LOG_MAX_BYTES") {
        let limit: u64 = limit
            .to_str()
            .expect("file log limit text")
            .parse()
            .expect("file log limit number");
        assert!(
            limit > 0 && limit <= 1 << 30,
            "network file logs require a positive limit at most 1 GiB"
        );
    }
}

pub(super) fn active() -> bool {
    BOUNDARY
        .get()
        .expect("initialize network boundary before workload preparation")
        .is_some()
}

fn lethal(kill: &File, cause: &File, reason: u8) -> ! {
    let frame = protocol::cause_frame(reason);
    assert!(protocol::decode_causes(&frame).is_some());
    // One <= PIPE_BUF nonblocking write to the outside owner. Even a failed
    // write cannot turn SIGKILL/exit70 into an expected guest refusal.
    unsafe {
        libc::write(cause.as_raw_fd(), frame.as_ptr().cast(), frame.len());
    }
    loop {
        let written = unsafe { libc::write(kill.as_raw_fd(), b"1\n".as_ptr().cast(), 2) };
        if written == 2 {
            break;
        }
        if written < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        break;
    }
    // The outside owner still kills/reaps the subtree if the write failed.
    unsafe { libc::_exit(70) }
}

fn nonblocking(fd: i32) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    assert!(flags >= 0 && unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == 0);
}

// The unbuffered sink retains every completed, below-limit read before another
// pipe read. An incomplete file is diagnostic evidence, never a cell receipt.
fn capture_to(
    reader: &mut impl Read,
    bytes: &mut Vec<u8>,
    total: &mut usize,
    retained: &mut impl Write,
) -> io::Result<bool> {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                *total = total.checked_add(count).expect("bounded capture count");
                if *total >= LOG_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::FileTooLarge,
                        "aggregate output reached 4 MiB",
                    ));
                }
                retained.write_all(&buffer[..count])?;
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
fn capture(reader: &mut impl Read, bytes: &mut Vec<u8>, total: &mut usize) -> io::Result<bool> {
    capture_to(reader, bytes, total, &mut io::sink())
}

fn capture_file(path: &Path) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

pub(super) fn run(
    evidence: &Path,
    label: &str,
    arguments: &[String],
    guest: &Path,
    guest_arguments: &[&str],
) -> Output {
    let boundary = BOUNDARY
        .get()
        .and_then(Option::as_ref)
        .expect("actual official cgroup capability");
    boundary.verify();
    assert!(
        !label.is_empty()
            && label
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    );
    let timer_cause = boundary.cause.try_clone().unwrap();
    let kill = boundary.kill.try_clone().unwrap();
    let started = Instant::now();
    let deadline = started + WALL;
    let (stop, receive) = mpsc::sync_channel::<Instant>(1);
    let watchdog = thread::spawn(move || {
        match receive.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(completed) if completed <= deadline => (),
            Err(mpsc::RecvTimeoutError::Disconnected) => lethal(&kill, &timer_cause, 4),
            _ => lethal(&kill, &timer_cause, 1),
        }
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_hermit"));
    command
        .args(arguments)
        .arg("--")
        .arg(guest)
        .args(guest_arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    fs::write(
        evidence.join(format!("{label}.command")),
        format!("{command:?}\n"),
    )
    .unwrap();
    // Create fresh logs under the original watchdog before the child starts.
    // Stale paths and write errors cannot be mistaken for completed capture.
    let mut retained_stdout = capture_file(&evidence.join(format!("{label}.stdout")))
        .unwrap_or_else(|_| lethal(&boundary.kill, &boundary.cause, 3));
    let mut retained_stderr = capture_file(&evidence.join(format!("{label}.stderr")))
        .unwrap_or_else(|_| lethal(&boundary.kill, &boundary.cause, 3));
    let mut child = command.spawn().expect("launch official bounded Hermit");
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    nonblocking(stdout.as_raw_fd());
    nonblocking(stderr.as_raw_fd());
    let (mut out, mut err, mut total) = (Vec::new(), Vec::new(), 0);
    let (mut out_done, mut err_done, mut status) = (false, false, None);
    while status.is_none() || !out_done || !err_done {
        if !out_done {
            out_done = capture_to(&mut stdout, &mut out, &mut total, &mut retained_stdout)
                .unwrap_or_else(|error| {
                    lethal(
                        &boundary.kill,
                        &boundary.cause,
                        if error.kind() == io::ErrorKind::FileTooLarge {
                            2
                        } else {
                            3
                        },
                    )
                });
        }
        if !err_done {
            err_done = capture_to(&mut stderr, &mut err, &mut total, &mut retained_stderr)
                .unwrap_or_else(|error| {
                    lethal(
                        &boundary.kill,
                        &boundary.cause,
                        if error.kind() == io::ErrorKind::FileTooLarge {
                            2
                        } else {
                            3
                        },
                    )
                });
        }
        if status.is_none() {
            status = child.try_wait().expect("reap bounded Hermit child");
        }
        if Instant::now() > deadline {
            lethal(&boundary.kill, &boundary.cause, 1);
        }
        if status.is_none() || !out_done || !err_done {
            thread::sleep(Duration::from_millis(2));
        }
    }
    // try_wait above has already reaped the child. wait returns that cached
    // status; keeping an explicit join also checks that the observation agrees.
    let reaped = child.wait().expect("join reaped bounded Hermit child");
    assert_eq!(status, Some(reaped));
    let status = reaped;
    assert!(
        status.code().is_some(),
        "Hermit terminated by a signal: {status}"
    );
    let receipt = serde_json::json!({
        "schema": "hermit-official-network-cell-v1", "case": boundary.case,
        "cgroup_device": boundary.device, "cgroup_inode": boundary.inode,
        "exit_code": status.code(), "wall_limit_seconds": 30,
        "memory_max_bytes": protocol::MEMORY_BYTES, "memory_swap_max_bytes": 0,
        "log_limit_bytes": LOG_BYTES, "captured_bytes": total,
        "truncated": false, "bound_hit": false,
        "elapsed_seconds": started.elapsed().as_secs_f64(),
        "cleanup_owner": "nextest-cpu-wrapper", "attempt_cleanup_pending": true,
        "disk_scope": "retained stdout and stderr are below 4 MiB; explicit file logs have their separate 1-GiB guard; no guest filesystem quota claim"
    });
    fs::write(
        evidence.join(format!("{label}.official-boundary.json")),
        serde_json::to_vec_pretty(&receipt).unwrap(),
    )
    .unwrap();
    let completed = Instant::now();
    if completed > deadline {
        lethal(&boundary.kill, &boundary.cause, 1);
    }
    stop.send(completed).expect("stop bounded cell watchdog");
    watchdog.join().expect("join bounded cell watchdog");
    Output {
        status,
        stdout: out,
        stderr: err,
    }
}

pub(super) fn assert_receipt(evidence: &Path, label: &str, expected_status: Option<i32>) {
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(evidence.join(format!("{label}.official-boundary.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(value["schema"], "hermit-official-network-cell-v1");
    assert_eq!(value["wall_limit_seconds"], 30);
    assert!(value["elapsed_seconds"].as_f64().is_some_and(|v| v <= 30.0));
    assert_eq!(value["memory_max_bytes"], protocol::MEMORY_BYTES);
    assert_eq!(value["memory_swap_max_bytes"], 0);
    assert_eq!(value["log_limit_bytes"], LOG_BYTES);
    assert!(
        value["captured_bytes"]
            .as_u64()
            .is_some_and(|v| v < LOG_BYTES as u64)
    );
    assert_eq!(value["bound_hit"], false);
    assert_eq!(value["truncated"], false);
    assert_eq!(value["cleanup_owner"], "nextest-cpu-wrapper");
    assert_eq!(value["attempt_cleanup_pending"], true);
    if let Some(status) = expected_status {
        assert_eq!(value["exit_code"], status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_capture_preserves_all_bytes_and_actual_eof() {
        let mut bytes = Vec::new();
        let mut count = 0;
        assert!(
            capture(
                &mut io::Cursor::new(b"stdout\0exact"),
                &mut bytes,
                &mut count
            )
            .unwrap()
        );
        assert_eq!(bytes, b"stdout\0exact");
        assert_eq!(count, bytes.len());
    }

    #[test]
    fn aggregate_stream_cap_rejects_the_exact_cap_before_retaining_excess() {
        let mut first = Vec::new();
        let mut second = Vec::new();
        let mut count = 0;
        assert!(
            capture(
                &mut io::Cursor::new(vec![b'a'; LOG_BYTES / 2]),
                &mut first,
                &mut count
            )
            .unwrap()
        );
        let error = capture(
            &mut io::Cursor::new(vec![b'b'; LOG_BYTES / 2]),
            &mut second,
            &mut count,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(count, LOG_BYTES);
        assert!(first.len() + second.len() < LOG_BYTES);
        assert_eq!(first, vec![b'a'; LOG_BYTES / 2]);
    }
}

#[cfg(test)]
mod persistence_tests {
    use std::os::unix::net::UnixStream;

    use super::*;

    #[test]
    fn incomplete_stream_is_retained_before_eof_and_success_bytes_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cell.stdout");
        let mut retained = capture_file(&path).unwrap();
        let (mut reader, mut writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        writer.write_all(b"first\0chunk").unwrap();
        let (mut bytes, mut total) = (Vec::new(), 0);
        assert!(!capture_to(&mut reader, &mut bytes, &mut total, &mut retained).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"first\0chunk");
        assert_eq!(bytes, b"first\0chunk");
        assert_eq!(total, bytes.len());
        writer.write_all(b"second").unwrap();
        drop(writer);
        assert!(capture_to(&mut reader, &mut bytes, &mut total, &mut retained).unwrap());
        assert_eq!(bytes, b"first\0chunksecond");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(total, bytes.len());
    }

    #[test]
    fn partial_sink_error_refuses_without_claiming_a_complete_chunk() {
        struct PartialThenError(Vec<u8>);
        impl Write for PartialThenError {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0.is_empty() {
                    self.0.extend_from_slice(&bytes[..2]);
                    Ok(2)
                } else {
                    Err(io::Error::from_raw_os_error(libc::ENOSPC))
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sink = PartialThenError(Vec::new());
        let (mut bytes, mut total) = (Vec::new(), 0);
        let error = capture_to(
            &mut io::Cursor::new(b"actual bytes"),
            &mut bytes,
            &mut total,
            &mut sink,
        )
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENOSPC));
        assert_eq!(sink.0, b"ac");
        assert!(bytes.is_empty());
        assert_eq!(total, b"actual bytes".len());
    }

    #[test]
    fn exact_aggregate_cap_refuses_before_writing_the_crossing_chunk() {
        let (mut bytes, mut retained, mut total) = (Vec::new(), Vec::new(), LOG_BYTES - 2);
        assert!(
            capture_to(
                &mut io::Cursor::new(b"a"),
                &mut bytes,
                &mut total,
                &mut retained,
            )
            .unwrap()
        );
        assert_eq!(total, LOG_BYTES - 1);
        assert_eq!(retained, b"a");
        let error = capture_to(
            &mut io::Cursor::new(b"b"),
            &mut bytes,
            &mut total,
            &mut retained,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(total, LOG_BYTES);
        assert_eq!(bytes, b"a");
        assert_eq!(retained, b"a");
    }

    #[test]
    fn existing_capture_path_is_refused_without_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cell.stderr");
        fs::write(&path, b"previous failure").unwrap();
        assert_eq!(
            capture_file(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(path).unwrap(), b"previous failure");
    }
}
