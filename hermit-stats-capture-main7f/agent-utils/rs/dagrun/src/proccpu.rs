//! Generation-bound, best-effort process-group CPU observations.
//!
//! Each member contributes its original own plus waited-child ticks. All samples precede
//! all lifecycle validations: a child credited to a sampled parent must subsequently be
//! excluded. A terminal pidfd alone is insufficient because an unreaped zombie still owns
//! its CPU. Only a fresh Z state from the *same paired stat FD* preserves that saved row.
//!
//! This is not cgroup accounting: escaped groups and activity between samples can be
//! missed. Ambiguous, missing or resource-limited evidence is unavailable, never zero.
//! One shared observation retains at most 1024 members across 256 registered groups,
//! with a one-second scan deadline and 64 descriptors reserved below the soft limit.
//!
//! Linux cannot list the members of a process group, so each snapshot walks every PID in
//! procfs. It does not re-read every PID's stat: a [`SessionCache`] remembers the session
//! of each PID an earlier snapshot read, and a PID is skipped when neither that session
//! nor the PID itself (its session after a setsid) is the session of a registered group,
//! because it can never join one of those groups. The cache
//! trusts an entry only while the kernel cannot have handed that PID to a new task.
//! Descriptor pressure can lower this ceiling. A shared refusal affects all readers
//! of that snapshot; it does not permit a partial population to be reported.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// Stable source identifier for kernel cgroup CPU accounting.
pub const CPU_SOURCE_CGROUP: &str = "cgroup";
/// Stable source identifier for best-effort process-group accounting.
pub const CPU_SOURCE_PROCFS: &str = "procfs-subtree";
const MAX_RECORD: usize = 16384;
const MAX_MEMBERS: usize = 1024;
const MAX_GROUPS: usize = 256;
const MAX_ENTRIES: usize = 65536;
const FD_RESERVE: u64 = 64;
const MAX_INTERRUPTS: usize = 16;
const SCAN_TIME: Duration = Duration::from_secs(1);
const SNAPSHOT_TTL: Duration = Duration::from_millis(500);

/// No complete authenticated observation is available; this is distinct from zero CPU.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unavailable(String);
impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Unavailable {}
impl From<io::Error> for Unavailable {
    fn from(value: io::Error) -> Self {
        Self(value.to_string())
    }
}
fn unavailable(reason: &str) -> Unavailable {
    Unavailable(reason.into())
}
fn check(deadline: Instant) -> Result<(), Unavailable> {
    if Instant::now() >= deadline {
        Err(unavailable("scan deadline"))
    } else {
        Ok(())
    }
}
fn checked_clk_tck(value: libc::c_long) -> Result<f64, Unavailable> {
    if value > 0 {
        Ok(value as f64)
    } else {
        Err(unavailable(&format!(
            "SC_CLK_TCK unavailable: nonpositive value {value}"
        )))
    }
}
fn clk_tck() -> Result<f64, Unavailable> {
    // SAFETY: sysconf is a query with no pointer arguments.
    let value = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    checked_clk_tck(value)
}
fn gone(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH))
}
fn allow_fds(required: u64) -> Result<(), Unavailable> {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: limit is writable storage of the type getrlimit requires.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: a successful getrlimit initialized the structure.
    let soft = unsafe { limit.assume_init() }.rlim_cur;
    if soft == libc::RLIM_INFINITY {
        return Ok(());
    }
    let mut used = 0_u64;
    for entry in fs::read_dir("/proc/self/fd")? {
        entry?;
        used += 1;
        if used > MAX_ENTRIES as u64 {
            return Err(unavailable("descriptor census bound"));
        }
    }
    if used.saturating_add(required).saturating_add(FD_RESERVE) > soft {
        return Err(unavailable("descriptor reserve"));
    }
    Ok(())
}
fn read_record(file: &File, deadline: Instant) -> Result<String, Unavailable> {
    read_record_io(file, deadline).map_err(Unavailable::from)
}
fn read_record_io(file: &File, deadline: Instant) -> io::Result<String> {
    read_bounded(file, deadline, MAX_RECORD)
}
fn read_bounded(file: &File, deadline: Instant, max: usize) -> io::Result<String> {
    let mut data = Vec::with_capacity(1024);
    let mut interrupts = 0;
    let mut block = [0_u8; 1024];
    loop {
        check(deadline).map_err(io::Error::other)?;
        match file.read_at(&mut block, data.len() as u64) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&block[..n]);
                if data.len() > max {
                    return Err(io::Error::other("oversized proc record"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                interrupts += 1;
                if interrupts > MAX_INTERRUPTS {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(String::from_utf8_lossy(&data).into_owned())
}
#[derive(Clone, Debug)]
struct Stat {
    pid: u32,
    group: u32,
    session: u32,
    state: char,
    ticks: u64,
}
fn stat(text: &str) -> Result<Stat, Unavailable> {
    let bad = || unavailable("malformed stat");
    let opening = text.find(" (").ok_or_else(bad)?;
    let close = text
        .rfind(')')
        .filter(|close| *close > opening)
        .ok_or_else(bad)?;
    let pid = text[..opening].parse::<u32>().map_err(|_| bad())?;
    let fields: Vec<_> = text[close + 1..].split_whitespace().collect();
    if pid == 0 || fields.len() < 15 || fields[0].len() != 1 {
        return Err(bad());
    }
    let group = fields[2].parse::<u32>().map_err(|_| bad())?;
    let session = fields[3].parse::<u32>().map_err(|_| bad())?;
    let mut ticks = 0_u64;
    for index in [11, 12, 13, 14] {
        ticks = ticks
            .checked_add(fields[index].parse::<u64>().map_err(|_| bad())?)
            .ok_or_else(|| unavailable("CPU overflow"))?;
    }
    Ok(Stat {
        pid,
        group,
        session,
        state: fields[0].chars().next().ok_or_else(bad)?,
        ticks,
    })
}
fn fd_pid(text: &str) -> Result<i64, Unavailable> {
    let values: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("Pid:"))
        .collect();
    if values.len() != 1 {
        return Err(unavailable("missing or duplicate pidfd Pid"));
    }
    let value = values[0]
        .trim()
        .parse::<i64>()
        .map_err(|_| unavailable("malformed pidfd Pid"))?;
    if value < -1 || value == 0 {
        return Err(unavailable("invalid pidfd Pid"));
    }
    Ok(value)
}

/// PIDs below this are never handed out again after the allocator wraps (Linux
/// `RESERVED_PIDS`), so a lap of the PID space is at most `pid_max - RESERVED_PIDS` steps.
const RESERVED_PIDS: u64 = 300;
/// /proc/stat holds one line per CPU; this bounds it on hosts with thousands of CPUs.
const MAX_STAT_RECORD: usize = 1 << 20;

/// Where the kernel's PID allocator stands: the last PID it handed out and the number of
/// live tasks (both from /proc/loadavg), and the tasks created since boot (/proc/stat
/// `processes`). Threads take their IDs from the same space, and all three count them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Allocator {
    last_pid: u32,
    tasks: u64,
    forks: u64,
}
fn parse_loadavg(text: &str) -> Option<(u32, u64)> {
    let fields: Vec<_> = text.split_whitespace().collect();
    let (_, tasks) = fields.get(3)?.split_once('/')?;
    Some((fields.get(4)?.parse().ok()?, tasks.parse().ok()?))
}
fn parse_forks(text: &str) -> Option<u64> {
    let mut values = text
        .lines()
        .filter_map(|line| line.strip_prefix("processes "));
    let value = values.next()?.trim().parse().ok()?;
    values.next().is_none().then_some(value)
}

/// Whether the allocator may have handed out PID `pid` again on its way from `from` to
/// `to`. It walks the PID space in a circle, one step per new task or per PID still in
/// use, so every PID it handed out lies on the arc after `from.last_pid` up to
/// `to.last_pid`, unless it may have gone the whole way round: then any PID may have been.
fn reallocated(from: Allocator, to: Allocator, pid_max: u32, pid: u32) -> bool {
    let Some(created) = to.forks.checked_sub(from.forks) else {
        return true;
    };
    let lap = u64::from(pid_max).saturating_sub(RESERVED_PIDS);
    if created.saturating_add(from.tasks.max(to.tasks)) >= lap {
        return true;
    }
    let (start, end) = (from.last_pid, to.last_pid);
    if created == 0 {
        return false;
    }
    if start < end {
        start < pid && pid <= end
    } else {
        pid > start || pid <= end
    }
}

/// The session of each PID an earlier snapshot read. A process joins a group only through
/// setpgid, only within its own session, and changes its session only through setsid,
/// which makes its session its own PID. So a cached PID's session is now either the
/// cached one or the PID itself, and the PID is skipped only when neither is the session
/// of a registered group: then it can never be a member of one. That covers a child read
/// between fork and setsid that then leads a registered group. The cache holds sessions,
/// not verdicts, so a group registered later re-evaluates every cached PID without a read.
///
/// An entry is trusted only while the kernel cannot have reused its PID, judged from
/// [`Allocator`] positions; when that cannot be proven the cache is emptied, and the next
/// snapshot reads every PID, as it did before there was a cache.
#[derive(Default)]
struct SessionCache {
    sessions: HashMap<u32, u32>,
    at: Option<Allocator>,
}
impl SessionCache {
    /// Start a snapshot at allocator position `now`, dropping each entry whose PID the
    /// allocator may have handed out since the previous snapshot started.
    fn begin(&mut self, now: Option<Allocator>, pid_max: Option<u32>) {
        match (self.at, now, pid_max) {
            (Some(previous), Some(now), Some(pid_max)) => self
                .sessions
                .retain(|pid, _| !reallocated(previous, now, pid_max, *pid)),
            _ => self.sessions.clear(),
        }
        self.at = now;
    }
    /// Whether this snapshot may skip reading `pid`.
    fn skip(&self, pid: u32, sessions: &HashSet<u32>) -> bool {
        !sessions.contains(&pid)
            && self
                .sessions
                .get(&pid)
                .is_some_and(|session| !sessions.contains(session))
    }
    fn record(&mut self, pid: u32, session: u32) {
        self.sessions.insert(pid, session);
    }
    /// Forget PIDs that no longer exist.
    fn retain(&mut self, seen: &HashSet<u32>) {
        self.sessions.retain(|pid, _| seen.contains(pid));
    }
}

struct Proc {
    root: File,
}
fn verify_namespace_status(status: &[u8], pid: u32) -> Result<(), Unavailable> {
    let namespace = crate::procstat::status_field(status, b"NSpid")
        .map_err(unavailable)?
        .ok_or_else(|| unavailable("procfs namespace metadata unavailable: NSpid missing"))?;
    let mut ids = namespace
        .split(|byte| matches!(byte, b' ' | b'\t'))
        .filter(|value| !value.is_empty());
    let observed = ids
        .next()
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse::<u32>().ok());
    if observed != Some(pid) || ids.next().is_some() {
        return Err(unavailable("procfs PID namespace mismatch"));
    }
    Ok(())
}
impl Proc {
    fn new(path: &Path) -> Result<Self, Unavailable> {
        allow_fds(2)?;
        let root = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: root is a live FD and fs is writable statfs storage.
        if unsafe { libc::fstatfs(root.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful fstatfs initialized fs.
        if unsafe { fs.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
            return Err(unavailable("root is not procfs"));
        }
        let proc = Self { root };
        let deadline = Instant::now() + SCAN_TIME;
        let status = read_record(&proc.open("self/status", deadline)?, deadline)?;
        verify_namespace_status(status.as_bytes(), std::process::id())?;
        let own = stat(&read_record(&proc.open("self/stat", deadline)?, deadline)?)?;
        if own.pid != std::process::id() {
            return Err(unavailable("procfs PID namespace mismatch"));
        }
        Ok(proc)
    }
    fn open(&self, path: &str, deadline: Instant) -> io::Result<File> {
        let path = CString::new(path).map_err(io::Error::other)?;
        for _ in 0..=MAX_INTERRUPTS {
            check(deadline).map_err(io::Error::other)?;
            // SAFETY: valid held dir FD and NUL-terminated path. Ownership of a successful
            // new descriptor is transferred exactly once to File below.
            let fd = unsafe {
                libc::openat(
                    self.root.as_raw_fd(),
                    path.as_ptr(),
                    libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                )
            };
            if fd >= 0 {
                return Ok(unsafe { File::from_raw_fd(fd) });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        Err(io::Error::other("open interrupted repeatedly"))
    }
    /// The allocator position, or `None` if procfs does not report it.
    fn allocator(&self, deadline: Instant) -> Option<Allocator> {
        let loadavg = read_record_io(&self.open("loadavg", deadline).ok()?, deadline).ok()?;
        let (last_pid, tasks) = parse_loadavg(&loadavg)?;
        let stat = read_bounded(
            &self.open("stat", deadline).ok()?,
            deadline,
            MAX_STAT_RECORD,
        )
        .ok()?;
        Some(Allocator {
            last_pid,
            tasks,
            forks: parse_forks(&stat)?,
        })
    }
    fn pid_max(&self, deadline: Instant) -> Option<u32> {
        read_record_io(&self.open("sys/kernel/pid_max", deadline).ok()?, deadline)
            .ok()?
            .trim()
            .parse()
            .ok()
    }
    fn pid(&self, pidfd: &File, deadline: Instant) -> Result<i64, Unavailable> {
        let record = self.open(&format!("self/fdinfo/{}", pidfd.as_raw_fd()), deadline)?;
        fd_pid(&read_record(&record, deadline)?)
    }
    fn pair(&self, pid: u32, deadline: Instant) -> Result<Pair, PairError> {
        check(deadline)?;
        allow_fds(3)?;
        let (pidfd, file, original) = bind_pair(
            pid,
            || open_pidfd(pid, deadline),
            || {
                self.open(&format!("{pid}/stat"), deadline)
                    .map_err(PairError::Io)
            },
            |file| {
                Ok(stat(
                    &read_record_io(file, deadline).map_err(PairError::Io)?,
                )?)
            },
            |pidfd| self.pid(pidfd, deadline).map_err(PairError::from),
        )?;
        Ok(Pair {
            pidfd,
            file,
            original,
        })
    }
}
fn open_pidfd(pid: u32, deadline: Instant) -> Result<File, PairError> {
    for _ in 0..=MAX_INTERRUPTS {
        check(deadline)?;
        // SAFETY: pidfd_open takes scalar arguments and returns a new owned descriptor.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
        if fd >= 0 {
            // SAFETY: syscall returned a new live FD; this is its sole owning File.
            return Ok(unsafe { File::from_raw_fd(fd as i32) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(PairError::Io(error));
        }
    }
    Err(unavailable("pidfd_open interrupted repeatedly").into())
}
fn bind_pair<P, S>(
    pid: u32,
    open_pidfd: impl FnOnce() -> Result<P, PairError>,
    open_stat: impl FnOnce() -> Result<S, PairError>,
    read_stat: impl FnOnce(&S) -> Result<Stat, PairError>,
    fdinfo: impl FnOnce(&P) -> Result<i64, PairError>,
) -> Result<(P, S, Stat), PairError> {
    let pidfd = open_pidfd()?;
    let file = open_stat()?;
    let original = read_stat(&file)?;
    // This fresh fdinfo read follows stat open. If the original generation detached,
    // its pidfd cannot authenticate a replacement PID, even one already in Z state.
    if fdinfo(&pidfd)? != i64::from(pid) || original.pid != pid {
        return Err(unavailable("pidfd/stat generation mismatch").into());
    }
    Ok((pidfd, file, original))
}
enum PairError {
    Io(io::Error),
    Unavailable(Unavailable),
}
impl From<Unavailable> for PairError {
    fn from(value: Unavailable) -> Self {
        Self::Unavailable(value)
    }
}
impl From<PairError> for Unavailable {
    fn from(value: PairError) -> Self {
        match value {
            PairError::Io(e) => e.into(),
            PairError::Unavailable(e) => e,
        }
    }
}
struct Pair {
    pidfd: File,
    file: File,
    original: Stat,
}
trait Observation {
    fn original(&self) -> &Stat;
    fn valid(&self, deadline: Instant) -> Result<bool, Unavailable>;
}
impl Observation for Pair {
    fn original(&self) -> &Stat {
        &self.original
    }
    fn valid(&self, deadline: Instant) -> Result<bool, Unavailable> {
        let mut event = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let mut result = None;
        for _ in 0..=MAX_INTERRUPTS {
            check(deadline)?;
            // SAFETY: event is one initialized, writable pollfd and timeout is zero.
            let count = unsafe { libc::poll(&mut event, 1, 0) };
            if count >= 0 {
                result = Some(count);
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error.into());
            }
        }
        let count = result.ok_or_else(|| unavailable("poll interrupted repeatedly"))?;
        accept(
            if count == 0 { 0 } else { event.revents },
            &self.original,
            || match read_record_io(&self.file, deadline) {
                Ok(text) => Ok(Some(stat(&text)?)),
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(None),
                Err(error) => Err(error.into()),
            },
        )
    }
}
fn accept(
    bits: i16,
    original: &Stat,
    fresh: impl FnOnce() -> Result<Option<Stat>, Unavailable>,
) -> Result<bool, Unavailable> {
    if bits == 0 {
        return Ok(true);
    }
    if bits & !(libc::POLLIN | libc::POLLHUP | libc::POLLRDNORM) != 0
        || bits & (libc::POLLIN | libc::POLLHUP) == 0
    {
        return Err(unavailable("unexpected pidfd poll result"));
    }
    let Some(current) = fresh()? else {
        return Ok(false);
    };
    if current.pid != original.pid {
        return Err(unavailable("held stat identity changed"));
    }
    match current.state {
        'Z' => Ok(true),
        'X' => Ok(false),
        _ => Err(unavailable("terminal pidfd with ambiguous task state")),
    }
}

trait Source {
    type Row: Observation;
    fn samples(
        &self,
        groups: &HashSet<u32>,
        deadline: Instant,
    ) -> Result<Vec<Self::Row>, Unavailable>;
}
/// The live procfs source. `sessions` are the sessions of the registered groups, and
/// `cache` persists across snapshots in the [`Registry`].
struct KernelSource<'a> {
    proc: &'a Proc,
    bound: usize,
    sessions: HashSet<u32>,
    cache: std::cell::RefCell<&'a mut SessionCache>,
    /// How many stat files this snapshot read.
    reads: std::cell::Cell<usize>,
}
impl KernelSource<'_> {
    /// Read `pid`'s stat, cache its session and, if it is in one of `groups`, pair it.
    fn consider(
        &self,
        pid: u32,
        groups: &HashSet<u32>,
        rows: &mut Vec<Pair>,
        deadline: Instant,
    ) -> Result<(), Unavailable> {
        self.reads.set(self.reads.get() + 1);
        let hint = match self
            .proc
            .open(&format!("{pid}/stat"), deadline)
            .and_then(|file| read_record_io(&file, deadline))
        {
            Ok(text) => stat(&text)?,
            Err(error) if gone(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        self.cache.borrow_mut().record(pid, hint.session);
        if !groups.contains(&hint.group) {
            return Ok(());
        }
        if rows.len() >= self.bound {
            return Err(unavailable("member bound"));
        }
        let pair = match self.proc.pair(pid, deadline) {
            Ok(pair) => pair,
            Err(PairError::Io(error)) if gone(&error) => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if groups.contains(&pair.original.group) {
            rows.push(pair);
        }
        Ok(())
    }
}
impl Source for KernelSource<'_> {
    type Row = Pair;
    fn samples(&self, groups: &HashSet<u32>, deadline: Instant) -> Result<Vec<Pair>, Unavailable> {
        let pid_max = self.proc.pid_max(deadline);
        let start = self.proc.allocator(deadline);
        self.cache.borrow_mut().begin(start, pid_max);
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut skipped = Vec::new();
        let entries = fs::read_dir(format!("/proc/self/fd/{}", self.proc.root.as_raw_fd()))?;
        for (index, entry) in entries.enumerate() {
            check(deadline)?;
            if index >= MAX_ENTRIES {
                return Err(unavailable("proc entry bound"));
            }
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let pid = name
                .parse::<u32>()
                .map_err(|_| unavailable("invalid proc PID"))?;
            seen.insert(pid);
            if self.cache.borrow().skip(pid, &self.sessions) {
                skipped.push(pid);
                continue;
            }
            self.consider(pid, groups, &mut rows, deadline)?;
        }
        // A skipped PID may have been handed to a new task while this snapshot ran.
        let end = self.proc.allocator(deadline);
        for pid in skipped {
            let fresh = match (start, end, pid_max) {
                (Some(start), Some(end), Some(pid_max)) => reallocated(start, end, pid_max, pid),
                _ => true,
            };
            if fresh {
                check(deadline)?;
                self.consider(pid, groups, &mut rows, deadline)?;
            }
        }
        self.cache.borrow_mut().retain(&seen);
        Ok(rows)
    }
}
fn scan<S: Source>(
    source: &S,
    groups: &HashSet<u32>,
    deadline: Instant,
) -> Result<HashMap<u32, u64>, Unavailable> {
    let rows = source.samples(groups, deadline)?;
    // Global phase barrier: never validate a chunk before later original samples.
    let mut totals = HashMap::<u32, u64>::new();
    for row in rows {
        check(deadline)?;
        if row.valid(deadline)? {
            let original = row.original();
            let value = totals.entry(original.group).or_default();
            *value = value
                .checked_add(original.ticks)
                .ok_or_else(|| unavailable("aggregate overflow"))?;
        }
    }
    Ok(totals)
}

struct Owner {
    key: u64,
    pgid: u32,
    /// The group's session: its leader's, which a group leader cannot leave.
    session: u32,
    proc: Proc,
    pair: Pair,
}
impl Owner {
    fn authenticate(&self, deadline: Instant) -> Result<(), Unavailable> {
        if self.proc.pid(&self.pair.pidfd, deadline)? != i64::from(self.pgid) {
            return Err(unavailable("owner generation gone"));
        }
        let current = stat(&read_record(&self.pair.file, deadline)?)?;
        if current.pid != self.pgid || current.group != self.pgid {
            return Err(unavailable("owner identity changed"));
        }
        if !self.pair.valid(deadline)? {
            return Err(unavailable("owner dead"));
        }
        Ok(())
    }
}
struct Snapshot {
    started: Instant,
    captured: Instant,
    /// Stat files the snapshot read, of the PIDs it walked.
    #[cfg(test)]
    reads: usize,
    keys: HashSet<u64>,
    values: Result<HashMap<u64, u64>, Unavailable>,
}
#[derive(Default)]
struct Registry {
    next: u64,
    owners: HashMap<u64, Weak<Owner>>,
    compatibility: HashMap<u32, (Arc<Owner>, Instant)>,
    snapshot: Option<Snapshot>,
    census: SessionCache,
}
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}
impl Registry {
    fn create(&mut self, pgid: u32, path: &Path) -> Result<Arc<Owner>, Unavailable> {
        self.create_bounded(pgid, path, MAX_GROUPS)
    }
    fn create_bounded(
        &mut self,
        pgid: u32,
        path: &Path,
        max_groups: usize,
    ) -> Result<Arc<Owner>, Unavailable> {
        if !(2..=i32::MAX as u32).contains(&pgid) {
            return Err(unavailable("invalid process group"));
        }
        // Either public API can reclaim expired compatibility registrations.
        self.compatibility
            .retain(|_, (_, used)| used.elapsed() <= SNAPSHOT_TTL);
        self.owners.retain(|_, owner| owner.strong_count() > 0);
        if self.owners.len() >= max_groups {
            return Err(unavailable("owner bound"));
        }
        let proc = Proc::new(path)?;
        let pair = proc
            .pair(pgid, Instant::now() + SCAN_TIME)
            .map_err(Unavailable::from)?;
        if pair.original.group != pgid {
            return Err(unavailable("owner is not the process-group leader"));
        }
        self.next = self
            .next
            .checked_add(1)
            .ok_or_else(|| unavailable("owner key exhausted"))?;
        let owner = Arc::new(Owner {
            key: self.next,
            pgid,
            session: pair.original.session,
            proc,
            pair,
        });
        self.owners.insert(owner.key, Arc::downgrade(&owner));
        Ok(owner)
    }
    fn seconds(&mut self, owner: &Owner) -> Result<f64, Unavailable> {
        let deadline = Instant::now() + SCAN_TIME;
        owner.authenticate(deadline)?;
        let fresh = self.snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.captured.elapsed() <= SNAPSHOT_TTL && snapshot.keys.contains(&owner.key)
        });
        if !fresh {
            let owners: Vec<_> = self.owners.values().filter_map(Weak::upgrade).collect();
            let active: Vec<_> = owners
                .iter()
                .filter(|entry| entry.authenticate(deadline).is_ok())
                .collect();
            let groups = active.iter().map(|entry| entry.pgid).collect();
            let source = KernelSource {
                proc: &owner.proc,
                bound: MAX_MEMBERS,
                sessions: active.iter().map(|entry| entry.session).collect(),
                cache: std::cell::RefCell::new(&mut self.census),
                reads: std::cell::Cell::new(0),
            };
            let started = Instant::now();
            let values = scan(&source, &groups, deadline).map(|totals| {
                let mut values = HashMap::new();
                for entry in active {
                    // Authenticate again: the root may have detached during the scan.
                    if entry.authenticate(deadline).is_ok() {
                        if let Some(ticks) = totals.get(&entry.pgid) {
                            values.insert(entry.key, *ticks);
                        }
                    }
                }
                values
            });
            #[cfg(test)]
            let reads = source.reads.get();
            self.snapshot = Some(Snapshot {
                started,
                captured: Instant::now(),
                #[cfg(test)]
                reads,
                keys: owners.iter().map(|entry| entry.key).collect(),
                values,
            });
        }
        owner.authenticate(deadline)?;
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| unavailable("missing snapshot"))?;
        if snapshot.captured.duration_since(snapshot.started) > SCAN_TIME {
            return Err(unavailable("scan deadline"));
        }
        let ticks = snapshot
            .values
            .as_ref()
            .map_err(Clone::clone)?
            .get(&owner.key)
            .ok_or_else(|| unavailable("no measured members"))?;
        Ok(*ticks as f64 / clk_tck()?)
    }
}

/// Owned invocation identity. Create at spawn and retain until its monitor stops.
/// This object never rebinds to a later occupant of the same numeric PID/PGID.
pub struct ProcessGroupCpu {
    owner: Arc<Owner>,
}
impl ProcessGroupCpu {
    /// Authenticate the process-group leader and register for shared sampling.
    pub fn new(pgid: u32) -> Result<Self, Unavailable> {
        let mut registry = registry()
            .lock()
            .map_err(|_| unavailable("poisoned CPU registry"))?;
        Ok(Self {
            owner: registry.create(pgid, Path::new("/proc"))?,
        })
    }
    /// Checked CPU seconds, or typed unavailability rather than a partial/zero sum.
    pub fn seconds(&self) -> Result<f64, Unavailable> {
        registry()
            .lock()
            .map_err(|_| unavailable("poisoned CPU registry"))?
            .seconds(&self.owner)
    }
}

/// Compatibility projection. New monitors should retain [`ProcessGroupCpu`] instead.
pub fn subtree_cpu_seconds(pgid: u32) -> Option<f64> {
    if !(2..=i32::MAX as u32).contains(&pgid) {
        return None;
    }
    let mut registry = registry().lock().ok()?;
    registry
        .compatibility
        .retain(|_, (_, used)| used.elapsed() <= SNAPSHOT_TTL);
    let cached = registry
        .compatibility
        .get(&pgid)
        .map(|(owner, _)| Arc::clone(owner));
    let owner = match cached {
        Some(owner) if owner.authenticate(Instant::now() + SCAN_TIME).is_ok() => owner,
        _ => {
            registry.compatibility.remove(&pgid);
            registry.create(pgid, Path::new("/proc")).ok()?
        }
    };
    registry
        .compatibility
        .insert(pgid, (Arc::clone(&owner), Instant::now()));
    registry.seconds(&owner).ok()
}

/// Uncached observation from an explicit authentic procfs/PID view, not synthetic stat files.
pub fn subtree_cpu_seconds_in(pgid: u32, proc_root: &Path) -> Option<f64> {
    if !(2..=i32::MAX as u32).contains(&pgid) {
        return None;
    }
    let mut local = Registry::default();
    let owner = local.create(pgid, proc_root).ok()?;
    local.seconds(&owner).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway procfs root. Deliberately NOT `tempfile`: adding a dev-dependency would
    /// need a registry fetch, and this test only needs a unique directory it removes itself.
    struct TmpRoot(PathBuf);
    impl TmpRoot {
        fn new(label: &str) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let p = std::env::temp_dir().join(format!(
                "proccpu-{}-{}-{}",
                label,
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TmpRoot(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TmpRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A synthetic procfs proves the field offsets and the aggregation rule without
    /// depending on live process timing. The layout deliberately includes a comm with a
    /// space and a ')' in it, which is the exact case a naive whitespace split gets wrong.
    fn write_stat(root: &Path, pid: u32, comm: &str, pgrp: u32, cpu: [u64; 4]) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        let mut f = vec![format!("{pid}"), format!("({comm})"), "R".into()];
        f.push("1".into()); // ppid
        f.push(pgrp.to_string()); // pgrp
        for _ in 0..6 {
            f.push("0".into()); // session..cmajflt fillers
        }
        f.push("0".into());
        f.push("0".into());
        f.push(cpu[0].to_string()); // utime
        f.push(cpu[1].to_string()); // stime
        f.push(cpu[2].to_string()); // cutime
        f.push(cpu[3].to_string()); // cstime
        fs::write(dir.join("stat"), f.join(" ")).unwrap();
    }

    #[test]
    fn sums_only_the_named_group_and_includes_reaped_children() {
        let tmp = TmpRoot::new("group");
        let root = tmp.path();
        // Two members of the target group, plus one process in a DIFFERENT group whose CPU
        // must not be attributed to the step.
        write_stat(root, 100, "leader", 100, [10, 5, 20, 5]);
        write_stat(root, 101, "child (x)", 100, [30, 0, 0, 0]);
        write_stat(root, 200, "stranger", 200, [9999, 9999, 9999, 9999]);
        let got = synthetic_seconds(100, root).unwrap();
        // (10+5+20+5) + (30) = 70 ticks
        assert!(
            (got - 70.0 / clk_tck().unwrap()).abs() < 1e-9,
            "expected the two group members' own+reaped CPU and nothing from the stranger, got {got}"
        );
    }

    #[test]
    fn zero_is_a_reading_but_absence_is_unknown() {
        let tmp = TmpRoot::new("absent");
        write_stat(tmp.path(), 100, "leader", 100, [0, 0, 0, 0]);
        assert_eq!(synthetic_seconds(100, tmp.path()), Some(0.0));
        assert_eq!(synthetic_seconds(999, tmp.path()), None);
    }

    #[test]
    fn unreadable_or_malformed_procfs_is_unknown() {
        let tmp = TmpRoot::new("malformed");
        assert_eq!(synthetic_seconds(100, &tmp.path().join("missing")), None);
        let dir = tmp.path().join("100");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("stat"), "malformed").unwrap();
        assert_eq!(synthetic_seconds(100, tmp.path()), None);
    }

    #[test]
    fn refuses_degenerate_pgids() {
        // pgid 0 means "my own group" and 1 is init; either would attribute unrelated CPU
        // to the step and reap it spuriously, so both must be refused, not measured.
        assert_eq!(subtree_cpu_seconds(0), None);
        assert_eq!(subtree_cpu_seconds(1), None);
    }
    struct Row {
        original: Stat,
        bits: i16,
        fresh: Result<Option<Stat>, Unavailable>,
    }
    impl Observation for Row {
        fn original(&self) -> &Stat {
            &self.original
        }
        fn valid(&self, _deadline: Instant) -> Result<bool, Unavailable> {
            accept(self.bits, &self.original, || self.fresh.clone())
        }
    }
    struct Files<'a>(&'a Path);
    impl Source for Files<'_> {
        type Row = Row;
        fn samples(
            &self,
            groups: &HashSet<u32>,
            _deadline: Instant,
        ) -> Result<Vec<Row>, Unavailable> {
            let mut rows = Vec::new();
            for entry in fs::read_dir(self.0)? {
                let original = stat(&fs::read_to_string(entry?.path().join("stat"))?)?;
                if groups.contains(&original.group) {
                    rows.push(Row {
                        original,
                        bits: 0,
                        fresh: Ok(None),
                    });
                }
            }
            Ok(rows)
        }
    }
    // Fixture adaptation only: fake lifecycle observations feed the same two-phase
    // aggregation. Live APIs never authenticate an ordinary directory as procfs.
    fn synthetic_seconds(pgid: u32, root: &Path) -> Option<f64> {
        scan(
            &Files(root),
            &HashSet::from([pgid]),
            Instant::now() + SCAN_TIME,
        )
        .ok()?
        .get(&pgid)
        .and_then(|ticks| clk_tck().ok().map(|rate| *ticks as f64 / rate))
    }

    #[test]
    fn shared_canonical_lifecycle_corpus() {
        struct Corpus<'a>(&'a serde_json::Value);
        impl Source for Corpus<'_> {
            type Row = Row;
            fn samples(
                &self,
                groups: &HashSet<u32>,
                _deadline: Instant,
            ) -> Result<Vec<Row>, Unavailable> {
                self.0["rows"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|raw| groups.contains(&(raw["group"].as_u64().unwrap() as u32)))
                    .map(|raw| {
                        let original = Stat {
                            pid: raw["pid"].as_u64().unwrap() as u32,
                            group: raw["group"].as_u64().unwrap() as u32,
                            session: 0,
                            state: 'R',
                            ticks: raw["ticks"].as_u64().unwrap(),
                        };
                        let fresh = if let Some(error) = raw["error"].as_str() {
                            Err(unavailable(error))
                        } else if raw["state"] == "gone" {
                            Ok(None)
                        } else {
                            Ok(Some(Stat {
                                pid: raw["fresh_pid"].as_u64().unwrap_or(u64::from(original.pid))
                                    as u32,
                                group: original.group,
                                session: 0,
                                state: raw["state"].as_str().unwrap().chars().next().unwrap(),
                                ticks: raw["fresh_ticks"].as_u64().unwrap_or(0),
                            }))
                        };
                        Ok(Row {
                            original,
                            bits: raw["poll"].as_i64().unwrap() as i16,
                            fresh,
                        })
                    })
                    .collect()
            }
        }
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/proccpu-generation.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let result = scan(
                &Corpus(case),
                &HashSet::from([100]),
                Instant::now() + SCAN_TIME,
            );
            if case["error"] == true {
                assert!(result.is_err(), "{}", case["name"]);
            } else {
                assert_eq!(
                    result.unwrap().get(&100).copied(),
                    case["expected"].as_u64(),
                    "{}",
                    case["name"]
                );
            }
            println!("canonical CPU case: {}", case["name"]);
        }
    }

    #[test]
    fn all_samples_precede_every_validation() {
        struct OrderedRow {
            row: Stat,
            events: Rc<RefCell<Vec<&'static str>>>,
        }
        impl Observation for OrderedRow {
            fn original(&self) -> &Stat {
                &self.row
            }
            fn valid(&self, _deadline: Instant) -> Result<bool, Unavailable> {
                assert_eq!(&self.events.borrow()[..3], &["sample", "sample", "sample"]);
                self.events.borrow_mut().push("validate");
                Ok(true)
            }
        }
        struct OrderedSource(Rc<RefCell<Vec<&'static str>>>);
        impl Source for OrderedSource {
            type Row = OrderedRow;
            fn samples(
                &self,
                _groups: &HashSet<u32>,
                _deadline: Instant,
            ) -> Result<Vec<OrderedRow>, Unavailable> {
                Ok((0..3)
                    .map(|index| {
                        self.0.borrow_mut().push("sample");
                        OrderedRow {
                            row: Stat {
                                pid: 100 + index,
                                group: 100,
                                session: 0,
                                state: 'R',
                                ticks: 1,
                            },
                            events: Rc::clone(&self.0),
                        }
                    })
                    .collect())
            }
        }
        let events = Rc::new(RefCell::new(Vec::new()));
        assert_eq!(
            scan(
                &OrderedSource(Rc::clone(&events)),
                &HashSet::from([100]),
                Instant::now() + SCAN_TIME
            )
            .unwrap()[&100],
            3
        );
        assert_eq!(
            *events.borrow(),
            ["sample", "sample", "sample", "validate", "validate", "validate"]
        );
    }

    #[test]
    fn stat_fdinfo_and_synthetic_root_refusals() {
        for text in ["Pid: -1\nPid: 100\n", "Pid: 0", "Pid: nope", ""] {
            assert!(fd_pid(text).is_err());
        }
        assert_eq!(fd_pid("Pid: -1\n").unwrap(), -1);
        assert_eq!(fd_pid("Pid: 100\n").unwrap(), 100);
        for counter in ["-1", "18446744073709551616"] {
            assert!(stat(&format!(
                "100 (x) R 1 100 {}{counter} 0 0 0",
                "0 ".repeat(8)
            ))
            .is_err());
        }
        let tmp = TmpRoot::new("not-procfs");
        write_stat(tmp.path(), 100, "leader", 100, [0, 0, 0, 0]);
        assert_eq!(subtree_cpu_seconds_in(100, tmp.path()), None);
    }

    struct Native {
        child: std::process::Child,
        output: std::io::BufReader<std::process::ChildStdout>,
        _root: TmpRoot,
    }
    impl Native {
        fn start(mode: &str) -> Self {
            use std::os::unix::process::CommandExt;
            use std::process::{Command, Stdio};
            let root = TmpRoot::new(mode);
            let source = root.path().join("helper.c");
            let binary = root.path().join("helper");
            fs::write(&source, include_str!("../tests/fixtures/proccpu-helper.c")).unwrap();
            assert!(Command::new("cc")
                .args(["-Wall", "-Wextra", "-Werror", "-pthread"])
                .arg(source)
                .arg("-o")
                .arg(&binary)
                .status()
                .unwrap()
                .success());
            let mut child = Command::new(binary)
                .arg(mode)
                .process_group(0)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let output = std::io::BufReader::new(child.stdout.take().unwrap());
            Self {
                child,
                output,
                _root: root,
            }
        }
        fn line(&mut self) -> String {
            use std::io::BufRead;
            let mut event = libc::pollfd {
                fd: self.output.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: event describes one live owned pipe; finite wait only in the native test.
            assert_eq!(
                unsafe { libc::poll(&mut event, 1, 5000) },
                1,
                "native fixture output deadline"
            );
            let mut line = String::new();
            self.output.read_line(&mut line).unwrap();
            line.trim().into()
        }
        fn send(&mut self, byte: u8) {
            use std::io::Write;
            self.child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(&[byte])
                .unwrap();
            self.child.stdin.as_mut().unwrap().flush().unwrap();
        }
    }
    impl Drop for Native {
        fn drop(&mut self) {
            // Only the fixture process started by this object is signalled/reaped.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn native_unreaped_zombie_and_reaping_preserve_cpu() {
        let mut child = Native::start("zombie");
        let zombie = child.line().parse::<u32>().unwrap();
        let reader = ProcessGroupCpu::new(child.child.id()).unwrap();
        let pair = reader
            .owner
            .proc
            .pair(zombie, Instant::now() + SCAN_TIME)
            .map_err(Unavailable::from)
            .unwrap();
        assert_eq!(pair.original.state, 'Z');
        assert!(pair.original.ticks > 0);
        assert!(pair.valid(Instant::now() + SCAN_TIME).unwrap());
        let before = reader.seconds().unwrap();
        assert!(before >= pair.original.ticks as f64 / clk_tck().unwrap());
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        assert!(!pair.valid(Instant::now() + SCAN_TIME).unwrap());
        registry().lock().unwrap().snapshot = None;
        assert!(reader.seconds().unwrap() >= before);
        child.send(b'x');
    }

    #[test]
    fn native_exited_leader_with_live_worker_remains_measurable() {
        let mut child = Native::start("leader");
        assert_eq!(child.line(), "worker");
        let reader = ProcessGroupCpu::new(child.child.id()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current = stat(&read_record(&reader.owner.pair.file, deadline).unwrap()).unwrap();
            if current.state == 'Z' {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(reader.owner.pair.valid(deadline).unwrap());
        let before = reader.seconds().unwrap();
        child.send(b'b');
        assert_eq!(child.line(), "worked");
        registry().lock().unwrap().snapshot = None;
        assert!(reader.seconds().unwrap() > before);
        child.send(b'x');
    }

    #[test]
    fn invocation_keys_and_shared_snapshot_do_not_rebind() {
        let mut child = Native::start("zombie");
        let _ = child.line();
        let mut registry = Registry::default();
        let first = registry
            .create(child.child.id(), Path::new("/proc"))
            .unwrap();
        let second = registry
            .create(child.child.id(), Path::new("/proc"))
            .unwrap();
        assert_ne!(first.key, second.key);
        let reading = registry.seconds(&first).unwrap();
        let captured = registry.snapshot.as_ref().unwrap().captured;
        assert_eq!(registry.seconds(&second).unwrap(), reading);
        assert_eq!(registry.snapshot.as_ref().unwrap().captured, captured);
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        child.send(b'x');
        assert!(child.child.wait().unwrap().success());
        assert!(
            registry.seconds(&first).is_err(),
            "cached reading requires a still-attached owner generation"
        );
        assert!(registry.seconds(&second).is_err());
    }
    #[test]
    fn scanner_member_bound_and_cached_error_are_unavailable() {
        let mut child = Native::start("zombie");
        let _ = child.line();
        let mut registry = Registry::default();
        let owner = registry
            .create(child.child.id(), Path::new("/proc"))
            .unwrap();
        let mut cache = SessionCache::default();
        let bounded = scan(
            &KernelSource {
                proc: &owner.proc,
                bound: 1,
                sessions: HashSet::from([owner.session]),
                cache: RefCell::new(&mut cache),
                reads: std::cell::Cell::new(0),
            },
            &HashSet::from([owner.pgid]),
            Instant::now() + SCAN_TIME,
        );
        assert_eq!(bounded.unwrap_err(), unavailable("member bound"));
        // A stored refusal is reused as a refusal; it cannot become a real zero or a
        // successful scan merely because a second monitor consumes the shared snapshot.
        registry.snapshot = Some(Snapshot {
            started: Instant::now(),
            captured: Instant::now(),
            reads: 0,
            keys: HashSet::from([owner.key]),
            values: Err(unavailable("injected scanner EACCES")),
        });
        assert_eq!(
            registry.seconds(&owner).unwrap_err(),
            unavailable("injected scanner EACCES")
        );
        assert_eq!(
            registry.seconds(&owner).unwrap_err(),
            unavailable("injected scanner EACCES")
        );
        for key in 0..MAX_GROUPS as u64 {
            registry.owners.insert(key, Arc::downgrade(&owner));
        }
        assert!(
            matches!(registry.create(child.child.id(), Path::new("/proc")), Err(error) if error == unavailable("owner bound"))
        );
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        child.send(b'x');
    }

    #[test]
    fn pid_t_range_is_refused_by_owned_and_compatibility_apis() {
        for pid in [0, 1, i32::MAX as u32 + 1, u32::MAX] {
            assert!(
                matches!(ProcessGroupCpu::new(pid), Err(error) if error == unavailable("invalid process group"))
            );
            assert_eq!(subtree_cpu_seconds(pid), None);
            assert_eq!(subtree_cpu_seconds_in(pid, Path::new("/missing")), None);
        }
    }

    #[test]
    fn new_owned_registration_evicts_expired_compatibility_owner() {
        let mut child = Native::start("zombie");
        let _ = child.line();
        let mut registry = Registry::default();
        let old = registry
            .create(child.child.id(), Path::new("/proc"))
            .unwrap();
        let old_key = old.key;
        registry.seconds(&old).unwrap();
        registry
            .compatibility
            .insert(old.pgid, (old, Instant::now() - Duration::from_secs(1)));
        let new = registry
            .create_bounded(child.child.id(), Path::new("/proc"), 1)
            .unwrap();
        assert_ne!(new.key, old_key);
        assert!(registry.compatibility.is_empty());
        assert_eq!(registry.owners.len(), 1);
        assert!(registry.seconds(&new).unwrap() >= 0.0);
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        child.send(b'x');
    }

    #[test]
    fn pairing_order_rejects_replacement_zombie_and_drops_both_handles() {
        struct Handle {
            name: &'static str,
            events: Rc<RefCell<Vec<&'static str>>>,
        }
        impl Drop for Handle {
            fn drop(&mut self) {
                self.events.borrow_mut().push(self.name);
            }
        }
        let events = Rc::new(RefCell::new(Vec::new()));
        let result = bind_pair(
            100,
            || {
                events.borrow_mut().push("pidfd-open");
                Ok(Handle {
                    name: "drop-pidfd",
                    events: Rc::clone(&events),
                })
            },
            || {
                events.borrow_mut().push("stat-open");
                Ok(Handle {
                    name: "drop-stat",
                    events: Rc::clone(&events),
                })
            },
            |_| {
                events.borrow_mut().push("stat-read");
                Ok(Stat {
                    pid: 100,
                    group: 100,
                    session: 0,
                    state: 'Z',
                    ticks: 999,
                })
            },
            |_| {
                events.borrow_mut().push("fdinfo");
                Ok(-1)
            },
        );
        assert!(
            matches!(result, Err(PairError::Unavailable(error)) if error == unavailable("pidfd/stat generation mismatch"))
        );
        assert_eq!(
            *events.borrow(),
            [
                "pidfd-open",
                "stat-open",
                "stat-read",
                "fdinfo",
                "drop-stat",
                "drop-pidfd"
            ]
        );
    }

    #[test]
    fn native_nonleader_exec_preserves_held_identity() {
        let mut child = Native::start("exec");
        assert_eq!(child.line(), "thread");
        let reader = ProcessGroupCpu::new(child.child.id()).unwrap();
        let key = reader.owner.key;
        let before = reader.seconds().unwrap();
        child.send(b'e');
        assert_eq!(child.line(), "executed");
        let deadline = Instant::now() + SCAN_TIME;
        assert_eq!(
            reader
                .owner
                .proc
                .pid(&reader.owner.pair.pidfd, deadline)
                .unwrap(),
            i64::from(child.child.id())
        );
        assert_eq!(
            stat(&read_record(&reader.owner.pair.file, deadline).unwrap())
                .unwrap()
                .pid,
            child.child.id()
        );
        assert!(reader.owner.pair.valid(deadline).unwrap());
        child.send(b'b');
        assert_eq!(child.line(), "worked");
        registry().lock().unwrap().snapshot = None;
        let after = reader.seconds().unwrap();
        assert!(after > before);
        assert_eq!(reader.owner.key, key);
        println!(
            "native nonleader exec pid={} owner={key} before_cpu={before} after_cpu={after}",
            child.child.id()
        );
        child.send(b'x');
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn native_ptrace_reparent_keeps_zombie_until_real_parent_reaps() {
        let mut child = Native::start("trace");
        let tracee = child.line().parse::<u32>().unwrap();
        let reader = ProcessGroupCpu::new(child.child.id()).unwrap();
        let pair = reader
            .owner
            .proc
            .pair(tracee, Instant::now() + SCAN_TIME)
            .map_err(Unavailable::from)
            .unwrap();
        let waited = || {
            let text = read_record(&reader.owner.pair.file, Instant::now() + SCAN_TIME).unwrap();
            let fields: Vec<_> = text
                .rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .collect();
            fields[13].parse::<u64>().unwrap() + fields[14].parse::<u64>().unwrap()
        };
        assert_eq!(pair.original.state, 'Z');
        assert!(pair.original.ticks > 0);
        assert!(pair.valid(Instant::now() + SCAN_TIME).unwrap());
        assert!(fs::read_to_string(format!("/proc/{tracee}/status"))
            .unwrap()
            .contains(&format!("TracerPid:\t{}\n", child.child.id())));
        let before_waited = waited();
        let before = reader.seconds().unwrap();
        child.send(b't');
        assert_eq!(child.line(), "detached");
        assert!(fs::read_to_string(format!("/proc/{tracee}/status"))
            .unwrap()
            .contains("TracerPid:\t0\n"));
        assert_eq!(
            waited(),
            before_waited,
            "ptracer EXIT_TRACE must not credit child CPU"
        );
        assert!(pair.valid(Instant::now() + SCAN_TIME).unwrap());
        assert_eq!(
            stat(&read_record(&pair.file, Instant::now() + SCAN_TIME).unwrap())
                .unwrap()
                .state,
            'Z'
        );
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        assert!(!pair.valid(Instant::now() + SCAN_TIME).unwrap());
        registry().lock().unwrap().snapshot = None;
        let after = reader.seconds().unwrap();
        assert!(after >= before);
        println!("native ptrace reparent pid={} tracee={tracee} zombie_ticks={} ptracer_waited={before_waited} before_cpu={before} after_cpu={after}", child.child.id(), pair.original.ticks);
        child.send(b'x');
        assert!(child.child.wait().unwrap().success());
    }

    #[test]
    fn positive_clock_rate_and_namespace_metadata_are_required() {
        assert_eq!(checked_clk_tck(100).unwrap(), 100.0);
        assert_eq!(checked_clk_tck(250).unwrap(), 250.0);
        for value in [0, -1] {
            assert!(checked_clk_tck(value)
                .unwrap_err()
                .to_string()
                .contains("SC_CLK_TCK unavailable"));
        }
        assert!(verify_namespace_status(b"NSpid:\t100\n", 100).is_ok());
        assert!(verify_namespace_status(b"Name:\ttest\n", 100)
            .unwrap_err()
            .to_string()
            .contains("NSpid missing"));
        for text in [
            b"NSpid: 100 1\n".as_slice(),
            b"NSpid: 101\n".as_slice(),
            b"NSpid: 100\nNSpid: 100\n".as_slice(),
            b"Name:\tx\nNSpid:\t100\nState:\tS\nNSpid:\t100\n".as_slice(),
        ] {
            assert!(verify_namespace_status(text, 100).is_err());
        }
    }

    fn run_isolated_native_case(name: &str) -> bool {
        use std::io::Write;

        const CASE: &str = "AU_PROCCPU_ISOLATED_CASE";
        match std::env::var(CASE) {
            Ok(case) => {
                assert_eq!(case, name, "unexpected isolated test recursion marker");
                return false;
            }
            Err(std::env::VarError::NotPresent) => {}
            Err(error) => panic!("invalid isolated test recursion marker: {error}"),
        }
        eprintln!(
            "native isolated case start: {name} parent={} thread={:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CASE, name)
            .output()
            .unwrap();
        std::io::stdout().lock().write_all(&output.stdout).unwrap();
        std::io::stderr().lock().write_all(&output.stderr).unwrap();
        eprintln!(
            "native isolated case finish: {name} status={}",
            output.status
        );
        assert!(output.status.success(), "isolated case failed: {name}");
        true
    }

    #[test]
    fn native_public_compatibility_and_unusual_comm() {
        if run_isolated_native_case("proccpu::tests::native_public_compatibility_and_unusual_comm")
        {
            return;
        }
        let mut child = Native::start("comm");
        let zombie = child.line().parse::<u32>().unwrap();
        let raw = fs::read(format!("/proc/{zombie}/stat")).unwrap();
        assert!(raw.windows(6).any(|bytes| bytes == b"p\xff)\n(\x80"));
        let pid = child.child.id();
        let first = subtree_cpu_seconds(pid).unwrap();
        assert!(first > 0.0);
        let key = registry().lock().unwrap().compatibility[&pid].0.key;
        assert_eq!(subtree_cpu_seconds(pid), Some(first));
        assert_eq!(registry().lock().unwrap().compatibility[&pid].0.key, key);
        assert!(subtree_cpu_seconds_in(pid, Path::new("/proc")).unwrap() >= first);
        std::thread::sleep(SNAPSHOT_TTL + Duration::from_millis(50));
        assert!(subtree_cpu_seconds(pid).unwrap() >= first);
        assert_ne!(registry().lock().unwrap().compatibility[&pid].0.key, key);
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        child.send(b'x');
        assert!(child.child.wait().unwrap().success());
        assert_eq!(subtree_cpu_seconds(pid), None);
        assert!(!registry().lock().unwrap().compatibility.contains_key(&pid));
    }

    #[test]
    fn native_shared_sampling_cost_and_availability() {
        if run_isolated_native_case("proccpu::tests::native_shared_sampling_cost_and_availability")
        {
            return;
        }
        fn self_cpu() -> f64 {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            // SAFETY: usage is writable storage; this only observes the test process.
            assert_eq!(
                unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) },
                0
            );
            let usage = unsafe { usage.assume_init() };
            usage.ru_utime.tv_sec as f64
                + usage.ru_utime.tv_usec as f64 / 1e6
                + usage.ru_stime.tv_sec as f64
                + usage.ru_stime.tv_usec as f64 / 1e6
        }
        for groups in [1, 4, 16] {
            let before = fs::read_dir("/proc/self/fd").unwrap().count();
            let mut children = Vec::new();
            let mut readers = Vec::new();
            for _ in 0..groups {
                let mut child = Native::start("zombie");
                child.line();
                readers.push(ProcessGroupCpu::new(child.child.id()).unwrap());
                children.push(child);
            }
            registry().lock().unwrap().snapshot = None;
            let first_start = Instant::now();
            let first_cpu = self_cpu();
            for reader in &readers {
                assert!(reader.seconds().unwrap() > 0.0);
            }
            let first_cpu = self_cpu() - first_cpu;
            let first_seconds = first_start.elapsed().as_secs_f64();
            let captured = registry()
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .captured;
            let cached_start = Instant::now();
            let cached_cpu = self_cpu();
            for _ in 0..10 {
                for reader in &readers {
                    assert!(reader.seconds().unwrap() > 0.0);
                }
            }
            let cached_cpu = self_cpu() - cached_cpu;
            let cached_seconds = cached_start.elapsed().as_secs_f64();
            let one_snapshot = registry()
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .captured
                == captured;
            let held = fs::read_dir("/proc/self/fd").unwrap().count();
            let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
            // SAFETY: writable rlimit storage for an observation only; no limits change.
            assert_eq!(
                unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
                0
            );
            let limit = unsafe { limit.assume_init() };
            println!(
                "PROCCPU_COST {}",
                serde_json::json!({"edition":"rust","groups":groups,"matching_processes":groups*2,"first_wall_s":first_seconds,"first_self_cpu_s":first_cpu,"cached_wall_s":cached_seconds,"cached_self_cpu_s":cached_cpu,"cached_calls":groups*10,"one_cached_snapshot":one_snapshot,"fd_before":before,"fd_held":held,"rlimit_nofile":[limit.rlim_cur,limit.rlim_max]})
            );
            drop(readers);
            for child in &mut children {
                child.send(b'r');
                assert_eq!(child.line(), "reaped");
                child.send(b'x');
                assert!(child.child.wait().unwrap().success());
            }
            drop(children);
            assert_eq!(fs::read_dir("/proc/self/fd").unwrap().count(), before);
        }
    }

    #[test]
    fn native_scheduler_retains_constructor_and_sample_failure_reasons() {
        const CASE: &str = "AU_PROCCPU_REASON_CONTROL";
        if let Ok(stage) = std::env::var(CASE) {
            fn pressure() {
                let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
                // SAFETY: only this isolated test subprocess lowers its own soft FD limit.
                assert_eq!(
                    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) },
                    0
                );
                let mut limit = unsafe { limit.assume_init() };
                limit.rlim_cur = 64;
                assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
            }
            let watcher = match stage.as_str() {
                "registration" => {
                    pressure();
                    None
                }
                "observation" => Some(std::thread::spawn(|| {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    loop {
                        if registry()
                            .lock()
                            .unwrap()
                            .owners
                            .values()
                            .any(|owner| owner.strong_count() > 0)
                        {
                            pressure();
                            return;
                        }
                        assert!(Instant::now() < deadline, "registration event deadline");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                })),
                _ => panic!("unknown isolated reason case"),
            };
            let cfg = crate::io::dag_from_json(r#"{"steps":[{"group":"g","job":"quiet","desc":"known refusal","cmd":"sleep 2.5","cpu_timeout":1,"timeout":30}]}"#).unwrap();
            let result = crate::scheduler::run_dag(&cfg, 1, false, 0);
            assert!(result.ok);
            if let Some(watcher) = watcher {
                watcher.join().unwrap();
            }
            return;
        }
        for stage in ["registration", "observation"] {
            let logs = TmpRoot::new("reason-log");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(logs.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "proccpu::tests::native_scheduler_retains_constructor_and_sample_failure_reasons", "--nocapture"])
                .env(CASE, stage).env(crate::attribution::LOG_DIR_ENV, logs.path()).output().unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success(),
                "{stage}: {stderr} {}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert_eq!(stderr.matches("CANNOT be enforced").count(), 1, "{stderr}");
            assert!(
                stderr.contains(&format!("{stage} failed: descriptor reserve")),
                "{stderr}"
            );
            if stage == "registration" {
                assert!(
                    stderr.contains("not retried after waiting starts"),
                    "{stderr}"
                );
            }
            println!("native scheduler reason {stage}: {stderr}");
        }
    }

    /// The shared session-cache corpus, also run by the other-language edition, so both make the
    /// same read decisions on the same snapshots.
    #[test]
    fn shared_session_cache_corpus() {
        fn allocator(raw: &serde_json::Value) -> Option<Allocator> {
            raw.as_object().map(|raw| Allocator {
                last_pid: raw["last_pid"].as_u64().unwrap() as u32,
                tasks: raw["tasks"].as_u64().unwrap(),
                forks: raw["forks"].as_u64().unwrap(),
            })
        }
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/proccpu-session-cache.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let pid_max = case["pid_max"].as_u64().map(|value| value as u32);
            let mut cache = SessionCache::default();
            for (index, step) in case["steps"].as_array().unwrap().iter().enumerate() {
                let start = allocator(&step["allocator"]);
                let end = step.get("end").map_or(start, allocator);
                let sessions: HashSet<u32> = step["group_sessions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_u64().unwrap() as u32)
                    .collect();
                cache.begin(start, pid_max);
                let mut read = Vec::new();
                let mut skipped = Vec::new();
                let mut seen = HashSet::new();
                let processes: Vec<(u32, u32)> = step["processes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|raw| {
                        (
                            raw["pid"].as_u64().unwrap() as u32,
                            raw["session"].as_u64().unwrap() as u32,
                        )
                    })
                    .collect();
                for &(pid, session) in &processes {
                    seen.insert(pid);
                    if cache.skip(pid, &sessions) {
                        skipped.push((pid, session));
                    } else {
                        cache.record(pid, session);
                        read.push(pid);
                    }
                }
                for (pid, session) in skipped {
                    let fresh = match (start, end, pid_max) {
                        (Some(start), Some(end), Some(pid_max)) => {
                            reallocated(start, end, pid_max, pid)
                        }
                        _ => true,
                    };
                    if fresh {
                        cache.record(pid, session);
                        read.push(pid);
                    }
                }
                cache.retain(&seen);
                read.sort_unstable();
                let mut expected: Vec<u32> = step["read"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|value| value.as_u64().unwrap() as u32)
                    .collect();
                expected.sort_unstable();
                assert_eq!(read, expected, "{} step {index}", case["name"]);
            }
            println!("session cache case: {}", case["name"]);
        }
    }

    #[test]
    fn allocator_records_parse_and_refuse_ambiguity() {
        assert_eq!(
            parse_loadavg("170.04 167.90 165.36 49/63307 3954637\n"),
            Some((3954637, 63307))
        );
        assert_eq!(parse_loadavg("1.0 1.0 1.0 1/2"), None);
        assert_eq!(
            parse_forks("cpu  1 2 3\nprocesses 1547387564\nprocs_running 4\n"),
            Some(1547387564)
        );
        assert_eq!(parse_forks("cpu 1\n"), None);
        assert_eq!(parse_forks("processes 1\nprocesses 2\n"), None);
    }

    /// On the live host a second snapshot reads only the PIDs it could not skip, and
    /// measures the same members.
    #[test]
    fn native_second_snapshot_skips_other_sessions() {
        let mut child = Native::start("zombie");
        let _ = child.line();
        let mut registry = Registry::default();
        let owner = registry
            .create(child.child.id(), Path::new("/proc"))
            .unwrap();
        let first = registry.seconds(&owner).unwrap();
        let cold = registry.snapshot.as_ref().unwrap().reads;
        registry.snapshot = None;
        let second = registry.seconds(&owner).unwrap();
        let warm = registry.snapshot.as_ref().unwrap().reads;
        println!(
            "PROCCPU_SESSION_CACHE {}",
            serde_json::json!({"cold_reads": cold, "warm_reads": warm, "cached_sessions": registry.census.sessions.len()})
        );
        assert!(first > 0.0 && second >= first);
        assert!(
            warm * 4 < cold,
            "the second snapshot read {warm} stat files of the {cold} the first read"
        );
        child.send(b'r');
        assert_eq!(child.line(), "reaped");
        child.send(b'x');
        assert!(child.child.wait().unwrap().success());
    }
}
