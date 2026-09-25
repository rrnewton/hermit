//! Creator authority comes from kernel capabilities and an actual manager query.
//! The launcher Child, creator pidfd and cgroup directory are distinct owners.
use std::collections::BTreeMap;
use std::ffi::CString;
use std::io::Read;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Instant;

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
fn filesystem(fd: RawFd) -> io::Result<libc::c_long> {
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
fn read_file(path: &str, cap: usize) -> io::Result<String> {
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
fn read_at(directory: RawFd, name: &str) -> io::Result<String> {
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
}
impl Launcher {
    pub fn retain(child: Child) -> Self {
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
        }
    }
    pub fn initialize(&mut self, directory: RawFd) -> io::Result<()> {
        require(
            self.pidfd.is_none() && self.reaped.is_none(),
            "launcher cannot be recaptured",
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
        Ok(())
    }
    pub fn drain(&mut self) -> io::Result<()> {
        require(
            self.refused.is_none() && self.pidfd.is_some(),
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
                let log = self.log_files[index]
                    .as_ref()
                    .ok_or_else(|| io::Error::other("retained log absent"))?
                    .as_raw_fd();
                let mut offset = before;
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
}

/// Created only by a retained query whose real child exited zero and whose
/// exact stdout/stderr pipes reached EOF within the original stage deadline.
#[derive(Debug)]
pub(super) struct ManagerSnapshot {
    unit: String,
    properties: BTreeMap<String, String>,
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
        }
    }
    fn evidence(&self) -> serde_json::Value {
        serde_json::json!({"argv":self.arguments,"pid":self.child.as_ref().map(Child::id),
            "pidfd_held":self.pidfd.is_some(),"stdout":super::hex(&self.stdout),"stderr":super::hex(&self.stderr),
            "eof":self.eof,"wait_code":self.reaped.and_then(|s|s.code()),"original_query_bound_seconds":2})
    }
    pub fn start(&mut self) -> io::Result<()> {
        require(self.child.is_none(), "manager query cannot restart")?;
        self.deadline = Some(Instant::now() + std::time::Duration::from_secs(2));
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
        self.child = Some(command.spawn()?); // owned before any fallible pipe setup
        let child = self.child.as_ref().unwrap();
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) } as i32;
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        self.pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
        // Even a fast terminal child remains an unreaped direct Child here.
        require(
            filesystem(raw)? == 0x5049_4446,
            "manager query pidfd type differs",
        )?;
        nonblocking(child.stdout.as_ref().unwrap().as_raw_fd())?;
        nonblocking(child.stderr.as_ref().unwrap().as_raw_fd())
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<bool> {
        let deadline = deadline.min(
            self.deadline
                .ok_or_else(|| io::Error::other("manager query original origin absent"))?,
        );
        require(
            Instant::now() < deadline,
            "original manager query deadline expired",
        )?;
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("manager query was not started"))?;
        drain(
            child.stdout.as_ref().unwrap().as_raw_fd(),
            &mut self.stdout,
            &mut self.eof[0],
        )?;
        drain(
            child.stderr.as_ref().unwrap().as_raw_fd(),
            &mut self.stderr,
            &mut self.eof[1],
        )?;
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
        require(
            Instant::now() < deadline,
            "metadata query completed after original deadline",
        )?;
        Ok(true)
    }
}
#[derive(Debug)]
pub(super) struct ManagerQuery {
    unit: String,
    query: CommandQuery,
}
impl ManagerQuery {
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
            (libc::RLIMIT_NOFILE, 128),
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
    pub fn readback(&self) -> io::Result<CgroupReadback> {
        require(
            self.captured && super::valid_nonce(&self.nonce),
            "creator has no retained admission",
        )?;
        let before = stat(self.directory.as_raw_fd())?;
        require(
            before.same_object(self.identity.as_ref().unwrap()),
            "held cgroup identity changed",
        )?;
        let procs = read_at(self.directory.as_raw_fd(), "cgroup.procs");
        let events = read_at(self.directory.as_raw_fd(), "cgroup.events");
        match (procs, events) {
            (Ok(procs), Ok(events)) => Ok(CgroupReadback {
                creator_terminal: terminal(self.pidfd.as_raw_fd())?,
                unlinked: false,
                procs: Some(procs),
                events: Some(events),
            }),
            (Err(error), _) | (_, Err(error)) => {
                require(
                    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENODEV)),
                    "cgroup readback failed without an unlink observation",
                )?;
                let after = stat(self.directory.as_raw_fd())?;
                require(
                    after.same_object(self.identity.as_ref().unwrap()) && after.links == 0,
                    "missing cgroup contents without retained unlink proof",
                )?;
                Ok(CgroupReadback {
                    creator_terminal: terminal(self.pidfd.as_raw_fd())?,
                    unlinked: true,
                    procs: None,
                    events: None,
                })
            }
        }
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
    pub fn snapshot(&self, index: usize) -> io::Result<Vec<u8>> {
        self.check()?;
        require(index < 2, "only control/profile snapshots are supported")?;
        let fd = self.fds[index].as_raw_fd();
        require(
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } == 0,
            "seq-file rewind failed",
        )?;
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0u8; 4096];
            let cap = buffer.len().min(1_048_577usize.saturating_sub(bytes.len()));
            require(cap > 0, "complete control snapshot exceeded original1MiB")?;
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), cap) };
            if count < 0 {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
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
