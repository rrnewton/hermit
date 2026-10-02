//! Shared bounded command and exact systemd/cgroup identity ownership.
//! Used by the CLI terminal owner and the official nextest provisioning owner.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

/// Command grandchildren must be adopted by the process that owns their
/// group-restricted waits. Being a descendant of a subreaper is insufficient.
pub(crate) fn require_command_parent() -> io::Result<()> {
    let mut enabled: libc::c_int = 0;
    if unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut enabled, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if enabled != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "command parent must retain child-subreaper ownership",
        ));
    }
    Ok(())
}

pub(crate) fn within(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "original Unix terminal deadline",
        ))
    } else {
        Ok(())
    }
}
pub(crate) fn pause(deadline: Instant) -> io::Result<()> {
    within(deadline)?;
    std::thread::sleep(
        deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(2)),
    );
    Ok(())
}
/// An atomic existence probe after reaping. Signal zero delivers no signal.
/// ESRCH alone proves absence; a reused PGID or EPERM conservatively prevents
/// success and is never targeted with a delivered signal after the owned wait.
pub(crate) fn group_absent(group: u32, deadline: Instant) -> io::Result<bool> {
    within(deadline)?;
    if group == 0 || group > i32::MAX as u32 {
        return Err(io::Error::other("invalid owned command group"));
    }
    let result = unsafe { libc::kill(-(group as i32), 0) };
    let error = (result < 0).then(io::Error::last_os_error);
    within(deadline)?;
    match error {
        None => Ok(false),
        Some(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(true),
        Some(error) if error.raw_os_error() == Some(libc::EPERM) => Ok(false),
        Some(error) => Err(error),
    }
}

/// Each command is retained before its first wait. WNOWAIT keeps the exclusive
/// child identity alive through the final process-group signal, then reaps once.
/// On timeout the still-owned child and its logs remain in the finalizer.
pub(crate) struct CommandFlight {
    pub(crate) child: Child,
    pub(crate) stdout: File,
    pub(crate) stderr: File,
    pub(crate) status: Option<ExitStatus>,
    pub(crate) adopted: Vec<(i32, i32)>,
}
impl CommandFlight {
    pub(crate) fn start(command: &mut Command) -> io::Result<Self> {
        Self::start_with_stdin(command, Stdio::null())
    }
    pub(crate) fn start_with_stdin(command: &mut Command, stdin: Stdio) -> io::Result<Self> {
        // Refuse before creating logs or spawning: a different outer subreaper
        // cannot satisfy this owner's descendant waits while this owner is live.
        require_command_parent()?;
        let stdout = tempfile::tempfile()?;
        let stderr = tempfile::tempfile()?;
        command
            .stdin(stdin)
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?);
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                let bound = libc::rlimit {
                    rlim_cur: 8192,
                    rlim_max: 8192,
                };
                if libc::setrlimit(libc::RLIMIT_FSIZE, &bound) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok(Self {
            child: command.spawn()?,
            stdout,
            stderr,
            status: None,
            adopted: Vec::new(),
        })
    }
    pub(crate) fn poll(&mut self, deadline: Instant) -> io::Result<Option<ExitStatus>> {
        self.poll_with_group_observation(deadline, group_absent)
    }
    pub(crate) fn poll_with_group_observation(
        &mut self,
        deadline: Instant,
        mut absent: impl FnMut(u32, Instant) -> io::Result<bool>,
    ) -> io::Result<Option<ExitStatus>> {
        within(deadline)?;
        if self.status.is_none() {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.child.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(None);
            }
            if unsafe { info.si_pid() } != self.child.id() as i32 {
                return Err(io::Error::other("wrong command wait identity"));
            }
            self.kill_group()?;
            self.status = Some(self.child.wait()?);
        }
        // The admitted command parent adopts grandchildren after the leader
        // exits. Reap only this owned process group; wait(-1) could steal an
        // independently owned controller or test status.
        reap_adopted(&mut self.adopted, deadline, || {
            let mut raw = 0;
            let pid = unsafe { libc::waitpid(-(self.child.id() as i32), &mut raw, libc::WNOHANG) };
            if pid < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok((pid, raw))
            }
        })?;
        if !absent(self.child.id(), deadline)? {
            return Ok(None);
        }
        within(deadline)?;
        Ok(self.status)
    }
    pub(crate) fn kill_group(&self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        if unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
    pub(crate) fn output(&mut self) -> io::Result<String> {
        if self.stdout.metadata()?.len() > 8192 || self.stderr.metadata()?.len() > 8192 {
            return Err(io::Error::other("unit command log bound exceeded"));
        }
        self.stdout.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        (&mut self.stdout).take(8193).read_to_string(&mut text)?;
        Ok(text)
    }
}

fn reap_adopted(
    adopted: &mut Vec<(i32, i32)>,
    deadline: Instant,
    mut wait: impl FnMut() -> io::Result<(i32, i32)>,
) -> io::Result<()> {
    loop {
        // The same absolute deadline covers successful reaps and EINTR retries.
        within(deadline)?;
        let result = wait();
        match result.as_ref() {
            Ok((pid, raw)) if *pid > 0 => {
                // Retain a consumed status even if the observation was late.
                adopted.push((*pid, *raw));
                if adopted.len() > 128 {
                    return Err(io::Error::other("command descendant census exceeded"));
                }
            }
            _ => {}
        }
        within(deadline)?;
        match result {
            Ok((0, _)) => return Ok(()),
            Ok((pid, _)) if pid > 0 => continue,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Ok(()),
            Err(error) => return Err(error),
            _ => return Err(io::Error::other("invalid adopted wait identity")),
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn expired_entry_never_calls_wait() {
        let mut calls = 0;
        let error = reap_adopted(&mut Vec::new(), Instant::now(), || {
            calls += 1;
            Ok((0, 0))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls, 0);
    }

    #[test]
    fn interrupted_wait_cannot_retry_after_original_deadline() {
        let deadline = Instant::now() + Duration::from_millis(2);
        let mut calls = 0;
        let error = reap_adopted(&mut Vec::new(), deadline, || {
            calls += 1;
            while Instant::now() < deadline {
                std::hint::spin_loop();
            }
            Err(io::Error::from_raw_os_error(libc::EINTR))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(calls, 1);
    }

    #[test]
    fn positive_reaps_are_retained_and_census_bound_is_not_relaxed() {
        let mut adopted = Vec::new();
        let mut results = [
            Ok((17, 23 << 8)),
            Ok((18, 9)),
            Err(io::Error::from_raw_os_error(libc::ECHILD)),
        ]
        .into_iter();
        reap_adopted(
            &mut adopted,
            Instant::now() + Duration::from_secs(1),
            || results.next().unwrap(),
        )
        .unwrap();
        assert_eq!(adopted, [(17, 23 << 8), (18, 9)]);
        let mut adopted = vec![(1, 0); 128];
        assert!(
            reap_adopted(
                &mut adopted,
                Instant::now() + Duration::from_secs(1),
                || Ok((19, 0))
            )
            .is_err()
        );
        assert_eq!(adopted.len(), 129);
    }
}

pub(crate) fn properties(text: &str) -> io::Result<BTreeMap<&str, &str>> {
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| io::Error::other("invalid unit property"))?;
        if result.insert(name, value).is_some() {
            return Err(io::Error::other("duplicate unit property"));
        }
    }
    Ok(result)
}

pub(crate) struct UnitIdentity {
    pub(crate) unit: String,
    pub(crate) invocation: String,
    pub(crate) cgroup: PathBuf,
    pub(crate) directory: File,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}
impl UnitIdentity {
    pub(crate) fn capture(unit: &str, text: &str) -> io::Result<Self> {
        let p = properties(text)?;
        let get = |name| {
            p.get(name)
                .copied()
                .ok_or_else(|| io::Error::other("missing unit property"))
        };
        let invocation = get("InvocationID")?;
        let group = get("ControlGroup")?;
        if get("Id")? != unit
            || get("LoadState")? != "loaded"
            || invocation.len() != 32
            || !invocation.bytes().all(|b| b.is_ascii_hexdigit())
            || invocation.bytes().all(|b| b == b'0')
            || !group.starts_with('/')
            || group.split('/').any(|part| matches!(part, "." | ".."))
            || !group.ends_with(&format!("/{unit}"))
        {
            return Err(io::Error::other("unit invocation/cgroup identity differs"));
        }
        let cgroup = Path::new("/sys/fs/cgroup").join(&group[1..]);
        let directory = File::options()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&cgroup)?;
        let meta = directory.metadata()?;
        Ok(Self {
            unit: unit.to_owned(),
            invocation: invocation.to_owned(),
            cgroup,
            directory,
            device: meta.dev(),
            inode: meta.ino(),
        })
    }
    pub(crate) fn drained(&self, text: &str) -> io::Result<bool> {
        let p = properties(text)?;
        if p.get("Id") != Some(&self.unit.as_str()) {
            return Err(io::Error::other("unit readback identity differs"));
        }
        if p.get("LoadState") != Some(&"not-found") {
            if p.get("InvocationID")
                .is_some_and(|v| !v.is_empty() && **v != self.invocation)
            {
                return Err(io::Error::other(
                    "unit was replaced during terminal readback",
                ));
            }
            return Ok(false);
        }
        match self.cgroup.symlink_metadata() {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Held directory identity, not an empty replacement pathname.
                Ok(self.directory.metadata()?.nlink() == 0)
            }
            Err(error) => Err(error),
            Ok(meta) if meta.dev() != self.device || meta.ino() != self.inode => {
                Err(io::Error::other("cgroup directory was replaced"))
            }
            Ok(_) => {
                let held = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
                let read = |name| -> io::Result<String> {
                    let mut value = String::new();
                    File::open(held.join(name))?
                        .take(8193)
                        .read_to_string(&mut value)?;
                    Ok(value)
                };
                let procs = read("cgroup.procs")?;
                let events = read("cgroup.events")?;
                if procs.len() > 8192 || events.len() > 8192 {
                    return Err(io::Error::other("cgroup census exceeds bound"));
                }
                let populated: Vec<_> = events
                    .lines()
                    .filter_map(|line| line.strip_prefix("populated "))
                    .collect();
                if populated.len() != 1 || !matches!(populated[0], "0" | "1") {
                    return Err(io::Error::other("invalid cgroup population census"));
                }
                // A still-present empty group is evidence of drain, but not
                // removal. Wait for systemd to retire this exact directory.
                if !procs.trim().is_empty() || populated[0] != "0" {
                    return Ok(false);
                }
                Ok(false)
            }
        }
    }
    pub(crate) fn receipt(&self) -> UnitDrainEvidence {
        UnitDrainEvidence {
            unit: self.unit.clone(),
            invocation: self.invocation.clone(),
            cgroup: self.cgroup.clone(),
            device: self.device,
            inode: self.inode,
        }
    }
}

/// This evidence is created only after actual helper/launcher terminal status,
/// systemd not-found and removal of the held original cgroup directory.
#[derive(Debug, serde::Serialize)]
pub struct UnitDrainEvidence {
    pub(crate) unit: String,
    pub(crate) invocation: String,
    pub(crate) cgroup: PathBuf,
    pub(crate) device: u64,
    pub(crate) inode: u64,
}
