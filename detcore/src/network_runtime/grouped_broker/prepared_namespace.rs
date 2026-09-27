//! Actual source mount-namespace custody. Metadata never issues a namespace
//! capability, and only the consuming real SourceTerminal transition exposes
//! a prepared owner to the leaf launcher.
use super::*;

const NSFS_MAGIC: libc::c_long = 0x6e73_6673;
const NS_GET_USERNS: libc::c_ulong = 0xb701;
const NS_GET_NSTYPE: libc::c_ulong = 0xb703;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
pub(in super::super) struct NamespaceIdentity {
    pub device: u64,
    pub inode: u64,
}
impl From<FileIdentity> for NamespaceIdentity {
    fn from(value: FileIdentity) -> Self {
        Self {
            device: value.device,
            inode: value.inode,
        }
    }
}

#[derive(Debug)]
pub(in super::super) struct NamespaceSnapshot {
    pid: libc::pid_t,
    mount: FileIdentity,
    user: FileIdentity,
    initial_user: FileIdentity,
    root: FileIdentity,
}
impl NamespaceSnapshot {
    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }
    pub fn identity(&self) -> NamespaceIdentity {
        self.mount.into()
    }
    pub fn owning_userns(&self) -> NamespaceIdentity {
        self.user.into()
    }
    pub fn root(&self) -> NamespaceIdentity {
        self.root.into()
    }
}

/// The only paths are derived from an actual pinned Creator PID and the
/// original system manager's PID1. This owns the actual bounded query child.
#[derive(Debug)]
pub(in super::super) struct NamespaceQuery {
    pid: libc::pid_t,
    query: CommandQuery,
}
impl NamespaceQuery {
    pub fn retain(pid: libc::pid_t) -> Self {
        Self {
            pid,
            query: CommandQuery::retain(vec![
                "-n".into(),
                "/usr/bin/stat".into(),
                "--dereference".into(),
                "--printf=%d %i %f %u %g\\n".into(),
                "--".into(),
                format!("/proc/{pid}/ns/mnt"),
                format!("/proc/{pid}/ns/user"),
                "/proc/1/ns/user".into(),
                format!("/proc/{pid}/root"),
            ]),
        }
    }
    pub fn start(&mut self) -> io::Result<()> {
        require(
            self.pid > 1,
            "namespace query requires an actual Creator PID",
        )?;
        self.query.start()
    }
    pub fn poll(&mut self, deadline: Instant) -> io::Result<Option<NamespaceSnapshot>> {
        if !self.query.poll(deadline)? {
            return Ok(None);
        }
        let text = std::str::from_utf8(&self.query.stdout).map_err(io::Error::other)?;
        require(
            text.ends_with('\n') && text.lines().count() == 4,
            "actual namespace query framing differs",
        )?;
        let mut rows = Vec::new();
        for line in text.lines() {
            let fields: Vec<_> = line.split(' ').collect();
            require(
                fields.len() == 5 && fields.iter().all(|v| !v.is_empty()),
                "actual namespace query field population differs",
            )?;
            let decimal = |text: &str| -> io::Result<u64> {
                require(
                    text.bytes().all(|b| b.is_ascii_digit()),
                    "namespace identity is not decimal",
                )?;
                let number: u64 = text.parse().map_err(io::Error::other)?;
                require(
                    number.to_string() == text,
                    "namespace identity is not canonical",
                )?;
                Ok(number)
            };
            require(
                fields[2]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "namespace mode is not canonical hexadecimal",
            )?;
            let mode = u32::from_str_radix(fields[2], 16).map_err(io::Error::other)?;
            require(
                format!("{mode:x}") == fields[2],
                "namespace mode framing differs",
            )?;
            rows.push(FileIdentity {
                device: decimal(fields[0])?,
                inode: decimal(fields[1])?,
                mode,
                uid: u32::try_from(decimal(fields[3])?).map_err(io::Error::other)?,
                gid: u32::try_from(decimal(fields[4])?).map_err(io::Error::other)?,
                links: 0,
                size: 0,
            });
        }
        let [mount, user, initial_user, root]: [FileIdentity; 4] = rows
            .try_into()
            .map_err(|_| io::Error::other("namespace query lost actual rows"))?;
        require(
            [mount, user, initial_user].iter().all(|v| {
                v.inode != 0 && v.mode & libc::S_IFMT == libc::S_IFREG && v.uid == 0 && v.gid == 0
            }) && user.same_owner(&initial_user)
                && root.inode != 0
                && root.mode & libc::S_IFMT == libc::S_IFDIR,
            "actual source namespace is not bound to the initial user namespace/root",
        )?;
        self.query.completed_custody(deadline)?;
        Ok(Some(NamespaceSnapshot {
            pid: self.pid,
            mount,
            user,
            initial_user,
            root,
        }))
    }
    pub fn completed_custody(&self, deadline: Instant) -> io::Result<CompletedQuery<'_>> {
        self.query.completed_custody(deadline)
    }
    pub(super) fn retire_successful_resources(&mut self, deadline: Instant) -> io::Result<()> {
        self.query.retire_successful_resources(deadline)
    }
    pub(super) fn successful_resources_retired(&self) -> io::Result<bool> {
        self.query.successful_resources_retired()
    }
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
    pub fn held_descriptors(&self) -> Vec<RawFd> {
        let mut fds = Vec::new();
        fds.extend(self.query.pidfd.iter().map(AsRawFd::as_raw_fd));
        if let Some(child) = &self.query.child {
            fds.extend(child.stdout.iter().map(AsRawFd::as_raw_fd));
            fds.extend(child.stderr.iter().map(AsRawFd::as_raw_fd));
        }
        fds
    }
}

/// Installed before validation. Refusal retains every namespace/query owner.
#[derive(Debug)]
pub(in super::super) struct NamespaceCustody {
    fd: OwnedFd,
    userns: Option<OwnedFd>,
    creator_pidfd: Option<OwnedFd>,
    query: NamespaceQuery,
    snapshot: Option<NamespaceSnapshot>,
    started: bool,
    census: CensusInventory,
    completion_census: bool,
    refusal: Option<Failure>,
}
impl NamespaceCustody {
    pub fn retain(fd: OwnedFd, pid: libc::pid_t) -> Self {
        Self {
            fd,
            userns: None,
            creator_pidfd: None,
            query: NamespaceQuery::retain(pid),
            snapshot: None,
            started: false,
            census: CensusInventory::retain(),
            completion_census: false,
            refusal: None,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result {
            self.refusal.get_or_insert_with(|| Failure::capture(error));
        }
        result
    }
    fn check(&self) -> io::Result<()> {
        if let Some(error) = &self.refusal {
            return Err(error.error());
        }
        require(
            filesystem(self.fd.as_raw_fd())? == NSFS_MAGIC
                && unsafe { libc::ioctl(self.fd.as_raw_fd(), NS_GET_NSTYPE) } == libc::CLONE_NEWNS,
            "prepared descriptor is not an actual mount namespace",
        )?;
        let flags = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        require(
            flags >= 0
                && flags & libc::O_ACCMODE == libc::O_RDONLY
                && unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFD) } == libc::FD_CLOEXEC,
            "prepared namespace flags differ",
        )
    }
    pub fn progress(&mut self, creator: &Creator, deadline: Instant) -> io::Result<bool> {
        let result = (|| {
            self.check()?;
            require(
                creator.peer.pid == self.query.pid && !terminal(creator.pidfd.as_raw_fd())?,
                "prepared namespace original Creator is not live",
            )?;
            pidfd_matches(creator.pidfd.as_raw_fd(), creator.peer.pid)?;
            if !self.started {
                self.started = true;
                let raw =
                    unsafe { libc::fcntl(creator.pidfd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
                if raw < 0 {
                    return Err(io::Error::last_os_error());
                }
                self.creator_pidfd = Some(unsafe { OwnedFd::from_raw_fd(raw) });
                let raw = unsafe { libc::ioctl(self.fd.as_raw_fd(), NS_GET_USERNS) };
                if raw < 0 {
                    return Err(io::Error::last_os_error());
                }
                self.userns = Some(unsafe { OwnedFd::from_raw_fd(raw) });
                require(
                    filesystem(raw)? == NSFS_MAGIC
                        && unsafe { libc::ioctl(raw, NS_GET_NSTYPE) } == libc::CLONE_NEWUSER,
                    "prepared namespace has no actual owning user namespace",
                )?;
                self.query.start()?;
                let descriptors = self.held_descriptors();
                self.census.observe(&descriptors, deadline)?;
            }
            if self.snapshot.is_none() {
                self.snapshot = self.query.poll(deadline)?;
            }
            let Some(snapshot) = &self.snapshot else {
                return Ok(false);
            };
            require(
                creator.admitted && snapshot.pid == creator.peer.pid,
                "prepared namespace lacks the queried admitted original Creator",
            )?;
            creator.process_policy()?;
            require(
                stat(self.fd.as_raw_fd())?.same_owner(&snapshot.mount)
                    && stat(self.userns.as_ref().unwrap().as_raw_fd())?.same_owner(&snapshot.user)
                    && snapshot.user.same_owner(&snapshot.initial_user),
                "held namespace differs from independent original Creator query",
            )?;
            pidfd_matches(
                self.creator_pidfd.as_ref().unwrap().as_raw_fd(),
                creator.peer.pid,
            )?;
            require(
                !terminal(self.creator_pidfd.as_ref().unwrap().as_raw_fd())?,
                "source terminated during namespace authentication",
            )?;
            self.query.completed_custody(deadline)?;
            if !self.completion_census {
                let descriptors = self.held_descriptors();
                self.census.observe(&descriptors, deadline)?;
                self.completion_census = true;
            }
            Ok(true)
        })();
        self.remember(result)
    }
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
    pub fn completed(&self, deadline: Instant) -> io::Result<()> {
        self.check()?;
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| io::Error::other("namespace query incomplete"))?;
        require(
            stat(self.fd.as_raw_fd())?.same_owner(&snapshot.mount)
                && stat(self.userns.as_ref().unwrap().as_raw_fd())?.same_owner(&snapshot.user),
            "retained namespace identity changed",
        )?;
        self.query.completed_custody(deadline).map(|_| ())
    }
    pub fn held_descriptors(&self) -> Vec<RawFd> {
        let mut fds = self.query.held_descriptors();
        fds.push(self.fd.as_raw_fd());
        fds.extend(self.userns.iter().map(AsRawFd::as_raw_fd));
        fds.extend(self.creator_pidfd.iter().map(AsRawFd::as_raw_fd));
        fds.extend(self.census.directory.iter().map(AsRawFd::as_raw_fd));
        fds
    }
    pub fn evidence(&self) -> serde_json::Value {
        self.query.evidence()
    }
    pub fn completed_record(&self, deadline: Instant) -> io::Result<serde_json::Value> {
        self.completed(deadline)?;
        self.query.completed_custody(deadline)?.record()
    }
    pub fn retire_custody(
        &mut self,
        deadline: Instant,
        cause: &io::Error,
    ) -> io::Result<QueryRetirement> {
        if self.completion_census {
            self.query.completed_custody(deadline)?;
            return Ok(QueryRetirement::Retired);
        }
        self.query.retire_custody(deadline, cause)
    }
}

#[derive(Debug)]
pub(in super::super) struct PreparedLeafNamespace {
    original: NamespaceCustody,
}
impl PreparedLeafNamespace {
    /// SourceTerminal is private, nonserialized, and issued only after the
    /// original complete native source and all child/query joins.
    pub fn from_terminal(source: &mut super::super::serial::SourceTerminal) -> io::Result<Self> {
        Ok(Self {
            original: source.take_namespace_owner()?,
        })
    }
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.original.fd()
    }
    pub fn identity(&self) -> NamespaceIdentity {
        self.original.snapshot.as_ref().unwrap().identity()
    }
    pub fn owning_userns(&self) -> NamespaceIdentity {
        self.original.snapshot.as_ref().unwrap().owning_userns()
    }
    pub fn root(&self) -> NamespaceIdentity {
        self.original.snapshot.as_ref().unwrap().root()
    }
    pub fn check_match(&self, actual: &NamespaceSnapshot) -> io::Result<()> {
        self.original.check()?;
        let original = self.original.snapshot.as_ref().unwrap();
        require(
            stat(self.original.fd.as_raw_fd())?.same_owner(&original.mount)
                && stat(self.original.userns.as_ref().unwrap().as_raw_fd())?
                    .same_owner(&original.user),
            "prepared namespace retained description changed",
        )?;
        require(
            self.identity() == actual.identity()
                && self.owning_userns() == actual.owning_userns()
                && self.root() == actual.root()
                && actual.user.same_owner(&actual.initial_user),
            "actual leaf namespace/user namespace/root differs from retained source",
        )
    }
    pub fn held_descriptors(&self) -> Vec<RawFd> {
        self.original.held_descriptors()
    }
    pub fn evidence(&self) -> serde_json::Value {
        self.original.evidence()
    }
}
