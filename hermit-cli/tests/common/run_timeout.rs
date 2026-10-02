/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
*/

//! Non-vacuous session census for the CLI timeout regressions. The session
//! leader remains an unreaped child until observation and cleanup are complete;
//! its reserved PID prevents a new, unrelated session from reusing the SID.

use std::fs;
use std::io;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::process::Child;
use std::process::Output;
use std::thread;
use std::time::Duration;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    pid: i32,
    session: i32,
    start_time: u64,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn disappeared(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}

fn numeric<T: std::str::FromStr>(bytes: &[u8], pid: i32, field: &str) -> io::Result<T> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(invalid(format!("pid {pid}: invalid stat {field}")));
    }
    // Only numeric fields are text. Linux permits arbitrary bytes in comm.
    std::str::from_utf8(bytes)
        .map_err(|_| invalid(format!("pid {pid}: non-ASCII stat {field}")))?
        .parse()
        .map_err(|_| invalid(format!("pid {pid}: invalid stat {field}")))
}

fn identity(root: &Path, pid: i32) -> io::Result<Option<Identity>> {
    let stat = match fs::read(root.join(pid.to_string()).join("stat")) {
        Ok(stat) => stat,
        Err(error) if disappeared(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let end_comm = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(|| invalid(format!("pid {pid}: stat has no closing comm delimiter")))?;
    let pid_end = stat[..end_comm]
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| invalid(format!("pid {pid}: stat has no pid delimiter")))?;
    if numeric::<i32>(&stat[..pid_end], pid, "pid")? != pid {
        return Err(invalid(format!("pid {pid}: stat pid does not match")));
    }
    let fields: Vec<_> = stat[end_comm + 1..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .collect();
    let session = numeric::<i32>(
        fields
            .get(3)
            .ok_or_else(|| invalid(format!("pid {pid}: missing stat session")))?,
        pid,
        "session",
    )?;
    let start_time = numeric::<u64>(
        fields
            .get(19)
            .ok_or_else(|| invalid(format!("pid {pid}: missing stat start time")))?,
        pid,
        "start time",
    )?;
    Ok(Some(Identity {
        pid,
        session,
        start_time,
    }))
}

struct Task {
    identity: Identity,
    pidfd: OwnedFd,
}

impl Task {
    fn open(expected: Identity) -> io::Result<Option<Self>> {
        // SAFETY: pidfd_open has no pointer arguments and returns a new owned fd.
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, expected.pid, 0) };
        if raw < 0 {
            let error = io::Error::last_os_error();
            return if disappeared(&error) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: this successful syscall transferred a fresh descriptor to us.
        let task = Self {
            identity: expected,
            pidfd: unsafe { OwnedFd::from_raw_fd(raw as i32) },
        };
        match task.revalidate()? {
            true => Ok(Some(task)),
            false => Ok(None),
        }
    }

    fn revalidate(&self) -> io::Result<bool> {
        match identity(Path::new("/proc"), self.identity.pid)? {
            None => Ok(false),
            Some(actual) if actual == self.identity => Ok(true),
            Some(actual) => Err(invalid(format!(
                "pid identity changed: expected {:?}, observed {actual:?}",
                self.identity
            ))),
        }
    }

    fn kill(&self) -> io::Result<()> {
        if !self.revalidate()? {
            return Ok(());
        }
        // SAFETY: the held pidfd names the admitted task even if it exits or
        // the numeric PID is reused after the revalidation. No numeric kill.
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if disappeared(&error) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

fn within(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "timeout census/output deadline",
        ))
    } else {
        Ok(())
    }
}

fn census(
    root: &Path,
    session: i32,
    terminal_leader: Option<i32>,
    deadline: Instant,
) -> io::Result<Vec<Task>> {
    within(deadline)?;
    let mut tasks = Vec::new();
    for entry in fs::read_dir(root)? {
        within(deadline)?;
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|byte| byte.is_ascii_digit()) || name.is_empty() {
            continue;
        }
        let pid = name
            .parse::<i32>()
            .map_err(|error| invalid(error.to_string()))?;
        let Some(current) = identity(root, pid)? else {
            continue;
        };
        if current.session != session || terminal_leader == Some(pid) {
            continue;
        }
        if let Some(task) = Task::open(current)? {
            tasks.push(task);
        }
    }
    within(deadline)?;
    Ok(tasks)
}

fn terminal(pid: i32) -> io::Result<bool> {
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: info points to writable storage. WNOWAIT observes, but does not
    // reap, our exact child; we reserve its PID/SID until all signaling ends.
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as u32,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded; zero si_pid denotes an unfinished child.
    Ok(unsafe { info.assume_init().si_pid() } == pid)
}

fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: fcntl operates on the valid pipe descriptor, with integer flags.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;

fn collect(pipe: &mut impl Read, bytes: &mut Vec<u8>, deadline: Instant) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    loop {
        within(deadline)?;
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                if bytes
                    .len()
                    .checked_add(count)
                    .is_none_or(|size| size > MAX_CAPTURE_BYTES)
                {
                    return Err(invalid("captured output exceeded 4 MiB per stream"));
                }
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

pub(super) struct Observation {
    pub output: Output,
    pub elapsed: Duration,
    pub drained: bool,
    pub survivors: Vec<i32>,
}

/// Preserve the tests' 15s output/exit and 20s drain bounds. Neither an unreadable
/// /proc entry nor an unfinished output reader can masquerade as an empty run.
/// Cleanup happens before returning output, so an output assertion cannot skip it.
pub(super) fn wait(mut child: Child, started: Instant) -> io::Result<Observation> {
    let session = child.id() as i32;
    let output_deadline = started + Duration::from_secs(15);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| invalid("stdout not piped"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| invalid("stderr not piped"))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut leader_task = None;
    let observed: io::Result<Duration> = (|| {
        let expected = identity(Path::new("/proc"), session)?
            .ok_or_else(|| invalid("owned session leader disappeared before observation"))?;
        if expected.session != session {
            return Err(invalid("child is not its own session leader"));
        }
        leader_task = Task::open(expected)?;
        if leader_task.is_none() {
            return Err(invalid(
                "owned session leader disappeared before pidfd admission",
            ));
        }
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        loop {
            within(output_deadline)?;
            let exited = match terminal(session) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            let out_done = collect(&mut stdout, &mut out, output_deadline)?;
            let err_done = collect(&mut stderr, &mut err, output_deadline)?;
            if exited && out_done && err_done {
                return Ok(started.elapsed());
            }
            thread::sleep(Duration::from_millis(5));
        }
    })();
    let drain_deadline = Instant::now() + Duration::from_secs(20);
    let mut tasks = Vec::new();
    let mut failure = observed.as_ref().err().map(ToString::to_string);
    let mut drained = false;
    loop {
        let leader = match terminal(session) {
            Ok(true) => Some(session),
            Ok(false) => None,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                if within(drain_deadline).is_err() {
                    failure = Some("waitid interrupted past drain deadline".into());
                    break;
                }
                continue;
            }
            Err(error) => {
                failure = Some(format!("waitid: {error}"));
                break;
            }
        };
        match census(Path::new("/proc"), session, leader, drain_deadline) {
            Ok(current) => tasks = current,
            Err(error) => {
                failure = Some(format!("session census: {error}"));
                break;
            }
        }
        if tasks.is_empty() && leader.is_some() {
            drained = true;
            break;
        }
        if failure.is_some() || Instant::now() >= drain_deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let survivors = tasks.iter().map(|task| task.identity.pid).collect();
    for task in &tasks {
        if let Err(error) = task.kill() {
            failure = Some(format!("owned task cleanup: {error}"));
        }
    }
    if failure.is_some()
        && let Some(leader) = &leader_task
        && let Err(error) = leader.kill()
    {
                failure = Some(format!("leader cleanup: {error}"));
            }
    // The leader stays reserved through every possible signal. A terminal child
    // can now be reaped without risking that a later cleanup kills a new SID.
    loop {
        match terminal(session) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
        within(drain_deadline)?;
        thread::sleep(Duration::from_millis(5));
    }
    let status = child.wait()?;
    if let Some(failure) = failure {
        return Err(io::Error::other(failure));
    }
    Ok(Observation {
        output: Output {
            status,
            stdout: out,
            stderr: err,
        },
        elapsed: observed?,
        drained,
        survivors,
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::process::Stdio;

    use super::*;

    #[test]
    fn census_refuses_missing_root_and_malformed_stat() {
        let root = tempfile::tempdir().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        assert!(census(&root.path().join("missing"), 1, None, deadline).is_err());
        fs::create_dir(root.path().join("123")).unwrap();
        fs::write(root.path().join("123/stat"), "123 (broken) Z 1").unwrap();
        assert!(census(root.path(), 1, None, deadline).is_err());
        fs::remove_file(root.path().join("123/stat")).unwrap();
        fs::create_dir(root.path().join("123/stat")).unwrap();
        assert!(census(root.path(), 1, None, deadline).is_err());
    }

    #[test]
    fn census_accepts_non_utf8_comm_and_rejects_non_ascii_numeric_fields() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("123")).unwrap();
        // A legal task comm may contain arbitrary bytes, spaces and ')'. This
        // unrelated session must not make a census of session 9 fail.
        let mut stat = b"123 (review-\xff-comm ) name) S 1 2 8".to_vec();
        for _field in 7..22 {
            stat.extend_from_slice(b" 0");
        }
        stat.extend_from_slice(b" 456\n");
        fs::write(root.path().join("123/stat"), &stat).unwrap();
        assert_eq!(
            identity(root.path(), 123).unwrap(),
            Some(Identity {
                pid: 123,
                session: 8,
                start_time: 456
            })
        );
        assert!(
            census(
                root.path(),
                9,
                None,
                Instant::now() + Duration::from_secs(2)
            )
            .unwrap()
            .is_empty()
        );
        *stat.iter_mut().rfind(|byte| **byte == b'6').unwrap() = 0xff;
        fs::write(root.path().join("123/stat"), &stat).unwrap();
        assert!(identity(root.path(), 123).is_err());
    }

    #[test]
    fn captured_stream_accepts_four_mib_and_refuses_one_byte_more() {
        let bound = 4 * 1024 * 1024;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut captured = Vec::new();
        assert!(
            collect(
                &mut io::Cursor::new(vec![b'x'; bound]),
                &mut captured,
                deadline
            )
            .unwrap()
        );
        assert_eq!(captured, vec![b'x'; bound]);
        let error = collect(&mut io::Cursor::new(*b"y"), &mut captured, deadline).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(captured, vec![b'x'; bound]);
        let mut stderr = Vec::new();
        assert!(
            collect(
                &mut io::Cursor::new(vec![b'e'; bound + 1]),
                &mut stderr,
                deadline
            )
            .is_err()
        );
        assert!(stderr.len() <= bound);
    }

    #[test]
    fn disappearance_is_narrow_and_deadlines_are_checked() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(identity(root.path(), 123).unwrap(), None);
        assert!(disappeared(&io::Error::from_raw_os_error(libc::ESRCH)));
        for code in [libc::EACCES, libc::EIO, libc::EINTR] {
            assert!(!disappeared(&io::Error::from_raw_os_error(code)));
        }
        fs::create_dir(root.path().join("123")).unwrap();
        assert!(census(root.path(), 1, None, Instant::now()).is_err());
    }

    #[test]
    fn pidfd_revalidation_refuses_changed_birth_or_session() {
        let actual = identity(Path::new("/proc"), std::process::id() as i32)
            .unwrap()
            .unwrap();
        for changed in [
            Identity {
                start_time: actual.start_time + 1,
                ..actual
            },
            Identity {
                session: actual.session + 1,
                ..actual
            },
        ] {
            assert!(Task::open(changed).is_err());
        }
    }

    fn native_child(script: &str) -> Child {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe in the pre-exec child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        command.spawn().unwrap()
    }

    #[test]
    fn census_pidfd_cleanup_leaves_unrelated_session_alive() {
        let mut owned = native_child("exec sleep 30");
        let mut unrelated = native_child("exec sleep 30");
        let session = owned.id() as i32;
        let tasks = census(
            Path::new("/proc"),
            session,
            None,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].identity.pid, session);
        tasks[0].kill().unwrap();
        owned.wait().unwrap();
        assert!(unrelated.try_wait().unwrap().is_none());
        let other = identity(Path::new("/proc"), unrelated.id() as i32)
            .unwrap()
            .unwrap();
        Task::open(other).unwrap().unwrap().kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[test]
    fn output_wait_keeps_terminal_leader_until_nonvacuous_census() {
        let child = native_child("printf stdout; printf stderr >&2");
        let result = wait(child, Instant::now()).unwrap();
        assert!(result.output.status.success());
        assert_eq!(result.output.stdout, b"stdout");
        assert_eq!(result.output.stderr, b"stderr");
        assert!(result.drained);
        assert!(result.survivors.is_empty());
    }
}
