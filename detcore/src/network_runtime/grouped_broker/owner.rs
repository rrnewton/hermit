//! Creator authority comes from kernel capabilities and an actual manager query.
//! The launcher Child, creator pidfd and cgroup directory are distinct owners.
#[path = "owner/user_unit.rs"]
mod user_unit;
pub(super) use user_unit::SourceAuthorityUnit;
pub(super) use user_unit::check_source_authority_policy;
#[path = "owner/remote_source.rs"]
mod remote_source;
pub(super) use remote_source::OutsideSourceRetirement;
pub(super) use remote_source::RemoteLauncherLease;
#[path = "prepared_namespace.rs"]
mod prepared_namespace;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Read;
use std::io::{self};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Instant;

pub(super) use prepared_namespace::*;

use super::Failure;
use super::Intent;
use super::require;
use super::wire::Credentials;
use super::wire::Packet;

pub(super) const PROPERTIES: &str = "Id,LoadState,ActiveState,SubState,Result,ExecMainCode,ExecMainStatus,MainPID,ExecMainPID,InvocationID,ControlGroup,TasksCurrent";

#[derive(Clone, Copy, Debug)]
pub(super) struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub links: u64,
    pub size: i64,
}
impl From<libc::stat> for FileIdentity {
    fn from(s: libc::stat) -> Self {
        Self {
            device: s.st_dev,
            inode: s.st_ino,
            mode: s.st_mode,
            uid: s.st_uid,
            gid: s.st_gid,
            links: s.st_nlink,
            size: s.st_size,
        }
    }
}
impl FileIdentity {
    pub fn same_object(&self, other: &Self) -> bool {
        self.device == other.device && self.inode == other.inode
    }
    pub fn same_owner(&self, other: &Self) -> bool {
        self.same_object(other)
            && self.mode == other.mode
            && self.uid == other.uid
            && self.gid == other.gid
    }
}
pub(super) fn stat(fd: RawFd) -> io::Result<FileIdentity> {
    let mut s = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, s.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(FileIdentity::from(unsafe { s.assume_init() }))
}
pub(super) fn filesystem(fd: RawFd) -> io::Result<libc::c_long> {
    let mut s = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstatfs(fd, s.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { s.assume_init() }.f_type)
}
pub(super) fn terminal(fd: RawFd) -> io::Result<bool> {
    require(
        filesystem(fd)? == 0x5049_4446,
        "retained description is not a pidfd",
    )?;
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let result = unsafe { libc::poll(&mut p, 1, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    require(
        result == 0
            || (result == 1
                && p.revents & libc::POLLIN != 0
                && p.revents & !(libc::POLLIN | libc::POLLHUP) == 0),
        "pidfd observation error",
    )?;
    Ok(result == 1)
}
pub(super) fn read_file(path: &str, cap: usize) -> io::Result<String> {
    let file = std::fs::File::open(path)?;
    read_bounded(file, cap)
}
fn read_bounded(file: std::fs::File, cap: usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    require(
        bytes.len() < cap && bytes.is_ascii(),
        "original bounded ASCII readback exceeded",
    )?;
    String::from_utf8(bytes).map_err(io::Error::other)
}
pub(super) fn read_at(directory: RawFd, name: &str) -> io::Result<String> {
    let name = CString::new(name).map_err(io::Error::other)?;
    let raw = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    read_bounded(unsafe { std::fs::File::from_raw_fd(raw) }, 4096)
}
pub(super) fn pidfd_matches(fd: RawFd, pid: libc::pid_t) -> io::Result<()> {
    require(!terminal(fd)?, "creator pidfd is already terminal")?;
    let info = read_file(&format!("/proc/self/fdinfo/{fd}"), 4096)?;
    let found: Vec<_> = info
        .lines()
        .filter_map(|s| s.strip_prefix("Pid:"))
        .map(str::trim)
        .collect();
    require(
        found == [pid.to_string()],
        "pidfd differs from kernel credential",
    )
}
pub(super) fn protected_holder() -> io::Result<()> {
    require(
        unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } == 0,
        "grouped holder must already be nondumpable",
    )?;
    let mut subreaper = 0;
    if unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut subreaper, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    require(
        subreaper == 1,
        "grouped holder requires existing subreaper ownership",
    )
}
fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
fn drain(fd: RawFd, bytes: &mut Vec<u8>, eof: &mut bool) -> io::Result<()> {
    if *eof {
        return Ok(());
    }
    let mut buffer = [0u8; 65_536];
    let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if count < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::WouldBlock {
            Ok(())
        } else {
            Err(error)
        };
    }
    *eof = count == 0;
    let left = 1_048_576usize.saturating_sub(bytes.len());
    bytes.extend_from_slice(&buffer[..(count as usize).min(left)]);
    require(count as usize <= left, "original1MiB stream bound exceeded")
}

#[derive(Debug)]
pub(super) struct Launcher {
    pub child: Child,
    pub pidfd: Option<OwnedFd>,
    pub reaped: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub eof: [bool; 2],
    pub log_files: [Option<OwnedFd>; 2],
    pipe_identities: [Option<(i32, FileIdentity)>; 2],
    pub logs_synced: bool,
    pub refused: Option<Failure>,
    initialization_attempted: bool,
    initialized: bool,
    original_pipes: [Option<RawFd>; 2],
    log_written: [usize; 2],
    held_logs_synced: [bool; 2],
    retirement_deadline: Option<Instant>,
    retirement_failure: Option<Failure>,
    custody_retired: bool,
    log_directory: Option<(RawFd, FileIdentity)>,
}
impl Launcher {
    pub fn retain(child: Child) -> Self {
        // Capture the actual handles before any fallible setup. No reopen can
        // replace a missing original output pipe during custody retirement.
        let original_pipes = [
            child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            child.stderr.as_ref().map(AsRawFd::as_raw_fd),
        ];
        Self {
            child,
            pidfd: None,
            reaped: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            eof: [false; 2],
            log_files: [None, None],
            pipe_identities: [None, None],
            logs_synced: false,
            refused: None,
            initialization_attempted: false,
            initialized: false,
            original_pipes,
            log_written: [0; 2],
            held_logs_synced: [false; 2],
            retirement_deadline: None,
            retirement_failure: None,
            custody_retired: false,
            log_directory: None,
        }
    }
    pub fn initialize(&mut self, directory: RawFd) -> io::Result<()> {
        let result = self.initialize_held(directory);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn initialize_held(&mut self, directory: RawFd) -> io::Result<()> {
        require(
            !self.initialization_attempted
                && self.refused.is_none()
                && self.retirement_deadline.is_none()
                && self.pidfd.is_none()
                && self.reaped.is_none(),
            "launcher cannot be recaptured",
        )?;
        self.initialization_attempted = true;
        let directory_identity = stat(directory)?;
        self.log_directory = Some((directory, directory_identity));
        require(
            directory_identity.mode & libc::S_IFMT == libc::S_IFDIR,
            "launcher log directory is not a held directory",
        )?;
        let pid = self.child.id() as libc::pid_t;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as u32,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            unsafe { info.si_pid() } == 0 && unsafe { libc::getpgid(pid) } == pid,
            "launcher must be the live unreaped direct process-group owner",
        )?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.pidfd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        pidfd_matches(fd, pid)?;
        for (index, pipe) in [
            self.child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            self.child.stderr.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .enumerate()
        {
            let pipe =
                pipe.ok_or_else(|| io::Error::other("actual launcher output pipe absent"))?;
            require(
                stat(pipe)?.mode & libc::S_IFMT == libc::S_IFIFO,
                "launcher output is not a pipe",
            )?;
            self.pipe_identities[index] = Some((pipe, stat(pipe)?));
            nonblocking(pipe)?;
            let name = if index == 0 {
                c"stdout.log"
            } else {
                c"stderr.log"
            };
            let fd = unsafe {
                libc::openat(
                    directory,
                    name.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            self.log_files[index] = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        self.initialized = true;
        Ok(())
    }
    pub fn drain(&mut self) -> io::Result<()> {
        let result = self.drain_admitted();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn drain_admitted(&mut self) -> io::Result<()> {
        require(
            self.refused.is_none()
                && self.pidfd.is_some()
                && self.initialized
                && self.retirement_deadline.is_none(),
            "launcher not admitted or refused",
        )?;
        let result = (|| {
            for index in 0..2 {
                let fd = if index == 0 {
                    self.child.stdout.as_ref().map(AsRawFd::as_raw_fd)
                } else {
                    self.child.stderr.as_ref().map(AsRawFd::as_raw_fd)
                }
                .ok_or_else(|| io::Error::other("retained output pipe absent"))?;
                let (original_fd, original_identity) = self.pipe_identities[index]
                    .ok_or_else(|| io::Error::other("launcher pipe identity absent"))?;
                require(
                    fd == original_fd && stat(fd)?.same_owner(&original_identity),
                    "actual retained launcher pipe changed",
                )?;
                let bytes = if index == 0 {
                    &mut self.stdout
                } else {
                    &mut self.stderr
                };
                let before = bytes.len();
                // The stream bytes are retained before checking overflow.
                let outcome = drain(fd, bytes, &mut self.eof[index]);
                if let Err(error) = &outcome {
                    // Unknown or discarded stream bytes cannot be repaired by
                    // a later EOF and must never gain custody completeness.
                    self.retirement_failure
                        .get_or_insert_with(|| Failure::capture(error));
                }
                let log = self.log_files[index]
                    .as_ref()
                    .ok_or_else(|| io::Error::other("retained log absent"))?
                    .as_raw_fd();
                let mut offset = self.log_written[index];
                require(
                    offset <= before,
                    "launcher log cursor exceeds retained bytes",
                )?;
                while offset < bytes.len() {
                    let n = unsafe {
                        libc::pwrite(
                            log,
                            bytes[offset..].as_ptr().cast(),
                            bytes.len() - offset,
                            offset as i64,
                        )
                    };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    require(n > 0, "launcher log made no progress")?;
                    offset += n as usize;
                    self.log_written[index] = offset;
                }
                outcome?;
            }
            Ok(())
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    pub fn reap_success(&mut self, directory: RawFd) -> io::Result<()> {
        let result = self.reap_admitted_success(directory);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn reap_admitted_success(&mut self, directory: RawFd) -> io::Result<()> {
        self.check_log_directory(directory)?;
        require(
            self.refused.is_none() && self.eof == [true, true],
            "launcher requires both actual pipe EOFs",
        )?;
        if !self.logs_synced {
            for file in &self.log_files {
                let fd = file
                    .as_ref()
                    .ok_or_else(|| io::Error::other("launcher log absent"))?
                    .as_raw_fd();
                if unsafe { libc::fsync(fd) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if unsafe { libc::fsync(directory) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.logs_synced = true;
        }
        let pid = self.child.id() as libc::pid_t;
        require(
            terminal(
                self.pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("launcher pidfd absent"))?
                    .as_raw_fd(),
            )?,
            "launcher remains live",
        )?;
        if self.reaped.is_none() {
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                unsafe { info.si_pid() } == pid
                    && info.si_code == libc::CLD_EXITED
                    && unsafe { info.si_status() } == 0,
                "launcher did not finish naturally",
            )?;
            self.reaped = self.child.try_wait()?; // retain irreversible wait before comparisons
        }
        require(
            self.reaped.is_some_and(|s| s.success()),
            "launcher exact wait failed",
        )?;
        let probe = unsafe { libc::kill(-pid, 0) };
        require(
            probe == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "launcher group not positively absent",
        )?;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        require(
            result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
            "serial owner still has a child or adopted descendant",
        )
    }
    /// Custody retirement of an actual failed natural child. Positive reap_success
    /// remains unchanged and will refuse this retained failed launcher forever.
    pub fn retire_failed(&mut self, directory: RawFd) -> io::Result<()> {
        let result = self.retire_admitted_failed(directory);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn retire_admitted_failed(&mut self, directory: RawFd) -> io::Result<()> {
        self.check_log_directory(directory)?;
        require(
            self.refused.is_none() && self.eof == [true, true],
            "launcher requires both actual pipe EOFs",
        )?;
        if !self.logs_synced {
            for file in &self.log_files {
                let fd = file
                    .as_ref()
                    .ok_or_else(|| io::Error::other("launcher log absent"))?
                    .as_raw_fd();
                if unsafe { libc::fsync(fd) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if unsafe { libc::fsync(directory) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.logs_synced = true;
        }
        let pid = self.child.id() as libc::pid_t;
        require(
            terminal(
                self.pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("launcher pidfd absent"))?
                    .as_raw_fd(),
            )?,
            "launcher remains live",
        )?;
        if self.reaped.is_none() {
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            if unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as u32,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            require(
                unsafe { info.si_pid() } == pid
                    && info.si_code == libc::CLD_EXITED
                    && unsafe { info.si_status() } != 0,
                "failed launcher lacks original natural nonzero wait",
            )?;
            self.reaped = self.child.try_wait()?; // retain irreversible wait before comparisons
        }
        require(
            self.reaped
                .is_some_and(|s| s.code().is_some_and(|code| code != 0)),
            "failed launcher unexpectedly succeeded",
        )?;
        let probe = unsafe { libc::kill(-pid, 0) };
        require(
            probe == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "launcher group not positively absent",
        )?;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        require(
            result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
            "serial owner still has a child or adopted descendant",
        )?;
        self.refused.get_or_insert_with(|| {
            Failure::capture(&io::Error::other("original launcher exited nonzero"))
        });
        Ok(())
    }

    /// Cleanup of the retained Child after failed/partial setup or cancellation.
    /// This cannot grant normal launcher success or manufacture a missing log.
    /// The caller supplies its already-fixed first-failure/stage deadline.
    pub fn retire_custody(
        &mut self,
        directory: RawFd,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.refused.get_or_insert_with(|| Failure::capture(cause));
        let deadline = *self.retirement_deadline.insert(
            self.retirement_deadline
                .map_or(deadline, |old| old.min(deadline)),
        );
        let result = self.retire_held(directory, deadline);
        if let Err(error) = &result {
            self.retirement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn retire_held(&mut self, directory: RawFd, deadline: Instant) -> io::Result<QueryRetirement> {
        require(
            Instant::now() < deadline,
            "launcher custody original deadline expired",
        )?;
        for index in 0..2 {
            let fd = if index == 0 {
                self.child.stdout.as_ref().map(AsRawFd::as_raw_fd)
            } else {
                self.child.stderr.as_ref().map(AsRawFd::as_raw_fd)
            }
            .ok_or_else(|| io::Error::other("retained output pipe absent"))?;
            require(
                self.original_pipes[index] == Some(fd),
                "original launcher pipe handle changed",
            )?;
            let identity = stat(fd)?;
            require(
                identity.mode & libc::S_IFMT == libc::S_IFIFO,
                "launcher output is not a pipe",
            )?;
            if let Some((original, identity_before)) = self.pipe_identities[index] {
                require(
                    original == fd && identity.same_owner(&identity_before),
                    "actual retained launcher pipe changed",
                )?;
            } else {
                self.pipe_identities[index] = Some((fd, identity));
            }
            // Finish only nonblocking observation of the original held pipe;
            // no admission, new pidfd, or missing log is acquired here.
            nonblocking(fd)?;
            let bytes = if index == 0 {
                &mut self.stdout
            } else {
                &mut self.stderr
            };
            let outcome = drain(fd, bytes, &mut self.eof[index]);
            if let Err(error) = &outcome {
                self.retirement_failure
                    .get_or_insert_with(|| Failure::capture(error));
            }
            if let Some(log) = &self.log_files[index] {
                let mut offset = self.log_written[index];
                require(
                    offset <= bytes.len(),
                    "launcher log cursor exceeds retained bytes",
                )?;
                while offset < bytes.len() {
                    let n = unsafe {
                        libc::pwrite(
                            log.as_raw_fd(),
                            bytes[offset..].as_ptr().cast(),
                            bytes.len() - offset,
                            offset as i64,
                        )
                    };
                    if n < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    require(n > 0, "launcher log made no progress")?;
                    offset += n as usize;
                    self.log_written[index] = offset;
                }
            }
            // A failed bounded read remains latched, while both original
            // pipes still advance toward actual EOF under this same deadline.
        }
        if self.reaped.is_none() {
            self.reaped = self.child.try_wait()?; // retain irreversible wait before comparisons
        }
        require(
            Instant::now() < deadline,
            "launcher custody original deadline expired",
        )?;
        if self.reaped.is_none() || self.eof != [true, true] {
            return Ok(QueryRetirement::Pending);
        }
        require(
            self.reaped.is_some_and(|s| s.code().is_some()),
            "launcher custody lacks original natural wait",
        )?;
        if let Some(pidfd) = &self.pidfd {
            require(terminal(pidfd.as_raw_fd())?, "launcher remains live")?;
        }
        let pid = self.child.id() as libc::pid_t;
        let group = unsafe { libc::kill(-pid, 0) };
        require(
            group == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "launcher group not positively absent",
        )?;
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        let result = unsafe {
            libc::waitid(
                libc::P_ALL,
                0,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        require(
            result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
            "serial owner still has a child or adopted descendant",
        )?;
        if let Some(failure) = &self.retirement_failure {
            return Err(failure.error());
        }
        self.check_log_directory(directory)?;
        for index in 0..2 {
            if let Some(log) = &self.log_files[index] {
                if !self.held_logs_synced[index] {
                    if unsafe { libc::fsync(log.as_raw_fd()) } != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    self.held_logs_synced[index] = true;
                }
            }
        }
        if unsafe { libc::fsync(directory) } != 0 {
            return Err(io::Error::last_os_error());
        }
        require(
            Instant::now() < deadline,
            "launcher custody original deadline expired",
        )?;
        // `logs_synced` retains its original complete-log meaning. Partial
        // custody has a separate per-original-file fsync readback below.
        if self.initialized
            && self.log_files.iter().all(Option::is_some)
            && self.held_logs_synced == [true, true]
        {
            self.logs_synced = true;
        }
        self.custody_retired = true;
        Ok(QueryRetirement::Retired)
    }
    fn check_log_directory(&self, directory: RawFd) -> io::Result<()> {
        let (original, identity) = self
            .log_directory
            .ok_or_else(|| io::Error::other("launcher original log directory absent"))?;
        require(
            identity.mode & libc::S_IFMT == libc::S_IFDIR
                && directory == original
                && stat(directory)?.same_owner(&identity),
            "launcher original log directory changed",
        )
    }
    pub fn custody_evidence(&self) -> serde_json::Value {
        use std::os::unix::process::ExitStatusExt;
        serde_json::json!({"pid":self.child.id(),"pidfd_held":self.pidfd.is_some(),
            "initialization_attempted":self.initialization_attempted,"initialized":self.initialized,
            "original_pipes":self.original_pipes,"eof":self.eof,
            "stdout":super::hex(&self.stdout),"stderr":super::hex(&self.stderr),
            "raw_wait_status":self.reaped.map(ExitStatus::into_raw),
            "wait_code":self.reaped.and_then(|s|s.code()),
            "log_files_held":self.log_files.each_ref().map(Option::is_some),
            "log_written":self.log_written,"held_logs_synced":self.held_logs_synced,
            "logs_synced":self.logs_synced,"custody_retired":self.custody_retired,
            "first_failure":self.refused.as_ref().map(|f|serde_json::json!({"errno":f.errno,"message":f.message})),
            "retirement_failure":self.retirement_failure.as_ref().map(|f|serde_json::json!({"errno":f.errno,"message":f.message}))})
    }
}

/// A duplicate of the actual retained source launcher pidfd. This does not own
/// its wait: the original Launcher must remain in the outer recovery scope.
#[derive(Debug)]
pub(super) struct LauncherLease {
    pid: libc::pid_t,
    pidfd: OwnedFd,
}
impl LauncherLease {
    pub(super) fn held_descriptor(&self) -> RawFd {
        self.pidfd.as_raw_fd()
    }
    /// The caller retains its actual Child and this output slot before capture.
    /// This lends no wait ownership and accepts no caller-provided numeric PID.
    pub(super) fn capture_child(child: &Child, slot: &mut Option<Self>) -> io::Result<()> {
        require(
            slot.is_none(),
            "original wrapper lease cannot be recaptured",
        )?;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id() as i32, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        *slot = Some(Self {
            pid: child.id() as i32,
            pidfd: unsafe { OwnedFd::from_raw_fd(raw as i32) },
        });
        // Retention precedes every native check; the actual outer Child owner
        // remains responsible for wait, pipes, group and first-failure cleanup.
        let lease = slot.as_ref().unwrap();
        require(
            !terminal(lease.pidfd.as_raw_fd())?,
            "original wrapper already terminal",
        )?;
        lease.check_live()
    }
    pub fn check_live(&self) -> io::Result<()> {
        pidfd_matches(self.pidfd.as_raw_fd(), self.pid)?;
        require(
            unsafe { libc::getpgid(self.pid) } == self.pid,
            "source launcher lost its original process group",
        )
    }
}
impl LauncherLease {
    fn check_failed_unreaped(&self, expected_status: i32) -> io::Result<()> {
        require(
            expected_status > 0 && terminal(self.pidfd.as_raw_fd())?,
            "failed source launcher is not actually terminal",
        )?;
        let info = read_file(
            &format!("/proc/self/fdinfo/{}", self.pidfd.as_raw_fd()),
            4096,
        )?;
        let found: Vec<_> = info
            .lines()
            .filter_map(|s| s.strip_prefix("Pid:"))
            .map(str::trim)
            .collect();
        require(
            found == [self.pid.to_string()] && unsafe { libc::getpgid(self.pid) } == self.pid,
            "failed source launcher lost original unreaped identity",
        )?;
        let mut wait = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as u32,
                &mut wait,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            unsafe { wait.si_pid() } == self.pid
                && wait.si_code == libc::CLD_EXITED
                && unsafe { wait.si_status() } == expected_status,
            "failed source launcher exact original wait differs",
        )
    }
}
impl Launcher {
    pub fn source_lease(&self) -> io::Result<LauncherLease> {
        require(self.reaped.is_none(), "source launcher already waited")?;
        let original = self
            .pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("source launcher original pidfd absent"))?;
        pidfd_matches(original.as_raw_fd(), self.child.id() as i32)?;
        let fd = unsafe { libc::fcntl(original.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let lease = LauncherLease {
            pid: self.child.id() as i32,
            pidfd: unsafe { OwnedFd::from_raw_fd(fd) },
        };
        lease.check_live()?;
        Ok(lease)
    }
}

/// Created only by a retained query whose real child exited zero and whose
/// exact stdout/stderr pipes reached EOF within the original stage deadline.
#[derive(Debug)]
pub(super) struct ManagerSnapshot {
    unit: String,
    properties: BTreeMap<String, String>,
}
/// Custody state is deliberately separate from every successful query Snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueryRetirement {
    NoChild,
    Pending,
    Retired,
}
#[derive(Debug)]
struct CommandQuery {
    arguments: Vec<String>,
    child: Option<Child>,
    pidfd: Option<OwnedFd>,
    deadline: Option<Instant>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    eof: [bool; 2],
    pub reaped: Option<ExitStatus>,
    original_pipes: [Option<RawFd>; 2],
    pipe_identities: [Option<FileIdentity>; 2],
    initialized: bool,
    refused: Option<Failure>,
    retirement_deadline: Option<Instant>,
    retirement_failure: Option<Failure>,
    custody_retired: bool,
    // Set only at the existing final successful poll sample, never by cleanup,
    // EOF/wait reconstruction, parsing diagnostics, or a later export request.
    completed: Option<QueryCompletion>,
    successful_resource_retirement: Option<SuccessfulQueryResourceRetirement>,
}
#[derive(Debug)]
struct QueryResourceClose {
    fd: RawFd,
    description: serde_json::Value,
    attempted: bool,
    raw: Option<i32>,
    errno: Option<i32>,
}
#[derive(Debug)]
struct SuccessfulQueryResourceRetirement {
    // Sampled from CompletedQuery while its actual three descriptions are held.
    // This record cannot be exported as a new CompletedQuery after their close.
    original: serde_json::Value,
    closes: Vec<QueryResourceClose>,
    complete: bool,
}
#[derive(Debug)]
struct QueryCompletion {
    sampled: Instant,
    cutoff: Instant,
}
/// Borrow of the original successful command owner. It is not a parsed manager
/// snapshot and cannot create Creator or manager-command authority.
#[derive(Debug)]
pub(super) struct CompletedQuery<'a> {
    original: &'a CommandQuery,
}
impl CompletedQuery<'_> {
    pub fn rights(&self) -> [BorrowedFd<'_>; 3] {
        let child = self.original.child.as_ref().unwrap();
        [
            self.original.pidfd.as_ref().unwrap().as_fd(),
            child.stdout.as_ref().unwrap().as_fd(),
            child.stderr.as_ref().unwrap().as_fd(),
        ]
    }
    pub fn record(&self) -> io::Result<serde_json::Value> {
        let mut record = self.original.evidence();
        record["completed_with_original_cutoff"] = serde_json::json!(true);
        record["original_descriptions"] = serde_json::to_value(
            self.rights()
                .iter()
                .map(|fd| describe_fd(fd.as_raw_fd()))
                .collect::<io::Result<Vec<_>>>()?,
        )?;
        Ok(record)
    }
}
impl CommandQuery {
    fn retain(arguments: Vec<String>) -> Self {
        Self {
            arguments,
            child: None,
            pidfd: None,
            deadline: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            eof: [false; 2],
            reaped: None,
            original_pipes: [None; 2],
            pipe_identities: [None; 2],
            initialized: false,
            refused: None,
            retirement_deadline: None,
            retirement_failure: None,
            custody_retired: false,
            completed: None,
            successful_resource_retirement: None,
        }
    }
    fn evidence(&self) -> serde_json::Value {
        use std::os::unix::process::ExitStatusExt;
        let mut record = serde_json::json!({"argv":self.arguments,"pid":self.child.as_ref().map(Child::id),
            "pidfd_held":self.pidfd.is_some(),"stdout":super::hex(&self.stdout),"stderr":super::hex(&self.stderr),
            "eof":self.eof,"wait_code":self.reaped.and_then(|s|s.code()),"original_query_bound_seconds":2,
            "raw_wait_status":self.reaped.map(ExitStatus::into_raw),
            "original_query_origin_present":self.deadline.is_some(),"initialized":self.initialized,
            "original_pipes":self.original_pipes,"custody_retired":self.custody_retired,
            "retirement_started":self.retirement_deadline.is_some(),
            "first_failure":self.refused.as_ref().map(|f|serde_json::json!({"errno":f.errno,"message":f.message})),
            "retirement_failure":self.retirement_failure.as_ref().map(|f|serde_json::json!({"errno":f.errno,"message":f.message}))});
        if let Some(retirement) = &self.successful_resource_retirement {
            record["successful_resource_retirement"] = serde_json::json!({"original":retirement.original,
                "complete":retirement.complete,"closes":retirement.closes.iter().map(|row|serde_json::json!({
                    "fd":row.fd,"description":row.description,"attempted":row.attempted,"raw":row.raw,"errno":row.errno})).collect::<Vec<_>>()});
        }
        record
    }
    pub fn start(&mut self) -> io::Result<()> {
        let mut command = Command::new(super::super::capability_unit::CAPABILITY_SUDO);
        command
            .env_clear()
            .envs(
                super::super::capability_unit::CAPABILITY_ENVIRONMENT
                    .iter()
                    .copied(),
            )
            .args(&self.arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // The shared initializer immediately follows retained spawn. Its native
        // EMFILE control uses these same two steps around an actual rlimit.
        self.spawn_retained(&mut command)?;
        self.initialize_child()
    }
    fn spawn_retained(&mut self, command: &mut Command) -> io::Result<()> {
        let result = (|| {
            require(
                self.child.is_none()
                    && self.deadline.is_none()
                    && self.refused.is_none()
                    && self.retirement_deadline.is_none(),
                "manager query cannot restart",
            )?;
            self.deadline = Some(Instant::now() + std::time::Duration::from_secs(2));
            self.child = Some(command.spawn()?); // own before any fallible setup
            let child = self.child.as_ref().unwrap();
            self.original_pipes = [
                child.stdout.as_ref().map(AsRawFd::as_raw_fd),
                child.stderr.as_ref().map(AsRawFd::as_raw_fd),
            ];
            Ok(())
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn initialize_child(&mut self) -> io::Result<()> {
        let result = (|| {
            require(
                !self.initialized
                    && self.pidfd.is_none()
                    && self.refused.is_none()
                    && self.retirement_deadline.is_none(),
                "manager query cannot recapture setup",
            )?;
            let child = self
                .child
                .as_ref()
                .ok_or_else(|| io::Error::other("manager query was not started"))?;
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            self.pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
            // Even a fast terminal child is the original unreaped direct Child.
            require(
                filesystem(raw)? == 0x5049_4446,
                "manager query pidfd type differs",
            )?;
            for (index, pipe) in [
                child.stdout.as_ref().map(AsRawFd::as_raw_fd),
                child.stderr.as_ref().map(AsRawFd::as_raw_fd),
            ]
            .into_iter()
            .enumerate()
            {
                let fd = pipe
                    .ok_or_else(|| io::Error::other("actual manager query output pipe absent"))?;
                require(
                    self.original_pipes[index] == Some(fd),
                    "original manager query pipe handle changed",
                )?;
                let identity = stat(fd)?;
                require(
                    identity.mode & libc::S_IFMT == libc::S_IFIFO,
                    "manager query output is not a pipe",
                )?;
                self.pipe_identities[index] = Some(identity);
                nonblocking(fd)?;
            }
            self.initialized = true;
            Ok(())
        })();
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn drain_held(&mut self, custody_only: bool) -> io::Result<()> {
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| io::Error::other("manager query was not started"))?;
        for (index, pipe) in [
            child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            child.stderr.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .enumerate()
        {
            let fd =
                pipe.ok_or_else(|| io::Error::other("actual manager query output pipe absent"))?;
            require(
                self.original_pipes[index] == Some(fd),
                "original manager query pipe handle changed",
            )?;
            let identity = stat(fd)?;
            require(
                identity.mode & libc::S_IFMT == libc::S_IFIFO,
                "manager query output is not a pipe",
            )?;
            if let Some(before) = self.pipe_identities[index] {
                require(
                    identity.same_owner(&before),
                    "actual retained manager query pipe changed",
                )?;
            } else {
                self.pipe_identities[index] = Some(identity);
            }
            nonblocking(fd)?;
            let bytes = if index == 0 {
                &mut self.stdout
            } else {
                &mut self.stderr
            };
            let outcome = drain(fd, bytes, &mut self.eof[index]);
            if let Err(error) = &outcome {
                // Preserve an original overflow/read refusal across the later
                // custody-only path; EOF cannot recreate discarded bytes.
                self.retirement_failure
                    .get_or_insert_with(|| Failure::capture(error));
            }
            // Cleanup continues bounded observation even after a latched
            // truncation; final custody success remains permanently refused.
            if !custody_only {
                outcome?;
            }
        }
        Ok(())
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<bool> {
        let result = self.poll_admitted(deadline);
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn poll_admitted(&mut self, deadline: Instant) -> io::Result<bool> {
        let deadline = deadline.min(
            self.deadline
                .ok_or_else(|| io::Error::other("manager query original origin absent"))?,
        );
        require(
            Instant::now() < deadline,
            "original manager query deadline expired",
        )?;
        require(
            self.initialized && self.refused.is_none() && self.retirement_deadline.is_none(),
            "manager query setup incomplete or refused",
        )?;
        self.drain_held(false)?;
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("manager query was not started"))?;
        if self.reaped.is_none() {
            self.reaped = child.try_wait()?;
        }
        if self.reaped.is_none() || self.eof != [true, true] {
            return Ok(false);
        }
        require(
            self.reaped.unwrap().success() && self.stderr.is_empty(),
            "actual manager query failed",
        )?;
        require(
            terminal(
                self.pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("manager pidfd absent"))?
                    .as_raw_fd(),
            )?,
            "manager query remains live",
        )?;
        let group = unsafe { libc::kill(-(child.id() as i32), 0) };
        require(
            group == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "manager query left a live process group",
        )?;
        let sampled = Instant::now();
        require(
            sampled < deadline,
            "metadata query completed after original deadline",
        )?;
        self.completed.get_or_insert(QueryCompletion {
            sampled,
            cutoff: deadline,
        });
        Ok(true)
    }
    fn completed_custody(&self, deadline: Instant) -> io::Result<CompletedQuery<'_>> {
        require(
            Instant::now() < deadline,
            "completed query export exceeded original stage",
        )?;
        let completed = self
            .completed
            .as_ref()
            .ok_or_else(|| io::Error::other("query lacks original successful completion sample"))?;
        let original = self
            .deadline
            .ok_or_else(|| io::Error::other("completed query original origin absent"))?;
        require(
            completed.sampled < completed.cutoff
                && completed.cutoff <= original
                && self.initialized
                && self.refused.is_none()
                && self.retirement_deadline.is_none()
                && self.retirement_failure.is_none()
                && !self.custody_retired
                && self.successful_resource_retirement.is_none()
                && self.eof == [true, true]
                && self.reaped.is_some_and(|s| s.code() == Some(0))
                && self.stderr.is_empty()
                && self.stdout.len() <= 1_048_576,
            "query completion custody incomplete or refused",
        )?;
        let child = self
            .child
            .as_ref()
            .ok_or_else(|| io::Error::other("completed query child absent"))?;
        for (index, pipe) in [
            child.stdout.as_ref().map(AsRawFd::as_raw_fd),
            child.stderr.as_ref().map(AsRawFd::as_raw_fd),
        ]
        .into_iter()
        .enumerate()
        {
            let fd =
                pipe.ok_or_else(|| io::Error::other("completed query original pipe absent"))?;
            let identity = self.pipe_identities[index]
                .ok_or_else(|| io::Error::other("completed query original pipe identity absent"))?;
            require(
                self.original_pipes[index] == Some(fd)
                    && stat(fd)?.same_owner(&identity)
                    && identity.mode & libc::S_IFMT == libc::S_IFIFO,
                "completed query original pipe changed",
            )?;
        }
        require(
            terminal(
                self.pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("completed query original pidfd absent"))?
                    .as_raw_fd(),
            )?,
            "completed query pidfd remains live",
        )?;
        let group = unsafe { libc::kill(-(child.id() as i32), 0) };
        require(
            group == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "completed query left a live process group",
        )?;
        require(
            Instant::now() < deadline,
            "completed query export exceeded original stage",
        )?;
        Ok(CompletedQuery { original: self })
    }
    fn retire_successful_resources(&mut self, deadline: Instant) -> io::Result<()> {
        let result = (|| {
            require(
                self.successful_resource_retirement.is_none(),
                "successful query resources cannot be retired twice",
            )?;
            let cutoff = deadline.min(
                self.deadline
                    .ok_or_else(|| io::Error::other("successful query original cutoff absent"))?,
            );
            let original = self.completed_custody(cutoff)?.record()?;
            self.successful_resource_retirement = Some(SuccessfulQueryResourceRetirement {
                original,
                closes: Vec::new(),
                complete: false,
            });
            // Each current native description is still owned when its receipt
            // is installed. Only then is its Rust owner consumed for one real
            // close; a failed/unknown close is retained and never retried.
            for index in 0..3 {
                require(
                    Instant::now() < cutoff,
                    "successful query resource retirement exceeded original cutoff",
                )?;
                let fd = match index {
                    0 => self.pidfd.as_ref().unwrap().as_raw_fd(),
                    1 => self
                        .child
                        .as_ref()
                        .unwrap()
                        .stdout
                        .as_ref()
                        .unwrap()
                        .as_raw_fd(),
                    _ => self
                        .child
                        .as_ref()
                        .unwrap()
                        .stderr
                        .as_ref()
                        .unwrap()
                        .as_raw_fd(),
                };
                let description = serde_json::to_value(describe_fd(fd)?)?;
                let retirement = self.successful_resource_retirement.as_mut().unwrap();
                require(
                    description == retirement.original["original_descriptions"][index],
                    "successful query original description changed before close",
                )?;
                retirement.closes.push(QueryResourceClose {
                    fd,
                    description,
                    attempted: false,
                    raw: None,
                    errno: None,
                });
                let transferred = match index {
                    0 => self.pidfd.take().unwrap().into_raw_fd(),
                    1 => self
                        .child
                        .as_mut()
                        .unwrap()
                        .stdout
                        .take()
                        .unwrap()
                        .into_raw_fd(),
                    _ => self
                        .child
                        .as_mut()
                        .unwrap()
                        .stderr
                        .take()
                        .unwrap()
                        .into_raw_fd(),
                };
                let row = retirement.closes.last_mut().unwrap();
                row.fd = transferred;
                require(
                    transferred == fd,
                    "successful query actual transferred description differs",
                )?;
                row.attempted = true;
                let raw = unsafe { libc::close(transferred) };
                let errno = if raw < 0 {
                    io::Error::last_os_error().raw_os_error()
                } else {
                    None
                };
                row.raw = Some(raw);
                row.errno = errno;
                if raw < 0 {
                    return Err(io::Error::from_raw_os_error(errno.unwrap_or(libc::EIO)));
                }
                require(
                    raw == 0,
                    "successful query close returned unexpected result",
                )?;
            }
            require(
                Instant::now() < cutoff,
                "successful query resources closed after original cutoff",
            )?;
            self.successful_resource_retirement
                .as_mut()
                .unwrap()
                .complete = true;
            Ok(())
        })();
        if let Err(error) = &result {
            self.retirement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn successful_resources_retired(&self) -> io::Result<bool> {
        let Some(retirement) = &self.successful_resource_retirement else {
            return Ok(false);
        };
        if let Some(failure) = &self.retirement_failure {
            return Err(failure.error());
        }
        require(
            retirement.complete
                && retirement.closes.len() == 3
                && retirement
                    .closes
                    .iter()
                    .all(|row| row.attempted && row.raw == Some(0) && row.errno.is_none())
                && self.pidfd.is_none()
                && self
                    .child
                    .as_ref()
                    .is_some_and(|child| child.stdout.is_none() && child.stderr.is_none()),
            "successful query resource retirement remains incomplete",
        )?;
        Ok(true)
    }
    fn retire_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        require(
            self.successful_resource_retirement.is_none(),
            "successful query resource retirement is distinct from failed custody",
        )?;
        self.refused.get_or_insert_with(|| Failure::capture(cause));
        let deadline = self
            .deadline
            .map_or(deadline, |original| original.min(deadline));
        let deadline = *self.retirement_deadline.insert(
            self.retirement_deadline
                .map_or(deadline, |old| old.min(deadline)),
        );
        let result = self.retire_held(deadline);
        if let Err(error) = &result {
            self.retirement_failure
                .get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn retire_held(&mut self, deadline: Instant) -> io::Result<QueryRetirement> {
        require(
            Instant::now() < deadline,
            "manager query custody original deadline expired",
        )?;
        if self.child.is_none() {
            require(
                self.pidfd.is_none()
                    && self.original_pipes == [None, None]
                    && self.eof == [false, false]
                    && self.reaped.is_none()
                    && self.stdout.is_empty()
                    && self.stderr.is_empty(),
                "no-child query contains acquired child state",
            )?;
            return Ok(QueryRetirement::NoChild);
        }
        self.drain_held(true)?;
        let child = self.child.as_mut().unwrap();
        if self.reaped.is_none() {
            self.reaped = child.try_wait()?;
        } // retain before any comparison
        require(
            Instant::now() < deadline,
            "manager query custody original deadline expired",
        )?;
        if self.reaped.is_none() || self.eof != [true, true] {
            return Ok(QueryRetirement::Pending);
        }
        require(
            self.reaped.is_some_and(|s| s.code().is_some()),
            "manager query custody lacks natural wait",
        )?;
        if let Some(pidfd) = &self.pidfd {
            require(terminal(pidfd.as_raw_fd())?, "manager query remains live")?;
        }
        let group = unsafe { libc::kill(-(child.id() as i32), 0) };
        require(
            group == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
            "manager query left a live process group",
        )?;
        require(
            Instant::now() < deadline,
            "manager query custody original deadline expired",
        )?;
        if let Some(failure) = &self.retirement_failure {
            return Err(failure.error());
        }
        self.custody_retired = true;
        Ok(QueryRetirement::Retired)
    }
}
#[derive(Debug)]
pub(super) struct ManagerQuery {
    unit: String,
    query: CommandQuery,
}
impl ManagerQuery {
    /// Actual currently owned handles only; no query success or retirement is
    /// inferred from this descriptive descriptor inventory.
    pub(super) fn held_descriptors(&self) -> Vec<RawFd> {
        let mut fds = Vec::new();
        if let Some(fd) = &self.query.pidfd {
            fds.push(fd.as_raw_fd());
        }
        if let Some(child) = &self.query.child {
            if let Some(fd) = &child.stdout {
                fds.push(fd.as_raw_fd());
            }
            if let Some(fd) = &child.stderr {
                fds.push(fd.as_raw_fd());
            }
        }
        if let Some(retirement) = &self.query.successful_resource_retirement {
            fds.extend(
                retirement
                    .closes
                    .iter()
                    .filter(|row| row.raw != Some(0))
                    .map(|row| row.fd),
            );
        }
        fds
    }
    pub(super) fn retire_successful_resources(&mut self, deadline: Instant) -> io::Result<()> {
        self.query.retire_successful_resources(deadline)
    }
    pub(super) fn successful_resources_retired(&self) -> io::Result<bool> {
        self.query.successful_resources_retired()
    }
    pub fn completed_custody(&self, deadline: Instant) -> io::Result<CompletedQuery<'_>> {
        self.query.completed_custody(deadline)
    }
    /// Retire only actual query custody under the caller's original fixed bound.
    /// No successful snapshot can be obtained through this path.
    pub fn retire_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.query.retire_custody(deadline, cause)
    }

    pub fn evidence(&self) -> serde_json::Value {
        self.query.evidence()
    }
    pub fn retain(unit: String) -> Self {
        let query = CommandQuery::retain(vec![
            "-n".into(),
            "/usr/bin/systemctl".into(),
            "show".into(),
            unit.clone(),
            format!("--property={PROPERTIES}"),
        ]);
        Self { unit, query }
    }
    pub fn start(&mut self) -> io::Result<()> {
        self.query.start()
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<Option<ManagerSnapshot>> {
        if !self.query.poll(deadline)? {
            return Ok(None);
        }
        let text = std::str::from_utf8(&self.query.stdout).map_err(io::Error::other)?;
        let mut properties = BTreeMap::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| io::Error::other("malformed manager property"))?;
            require(
                properties
                    .insert(key.to_owned(), value.to_owned())
                    .is_none(),
                "duplicate manager property",
            )?;
        }
        require(
            text.is_ascii()
                && text.ends_with('\n')
                && properties.len() == PROPERTIES.split(',').count()
                && PROPERTIES
                    .split(',')
                    .all(|name| properties.contains_key(name)),
            "manager property population or framing differs",
        )?;
        Ok(Some(ManagerSnapshot {
            unit: self.unit.clone(),
            properties,
        }))
    }
}

/// A stop command can only be started against a retained, actually terminal
/// creator after its same-InvocationID terminal snapshot. Its subprocess owner
/// is installed in the outer Holder before spawning; dropping an operation
/// never drops a temporary command owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopStart {
    Pending,
    Started,
}
#[derive(Debug)]
pub(super) struct ManagerStop {
    query: CommandQuery,
    attempted: bool,
    started: bool,
    start_deadline: Option<Instant>,
    refused: Option<Failure>,
}
impl ManagerStop {
    pub fn completed_custody(&self, deadline: Instant) -> io::Result<CompletedQuery<'_>> {
        require(
            self.attempted && self.started && self.refused.is_none(),
            "completed source stop was never successfully started",
        )?;
        self.query.completed_custody(deadline)
    }
    pub fn retain(creator: &Creator) -> Self {
        Self {
            query: CommandQuery::retain(vec![
                "-n".into(),
                "/usr/bin/systemctl".into(),
                "stop".into(),
                creator.unit.clone(),
            ]),
            attempted: false,
            started: false,
            start_deadline: None,
            refused: None,
        }
    }
    pub fn started(&self) -> io::Result<bool> {
        if let Some(failure) = &self.refused {
            return Err(failure.error());
        }
        Ok(self.started)
    }
    // A retained owner's failed durable intent is as irreversible as a failed
    // check/spawn. Holder calls this before returning any post-retention error.
    pub fn refuse(&mut self, error: &io::Error) {
        self.attempted = true;
        self.refused.get_or_insert_with(|| Failure::capture(error));
    }
    /// Pending is an observation of the original linked cgroup, never an
    /// attempted command. Only a complete proof in this call reaches start.
    pub fn try_start(
        &mut self,
        creator: &Creator,
        snapshot: &ManagerSnapshot,
        launcher: &LauncherLease,
        deadline: Instant,
    ) -> io::Result<StopStart> {
        let exact_unit = self.query.arguments.last() == Some(&creator.unit);
        self.try_start_checked(
            deadline,
            || {
                creator.check_terminal_manager(snapshot, false)?;
                require(
                    snapshot.properties.get("ActiveState").map(String::as_str) == Some("active")
                        && snapshot.properties.get("SubState").map(String::as_str)
                            == Some("exited")
                        && snapshot.properties.get("MainPID").map(String::as_str) == Some("0"),
                    "source unit is not the retained active exited invocation",
                )?;
                launcher.check_live()?;
                require(exact_unit, "retained source stop owner changed")?;
                // This is the sole fresh cgroup terminal proof for this attempt.
                // No second strict read can race it before the one-shot spawn.
                creator.readback_progress()?.terminal_progress()
            },
            CommandQuery::start,
        )
    }
    // The private transition is shared with controlled-premise qualification;
    // production supplies only the native check above and CommandQuery::start.
    fn try_start_checked(
        &mut self,
        deadline: Instant,
        check: impl FnOnce() -> io::Result<TerminalProgress>,
        start: impl FnOnce(&mut CommandQuery) -> io::Result<()>,
    ) -> io::Result<StopStart> {
        if let Some(failure) = &self.refused {
            return Err(failure.error());
        }
        let result = (|| {
            require(
                !self.attempted && !self.started,
                "retained source stop reused or late",
            )?;
            let deadline = *self.start_deadline.insert(
                self.start_deadline
                    .map_or(deadline, |original| original.min(deadline)),
            );
            require(
                Instant::now() < deadline,
                "retained source stop reused or late",
            )?;
            match check()? {
                TerminalProgress::Pending => return Ok(StopStart::Pending),
                TerminalProgress::Complete => {}
            }
            require(
                Instant::now() < deadline,
                "retained source stop reused or late",
            )?;
            self.attempted = true;
            start(&mut self.query)?;
            self.started = true;
            Ok(StopStart::Started)
        })();
        if let Err(error) = &result {
            self.refuse(error);
        }
        result
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<bool> {
        if let Some(failure) = &self.refused {
            return Err(failure.error());
        }
        let result = (|| {
            require(self.started, "source stop command was not started")?;
            let deadline = deadline.min(
                self.start_deadline
                    .ok_or_else(|| io::Error::other("source stop original cutoff absent"))?,
            );
            self.query.poll(deadline)
        })();
        if let Err(error) = &result {
            self.refuse(error);
        }
        result
    }
    pub fn evidence(&self) -> serde_json::Value {
        self.query.evidence()
    }
}
/// Failed transient units retain manager state after stop. Forget only a proved
/// failed original invocation after persisting that failure in both owners.
/// reset-failed is namespace retirement, never successful source evidence.
#[derive(Debug)]
pub(super) struct ManagerForgetFailed {
    query: CommandQuery,
    attempted: bool,
}
impl ManagerForgetFailed {
    pub fn retain(creator: &Creator) -> Self {
        Self {
            query: CommandQuery::retain(vec![
                "-n".into(),
                "/usr/bin/systemctl".into(),
                "reset-failed".into(),
                creator.unit.clone(),
            ]),
            attempted: false,
        }
    }
    pub fn start(
        &mut self,
        creator: &Creator,
        snapshot: &ManagerSnapshot,
        launcher: &LauncherLease,
        deadline: Instant,
    ) -> io::Result<()> {
        require(
            !self.attempted && Instant::now() < deadline,
            "failed unit retirement reused or late",
        )?;
        self.attempted = true;
        creator.check_terminal_snapshot(snapshot, false)?;
        let p = &snapshot.properties;
        require(
            p.get("ActiveState").map(String::as_str) == Some("failed")
                && p.get("SubState").map(String::as_str) == Some("failed")
                && p.get("MainPID").map(String::as_str) == Some("0")
                && p.get("ExecMainCode").map(String::as_str) == Some("1")
                && p.get("Result").map(String::as_str) == Some("exit-code")
                && creator.readback()?.unlinked,
            "failed unit retirement lacks actual original failed empty source",
        )?;
        let status = p
            .get("ExecMainStatus")
            .unwrap()
            .parse::<i32>()
            .map_err(io::Error::other)?;
        launcher.check_failed_unreaped(status)?;
        require(
            self.query.arguments.last() == Some(&creator.unit),
            "failed unit retirement owner changed",
        )?;
        self.query.start()
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<bool> {
        self.query.poll(deadline)
    }
    pub fn evidence(&self) -> serde_json::Value {
        self.query.evidence()
    }
}
impl Creator {
    // Static manager policy is checked even if the cgroup is temporarily
    // unreadable. A missing/different invocation never becomes Pending.
    fn check_terminal_manager(&self, snapshot: &ManagerSnapshot, natural: bool) -> io::Result<()> {
        require(
            self.captured && terminal(self.pidfd.as_raw_fd())?,
            "source creator is not actually terminal",
        )?;
        self.check_snapshot(snapshot, false)?;
        let p = &snapshot.properties;
        let code = p.get("ExecMainCode").map(String::as_str);
        let status = p.get("ExecMainStatus").map(String::as_str);
        let result = p.get("Result").map(String::as_str);
        require(
            if natural {
                code == Some("1") && status == Some("0") && result == Some("success")
            } else {
                matches!(code, Some("1" | "2" | "3"))
                    && status.is_some_and(|s| s.parse::<u32>().is_ok())
            },
            "actual source terminal result differs",
        )
    }
    pub fn check_terminal_snapshot_progress(
        &self,
        snapshot: &ManagerSnapshot,
        natural: bool,
    ) -> io::Result<TerminalProgress> {
        self.check_terminal_manager(snapshot, natural)?;
        self.readback_progress()?.terminal_progress()
    }
    pub fn check_terminal_snapshot(
        &self,
        snapshot: &ManagerSnapshot,
        natural: bool,
    ) -> io::Result<()> {
        require(
            self.check_terminal_snapshot_progress(snapshot, natural)? == TerminalProgress::Complete,
            "missing cgroup contents without retained unlink proof",
        )
    }
}
impl ManagerSnapshot {
    pub(super) fn unit(&self) -> &str {
        &self.unit
    }
    pub(super) fn property(&self, key: &str) -> Option<&str> {
        self.properties.get(key).map(String::as_str)
    }
    pub fn evidence(&self) -> serde_json::Value {
        serde_json::json!({"unit":self.unit, "properties":self.properties})
    }
}

/// Held expected image and exact entry argv. Capture precedes source admission;
/// a pathname or claimed digest alone cannot authenticate an executing source.
#[derive(Debug)]
pub(super) struct EntryImage {
    file: OwnedFd,
    arguments: Vec<std::ffi::OsString>,
    identity: Option<FileIdentity>,
    digest: Option<[u8; 32]>,
}
impl EntryImage {
    pub fn retain(file: OwnedFd, arguments: Vec<std::ffi::OsString>) -> Self {
        Self {
            file,
            arguments,
            identity: None,
            digest: None,
        }
    }
    pub fn initialize(&mut self) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;

        use sha2::Digest;
        require(
            self.identity.is_none(),
            "entry image cannot initialize twice",
        )?;
        let before = stat(self.file.as_raw_fd())?;
        self.identity = Some(before);
        require(
            before.mode & libc::S_IFMT == libc::S_IFREG
                && before.mode & 0o111 != 0
                && before.size >= 64
                && before.size <= 512 * 1024 * 1024,
            "expected entry is not a bounded executable regular image",
        )?;
        require(
            !self.arguments.is_empty()
                && self.arguments.iter().all(|a| !a.as_bytes().contains(&0))
                && self
                    .arguments
                    .iter()
                    .map(|a| a.as_bytes().len() + 1)
                    .sum::<usize>()
                    <= 65536,
            "expected entry argv is empty, oversized or contains NUL",
        )?;
        let mut hash = sha2::Sha256::new();
        let mut offset = 0i64;
        let mut buffer = [0u8; 65536];
        while offset < before.size {
            let limit = buffer.len().min((before.size - offset) as usize);
            let n = unsafe {
                libc::pread(
                    self.file.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    limit,
                    offset,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            require(n > 0, "expected executable changed while hashing")?;
            if offset == 0 {
                require(
                    n >= 6 && buffer[..6] == *b"\x7fELF\x02\x01",
                    "expected entry is not a native little-endian ELF",
                )?;
            }
            hash.update(&buffer[..n as usize]);
            offset += n as i64;
        }
        let after = stat(self.file.as_raw_fd())?;
        require(
            before.same_owner(&after) && before.size == after.size,
            "expected executable identity changed",
        )?;
        self.digest = Some(hash.finalize().into());
        Ok(())
    }
    fn authenticate(&self, actual: &EntrySnapshot, creator: &Creator) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let held = stat(self.file.as_raw_fd())?;
        let before = self
            .identity
            .as_ref()
            .ok_or_else(|| io::Error::other("expected entry identity absent"))?;
        require(
            held.same_owner(before)
                && held.size == before.size
                && actual.pid == creator.peer.pid
                && self.digest.as_ref() == Some(&actual.digest),
            "actual creator executable differs from held expected entry",
        )?;
        let file = std::fs::File::open(format!("/proc/{}/cmdline", creator.peer.pid))?;
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes)?;
        let expected: Vec<u8> = self
            .arguments
            .iter()
            .flat_map(|a| a.as_bytes().iter().copied().chain(std::iter::once(0)))
            .collect();
        require(
            bytes == expected,
            "actual creator argv differs from trusted private entry",
        )?;
        pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)
    }
}
/// Constructed only after an actual owned privileged read of the executable
/// behind the continuously live creator PID, not from a peer's image report.
#[derive(Debug)]
pub(super) struct EntrySnapshot {
    pid: libc::pid_t,
    digest: [u8; 32],
}
#[derive(Debug)]
pub(super) struct EntryQuery {
    pid: libc::pid_t,
    query: CommandQuery,
}
impl EntryQuery {
    pub(super) fn retire_successful_resources(&mut self, deadline: Instant) -> io::Result<()> {
        self.query.retire_successful_resources(deadline)
    }
    pub(super) fn successful_resources_retired(&self) -> io::Result<bool> {
        self.query.successful_resources_retired()
    }
    pub fn completed_custody(&self, deadline: Instant) -> io::Result<CompletedQuery<'_>> {
        self.query.completed_custody(deadline)
    }
    /// Retire only actual query custody under the caller's original fixed bound.
    /// No successful snapshot can be obtained through this path.
    pub fn retire_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.query.retire_custody(deadline, cause)
    }

    pub fn evidence(&self) -> serde_json::Value {
        self.query.evidence()
    }

    pub fn retain(pid: libc::pid_t) -> Self {
        Self {
            pid,
            query: CommandQuery::retain(vec![
                "-n".into(),
                "/usr/bin/sha256sum".into(),
                "--".into(),
                format!("/proc/{pid}/exe"),
            ]),
        }
    }
    pub fn start(&mut self) -> io::Result<()> {
        require(self.pid > 1, "invalid actual creator PID")?;
        self.query.start()
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<Option<EntrySnapshot>> {
        if !self.query.poll(deadline)? {
            return Ok(None);
        }
        let output = std::str::from_utf8(&self.query.stdout).map_err(io::Error::other)?;
        let suffix = format!("  /proc/{}/exe\n", self.pid);
        require(
            output.len() == 64 + suffix.len() && output.ends_with(&suffix),
            "actual executable digest query framing differs",
        )?;
        let text = &output[..64];
        require(
            text.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "actual executable digest is malformed",
        )?;
        let mut digest = [0u8; 32];
        for (i, b) in digest.iter_mut().enumerate() {
            *b = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(io::Error::other)?;
        }
        Ok(Some(EntrySnapshot {
            pid: self.pid,
            digest,
        }))
    }
}
const CONTROL_PATHS: [&str; 3] = [
    "/sys/kernel/tracing/kprobe_events",
    "/sys/kernel/tracing/kprobe_profile",
    "/sys/kernel/tracing/events",
];
/// Independently obtained fixed-path metadata. The actual privileged stat
/// child uses no peer-supplied path, shell, dereference option or ambient input.
#[derive(Debug)]
pub(super) struct RoleSnapshot {
    identities: [FileIdentity; 3],
}
#[derive(Debug)]
pub(super) struct RoleQuery {
    query: CommandQuery,
}
impl RoleQuery {
    pub(super) fn retire_successful_resources(&mut self, deadline: Instant) -> io::Result<()> {
        self.query.retire_successful_resources(deadline)
    }
    pub(super) fn successful_resources_retired(&self) -> io::Result<bool> {
        self.query.successful_resources_retired()
    }
    pub fn completed_custody(&self, deadline: Instant) -> io::Result<CompletedQuery<'_>> {
        self.query.completed_custody(deadline)
    }
    /// Retire only actual query custody under the caller's original fixed bound.
    /// No successful snapshot can be obtained through this path.
    pub fn retire_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.query.retire_custody(deadline, cause)
    }

    pub fn evidence(&self) -> serde_json::Value {
        self.query.evidence()
    }

    pub fn retain() -> Self {
        let mut args = vec![
            "-n".into(),
            "/usr/bin/stat".into(),
            "--format=%d %i %f %u %g".into(),
            "--".into(),
        ];
        args.extend(CONTROL_PATHS.into_iter().map(str::to_owned));
        Self {
            query: CommandQuery::retain(args),
        }
    }
    pub fn start(&mut self) -> io::Result<()> {
        self.query.start()
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<Option<RoleSnapshot>> {
        if !self.query.poll(deadline)? {
            return Ok(None);
        }
        let text = std::str::from_utf8(&self.query.stdout).map_err(io::Error::other)?;
        require(
            text.is_ascii() && text.ends_with('\n'),
            "fixed tracefs metadata framing differs",
        )?;
        let lines: Vec<_> = text.lines().collect();
        require(
            lines.len() == 3,
            "fixed tracefs metadata population differs",
        )?;
        let mut result = Vec::new();
        for (i, line) in lines.into_iter().enumerate() {
            let fields: Vec<_> = line.split(' ').collect();
            require(
                fields.len() == 5 && fields.iter().all(|s| !s.is_empty()),
                "fixed tracefs metadata fields differ",
            )?;
            let decimal = |v: &str| -> io::Result<u64> {
                require(
                    v.bytes().all(|b| b.is_ascii_digit()),
                    "metadata decimal malformed",
                )?;
                v.parse().map_err(io::Error::other)
            };
            require(
                fields[2]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "metadata mode malformed",
            )?;
            let mode = u32::from_str_radix(fields[2], 16).map_err(io::Error::other)?;
            let identity = FileIdentity {
                device: decimal(fields[0])?,
                inode: decimal(fields[1])?,
                mode,
                uid: decimal(fields[3])?.try_into().map_err(io::Error::other)?,
                gid: decimal(fields[4])?.try_into().map_err(io::Error::other)?,
                links: 0,
                size: 0,
            };
            require(
                identity.uid == 0
                    && identity.gid == 0
                    && identity.inode != 0
                    && mode & libc::S_IFMT == if i == 2 { libc::S_IFDIR } else { libc::S_IFREG },
                "fixed tracefs role is symlink, wrong type or owner",
            )?;
            result.push(identity);
        }
        Ok(Some(RoleSnapshot {
            identities: result.try_into().unwrap(),
        }))
    }
}

pub(super) fn validate_process_status(status: &str, peer: Credentials) -> io::Result<()> {
    let keys = [
        "Pid",
        "Tgid",
        "Uid",
        "Gid",
        "TracerPid",
        "NoNewPrivs",
        "CapInh",
        "CapPrm",
        "CapEff",
        "CapBnd",
        "CapAmb",
    ];
    let mut fields = BTreeMap::new();
    for line in status.lines() {
        let Some((name, value)) = line.split_once(':') else {
            return Err(io::Error::other("actual process status framing malformed"));
        };
        if keys.contains(&name) {
            require(
                fields.insert(name, value.trim()).is_none(),
                "duplicate actual process authority field",
            )?;
        }
    }
    require(
        fields.len() == keys.len(),
        "actual process authority population incomplete",
    )?;
    let pid = peer.pid.to_string();
    require(
        fields["Pid"] == pid
            && fields["Tgid"] == pid
            && fields["TracerPid"] == "0"
            && fields["NoNewPrivs"] == "1",
        "actual creator PID, tracer or NoNewPrivileges differs",
    )?;
    for (name, expected) in [("Uid", peer.uid), ("Gid", peer.gid)] {
        let values: Vec<_> = fields[name].split_whitespace().collect();
        require(
            values.len() == 4 && values.iter().all(|v| *v == expected.to_string()),
            "actual creator real/effective/saved/filesystem credentials differ",
        )?;
    }
    // Exact shared launch policy: NET_ADMIN, SYS_PTRACE, SYS_RESOURCE, PERFMON,
    // BPF; no DAC_OVERRIDE, SYS_ADMIN or other unrequested privilege.
    const CAPS: u64 = (1 << 12) | (1 << 19) | (1 << 24) | (1 << 38) | (1 << 39);
    for name in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        let value = fields[name];
        require(
            value.len() == 16
                && value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                && u64::from_str_radix(value, 16).map_err(io::Error::other)? == CAPS,
            "actual creator capability set differs from exact original policy",
        )?;
    }
    Ok(())
}

#[derive(Debug)]
pub(super) struct Creator {
    pub pidfd: OwnedFd,
    pub directory: OwnedFd,
    pub peer: Credentials,
    unit: String,
    invocation: String,
    nonce: String,
    cgroup: Option<String>,
    identity: Option<FileIdentity>,
    pub captured: bool,
    pub admitted: bool,
}
#[derive(Debug)]
pub(super) struct CgroupReadback {
    pub creator_terminal: bool,
    pub unlinked: bool,
    pub procs: Option<String>,
    pub events: Option<String>,
}
/// Descriptive observation only. Pending grants no admission, emptiness,
/// unlink, stop/reset, source-retirement, or provider authority.
#[derive(Debug)]
pub(super) enum CgroupReadbackProgress {
    Observed(CgroupReadback),
    Pending(CgroupPending),
}
#[derive(Debug)]
pub(super) struct CgroupPending {
    pub original: FileIdentity,
    pub before: FileIdentity,
    pub after: FileIdentity,
    pub creator_terminal: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalProgress {
    Pending,
    Complete,
}
impl CgroupReadbackProgress {
    pub fn creator_terminal(&self) -> bool {
        match self {
            Self::Observed(value) => value.creator_terminal,
            Self::Pending(value) => value.creator_terminal,
        }
    }
    fn strict(self) -> io::Result<CgroupReadback> {
        match self {
            Self::Observed(value) => Ok(value),
            Self::Pending(_) => Err(io::Error::other(
                "missing cgroup contents without retained unlink proof",
            )),
        }
    }
    fn terminal_progress(self) -> io::Result<TerminalProgress> {
        require(
            self.creator_terminal(),
            "source creator is not actually terminal",
        )?;
        match self {
            Self::Pending(_) => Ok(TerminalProgress::Pending),
            Self::Observed(actual) => {
                require(
                    actual.unlinked
                        || (actual.procs.as_deref() == Some("")
                            && actual
                                .events
                                .as_ref()
                                .is_some_and(|v| v.lines().any(|s| s == "populated 0"))),
                    "source cgroup is still populated",
                )?;
                Ok(TerminalProgress::Complete)
            }
        }
    }
}
/// Read only the supplied retained capabilities. The caller must independently
/// bind their original custody and manager identity; this creates no Creator.
pub(super) fn read_retained_cgroup(
    pidfd: BorrowedFd<'_>,
    directory: BorrowedFd<'_>,
    original: &FileIdentity,
) -> io::Result<CgroupReadbackProgress> {
    let before = stat(directory.as_raw_fd())?;
    require(before.same_object(original), "held cgroup identity changed")?;
    let procs = read_at(directory.as_raw_fd(), "cgroup.procs");
    let events = read_at(directory.as_raw_fd(), "cgroup.events");
    classify_cgroup_readback(
        original,
        before,
        procs,
        events,
        || stat(directory.as_raw_fd()),
        || terminal(pidfd.as_raw_fd()),
    )
}
// Native production and explicitly controlled-premise qualification share the
// classification. Lazy native callbacks preserve the original syscall order.
fn classify_cgroup_readback(
    original: &FileIdentity,
    before: FileIdentity,
    procs: io::Result<String>,
    events: io::Result<String>,
    after: impl FnOnce() -> io::Result<FileIdentity>,
    creator_terminal: impl FnOnce() -> io::Result<bool>,
) -> io::Result<CgroupReadbackProgress> {
    require(before.same_object(original), "held cgroup identity changed")?;
    // Classify BOTH errors. One missing name cannot conceal an unrelated error.
    for result in [&procs, &events] {
        if let Err(error) = result {
            require(
                matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENODEV)),
                "cgroup readback failed without an unlink observation",
            )?;
        }
    }
    match (procs, events) {
        (Ok(procs), Ok(events)) => Ok(CgroupReadbackProgress::Observed(CgroupReadback {
            creator_terminal: creator_terminal()?,
            unlinked: false,
            procs: Some(procs),
            events: Some(events),
        })),
        _ => {
            let after = after()?;
            require(
                after.same_object(original),
                "missing cgroup contents without retained unlink proof",
            )?;
            let creator_terminal = creator_terminal()?;
            if after.links == 0 {
                Ok(CgroupReadbackProgress::Observed(CgroupReadback {
                    creator_terminal,
                    unlinked: true,
                    procs: None,
                    events: None,
                }))
            } else {
                Ok(CgroupReadbackProgress::Pending(CgroupPending {
                    original: *original,
                    before,
                    after,
                    creator_terminal,
                }))
            }
        }
    }
}
impl Creator {
    /// Grammar may fail while Packet still owns every right. After this returns,
    /// the caller installs the candidate before authenticating its descriptors.
    pub fn retain(packet: &mut Packet, intent: &Intent, unit: &str) -> io::Result<Self> {
        require(
            packet.credentials.len() == 1,
            "creator lacks exact kernel credentials",
        )?;
        let peer = packet.credentials[0];
        require(
            peer.pid > 1
                && peer.uid == unsafe { libc::getuid() }
                && peer.gid == unsafe { libc::getgid() }
                && peer.uid != 0
                && peer.uid == unsafe { libc::geteuid() }
                && peer.gid == unsafe { libc::getegid() },
            "creator requires exact non-root same-UID credentials",
        )?;
        packet.exact(2, peer)?;
        let prefix = format!("UNIT_CREATED unit={unit} invocation=");
        let text = std::str::from_utf8(&packet.bytes).map_err(io::Error::other)?;
        let tail = text
            .strip_prefix(&prefix)
            .ok_or_else(|| io::Error::other("creator unit differs"))?;
        let (invocation, suffix) = tail
            .split_once(" pid=")
            .ok_or_else(|| io::Error::other("creator invocation absent"))?;
        require(
            super::valid_nonce(invocation),
            "creator invocation malformed",
        )?;
        require(
            suffix == format!("{} nonce={}\n", peer.pid, intent.nonce),
            "creator nonce or PID differs",
        )?;
        let pidfd = packet.rights.remove(0);
        let directory = packet.rights.remove(0);
        Ok(Self {
            pidfd,
            directory,
            peer,
            unit: unit.to_owned(),
            invocation: invocation.to_owned(),
            nonce: intent.nonce.clone(),
            cgroup: None,
            identity: None,
            captured: false,
            admitted: false,
        })
    }
    pub fn authenticate(
        &mut self,
        manager: &ManagerSnapshot,
        entry: &EntryImage,
        image: &EntrySnapshot,
    ) -> io::Result<()> {
        require(!self.admitted, "creator cannot be authenticated twice")?;
        require(
            filesystem(self.directory.as_raw_fd())? == 0x6367_7270,
            "creator directory is not cgroup2",
        )?;
        let flags = unsafe { libc::fcntl(self.directory.as_raw_fd(), libc::F_GETFL) };
        require(
            flags >= 0 && flags & libc::O_ACCMODE == libc::O_RDONLY,
            "cgroup descriptor must be read-only",
        )?;
        pidfd_matches(self.pidfd.as_raw_fd(), self.peer.pid)?;
        let text = read_file(&format!("/proc/{}/cgroup", self.peer.pid), 4096)?;
        let lines: Vec<_> = text.lines().collect();
        require(lines.len() == 1, "creator cgroup membership ambiguous")?;
        let cgroup = lines[0]
            .strip_prefix("0::")
            .ok_or_else(|| io::Error::other("creator lacks unified membership"))?;
        require(
            cgroup.starts_with('/')
                && cgroup != "/"
                && cgroup.split('/').all(|p| !matches!(p, "." | "..")),
            "creator cgroup path malformed",
        )?;
        self.cgroup = Some(cgroup.to_owned());
        self.identity = Some(stat(self.directory.as_raw_fd())?);
        self.check_snapshot(manager, true)?;
        let path = CString::new(format!("/sys/fs/cgroup{cgroup}")).map_err(io::Error::other)?;
        let mut actual = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::lstat(path.as_ptr(), actual.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let held = self.identity.as_ref().unwrap();
        require(
            held.mode & libc::S_IFMT == libc::S_IFDIR
                && held.same_object(&FileIdentity::from(unsafe { actual.assume_init() })),
            "received cgroup differs from actual creator membership",
        )?;
        let procs = read_at(self.directory.as_raw_fd(), "cgroup.procs")?;
        require(
            procs.lines().any(|p| p == self.peer.pid.to_string())
                && !terminal(self.pidfd.as_raw_fd())?,
            "creator not live in retained cgroup",
        )?;
        for (name, expected) in [
            ("memory.max", "268435456\n"),
            ("memory.swap.max", "0\n"),
            ("pids.max", "8\n"),
            ("cpu.max", "100000 100000\n"),
        ] {
            require(
                read_at(self.directory.as_raw_fd(), name)? == expected,
                "actual original cgroup resource bound differs",
            )?;
        }
        self.captured = true; // original kernel/manager/cgroup custody, before stricter policy
        entry.authenticate(image, self)?;
        self.process_policy()?;
        self.admitted = true;
        Ok(())
    }
    pub fn process_policy(&self) -> io::Result<()> {
        pidfd_matches(self.pidfd.as_raw_fd(), self.peer.pid)?;
        let status = read_file(&format!("/proc/{}/status", self.peer.pid), 16384)?;
        validate_process_status(&status, self.peer)?;
        for (resource, expected) in [
            (
                libc::RLIMIT_NOFILE,
                super::super::capability_unit::CAPABILITY_UNIT_NOFILE,
            ),
            (libc::RLIMIT_FSIZE, 1048576),
            (libc::RLIMIT_CORE, 0),
        ] {
            let mut actual = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            if unsafe {
                libc::prlimit(
                    self.peer.pid,
                    resource,
                    std::ptr::null(),
                    actual.as_mut_ptr(),
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            let actual = unsafe { actual.assume_init() };
            require(
                actual.rlim_cur == expected && actual.rlim_max == expected,
                "actual creator original resource limit differs",
            )?;
        }
        // A held live pidfd brackets every numeric-PID kernel read. The same
        // trusted early entry cannot be replaced by PID reuse during admission.
        pidfd_matches(self.pidfd.as_raw_fd(), self.peer.pid)
    }
    pub fn check_snapshot(&self, manager: &ManagerSnapshot, require_live: bool) -> io::Result<()> {
        let p = &manager.properties;
        let pid = self.peer.pid.to_string();
        require(
            manager.unit == self.unit
                && p.get("Id") == Some(&self.unit)
                && p.get("LoadState").map(String::as_str) == Some("loaded")
                && p.get("InvocationID") == Some(&self.invocation),
            "manager invocation differs from retained creator",
        )?;
        let main = p
            .get("MainPID")
            .ok_or_else(|| io::Error::other("manager MainPID missing"))?;
        let group = self
            .cgroup
            .as_ref()
            .ok_or_else(|| io::Error::other("retained cgroup path absent"))?;
        if require_live {
            require(
                main == &pid
                    && p.get("ExecMainPID") == Some(&pid)
                    && p.get("ControlGroup") == Some(group)
                    && !terminal(self.pidfd.as_raw_fd())?,
                "creator/task/manager/cgroup join unproven",
            )?;
        } else {
            require(
                p.get("ExecMainPID") == Some(&pid)
                    && (main == "0" || main == &pid)
                    && p.get("ControlGroup")
                        .is_some_and(|value| value.is_empty() || value == group),
                "manager creator identity changed since capture",
            )?;
        }
        Ok(())
    }
    pub fn receipt(&self) -> serde_json::Value {
        serde_json::json!({"unit":self.unit,"invocation":self.invocation,"pid":self.peer.pid,"nonce":self.nonce,
            "credentials":{"pid":self.peer.pid,"uid":self.peer.uid,"gid":self.peer.gid}})
    }
    pub fn evidence(&self) -> io::Result<serde_json::Value> {
        require(
            self.admitted,
            "unadmitted creator has no ownership evidence",
        )?;
        let mut value = self.receipt();
        let map = value.as_object_mut().unwrap();
        let identity = self.identity.as_ref().unwrap();
        for (name, value) in [
            ("cgroup", serde_json::json!(self.cgroup)),
            ("device", serde_json::json!(identity.device)),
            ("inode", serde_json::json!(identity.inode)),
            ("creator_pidfd_held", serde_json::json!(true)),
            ("cgroup_directory_held", serde_json::json!(true)),
            ("cgroup_kill_description_held", serde_json::json!(false)),
            ("kind", serde_json::json!("same-uid-readonly-cgroup-v1")),
            ("receiver_uid", serde_json::json!(unsafe { libc::getuid() })),
            (
                "receiver_euid",
                serde_json::json!(unsafe { libc::geteuid() }),
            ),
        ] {
            map.insert(name.to_owned(), value);
        }
        Ok(value)
    }
    /// Captured cleanup identity, not admitted ownership evidence. The actual
    /// cgroup and pidfd stay retained across rejection; this record grants no
    /// SourceTerminal, control-role, image or provider admission authority.
    pub fn captured_custody_identity(&self) -> io::Result<serde_json::Value> {
        require(
            self.captured && self.identity.is_some() && self.cgroup.is_some(),
            "creator cleanup identity was never captured",
        )?;
        self.readback()?;
        require(
            filesystem(self.pidfd.as_raw_fd())? == 0x5049_4446,
            "creator cleanup pidfd type differs",
        )?;
        let mut value = self.receipt();
        let map = value.as_object_mut().unwrap();
        let identity = self.identity.as_ref().unwrap();
        for (name, value) in [
            ("cgroup", serde_json::json!(self.cgroup)),
            ("device", serde_json::json!(identity.device)),
            ("inode", serde_json::json!(identity.inode)),
            ("creator_pidfd_held", serde_json::json!(true)),
            ("cgroup_directory_held", serde_json::json!(true)),
            ("cgroup_kill_description_held", serde_json::json!(false)),
            (
                "kind",
                serde_json::json!("captured-original-cgroup-custody-v1"),
            ),
            ("receiver_uid", serde_json::json!(unsafe { libc::getuid() })),
            (
                "receiver_euid",
                serde_json::json!(unsafe { libc::geteuid() }),
            ),
        ] {
            map.insert(name.to_owned(), value);
        }
        Ok(value)
    }
    pub fn readback_progress(&self) -> io::Result<CgroupReadbackProgress> {
        require(
            self.captured && super::valid_nonce(&self.nonce),
            "creator has no retained admission",
        )?;
        read_retained_cgroup(
            self.pidfd.as_fd(),
            self.directory.as_fd(),
            self.identity
                .as_ref()
                .ok_or_else(|| io::Error::other("creator original identity absent"))?,
        )
    }
    pub fn readback(&self) -> io::Result<CgroupReadback> {
        self.readback_progress()?.strict()
    }
}

#[derive(Debug)]
pub(super) struct Controls {
    pub fds: Vec<OwnedFd>,
    identities: Vec<FileIdentity>,
    flags: Vec<i32>,
    admitted: bool,
}
impl Controls {
    pub fn retain(packet: &mut Packet, creator: &Creator, intent: &Intent) -> io::Result<Self> {
        require(
            creator.admitted && !terminal(creator.pidfd.as_raw_fd())?,
            "roles require a retained live creator",
        )?;
        packet.exact(3, creator.peer)?;
        let expected = serde_json::json!({"schema":"hermit-grouped-roles-v1","nonce":intent.nonce,
            "incarnation":intent.incarnation,"family":"controls","roles":[
                {"role":"CONTROL","path":"/sys/kernel/tracing/kprobe_events","name":"hermit_kprobe_control"},
                {"role":"PROFILE","path":"/sys/kernel/tracing/kprobe_profile","name":"hermit_kprobe_profile"},
                {"role":"EVENTS","path":"/sys/kernel/tracing/events","name":"hermit_trace_events"}]});
        require(
            packet.bytes == super::journal::canonical(&expected)?,
            "manager role envelope changed",
        )?;
        let state = creator.readback()?;
        require(
            !state.creator_terminal
                && !state.unlinked
                && state
                    .procs
                    .as_ref()
                    .is_some_and(|p| p.lines().any(|line| line == creator.peer.pid.to_string())),
            "creator left retained cgroup during role transfer",
        )?;
        Ok(Self {
            fds: std::mem::take(&mut packet.rights),
            identities: Vec::new(),
            flags: Vec::new(),
            admitted: false,
        })
    }
    pub fn authenticate(&mut self, named: &RoleSnapshot) -> io::Result<()> {
        require(
            !self.admitted && self.fds.len() == 3,
            "control descriptions already admitted or incomplete",
        )?;
        for (index, fd) in self.fds.iter().enumerate() {
            let identity = stat(fd.as_raw_fd())?;
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            require(
                identity.same_owner(&named.identities[index])
                    && filesystem(fd.as_raw_fd())? == 0x7472_6163
                    && identity.uid == 0
                    && identity.gid == 0
                    && identity.mode & libc::S_IFMT
                        == (if index == 2 {
                            libc::S_IFDIR
                        } else {
                            libc::S_IFREG
                        })
                    && flags >= 0
                    && flags & libc::O_ACCMODE
                        == (if index == 0 {
                            libc::O_RDWR
                        } else {
                            libc::O_RDONLY
                        })
                    && flags & !(libc::O_ACCMODE | 0o100000 | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                        == 0
                    && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
                "manager role is not its actual bounded root-owned tracefs description",
            )?;
            self.identities.push(identity);
            self.flags.push(flags);
        }
        self.description_matrix()?;
        self.admitted = true;
        Ok(())
    }
    fn description_matrix(&self) -> io::Result<Vec<(usize, usize, i64)>> {
        let mut rows = Vec::new();
        for (i, left) in self.fds.iter().enumerate() {
            for (j, right) in self.fds.iter().enumerate().skip(i) {
                let raw = unsafe {
                    libc::syscall(
                        libc::SYS_kcmp,
                        libc::getpid(),
                        libc::getpid(),
                        0,
                        left.as_raw_fd(),
                        right.as_raw_fd(),
                    )
                };
                require(
                    if i == j {
                        raw == 0
                    } else {
                        (1..=3).contains(&raw)
                    },
                    "manager descriptions alias or OFD comparison failed",
                )?;
                rows.push((i, j, raw));
            }
        }
        Ok(rows)
    }
    pub fn check(&self) -> io::Result<()> {
        protected_holder()?;
        require(self.admitted, "control roles were not admitted")?;
        for (i, fd) in self.fds.iter().enumerate() {
            require(
                stat(fd.as_raw_fd())?.same_owner(&self.identities[i])
                    && unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) } == self.flags[i],
                "retained manager role changed",
            )?;
        }
        self.description_matrix().map(|_| ())
    }
    pub fn evidence(&self) -> io::Result<serde_json::Value> {
        self.check()?;
        Ok(
            serde_json::json!({"identities":self.identities.iter().zip(&self.flags).map(|(s,f)|serde_json::json!({"dev":s.device,"inode":s.inode,"mode":s.mode,"flags":f})).collect::<Vec<_>>(),"description_matrix":self.description_matrix()?}),
        )
    }
    pub fn snapshot_with_lease(
        &self,
        lease: &mut super::adoption::LocalReadLease<'_>,
        index: usize,
        deadline: Instant,
    ) -> io::Result<Vec<u8>> {
        self.snapshot_guarded(index, deadline, || lease.check(self, deadline))
    }
    pub(super) fn snapshot_for_cleanup(
        &self,
        lease: &mut super::cleanup::ReadEpoch<'_>,
        index: usize,
        deadline: Instant,
    ) -> io::Result<Vec<u8>> {
        self.snapshot_guarded(index, deadline, || lease.check(self, deadline))
    }
    fn snapshot_guarded(
        &self,
        index: usize,
        deadline: Instant,
        mut check: impl FnMut() -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        check()?;
        self.check()?;
        require(index < 2, "only control/profile snapshots are supported")?;
        let fd = self.fds[index].as_raw_fd();
        check()?;
        require(
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } == 0,
            "seq-file rewind failed",
        )?;
        let mut bytes = Vec::new();
        loop {
            check()?;
            let mut buffer = [0u8; 4096];
            let cap = buffer.len().min(1_048_577usize.saturating_sub(bytes.len()));
            require(cap > 0, "complete control snapshot exceeded original1MiB")?;
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), cap) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                check()?;
                return Ok(bytes);
            }
            bytes.extend_from_slice(&buffer[..count as usize]);
            require(
                bytes.len() <= 1_048_576,
                "complete control snapshot exceeded original1MiB",
            )?;
        }
    }
}

// Production extension planned for reviewed composition into owner.rs. It
// reuses the existing retained CommandQuery and original failed-wait checks.
pub(super) fn check_parent_failed_launcher(
    launcher: &LauncherLease,
    status: i32,
) -> io::Result<()> {
    launcher.check_failed_unreaped(status)
}
pub(super) fn parent_launcher_terminal(launcher: &LauncherLease) -> io::Result<bool> {
    terminal(launcher.pidfd.as_raw_fd())
}
#[derive(Debug)]
enum ParentForgetState {
    Retained,
    Submitted,
    Refused(Failure),
}
#[derive(Debug)]
pub(super) struct ParentForgetFailed {
    query: CommandQuery,
    state: ParentForgetState,
}
impl ParentForgetFailed {
    pub fn retain(proof: &super::parent_launch::ParentFailedUnitProof<'_>) -> Self {
        Self {
            query: CommandQuery::retain(vec![
                "-n".into(),
                "/usr/bin/systemctl".into(),
                "reset-failed".into(),
                proof.unit().to_owned(),
            ]),
            state: ParentForgetState::Retained,
        }
    }
    pub fn started(&self) -> bool {
        matches!(self.state, ParentForgetState::Submitted)
    }
    pub fn start(
        &mut self,
        proof: super::parent_launch::ParentFailedUnitProof<'_>,
        deadline: Instant,
    ) -> io::Result<()> {
        if let ParentForgetState::Refused(error) = &self.state {
            return Err(error.error());
        }
        let result = (|| {
            require(
                matches!(self.state, ParentForgetState::Retained),
                "parent failed unit reset cannot repeat",
            )?;
            proof.validate_for_start(deadline)?;
            require(
                self.query
                    .arguments
                    .last()
                    .is_some_and(|unit| unit == proof.unit()),
                "parent failed reset unit differs from native proof",
            )?;
            self.state = ParentForgetState::Submitted;
            self.query.start()
        })();
        if let Err(error) = &result {
            self.state = ParentForgetState::Refused(Failure::capture(error));
        }
        result
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<bool> {
        match &self.state {
            ParentForgetState::Refused(error) => return Err(error.error()),
            ParentForgetState::Retained => {
                return Err(io::Error::other("parent failed reset was not submitted"));
            }
            ParentForgetState::Submitted => {}
        }
        let result = self.query.poll(deadline);
        if let Err(error) = &result {
            self.state = ParentForgetState::Refused(Failure::capture(error));
        }
        result
    }
    pub fn retire_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        self.query.retire_custody(deadline, cause)
    }
    pub fn evidence(&self) -> serde_json::Value {
        serde_json::json!({"state":match &self.state{ParentForgetState::Retained=>"retained",ParentForgetState::Submitted=>"submitted",ParentForgetState::Refused(_)=>"refused"},"failure":match &self.state{ParentForgetState::Refused(error)=>Some(&error.message),_=>None},"query":self.query.evidence()})
    }
}

/// Custody of the actual received Creator before either query result admitted
/// any identity. This owns the same original descriptors, without duplication,
/// and never changes the original Creator's captured/admitted flags.
#[derive(Debug)]
pub(super) struct PartialCreatorCustody {
    original: Creator,
    membership: Option<String>,
    directory_identity: Option<FileIdentity>,
    live_verified: bool,
    attempted: bool,
    deadline: Option<Instant>,
    refused: Option<Failure>,
}
impl PartialCreatorCustody {
    pub fn retain(original: Creator) -> Self {
        Self {
            original,
            membership: None,
            directory_identity: None,
            live_verified: false,
            attempted: false,
            deadline: None,
            refused: None,
        }
    }
    pub fn original(&self) -> &Creator {
        &self.original
    }
    fn original_scope(&self) -> io::Result<()> {
        require(
            !self.original.captured
                && !self.original.admitted
                && self.original.identity.is_none()
                && self.original.cgroup.is_none(),
            "partial custody changed original uncaptured Creator state",
        )
    }
    fn cutoff(&self) -> io::Result<Instant> {
        self.original_scope()?;
        let deadline = self
            .deadline
            .ok_or_else(|| io::Error::other("partial custody original deadline absent"))?;
        require(
            Instant::now() < deadline,
            "partial custody original deadline expired",
        )?;
        Ok(deadline)
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refused.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    /// Call while the actual original source is live. This verifies descriptor
    /// correspondence only; it performs no manager query or image admission.
    pub fn begin(&mut self, deadline: Instant) -> io::Result<()> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        let result = (|| {
            require(
                !self.attempted,
                "partial custody live binding cannot repeat",
            )?;
            self.attempted = true;
            self.deadline = Some(deadline);
            self.cutoff()?;
            let source = &self.original;
            require(
                filesystem(source.pidfd.as_raw_fd())? == 0x5049_4446
                    && unsafe { libc::fcntl(source.pidfd.as_raw_fd(), libc::F_GETFD) }
                        == libc::FD_CLOEXEC,
                "partial original pidfd type or CLOEXEC differs",
            )?;
            pidfd_matches(source.pidfd.as_raw_fd(), source.peer.pid)?;
            require(
                filesystem(source.directory.as_raw_fd())? == 0x6367_7270,
                "partial original directory is not cgroup2",
            )?;
            let flags = unsafe { libc::fcntl(source.directory.as_raw_fd(), libc::F_GETFL) };
            require(
                flags >= 0
                    && flags & libc::O_ACCMODE == libc::O_RDONLY
                    && unsafe { libc::fcntl(source.directory.as_raw_fd(), libc::F_GETFD) }
                        == libc::FD_CLOEXEC,
                "partial original cgroup access flags differ",
            )?;
            self.directory_identity = Some(stat(source.directory.as_raw_fd())?);
            let text = read_file(&format!("/proc/{}/cgroup", source.peer.pid), 4096)?;
            let lines: Vec<_> = text.lines().collect();
            require(lines.len() == 1, "partial source membership ambiguous")?;
            let group = lines[0]
                .strip_prefix("0::")
                .ok_or_else(|| io::Error::other("partial source lacks unified membership"))?;
            self.membership = Some(group.to_owned());
            require(
                group.starts_with('/')
                    && group != "/"
                    && group.split('/').all(|p| !matches!(p, "." | "..")),
                "partial source cgroup path malformed",
            )?;
            let path = CString::new(format!("/sys/fs/cgroup{group}")).map_err(io::Error::other)?;
            let mut named = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::lstat(path.as_ptr(), named.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let held = self.directory_identity.as_ref().unwrap();
            require(
                held.mode & libc::S_IFMT == libc::S_IFDIR
                    && held.same_object(&FileIdentity::from(unsafe { named.assume_init() }))
                    && stat(source.directory.as_raw_fd())?.same_object(held),
                "partial received cgroup differs from actual live membership",
            )?;
            require(
                read_at(source.directory.as_raw_fd(), "cgroup.procs")?
                    .lines()
                    .any(|s| s == source.peer.pid.to_string()),
                "partial source absent from original held cgroup",
            )?;
            require(
                read_file(&format!("/proc/{}/cgroup", source.peer.pid), 4096)? == text,
                "partial source membership changed during binding",
            )?;
            pidfd_matches(source.pidfd.as_raw_fd(), source.peer.pid)?;
            self.cutoff()?;
            self.live_verified = true;
            Ok(())
        })();
        self.remember(result)
    }
    pub fn terminal(&mut self, deadline: Instant) -> io::Result<bool> {
        if let Some(error) = &self.refused {
            return Err(error.error());
        }
        let result = (|| {
            self.deadline = self.deadline.map(|fixed| fixed.min(deadline));
            self.cutoff()?;
            require(
                self.live_verified,
                "partial custody lacks original live descriptor binding",
            )?;
            let state = read_retained_cgroup(
                self.original.pidfd.as_fd(),
                self.original.directory.as_fd(),
                self.directory_identity.as_ref().unwrap(),
            )?;
            self.cutoff()?;
            match state {
                CgroupReadbackProgress::Pending(_) => Ok(false),
                CgroupReadbackProgress::Observed(actual) => {
                    Ok(actual.creator_terminal && actual.unlinked)
                }
            }
        })();
        self.remember(result)
    }
    pub fn terminal_record(&mut self, deadline: Instant) -> io::Result<serde_json::Value> {
        require(
            self.terminal(deadline)?,
            "partial original descriptors are not terminal/unlinked",
        )?;
        let identity = self.directory_identity.as_ref().unwrap();
        let source = &self.original;
        Ok(
            serde_json::json!({"kind":"uncaptured-original-descriptor-custody-v1",
            "unit":source.unit,"nonce":source.nonce,"invocation":source.invocation,"pid":source.peer.pid,
            "credentials":{"pid":source.peer.pid,"uid":source.peer.uid,"gid":source.peer.gid},
            "cgroup":self.membership,"device":identity.device,"inode":identity.inode,
            "creator_pidfd_held":true,"cgroup_directory_held":true,"cgroup_kill_description_held":false,
            "creator_captured":source.captured,"creator_admitted":source.admitted,
            "receiver_uid":unsafe{libc::getuid()},"receiver_euid":unsafe{libc::geteuid()}}),
        )
    }
    pub fn diagnostics(&self) -> serde_json::Value {
        serde_json::json!({"original_creator":self.original.receipt(),"creator_captured":self.original.captured,
            "creator_admitted":self.original.admitted,"live_binding_verified":self.live_verified,
            "original_membership_observed":self.membership,"original_directory_identity_observed":self.directory_identity.as_ref().map(|i|serde_json::json!({"device":i.device,"inode":i.inode,"mode":i.mode,"links":i.links})),
            "first_custody_failure":self.refused.as_ref().map(|e|&e.message),"initialization_attempted":self.attempted,
            "deadline_pinned":self.deadline.is_some(),"manager_snapshot_constructed":false,"source_terminal_issued":false})
    }
}

/// Descriptive complete seq-file observations, never Provider or source
/// authority. Only the consuming serial join may use them in its own token.
#[derive(Debug)]
pub(super) struct CreatedObservations {
    definitions: Vec<u8>,
    profile: Vec<u8>,
}
pub(super) fn observe_created(
    lease: &mut super::adoption::LocalReadLease<'_>,
    intent: &Intent,
) -> io::Result<CreatedObservations> {
    let definitions = lease.snapshot(0)?;
    let profile = lease.snapshot(1)?;
    require(
        definition_mask(intent, &definitions)? == 0x1ffff,
        "fresh definitions lack exactly seventeen owned sites",
    )?;
    profile_count(intent, &profile, 17)?;
    Ok(CreatedObservations {
        definitions,
        profile,
    })
}
fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
/// Exact Rust counterpart of unchanged ap_grouped_census. Foreign names are
/// parsed too: another group cannot hide a collision on our profile event.
fn definition_mask(intent: &Intent, bytes: &[u8]) -> io::Result<u32> {
    require(
        bytes.len() <= 1_048_576,
        "definition census exceeds original1MiB",
    )?;
    let mut seen = 0u32;
    let mut at = 0;
    while at < bytes.len() {
        let end = at
            + bytes[at..]
                .iter()
                .position(|b| *b == b'\n')
                .ok_or_else(|| io::Error::other("definition census lacks final newline"))?;
        let row = &bytes[at..end];
        require(
            !row.is_empty() && row.iter().all(|b| (32..=126).contains(b)),
            "definition census framing differs",
        )?;
        let colon = row
            .iter()
            .position(|b| *b == b':')
            .ok_or_else(|| io::Error::other("definition census lacks type delimiter"))?;
        require(
            colon > 0
                && (row[0] == b'p' || row[0] == b'r')
                && row[1..colon]
                    .iter()
                    .all(|b| row[0] == b'r' && b.is_ascii_digit()),
            "definition census type differs",
        )?;
        let mut slash = colon + 1;
        while slash < row.len() && name_byte(row[slash]) {
            slash += 1;
        }
        require(
            slash > colon + 1 && slash < row.len() && row[slash] == b'/',
            "definition census group differs",
        )?;
        let mut space = slash + 1;
        while space < row.len() && name_byte(row[space]) {
            space += 1;
        }
        require(
            space > slash + 1 && space < row.len() && row[space] == b' ',
            "definition census event differs",
        )?;
        let group = &row[colon + 1..slash] == intent.group().as_bytes();
        let event = &row[slash + 1..space] == intent.event().as_bytes();
        if group || event {
            require(group && event, "definition census owned-name collision")?;
            let mut role = None;
            for candidate in 1..=17 {
                if intent.command(candidate, 0)?.as_bytes() == &bytes[at..=end] {
                    role = Some(candidate);
                    break;
                }
            }
            let role =
                role.ok_or_else(|| io::Error::other("definition census owned site differs"))?;
            let bit = 1 << (role - 1);
            require(seen & bit == 0, "definition census duplicated owned site")?;
            seen |= bit;
        }
        at = end + 1;
    }
    Ok(seen)
}
/// Exact formatting and u64 domain of unchanged ap_grouped_profile, including
/// malformed foreign rows and every owned miss. Owned duplicate event names are
/// the seventeen physical sites; no deduplication is permitted.
fn profile_count(intent: &Intent, bytes: &[u8], expected: usize) -> io::Result<()> {
    require(
        bytes.len() <= 1_048_576 && expected <= 17,
        "profile census bound differs",
    )?;
    let text = std::str::from_utf8(bytes).map_err(io::Error::other)?;
    require(
        text.is_ascii() && (text.is_empty() || text.ends_with('\n')),
        "profile census framing differs",
    )?;
    let mut owned = 0;
    for row in text.split_inclusive('\n') {
        require(
            row.starts_with("  ") && row.ends_with('\n'),
            "profile census row framing differs",
        )?;
        let body = &row[2..row.len() - 1];
        let end = body.bytes().take_while(|b| name_byte(*b)).count();
        require(
            (1..=255).contains(&end) && body.as_bytes().get(end) == Some(&b' '),
            "profile census name differs",
        )?;
        let name = &body[..end];
        let numbers: Vec<_> = body[end..].split(' ').filter(|s| !s.is_empty()).collect();
        require(
            numbers.len() == 2
                && numbers
                    .iter()
                    .all(|s| s.bytes().all(|b| b.is_ascii_digit())),
            "profile census numeric fields differ",
        )?;
        let hits: u64 = numbers[0].parse().map_err(io::Error::other)?;
        let misses: u64 = numbers[1].parse().map_err(io::Error::other)?;
        require(
            row == format!("  {name:<44} {hits:>15} {misses:>15}\n"),
            "profile census exact fixed format differs",
        )?;
        if name == intent.event() {
            owned += 1;
            require(
                misses == 0 && owned <= expected,
                "profile census owned miss or extra row",
            )?;
        }
    }
    require(
        owned == expected,
        "profile census owned row population differs",
    )
}

#[derive(Debug)]
pub(super) struct CensusRow {
    fd: RawFd,
    identity: FileIdentity,
    status_flags: i32,
    descriptor_flags: i32,
}
#[derive(Debug)]
pub(super) struct CensusInventory {
    directory: Option<OwnedFd>,
    observations: Vec<Vec<CensusRow>>,
}

/// Read-only native observation. Deserializing one cannot create an FD or a
/// Creator/query owner; consumers must compare it with actual received rights.
#[derive(Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DescriptionRecord {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    links: u64,
    size: i64,
    filesystem: i64,
    status_flags: i32,
    descriptor_flags: i32,
}
pub(super) fn describe_fd(fd: RawFd) -> io::Result<DescriptionRecord> {
    let identity = stat(fd)?;
    let status_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    require(
        status_flags >= 0 && descriptor_flags == libc::FD_CLOEXEC,
        "exported original description flags invalid",
    )?;
    let filesystem = filesystem(fd)? as i64;
    require(
        stat(fd)?.same_owner(&identity),
        "exported original description changed during observation",
    )?;
    Ok(DescriptionRecord {
        device: identity.device,
        inode: identity.inode,
        mode: identity.mode,
        uid: identity.uid,
        gid: identity.gid,
        links: identity.links,
        size: identity.size,
        filesystem,
        status_flags,
        descriptor_flags,
    })
}
pub(super) fn verify_description(fd: RawFd, expected: &serde_json::Value) -> io::Result<()> {
    let expected: DescriptionRecord = serde_json::from_value(expected.clone())?;
    require(
        describe_fd(fd)? == expected,
        "exported original description identity or flags differ",
    )
}
pub(super) fn check_no_children() -> io::Result<()> {
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    let result = unsafe {
        libc::waitid(
            libc::P_ALL,
            0,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    require(
        result == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD),
        "terminal Keeper still has a child or adopted descendant",
    )
}
impl CensusInventory {
    pub fn retain() -> Self {
        Self {
            directory: None,
            observations: Vec::new(),
        }
    }
    /// Read-only census observations. These numbers do not transfer ownership;
    /// the retained process scope must make and record each explicit close.
    pub(super) fn held_descriptor(&self) -> io::Result<i32> {
        Ok(self
            .directory
            .as_ref()
            .ok_or_else(|| io::Error::other("actual census descriptor absent"))?
            .as_raw_fd())
    }
    pub(super) fn last_descriptors(&self) -> io::Result<Vec<i32>> {
        Ok(self
            .observations
            .last()
            .ok_or_else(|| io::Error::other("actual census not observed"))?
            .iter()
            .map(|row| row.fd)
            .collect())
    }
    /// Actual process-wide inventory, including the continuously retained
    /// enumeration descriptor. There is no disappearing-descriptor exemption.
    pub fn observe(&mut self, required: &[RawFd], deadline: Instant) -> io::Result<()> {
        require(
            Instant::now() < deadline && self.observations.len() < 128,
            "actual FD census original deadline or128 receipts exceeded",
        )?;
        if self.directory.is_none() {
            let raw = unsafe {
                libc::open(
                    c"/proc/self/fd".as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            self.directory = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        }
        let fd = self.directory.as_ref().unwrap().as_raw_fd();
        require(
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } == 0,
            "actual FD census directory rewind failed",
        )?;
        // Retain partial rows before any interpretation or later failure.
        self.observations.push(Vec::new());
        let rows = self.observations.last_mut().unwrap();
        loop {
            require(
                Instant::now() < deadline,
                "actual FD census original deadline expired",
            )?;
            let mut buffer = [0u8; 4096];
            let count = unsafe {
                libc::syscall(libc::SYS_getdents64, fd, buffer.as_mut_ptr(), buffer.len())
            };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                break;
            }
            require(
                count as usize <= buffer.len(),
                "actual FD census native extent differs",
            )?;
            let mut at = 0;
            while at < count as usize {
                require(
                    count as usize - at >= 20,
                    "actual FD census entry truncated",
                )?;
                let size = u16::from_ne_bytes([buffer[at + 16], buffer[at + 17]]) as usize;
                require(
                    size >= 20 && size <= count as usize - at,
                    "actual FD census entry extent differs",
                )?;
                let names = &buffer[at + 19..at + size];
                let end = names
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or_else(|| io::Error::other("actual FD census name unterminated"))?;
                let name = &names[..end];
                at += size;
                if name == b"." || name == b".." {
                    continue;
                }
                require(
                    !name.is_empty() && name.iter().all(u8::is_ascii_digit),
                    "actual FD census name is not a descriptor",
                )?;
                let text = std::str::from_utf8(name).map_err(io::Error::other)?;
                let current: i32 = text.parse().map_err(io::Error::other)?;
                let duplicate = rows.iter().any(|row| row.fd == current);
                require(
                    current >= 0 && current.to_string() == text && !duplicate && rows.len() < 128,
                    &format!(
                        "actual FD census duplicated descriptor or exceeded128; current={current}; rows={}; duplicate={duplicate}",
                        rows.len()
                    ),
                )?;
                let identity = stat(current)?;
                let status_flags = unsafe { libc::fcntl(current, libc::F_GETFL) };
                let descriptor_flags = unsafe { libc::fcntl(current, libc::F_GETFD) };
                require(
                    status_flags >= 0
                        && descriptor_flags >= 0
                        && stat(current)?.same_owner(&identity),
                    "actual FD census descriptor changed",
                )?;
                rows.push(CensusRow {
                    fd: current,
                    identity,
                    status_flags,
                    descriptor_flags,
                });
            }
        }
        require(
            rows.iter().any(|row| row.fd == fd)
                && required
                    .iter()
                    .all(|needed| rows.iter().any(|row| row.fd == *needed)),
            "actual FD census lacks retained required owner",
        )?;
        rows.sort_by_key(|row| row.fd);
        require(
            Instant::now() < deadline,
            "actual FD census original deadline expired",
        )
    }
}

// Descriptive census over two complete reads performed through the private
// cleanup epoch. It cannot issue source, cursor, or Provider authority.
pub(super) fn check_cleanup_observations(
    intent: &Intent,
    definitions: &[u8],
    profile: &[u8],
    eligible: u32,
) -> io::Result<()> {
    require(
        eligible != 0 && eligible <= 0x1ffff,
        "cleanup eligible prefix invalid",
    )?;
    let actual = definition_mask(intent, definitions)?;
    require(
        actual & !eligible == 0,
        "cleanup census exceeds jointly acknowledged attempts",
    )?;
    profile_count(intent, profile, actual.count_ones() as usize)
}

/// Descriptive result from the same strict owned-prefix parsers. The caller
/// still needs its private native-terminal and exclusive-read authority.
pub(super) fn runtime_creation_observed_mask(
    intent: &Intent,
    definitions: &[u8],
    profile: &[u8],
    eligible: u32,
) -> io::Result<u32> {
    check_cleanup_observations(intent, definitions, profile, eligible)?;
    definition_mask(intent, definitions)
}

/// Original terminal source wrapper observed without consuming its wait. The
/// live cleanup Keeper remains a separate child; this does not run or relax any
/// finalizer's unchanged global ECHILD check.
pub(super) struct FailedLauncherProof<'a> {
    original: &'a Launcher,
    deadline: Instant,
    status: i32,
}
impl FailedLauncherProof<'_> {
    pub(super) fn check(&self) -> io::Result<()> {
        require(
            Instant::now() < self.deadline
                && self.original.eof == [true, true]
                && self.original.logs_synced
                && self.original.reaped.is_none()
                && self.original.retirement_failure.is_none(),
            "original failed source Launcher custody changed",
        )?;
        let pid = self.original.child.id() as i32;
        let pidfd = self
            .original
            .pidfd
            .as_ref()
            .ok_or_else(|| io::Error::other("original source pidfd absent"))?;
        require(
            terminal(pidfd.as_raw_fd())? && unsafe { libc::getpgid(pid) } == pid,
            "original failed source Launcher terminal/group custody differs",
        )?;
        let mut wait = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as u32,
                &mut wait,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            unsafe { wait.si_pid() } == pid
                && wait.si_code == libc::CLD_EXITED
                && unsafe { wait.si_status() } == self.status
                && self.status > 0,
            "original failed source WNOWAIT changed",
        )
    }
    pub(super) fn observation(&self) -> io::Result<serde_json::Value> {
        self.check()?;
        Ok(
            serde_json::json!({"pid":self.original.child.id(),"waitid_raw":0,
            "waitid_pid":self.original.child.id(),"waitid_code":libc::CLD_EXITED,"waitid_status":self.status,
            "wait_consumed":false,"stdout_eof":self.original.eof[0],"stderr_eof":self.original.eof[1],
            "logs_synced":self.original.logs_synced,"global_ECHILD_claimed":false}),
        )
    }
}
impl Launcher {
    pub(super) fn failed_source_terminal(
        &mut self,
        directory: RawFd,
        deadline: Instant,
    ) -> io::Result<Option<FailedLauncherProof<'_>>> {
        require(
            Instant::now() < deadline && self.reaped.is_none(),
            "failed source terminal observation reused or late",
        )?;
        self.check_log_directory(directory)?;
        self.drain()?;
        if self.eof != [true, true]
            || !terminal(
                self.pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("original source pidfd absent"))?
                    .as_raw_fd(),
            )?
        {
            return Ok(None);
        }
        if !self.logs_synced {
            for file in &self.log_files {
                let file = file
                    .as_ref()
                    .ok_or_else(|| io::Error::other("original source log absent"))?;
                if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if unsafe { libc::fsync(directory) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.logs_synced = true;
        }
        let pid = self.child.id() as i32;
        let mut wait = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as u32,
                &mut wait,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        require(
            unsafe { wait.si_pid() } == pid
                && wait.si_code == libc::CLD_EXITED
                && unsafe { wait.si_status() } > 0,
            "failed source Launcher lacks actual natural nonzero wait",
        )?;
        let proof = FailedLauncherProof {
            original: self,
            deadline,
            status: unsafe { wait.si_status() },
        };
        proof.check()?;
        Ok(Some(proof))
    }
}

/// Descriptive exact absence validation over complete reads already performed
/// under the runtime owner's private exclusive cursor. Never issues a cursor.
pub(super) fn check_runtime_absence(
    intent: &Intent,
    definitions: &[u8],
    profile: &[u8],
    events: BorrowedFd<'_>,
) -> io::Result<()> {
    require(
        definition_mask(intent, definitions)? == 0,
        "runtime owned definitions remain",
    )?;
    profile_count(intent, profile, 0)?;
    for path in [
        intent.group(),
        format!("{}/{}", intent.group(), intent.event()),
    ] {
        let name = std::ffi::CString::new(path).map_err(io::Error::other)?;
        let mut value = std::mem::MaybeUninit::<libc::stat>::uninit();
        let raw = unsafe {
            libc::fstatat(
                events.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let error = (raw == -1).then(io::Error::last_os_error);
        require(
            raw == -1 && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ENOENT),
            "runtime owned event directory remains or absence query failed",
        )?;
    }
    Ok(())
}

// This child module reuses the original private bounded query implementation;
// its only paths are the exact run-derived leaf roles, never caller paths.
#[path = "leaf_delegate.rs"]
mod leaf_delegate;
pub(super) use leaf_delegate::LeafDelegate;

#[path = "runtime_parent.rs"]
mod runtime_parent;
pub use runtime_parent::GroupedParentOwner;
