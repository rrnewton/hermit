//! Private evidence for an original Socket's finite-close birth. Queries run
//! in the existing observation owner, never in the guest or at Close.
//! https://github.com/rrnewton/hermit/issues/3700
use std::ffi::CString;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use super::ForegroundRoot;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Admission;
use crate::scheduler::ordinary_fd::OrdinaryFdObservation;

const CGROUP_NS_INIT_INO: u64 = 0xefff_fffb;
const CGROUP2_MAGIC: libc::c_long = 0x6367_7270;
const PIDFS_MAGIC: libc::c_long = 0x5049_4446;
const BPF_PROG_QUERY: libc::c_uint = 16;
const BPF_F_QUERY_EFFECTIVE: u32 = 1;
const GETSOCKOPT: u32 = 21;
const SETSOCKOPT: u32 = 22;

/// Temporary finite-Close investigation on the existing supervisor channel.
/// No tracing record, guest output, query or authority is produced here.
/// Only fixed labels/numeric fields are passed; cap each process at 64 rows.
/// Missing/limited diagnostic evidence never proves admission or success.
pub(crate) fn decline_diagnostic(fields: std::fmt::Arguments<'_>) {
    use std::io::Write;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    static ROWS: AtomicUsize = AtomicUsize::new(0);
    let row = ROWS
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
            Some(n.saturating_add(1).min(65))
        })
        .unwrap();
    if row > 64 {
        return;
    }
    let line = if row == 64 {
        "hermit: finite-close-diagnostic row-limit=64\n".to_owned()
    } else {
        format!("hermit: finite-close-diagnostic row={row} {fields}\n")
    };
    let bytes = if line.len() <= 1024 {
        line.as_bytes()
    } else {
        b"hermit: finite-close-diagnostic row-too-long\n"
    };
    let _ = crate::util::RetryingStderr.write_all(bytes);
}

// Emit only the numeric prefix, never the existing private error's host paths.
fn diagnostic_failure(snapshot: &Snapshot) -> (u8, Option<i32>) {
    let Some(error) = snapshot.error.as_deref() else {
        return (0, None);
    };
    let Some(fields) = error.strip_prefix("finite-close-stage=") else {
        return (255, None);
    };
    let mut fields = fields.splitn(3, ';');
    let stage = fields.next().and_then(|s| s.parse().ok()).unwrap_or(255);
    let errno = fields
        .next()
        .and_then(|s| s.strip_prefix("errno="))
        .and_then(|s| s.parse().ok())
        .filter(|n| *n != 0);
    (stage, errno)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Node {
    device: u64,
    inode: u64,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Creator {
    pidfd: Node,
    pid: u32,
    tgid: u32,
    cgroup: u64,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Directory {
    node: Node,
    mount: u64,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Query {
    attach: u32,
    flags: u32,
    returned: i32,
    errno: Option<i32>,
    count: u32,
    attach_flags: u32,
    revision: u64,
}
impl Query {
    fn empty_effective(&self, attach: u32) -> bool {
        self.attach == attach
            && self.flags == BPF_F_QUERY_EFFECTIVE
            && self.returned == 0
            && self.errno.is_none()
            && self.count == 0
            && self.attach_flags == 0
            && self.revision == 0
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Release {
    returned: i32,
    errno: Option<i32>,
}

/// Full raw receipt remains private and deliberately redacts host identities
/// in Debug: adding evidence must not add cgroup paths/IDs to deterministic logs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Snapshot {
    creator: Option<Creator>,
    namespace: Option<Node>,
    membership: Option<String>,
    directory: Option<Directory>,
    queries: Option<[Query; 2]>,
    releases: Vec<Release>,
    opened: usize,
    error: Option<String>,
}
impl std::fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SocketBirthPolicySnapshot(private)")
    }
}
impl Snapshot {
    fn new() -> Self {
        Self {
            creator: None,
            namespace: None,
            membership: None,
            directory: None,
            queries: None,
            releases: Vec::new(),
            opened: 0,
            error: None,
        }
    }
    pub(super) fn released(&self) -> bool {
        self.opened == self.releases.len()
            && self
                .releases
                .iter()
                .all(|r| r.returned == 0 && r.errno.is_none())
    }
    fn identity_complete(&self) -> bool {
        self.error.is_none()
            && self.released()
            && self
                .creator
                .as_ref()
                .is_some_and(|c| c.pid != 0 && c.pid == c.tgid)
            && self
                .namespace
                .as_ref()
                .is_some_and(|n| n.inode == CGROUP_NS_INIT_INO)
            && self.membership.is_some()
            && self.directory.as_ref().is_some_and(|d| d.mount != 0)
    }
    pub(super) fn admits_getter(&self) -> bool {
        self.identity_complete()
            && self.queries.as_ref().is_some_and(|q| {
                q[0].empty_effective(GETSOCKOPT) && q[1].empty_effective(SETSOCKOPT)
            })
    }
    fn same_birth(&self, after: &Self) -> bool {
        self.identity_complete()
            && after.admits_getter()
            // PIDFD_GET_INFO reports PID/TGID in each observer's PID namespace;
            // mount IDs likewise belong to that observer. snapshot() authenticates
            // each local lookup before/after. Cross-owner joins use the retained
            // kernel objects, never equality of those unrelated coordinates.
            && self.creator.as_ref().zip(after.creator.as_ref()).is_some_and(|(a, b)| {
                a.pidfd == b.pidfd && a.cgroup == b.cgroup
            })
            && self.namespace == after.namespace
            && self.membership == after.membership
            && self.directory.as_ref().zip(after.directory.as_ref()).is_some_and(|(a, b)| a.node == b.node)
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Observation {
    pub(super) policy: Snapshot,
    returned: Option<i32>,
    errno: Option<i32>,
    length: u32,
    value: [i32; 2],
}
impl std::fmt::Debug for Observation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SocketBirthObservation(private)")
    }
}
impl Observation {
    fn disabled_linger(&self) -> Option<(i32, i32)> {
        (self.policy.admits_getter()
            && self.returned == Some(0)
            && self.errno.is_none()
            && self.length == 8
            && self.value[0] == 0)
            .then_some((self.value[0], self.value[1]))
    }
}
pub(super) fn observe(target: BorrowedFd<'_>, socket: BorrowedFd<'_>) -> Observation {
    let policy = snapshot(target, true);
    let mut receipt = Observation {
        policy,
        returned: None,
        errno: None,
        length: 8,
        value: [0; 2],
    };
    if receipt.policy.admits_getter() {
        let raw = unsafe {
            libc::getsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                receipt.value.as_mut_ptr().cast(),
                &raw mut receipt.length,
            )
        };
        receipt.returned = Some(raw);
        receipt.errno = (raw < 0).then(|| {
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        });
    }
    receipt
}

/// Minted by the real sole-initial Normal borrower, not by a socket argument.
/// This owns no new descriptor and grants no policy-absence fact by itself.
#[derive(Clone)]
pub(crate) struct SocketBirthAuthority {
    root: Arc<ForegroundRoot>,
    owner: NetworkStreamOwner,
    epoch: u64,
    admission: Admission,
}
impl std::fmt::Debug for SocketBirthAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SocketBirthAuthority(private)")
    }
}
impl SocketBirthAuthority {
    pub(crate) fn from_original(
        root: Arc<ForegroundRoot>,
        grant: &OrdinaryFdObservation<'_>,
        admission: &Admission,
    ) -> io::Result<Self> {
        let a = &admission.arguments;
        if !grant.admits_sole_initial_root(&root)
            || a.kind != crate::network_replay::original_connect::Kind::Socket
            || a.fd != libc::AF_INET
            || a.address as u32 as i32 & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                != libc::SOCK_STREAM
            || !matches!(a.length, 0 | libc::IPPROTO_TCP)
            || a.files != root.files()
        {
            return Err(io::Error::other(
                "Socket birth lacks original sole Normal authority",
            ));
        }
        Ok(Self {
            owner: grant.owner(),
            epoch: grant.epoch(),
            root,
            admission: admission.clone(),
        })
    }
    pub(crate) fn validate(
        &self,
        grant: &OrdinaryFdObservation<'_>,
        admission: &Admission,
    ) -> io::Result<()> {
        if grant.owner() != self.owner
            || grant.epoch() != self.epoch
            || !grant.admits_sole_initial_root(&self.root)
            || admission != &self.admission
        {
            return Err(io::Error::other(
                "Socket birth changed original Normal/call",
            ));
        }
        Ok(())
    }
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        &self.root
    }
}

#[derive(Clone, Debug)]
pub(super) struct Plan {
    pub(super) authority: SocketBirthAuthority,
    before: Snapshot,
}
impl Plan {
    pub(super) fn capture(authority: SocketBirthAuthority, target: BorrowedFd<'_>) -> Self {
        let mut before = snapshot(target, false);
        if before
            .creator
            .as_ref()
            .is_some_and(|c| c.pid != authority.root.thread() as u32)
        {
            before.error = Some(
                "finite-close-stage=16;errno=0; Socket birth PIDFD differs from original root"
                    .into(),
            );
        }
        decline_diagnostic(format_args!(
            "phase=birth-before call={} domain={} stage={} errno={:?} identity={} opened={} closed={} released={}",
            authority.admission.call.native_command_call(),
            authority.admission.arguments.fd,
            diagnostic_failure(&before).0,
            diagnostic_failure(&before).1,
            before.identity_complete(),
            before.opened,
            before.releases.len(),
            before.released()
        ));
        Self { authority, before }
    }
    pub(super) fn released(&self) -> bool {
        self.before.released()
    }

    pub(super) fn complete(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        after: &Snapshot,
        observation: &Observation,
    ) -> io::Result<Option<Arc<Completed>>> {
        if owner != self.authority.owner || admission != &self.authority.admission {
            return Err(io::Error::other(
                "Socket policy receipt changed original call",
            ));
        }
        if !self.before.released() || !after.released() {
            decline_diagnostic(format_args!(
                "phase=birth-release-refused call={} before_opened={} before_closed={} after_opened={} after_closed={} before_errno={:?} after_errno={:?}",
                admission.call.native_command_call(),
                self.before.opened,
                self.before.releases.len(),
                after.opened,
                after.releases.len(),
                self.before.releases.iter().find_map(|r| r.errno),
                after.releases.iter().find_map(|r| r.errno)
            ));
            return Err(io::Error::other(
                "Socket birth directory release remains unresolved",
            ));
        }
        let linger = observation.disabled_linger();
        decline_diagnostic(format_args!(
            "phase=birth-complete call={} before_stage={} before_errno={:?} after_stage={} after_errno={:?} same_birth={} policy_same={} linger_rc={:?} linger_errno={:?} linger_len={} linger_on={} eligible={}",
            admission.call.native_command_call(),
            diagnostic_failure(&self.before).0,
            diagnostic_failure(&self.before).1,
            diagnostic_failure(after).0,
            diagnostic_failure(after).1,
            self.before.same_birth(after),
            after == &observation.policy,
            observation.returned,
            observation.errno,
            observation.length,
            observation.value[0],
            after == &observation.policy && self.before.same_birth(after) && linger.is_some()
        ));
        if let Some(queries) = &after.queries {
            for q in queries {
                decline_diagnostic(format_args!(
                    "phase=birth-query call={} attach={} rc={} errno={:?} count={} flags={} revision={}",
                    admission.call.native_command_call(),
                    q.attach,
                    q.returned,
                    q.errno,
                    q.count,
                    q.attach_flags,
                    q.revision
                ));
            }
        }
        if after != &observation.policy || !self.before.same_birth(after) || linger.is_none() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Completed {
            admission: admission.clone(),
            owner,
            root: self.authority.root.clone(),
            before: self.before.clone(),
            after: after.clone(),
            linger: linger.unwrap(),
        })))
    }
}

#[derive(Clone)]
pub(crate) struct Completed {
    root: Arc<ForegroundRoot>,
    admission: Admission,
    owner: NetworkStreamOwner,
    before: Snapshot,
    after: Snapshot,
    linger: (i32, i32),
}
impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CompletedSocketBirthPolicy(private)")
    }
}
impl Completed {
    pub(crate) fn matches_initial_root(&self, root: &ForegroundRoot) -> bool {
        std::ptr::eq(self.root.as_ref(), root.initial_ancestor())
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
            && self.admission == other.admission
            && self.owner == other.owner
            && self.before == other.before
            && self.after == other.after
            && self.linger == other.linger
    }
    pub(crate) fn validates(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> bool {
        self.owner == owner
            && self.admission.call == call
            && self.before.same_birth(&self.after)
            && self.linger.0 == 0
    }
}

fn stat_node(fd: BorrowedFd<'_>, magic: Option<libc::c_long>) -> io::Result<Node> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0
        || unsafe { libc::fstatfs(fd.as_raw_fd(), fs.as_mut_ptr()) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    let fs = unsafe { fs.assume_init() };
    if magic.is_some_and(|m| fs.f_type != m) {
        return Err(io::Error::other("Socket birth filesystem changed"));
    }
    Ok(Node {
        device: stat.st_dev,
        inode: stat.st_ino,
    })
}
#[repr(C)]
#[derive(Default)]
struct PidInfo {
    mask: u64,
    cgroup: u64,
    pid: u32,
    tgid: u32,
    ppid: u32,
    credentials: [u32; 8],
    exit: i32,
}
fn creator(target: BorrowedFd<'_>) -> io::Result<Creator> {
    let pidfd = stat_node(target, Some(PIDFS_MAGIC))?;
    let mut info = PidInfo {
        mask: 1 | 4 | 8,
        ..PidInfo::default()
    };
    // Version-zero, 64-byte PIDFD_GET_INFO. Returned mask is authoritative.
    const PIDFD_GET_INFO: libc::c_ulong = 0xc040_ff0b;
    if unsafe { libc::ioctl(target.as_raw_fd(), PIDFD_GET_INFO, &raw mut info) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if info.mask & 5 != 5 || info.mask & 8 != 0 || info.pid == 0 || info.cgroup == 0 {
        return Err(io::Error::other(
            "Socket birth target is not a live exact cgroup member",
        ));
    }
    Ok(Creator {
        pidfd,
        pid: info.pid,
        tgid: info.tgid,
        cgroup: info.cgroup,
    })
}
fn read_bounded(path: &str, limit: usize) -> io::Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("Socket birth proc record exceeds bound"));
    }
    String::from_utf8(bytes).map_err(io::Error::other)
}
fn membership(text: &str) -> io::Result<String> {
    let body = text
        .strip_suffix('\n')
        .ok_or_else(|| io::Error::other("missing cgroup terminator"))?;
    let path = body
        .strip_prefix("0::")
        .ok_or_else(|| io::Error::other("non-unified cgroup membership"))?;
    if !path.starts_with('/')
        || path.contains(['\n', '\r', '\\'])
        || path.split('/').any(|s| matches!(s, "." | ".."))
    {
        return Err(io::Error::other("ambiguous cgroup membership"));
    }
    Ok(path.into())
}
fn mount(text: &str) -> io::Result<(u64, String)> {
    let mut found = None;
    for line in text.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let fields: Vec<_> = before.split_whitespace().collect();
        if after.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        if fields.len() < 6
            || fields[3] != "/"
            || !fields[4].starts_with('/')
            || fields[4].contains('\\')
            || found.is_some()
        {
            return Err(io::Error::other("ambiguous cgroup2 mount/root"));
        }
        let id = fields[0].parse().map_err(io::Error::other)?;
        found = Some((id, fields[4].to_owned()));
    }
    found.ok_or_else(|| io::Error::other("no full unified cgroup2 mount"))
}
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}
fn open_directory(at: i32, path: &str, beneath: bool) -> io::Result<OwnedFd> {
    let path = CString::new(path).map_err(io::Error::other)?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: 0x02 | 0x04 | if beneath { 0x01 | 0x08 } else { 0 },
    };
    let raw = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            at,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) })
}
fn mount_id(fd: BorrowedFd<'_>) -> io::Result<u64> {
    let text = read_bounded(
        &format!("/proc/thread-self/fdinfo/{}", fd.as_raw_fd()),
        4096,
    )?;
    let mut values = text.lines().filter_map(|s| s.strip_prefix("mnt_id:\t"));
    let value = values
        .next()
        .ok_or_else(|| io::Error::other("directory lacks mount identity"))?;
    if values.next().is_some() {
        return Err(io::Error::other("duplicate mount identity"));
    }
    value.parse().map_err(io::Error::other)
}
#[repr(C)]
#[derive(Default)]
struct QueryAttr {
    target: u32,
    attach: u32,
    flags: u32,
    attach_flags: u32,
    ids: u64,
    count: u32,
    padding: u32,
    per_program_flags: u64,
    link_ids: u64,
    link_flags: u64,
    revision: u64,
}
fn query(fd: BorrowedFd<'_>, attach: u32) -> Query {
    let mut attr = QueryAttr {
        target: fd.as_raw_fd() as u32,
        attach,
        flags: BPF_F_QUERY_EFFECTIVE,
        ..QueryAttr::default()
    };
    let returned = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            BPF_PROG_QUERY,
            &raw mut attr,
            std::mem::size_of::<QueryAttr>(),
        )
    } as i32;
    let errno = (returned < 0).then(|| {
        io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO)
    });
    Query {
        attach,
        flags: attr.flags,
        returned,
        errno,
        count: attr.count,
        attach_flags: attr.attach_flags,
        revision: attr.revision,
    }
}

/// Synchronous and cancellation-free within its existing native worker/service
/// dispatch. Every opened directory/namespace is explicitly closed even when a
/// later read/query fails; failed close is retained and cannot issue authority.
pub(super) fn snapshot(target: BorrowedFd<'_>, effective: bool) -> Snapshot {
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    let mut receipt = Snapshot::new();
    let mut opened: Vec<OwnedFd> = Vec::new();
    let mut diagnostic_stage = 1;
    let observed = (|| -> io::Result<()> {
        let before = creator(target)?;
        diagnostic_stage = 2;
        let local_ns = std::fs::metadata("/proc/thread-self/ns/cgroup")?;
        if local_ns.ino() != CGROUP_NS_INIT_INO {
            return Err(io::Error::other(
                "observer is not in initial cgroup namespace",
            ));
        }
        diagnostic_stage = 3;
        const PIDFD_GET_CGROUP_NAMESPACE: libc::c_ulong = 0xff01;
        let raw = unsafe { libc::ioctl(target.as_raw_fd(), PIDFD_GET_CGROUP_NAMESPACE, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        opened.push(unsafe { OwnedFd::from_raw_fd(raw) });
        receipt.opened += 1;
        diagnostic_stage = 4;
        let ns = stat_node(opened.last().unwrap().as_fd(), None)?;
        if ns.inode != CGROUP_NS_INIT_INO || ns.device != local_ns.dev() {
            return Err(io::Error::other(
                "creator cgroup namespace is not the authenticated initial namespace",
            ));
        }
        receipt.namespace = Some(ns);
        diagnostic_stage = 5;
        let member = membership(&read_bounded(
            &format!("/proc/{}/cgroup", before.pid),
            4096,
        )?)?;
        diagnostic_stage = 6;
        let (expected_mount, mountpoint) =
            mount(&read_bounded("/proc/thread-self/mountinfo", 1024 * 1024)?)?;
        diagnostic_stage = 7;
        opened.push(open_directory(libc::AT_FDCWD, &mountpoint, false)?);
        receipt.opened += 1;
        let root = opened.last().unwrap();
        diagnostic_stage = 8;
        stat_node(root.as_fd(), Some(CGROUP2_MAGIC))?;
        diagnostic_stage = 9;
        if mount_id(root.as_fd())? != expected_mount {
            return Err(io::Error::other("cgroup2 mount changed during resolution"));
        }
        diagnostic_stage = 10;
        let relative = member.strip_prefix('/').unwrap();
        opened.push(open_directory(
            root.as_raw_fd(),
            if relative.is_empty() { "." } else { relative },
            true,
        )?);
        receipt.opened += 1;
        let directory = opened.last().unwrap();
        diagnostic_stage = 11;
        let node = stat_node(directory.as_fd(), Some(CGROUP2_MAGIC))?;
        diagnostic_stage = 12;
        let actual_mount = mount_id(directory.as_fd())?;
        if actual_mount != expected_mount || node.inode != before.cgroup {
            return Err(io::Error::other(
                "resolved cgroup is not the retained task's actual cgroup",
            ));
        }
        receipt.directory = Some(Directory {
            node,
            mount: actual_mount,
        });
        receipt.membership = Some(member);
        receipt.creator = Some(before.clone());
        if effective {
            receipt.queries = Some([
                query(directory.as_fd(), GETSOCKOPT),
                query(directory.as_fd(), SETSOCKOPT),
            ]);
        }
        diagnostic_stage = 14;
        if creator(target)? != before
            || membership(&read_bounded(
                &format!("/proc/{}/cgroup", before.pid),
                4096,
            )?)? != *receipt.membership.as_ref().unwrap()
        {
            return Err(io::Error::other(
                "Socket birth creator moved during observation",
            ));
        }
        Ok(())
    })();
    if let Err(error) = observed {
        receipt.error = Some(format!(
            "finite-close-stage={diagnostic_stage};errno={}; {error}",
            error.raw_os_error().unwrap_or(0)
        ));
    }
    for fd in opened.into_iter().rev() {
        let returned = unsafe { libc::close(fd.into_raw_fd()) };
        receipt.releases.push(Release {
            returned,
            errno: (returned < 0).then(|| {
                io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO)
            }),
        });
    }
    receipt
}

/// Controlled host-policy premise only. The authority and original admission
/// still come from the real Global/engine path, and the production completion
/// predicate issues the private receipt.
#[cfg(test)]
pub(crate) fn controlled_birth_receipt(
    authority: SocketBirthAuthority,
    owner: NetworkStreamOwner,
    admission: &Admission,
) -> io::Result<Arc<Completed>> {
    tests::complete_controlled_birth(authority, owner, admission)
}

#[cfg(test)]
#[path = "socket_birth_policy/tests.rs"]
mod tests;
