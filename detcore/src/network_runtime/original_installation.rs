//! Original Socket/Accept installations enter the existing semantic publisher.
//! A journal interval proves a historical effect. It does not, on its own,
//! authorize replacing the current numeric slot.

use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::Mutex;

use super::accepted::Resolved;
use super::fd_journal::History;
use super::fd_journal::Transition;
use crate::network_replay::NetworkAcceptLeaseId;
use crate::network_replay::NetworkFdPublicationPermit;
use crate::network_replay::NetworkStreamCallId;
use crate::network_replay::NetworkStreamOwner;
use crate::tool_local::FileMetadata;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Socket(NetworkStreamCallId),
    Openat(NetworkStreamCallId),
    EpollCreate(NetworkStreamCallId),
    Accepted {
        lease: NetworkAcceptLeaseId,
        child: Resolved,
    },
}

/// Annotation returned only after the existing exact metadata transaction.
/// These RPC fields carry no provider/installation authority of their own.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct OpenatPublication {
    pub binding: Option<crate::types::FdSlotBinding>,
    pub live: bool,
    pub stat: Option<crate::stat::DetStat>,
    pub resolved_path: Option<std::path::PathBuf>,
}

/// Complete observations supplied to the same synchronous metadata publisher.
#[derive(Clone)]
pub(crate) struct TerminalProfile {
    pub opened: Option<crate::network_replay::original_installation::OpenatEnrollment>,
    pub stat: Option<crate::stat::DetStat>,
    pub fresh: Option<crate::network_replay::original_installation::FreshStreamEnrollment>,
}
pub(crate) type TerminalPublisher = dyn Fn(
        NetworkStreamOwner,
        &crate::network_replay::original_connect::Admission,
        &Installation,
        TerminalProfile,
    ) -> Result<crate::types::FdSlotBinding, crate::network_replay::NetworkReplayError>
    + Send
    + Sync;

/// A complete actual negative sys_exit plus a fresh, fully collected journal
/// cut with no installation under this command. Neither ESRCH nor final wait
/// can construct this certificate.
#[derive(Debug, Clone)]
pub(crate) struct NoInstallation {
    owner: Owner,
    admission: crate::network_replay::original_connect::Admission,
    returned: i64,
    command: u64,
    through: u64,
}
impl NoInstallation {
    pub(crate) fn returned(&self) -> i64 {
        self.returned
    }
    pub(crate) fn command(&self) -> u64 {
        self.command
    }
    pub(crate) fn validate(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<()> {
        if self.owner.owner != owner
            || self.owner.files != admission.arguments.files
            || self.admission != *admission
            || self.returned >= 0
            || self.command == 0
        {
            return Err(io::Error::other(
                "negative allocator receipt changed original owner/result",
            ));
        }
        // Retaining the complete cut is part of custody even when it is empty.
        let _ = self.through;
        Ok(())
    }
}

/// The existing command/task/table owner captured before original injection.
/// Its metadata object is retained; a later numeric descriptor lookup cannot
/// substitute either the object or the descriptor's physical generation.
#[derive(Debug, Clone)]
pub(super) struct Owner {
    pub owner: NetworkStreamOwner,
    pub metadata: Arc<Mutex<FileMetadata>>,
    pub files: crate::types::FilesId,
    pub provider: u64,
    pub task: u64,
    pub start: u64,
    pub table: u64,
}

impl Owner {
    pub(super) fn same(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.files == other.files
            && Arc::ptr_eq(&self.metadata, &other.metadata)
            && (self.provider, self.task, self.start, self.table)
                == (other.provider, other.task, other.start, other.table)
    }
}

/// Socket uses the existing fixed-size original-result bytes for its actual
/// fd_install interval. Connect's sockaddr interpretation remains unchanged.
pub(super) fn socket_result(
    r: &super::accepted_provider::OriginalResult,
    raw: i64,
) -> io::Result<Option<(u64, u64, i32)>> {
    allocator_result(
        r,
        raw,
        crate::network_replay::original_connect::Kind::Socket,
        0,
    )
}

pub(crate) fn allocator_result(
    r: &super::accepted_provider::OriginalResult,
    raw: i64,
    kind: crate::network_replay::original_connect::Kind,
    original_count: u64,
) -> io::Result<Option<(u64, u64, i32)>> {
    let opened = kind == crate::network_replay::original_connect::Kind::Openat;
    let epoll = matches!(
        kind,
        crate::network_replay::original_connect::Kind::EpollCreate { .. }
    );
    let used = if opened || epoll { 40 } else { 24 };
    if !kind.allocator()
        || !(-4095..=i64::from(i32::MAX)).contains(&raw)
        || i64::from(r.returned) != raw
        || r.complete != 1
        || r.problem != 0
        || r.reserved != 0
        || r.selection.fdput_flags != 0
        || r.selection.original_count != original_count
        || r.copy_entered != 0
        || r.copy_returned != 0
        || r.copy_remaining != 0
        || r.audit_entered != 0
        || r.audit_returned != 0
        || r.audit_result != 0
        || r.security_entered != 0
        || r.security_returned != 0
        || r.security_result != 0
        || r.address.len() != 128
        || r.address[20..24].iter().any(|b| *b != 0)
        || r.address[used..].iter().any(|b| *b != 0)
    {
        return Err(io::Error::other(
            "Socket result changed its original path/ABI",
        ));
    }
    let begin = u64::from_ne_bytes(r.address[0..8].try_into().unwrap());
    let end = u64::from_ne_bytes(r.address[8..16].try_into().unwrap());
    let fd = i32::from_ne_bytes(r.address[16..20].try_into().unwrap());
    if raw < 0 {
        if r.selection.file != 0 || r.address.iter().any(|b| *b != 0) {
            return Err(io::Error::other(
                "negative Socket result followed an installation",
            ));
        }
        Ok(None)
    } else if r.selection.file != 0 && begin != 0 && end > begin && i64::from(fd) == raw {
        if opened {
            let mode = u32::from_ne_bytes(r.address[24..28].try_into().unwrap());
            if mode & libc::S_IFMT == 0 || mode > 0o177777 {
                return Err(io::Error::other(
                    "Openat lacks its original borrowed-file class",
                ));
            }
        }
        if epoll {
            if r.selection.user_address != kind.syscall() as u64 {
                return Err(io::Error::other(
                    "epoll receipt changed legacy/create1 original syscall",
                ));
            }
            epoll_profile(r)?;
        }
        Ok(Some((begin, end, fd)))
    } else {
        Err(io::Error::other(
            "positive Socket result lacks its exact install interval",
        ))
    }
}

/// Class at fd_install entry, while the allocator still owns its file reference.
/// This can describe a subsequently removed historical installation. It cannot
/// replace filesystem getattr or authenticate a later numeric descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocatedProfile {
    pub mode: u32,
    pub status_flags: i32,
    pub device_major: u32,
    pub device_minor: u32,
}
impl AllocatedProfile {
    pub(crate) fn kind(self) -> io::Result<crate::fd::FdType> {
        crate::fd::FdType::from_initial_profile(
            self.mode,
            self.status_flags as u32,
            self.device_major,
            self.device_minor,
        )
        .ok_or_else(|| io::Error::other("original Openat has no supported actual file class"))
    }
}
fn allocated_profile(
    result: &super::accepted_provider::OriginalResult,
) -> io::Result<AllocatedProfile> {
    if result.address.len() != 128 || result.returned < 0 || result.selection.file == 0 {
        return Err(io::Error::other(
            "Openat profile lacks a positive original installation",
        ));
    }
    let word = |at| u32::from_ne_bytes(result.address[at..at + 4].try_into().unwrap());
    let profile = AllocatedProfile {
        mode: word(24),
        status_flags: word(28) as i32,
        device_major: word(32),
        device_minor: word(36),
    };
    profile.kind()?;
    Ok(profile)
}

/// Actual epoll file/descriptor flags at the original installation. Creation
/// identity comes from the selected syscall, not the anonymous inode's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EpollProfile {
    pub status_flags: i32,
    pub descriptor_flags: i32,
}
fn epoll_profile(result: &super::accepted_provider::OriginalResult) -> io::Result<EpollProfile> {
    let s = &result.selection;
    if result.address.len() != 128
        || result.returned < 0
        || s.file == 0
        || !matches!(s.user_address, 213 | 291)
        || s.address_length != 0
        || s.original_count != 0
        || (s.user_address == 213 && s.requested_fd <= 0)
        || (s.user_address == 291 && s.requested_fd & !libc::EPOLL_CLOEXEC != 0)
        || u32::from_ne_bytes(result.address[32..36].try_into().unwrap()) != 1
        || result.address[36..].iter().any(|byte| *byte != 0)
    {
        return Err(io::Error::other(
            "epoll profile lacks its exact successful original creator",
        ));
    }
    let profile = EpollProfile {
        status_flags: i32::from_ne_bytes(result.address[24..28].try_into().unwrap()),
        descriptor_flags: i32::from_ne_bytes(result.address[28..32].try_into().unwrap()),
    };
    if profile.status_flags & libc::O_CLOEXEC != 0
        || profile.descriptor_flags & !libc::FD_CLOEXEC != 0
    {
        return Err(io::Error::other(
            "epoll profile confuses OFD and descriptor flags",
        ));
    }
    Ok(profile)
}

/// Physical OFD identity retained only on the existing semantic description.
/// This has no deserializer and is issued only by a checked provider relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    provider: u64,
    file: u64,
}

impl FileIdentity {
    pub(super) fn provider_file(self) -> (u64, u64) {
        (self.provider, self.file)
    }
    /// Explicit component authority; excluded from every production build.
    #[cfg(test)]
    pub(crate) fn controlled_fixture(provider: u64, file: u64) -> Self {
        assert!(provider != 0 && file != 0);
        Self { provider, file }
    }
    pub(crate) fn matches(self, provider: u64, file: u64) -> bool {
        self.provider == provider && self.file == file
    }
    // Only the retained checked census may issue initial file authority. The
    // serialized view/claim is compared with that private association first.
    pub(super) fn from_initial_census(
        association: &super::physical::InitialTableAssociation,
        claim: &super::physical::InitialTableClaim,
    ) -> io::Result<Vec<(crate::types::FdSlotBinding, Self)>> {
        association.check_claim(claim)?;
        let provider = association.native_provider();
        if provider == 0 {
            return Err(io::Error::other("initial census provider missing"));
        }
        Ok(claim
            .slots
            .iter()
            .zip(&claim.view.descriptors)
            .map(|(slot, native)| {
                (
                    slot.binding,
                    Self {
                        provider,
                        file: native.physical_file,
                    },
                )
            })
            .collect())
    }
}

/// A complete checked post-installation removal and optional final file event.
/// The raw rows remain here until the same semantic publication is acknowledged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Removed {
    begin: super::accepted_provider::FdEvent,
    end: super::accepted_provider::FdEvent,
    retired: Option<super::accepted_provider::FdEvent>,
}

/// Private checked receipt. No RPC deserializer or public constructor can
/// manufacture this from a returned FD or an adapter's OpenFileId.
#[derive(Debug, Clone)]
pub(crate) struct Installation {
    owner: Owner,
    permit: Option<NetworkFdPublicationPermit>,
    source: Source,
    command: u64,
    fd: i32,
    file: u64,
    begin: u64,
    end: u64,
    through: u64,
    interference: Vec<u64>,
    removed: Option<Removed>,
    // Exact relevant physical prefix; no lifecycle row is dropped on recovery.
    reconciled: Option<Vec<super::accepted_provider::FdEvent>>,
    allocated: Option<AllocatedProfile>,
    epoll: Option<EpollProfile>,
}

impl Installation {
    /// `through` is a fresh provider counter cut requested while this exact
    /// publication permit is held, then completely collected by History.
    /// It is not the cached status of an earlier installation read.
    pub(super) fn checked(
        owner: Owner,
        permit: NetworkFdPublicationPermit,
        source: Source,
        command: u64,
        fd: i32,
        file: u64,
        begin: u64,
        end: u64,
        through: u64,
        history: &History,
    ) -> io::Result<Self> {
        if permit.owner != owner.owner || permit.files != owner.files {
            return Err(io::Error::other(
                "original installation changed initial publication owner",
            ));
        }
        Self::checked_effect(
            owner,
            Some(permit),
            source,
            command,
            fd,
            file,
            begin,
            end,
            through,
            history,
        )
    }

    /// A terminal cut proves physical history only. It neither invents a table
    /// permit nor authorizes publication into a surviving table.
    pub(super) fn checked_terminal(
        owner: Owner,
        source: Source,
        command: u64,
        fd: i32,
        file: u64,
        begin: u64,
        end: u64,
        through: u64,
        history: &History,
    ) -> io::Result<Self> {
        Self::checked_effect(
            owner, None, source, command, fd, file, begin, end, through, history,
        )
        .and_then(|receipt| receipt.reconcile(history))
    }

    fn checked_effect(
        owner: Owner,
        permit: Option<NetworkFdPublicationPermit>,
        source: Source,
        command: u64,
        fd: i32,
        file: u64,
        begin: u64,
        end: u64,
        through: u64,
        history: &History,
    ) -> io::Result<Self> {
        if command == 0
            || fd < 0
            || file == 0
            || begin == 0
            || end <= begin
            || through < end
            || permit.is_some_and(|p| p.files != owner.files)
            || owner.provider == 0
            || owner.task == 0
            || owner.start == 0
            || owner.table == 0
            || history.next()? <= through
        {
            return Err(io::Error::other(
                "original installation lacks exact owner/prefix custody",
            ));
        }
        let Some(Transition::Install {
            begin: first,
            end: last,
        }) = history.transition(end)?
        else {
            return Err(io::Error::other(
                "original installation lacks paired install endpoints",
            ));
        };
        if first.sequence != begin
            || last.sequence != end
            || first.task != owner.task
            || first.task_start != owner.start
            || first.table != owner.table
            || first.file != file
            || first.fd != fd
            || first.accept_command != command
        {
            return Err(io::Error::other(
                "original installation changed its retained command/actor/file",
            ));
        }
        let interference = history.installation_interference(begin, end, through, owner.table, fd);
        Ok(Self {
            owner,
            permit,
            source,
            command,
            fd,
            file,
            begin,
            end,
            through,
            interference,
            removed: None,
            reconciled: None,
            allocated: None,
            epoll: None,
        })
    }

    /// Reconcile only complete physical transitions. A nonfinal table put
    /// preserves the slot; an exact close, exec removal, or final table drain
    /// removes it. Open intervals, aliases, copies, replacements, and reuse
    /// retain their raw evidence and refuse publication.
    fn reconcile(mut self, history: &History) -> io::Result<Self> {
        use std::collections::BTreeSet;
        let rows = history.installation_effect_rows(
            self.begin,
            self.end,
            self.through,
            self.owner.table,
            self.fd,
            self.file,
        )?;
        let mut covered = BTreeSet::new();
        let mut removal = None;
        let mut removed_at = None;
        let mut file_retired = None;
        for row in &rows {
            let Some(transition) = history.transition(row.sequence)? else {
                continue;
            };
            match transition {
                Transition::Remove {
                    begin: Some(begin),
                    end,
                } if begin.kind == 10
                    && end.kind == 3
                    && begin.sequence > self.end
                    && begin.table == self.owner.table
                    && begin.fd == self.fd
                    && begin.file == 0
                    && end.table == self.owner.table
                    && end.fd == self.fd
                    && end.file == self.file
                    && begin.accept_command == 0
                    && end.accept_command == 0 =>
                {
                    if removed_at.replace(end.sequence).is_some() {
                        return Err(io::Error::other("installation was removed more than once"));
                    }
                    covered.extend([begin.sequence, end.sequence]);
                    removal = Some((begin, end));
                }
                Transition::Put {
                    begin,
                    retired,
                    end,
                } if begin.sequence > self.end
                    && begin.table == self.owner.table
                    && end.table == self.owner.table =>
                {
                    covered.extend([begin.sequence, end.sequence]);
                    if let Some(retired) = retired {
                        covered.insert(retired.sequence);
                        // A final put drains every remaining slot of this exact
                        // table. A prior exact close remains the earlier effect.
                        if removed_at.is_none() {
                            removed_at = Some(begin.sequence);
                            removal = Some((begin, end));
                        }
                    }
                }
                Transition::Exec {
                    begin,
                    removed,
                    end,
                } if begin.sequence > self.end
                    && begin.table == self.owner.table
                    && end.table == self.owner.table =>
                {
                    covered.extend([begin.sequence, end.sequence]);
                    for effect in removed
                        .into_iter()
                        .filter(|e| e.fd == self.fd || e.file == self.file)
                    {
                        if effect.fd != self.fd
                            || effect.file != self.file
                            || removed_at.replace(effect.sequence).is_some()
                        {
                            return Err(io::Error::other(
                                "exec changed or duplicated the installed file",
                            ));
                        }
                        covered.insert(effect.sequence);
                        removal = Some((begin.clone(), end.clone()));
                    }
                }
                Transition::FileRetired(retired) if retired.file == self.file => {
                    if file_retired.replace(retired.clone()).is_some() {
                        return Err(io::Error::other("installation file retired more than once"));
                    }
                    covered.insert(retired.sequence);
                }
                _ => {
                    return Err(io::Error::other(
                        "installation effects require further ordered reconciliation",
                    ));
                }
            }
        }
        if file_retired
            .as_ref()
            .is_some_and(|r| removed_at.is_none_or(|at| r.sequence <= at))
        {
            return Err(io::Error::other(
                "file retirement lacks preceding exact removal",
            ));
        }
        let expected: BTreeSet<_> = rows.iter().map(|row| row.sequence).collect();
        if covered != expected {
            return Err(io::Error::other(
                "installation reconciliation has an open or omitted transition",
            ));
        }
        self.removed = removal.map(|(begin, end)| Removed {
            begin,
            end,
            retired: file_retired,
        });
        self.reconciled = Some(rows);
        Ok(self)
    }

    pub(crate) fn original_owner(&self) -> NetworkStreamOwner {
        self.owner.owner
    }
    pub(crate) fn files(&self) -> crate::types::FilesId {
        self.owner.files
    }

    /// Bind already-checked physical facts to an actual newly acquired permit.
    /// The engine separately authenticates the successor's same-table/Arc
    /// authority before calling the common installation publisher.
    pub(crate) fn for_terminal_publication(
        &self,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<Self> {
        if self.permit.is_some() || permit.files != self.owner.files || self.reconciled.is_none() {
            return Err(io::Error::other(
                "terminal installation changed its physical cut or table",
            ));
        }
        let mut bound = self.clone();
        bound.permit = Some(permit);
        Ok(bound)
    }

    pub(crate) fn validate_terminal(
        &self,
        owner: NetworkStreamOwner,
        actual: &Arc<Mutex<FileMetadata>>,
        local: &FileMetadata,
    ) -> io::Result<()> {
        if owner != self.owner.owner
            || self.permit.is_some()
            || self.reconciled.is_none()
            || !Arc::ptr_eq(actual, &self.owner.metadata)
            || local.files_id != self.owner.files
        {
            return Err(io::Error::other(
                "terminal installation changed original Call/metadata custody",
            ));
        }
        Ok(())
    }

    pub(crate) fn file_identity(&self) -> FileIdentity {
        FileIdentity {
            provider: self.owner.provider,
            file: self.file,
        }
    }
    pub(crate) fn matches_observed_file(&self, provider: u64, file: u64) -> bool {
        self.owner.provider == provider && self.file == file
    }
    pub(crate) fn removed_before_publication(&self) -> bool {
        self.removed.is_some()
    }
    pub(crate) fn allocated_profile(&self) -> io::Result<AllocatedProfile> {
        self.allocated
            .ok_or_else(|| io::Error::other("installation lacks its original Openat profile"))
    }

    /// A resumed publication may observe a later complete cut, but never a
    /// different original effect or a cut older than its retained proof.
    /// A later terminal cut may add only fully checked lifecycle effects to
    /// the same original allocation. The prior publication bytes stay intact.
    pub(crate) fn extends_original(&self, prior: &Self) -> bool {
        self.owner.same(&prior.owner)
            && self.source == prior.source
            && self.command == prior.command
            && self.fd == prior.fd
            && self.file == prior.file
            && self.begin == prior.begin
            && self.end == prior.end
            && self.through >= prior.through
            && self.allocated == prior.allocated
            && self.epoll == prior.epoll
            && self.reconciled.as_ref().is_some_and(|rows| {
                prior
                    .reconciled
                    .as_ref()
                    .is_none_or(|old| old.iter().all(|row| rows.contains(row)))
            })
    }

    pub(crate) fn resumes(&self, prior: &Self) -> bool {
        self.owner.same(&prior.owner)
            && self.permit == prior.permit
            && self.source == prior.source
            && self.command == prior.command
            && self.fd == prior.fd
            && self.file == prior.file
            && self.begin == prior.begin
            && self.end == prior.end
            && self.through >= prior.through
            && self.interference == prior.interference
            && self.removed == prior.removed
            && self.allocated == prior.allocated
            && self.epoll == prior.epoll
            && self.reconciled == prior.reconciled
            && (self.interference.is_empty() || self.reconciled.is_some())
    }

    pub(crate) fn epoll_profile(&self) -> io::Result<EpollProfile> {
        if !matches!(self.source, Source::EpollCreate(_)) {
            return Err(io::Error::other(
                "epoll profile changed original allocator family",
            ));
        }
        self.epoll
            .ok_or_else(|| io::Error::other("epoll installation lost its actual original flags"))
    }
    pub(crate) fn metadata(&self) -> Arc<Mutex<FileMetadata>> {
        self.owner.metadata.clone()
    }
    pub(crate) fn source(&self) -> Source {
        self.source
    }
    pub(crate) fn fd(&self) -> i32 {
        self.fd
    }
    pub(crate) fn matches_original(
        &self,
        command: u64,
        selected: (u64, u64, u64, u64, u64),
    ) -> bool {
        self.command == command
            && selected
                == (
                    self.owner.provider,
                    self.owner.task,
                    self.owner.start,
                    self.owner.table,
                    self.file,
                )
    }
    pub(crate) fn validate_publication(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
        actual: &Arc<Mutex<FileMetadata>>,
        local: &FileMetadata,
    ) -> io::Result<()> {
        if owner != permit.owner
            || Some(permit) != self.permit
            || local.files_id != self.owner.files
            || !Arc::ptr_eq(actual, &self.owner.metadata)
            || self.command == 0
            || self.file == 0
            || self.begin == 0
            || self.end <= self.begin
            || self.through < self.end
        {
            return Err(io::Error::other(
                "original installation changed publication custody",
            ));
        }
        if !self.interference.is_empty() && self.reconciled.is_none() {
            // Preserve the original receipt and complete interference list.
            // Applying install/remove/reuse in order is a remaining activation
            // obligation; refusing here must never overwrite a later alias.
            return Err(io::Error::other(
                "original installation has unreconciled journal interference",
            ));
        }
        Ok(())
    }
}

impl super::NetworkRuntimeResources {
    pub(crate) fn original_openat_published(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<Option<OpenatPublication>> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        if state.admission != *admission {
            return Err(io::Error::other("Openat publication changed Call"));
        }
        Ok(state.openat_published.clone())
    }
    pub(crate) fn retain_original_openat_published(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
        published: OpenatPublication,
    ) -> io::Result<()> {
        self.retain_original_socket_installed(owner, admission, published.binding)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        if admission.arguments.kind != crate::network_replay::original_connect::Kind::Openat
            || state
                .openat_published
                .as_ref()
                .is_some_and(|prior| prior != &published)
        {
            return Err(io::Error::other(
                "Openat publication changed exact retained annotations",
            ));
        }
        state.openat_published = Some(published);
        Ok(())
    }

    pub(crate) fn original_socket_installed(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<Option<Option<crate::types::FdSlotBinding>>> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        if state.admission != *admission {
            return Err(io::Error::other("Socket publication changed Call"));
        }
        Ok(state.installed)
    }

    pub(crate) fn retain_original_socket_installed(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
        binding: Option<crate::types::FdSlotBinding>,
    ) -> io::Result<()> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        let result = state
            .completion
            .as_ref()
            .ok_or_else(|| io::Error::other("Socket lost completion"))?;
        if state.admission != *admission
            || !state.retired
            || !state.close_queued
            || binding.map(|b| i64::from(b.slot.fd))
                != (result.original.returned >= 0).then_some(i64::from(result.original.returned))
            || state.installed.is_some_and(|prior| prior != binding)
        {
            return Err(io::Error::other(
                "Socket publication changed its installed generation",
            ));
        }
        state.installed = Some(binding);
        Ok(())
    }

    /// Retained same-Call observation, never another numeric-FD lookup.
    pub(crate) fn original_socket_observation(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<Option<super::installation_observation::Checked>> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        if state.admission != *admission
            || !state.retired
            || !state.close_queued
            || admission.arguments.kind != crate::network_replay::original_connect::Kind::Socket
        {
            return Err(io::Error::other(
                "Socket observation changed completed original custody",
            ));
        }
        let effect = state
            .completion
            .as_ref()
            .ok_or_else(|| io::Error::other("Socket lost native completion"))?;
        match &effect.socket {
            Some(observation) => observation.checked(effect),
            None => Ok(None),
        }
    }

    pub(crate) async fn original_allocator_installation(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<Option<Installation>> {
        let state = self
            .shared
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .clone();
        if state.admission != *admission
            || !state.retired
            || !state.close_queued
            || !admission.arguments.kind.allocator()
        {
            return Err(io::Error::other(
                "allocator installation lacks completed original custody",
            ));
        }
        let bound = state
            .installation_owner
            .ok_or_else(|| io::Error::other("Socket lacks Prepared metadata"))?;
        let effect = state
            .completion
            .ok_or_else(|| io::Error::other("Socket lacks original result"))?;
        let result = allocator_result(
            &effect.original,
            i64::from(effect.original.returned),
            admission.arguments.kind,
            admission.arguments.original_count,
        )?;
        let command = state
            .prepared
            .ok_or_else(|| io::Error::other("Socket lacks provider command"))?
            .1;
        if effect.original.selection.command != command {
            return Err(io::Error::other("Socket installation changed command"));
        }
        let through = self.installation_cut(owner, permit).await?;
        let journal = self.shared.fd_journal.lock().await;
        match result {
            Some((begin, end, fd)) => Installation::checked(
                bound,
                permit,
                match admission.arguments.kind {
                    crate::network_replay::original_connect::Kind::Socket => {
                        Source::Socket(admission.call)
                    }
                    crate::network_replay::original_connect::Kind::Openat => {
                        Source::Openat(admission.call)
                    }
                    crate::network_replay::original_connect::Kind::EpollCreate { .. } => {
                        Source::EpollCreate(admission.call)
                    }
                    _ => unreachable!("allocator checked above"),
                },
                command,
                fd,
                effect.original.selection.file,
                begin,
                end,
                through,
                journal.history(),
            )
            .and_then(|mut receipt| {
                if admission.arguments.kind == crate::network_replay::original_connect::Kind::Openat
                {
                    receipt.allocated = Some(allocated_profile(&effect.original)?);
                } else if matches!(
                    admission.arguments.kind,
                    crate::network_replay::original_connect::Kind::EpollCreate { .. }
                ) {
                    receipt.epoll = Some(epoll_profile(&effect.original)?);
                }
                receipt.reconcile(journal.history())
            })
            .map(Some),
            None => {
                // The positive original sys_exit/READY claim is mandatory above.
                // Absence alone never certifies a failed or cancelled invocation.
                if journal.history().contains_command(command)? {
                    return Err(io::Error::other(
                        "negative Socket result contradicts retained journal",
                    ));
                }
                Ok(None)
            }
        }
    }

    pub(crate) async fn original_socket_installation(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<Option<Installation>> {
        if admission.arguments.kind != crate::network_replay::original_connect::Kind::Socket {
            return Err(io::Error::other(
                "Socket publication wrapper changed allocator kind",
            ));
        }
        self.original_allocator_installation(owner, admission, permit)
            .await
    }

    pub(crate) fn original_allocator_metadata(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<(crate::types::FilesId, Arc<Mutex<FileMetadata>>)> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let state = calls.original(owner, admission.call)?;
        if state.admission != *admission || !admission.arguments.kind.allocator() {
            return Err(io::Error::other("allocator metadata changed Call"));
        }
        let bound = state
            .installation_owner
            .as_ref()
            .ok_or_else(|| io::Error::other("allocator lacks original Prepared metadata"))?;
        Ok((bound.files, bound.metadata.clone()))
    }

    pub(crate) fn bind_accepted_installation_metadata(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        files: crate::types::FilesId,
        metadata: Arc<Mutex<FileMetadata>>,
    ) -> io::Result<()> {
        let (provider, task, start, table) = self
            .shared
            .physical
            .lock()
            .unwrap()
            .installation_identity(owner)?;
        self.shared
            .accepted
            .lock()
            .unwrap()
            .bind_installation_owner(
                owner,
                lease,
                Owner {
                    owner,
                    metadata,
                    files,
                    provider,
                    task,
                    start,
                    table,
                },
            )
    }

    pub(crate) fn accepted_installation_metadata(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<(crate::types::FilesId, Arc<Mutex<FileMetadata>>)> {
        let retained = self
            .shared
            .accepted
            .lock()
            .unwrap()
            .installation_owner(owner, lease)?;
        Ok((retained.files, retained.metadata))
    }

    pub(crate) fn accepted_installation_admission(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<crate::network_replay::NetworkFdPublicationAdmission>> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .installation_admission(owner, lease)
    }
    pub(crate) fn retain_accepted_installation_admission(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        admission: &crate::network_replay::NetworkFdPublicationAdmission,
    ) -> io::Result<()> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .retain_installation_admission(owner, lease, admission)
    }
    pub(crate) fn accepted_installed_binding(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Option<crate::types::FdSlotBinding>> {
        self.shared.accepted.lock().unwrap().installed(owner, lease)
    }

    pub(crate) fn accepted_installation_flags(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<nix::fcntl::OFlag> {
        let flags = self
            .shared
            .accepted
            .lock()
            .unwrap()
            .installation_flags(owner, lease)?;
        if flags & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) != 0 {
            return Err(io::Error::other(
                "installed accept contains rejected Linux flags",
            ));
        }
        Ok(nix::fcntl::OFlag::from_bits_truncate(flags))
    }

    pub(crate) fn retain_accepted_installed_binding(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        binding: crate::types::FdSlotBinding,
    ) -> io::Result<()> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .retain_installed(owner, lease, binding)
    }

    pub(crate) async fn accepted_original_installation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<Installation> {
        let through = self.installation_cut(owner, permit).await?;
        let journal = self.shared.fd_journal.lock().await;
        self.shared
            .accepted
            .lock()
            .unwrap()
            .original_installation(owner, lease, permit, through, journal.history())?
            .reconcile(journal.history())
    }

    pub(crate) fn accepted_captured_result(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<Result<i32, i32>> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .captured_result(owner, lease)
    }
    pub(crate) async fn accepted_original_no_installation(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<super::accepted::NoInstallation> {
        if let Some(receipt) = self
            .shared
            .accepted
            .lock()
            .unwrap()
            .retained_no_installation(owner, lease, permit)?
        {
            return Ok(receipt);
        }
        let through = self.installation_cut(owner, permit).await?;
        let journal = self.shared.fd_journal.lock().await;
        self.shared.accepted.lock().unwrap().no_installation(
            owner,
            lease,
            permit,
            through,
            journal.history(),
        )
    }
    pub(crate) fn accepted_no_installation_published(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<bool> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .no_installation_published(owner, lease)
    }
    pub(crate) fn retain_accepted_no_installation_published(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
        receipt: &super::accepted::NoInstallation,
    ) -> io::Result<()> {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .retain_no_installation_published(owner, lease, receipt)
    }

    /// Metadata only, borrowed from the already owned accepted file. This
    /// creates no new descriptor/reference and performs no read on the stream.
    pub(crate) fn accepted_installation_stat(
        &self,
        owner: NetworkStreamOwner,
        lease: NetworkAcceptLeaseId,
    ) -> io::Result<crate::stat::DetStat> {
        let accepted = self.shared.accepted.lock().unwrap();
        let stat = super::installation_observation::held_stat(accepted.pin(owner, lease)?.as_fd())?;
        if stat.mode & libc::S_IFMT != libc::S_IFSOCK {
            return Err(io::Error::other(
                "accepted held installation is not a socket",
            ));
        }
        Ok(stat)
    }

    /// Collect a fresh finite journal prefix under the existing logical table
    /// permit. The caller revalidates that same permit and owner at commit;
    /// cancellation retains the controller request and every collected row.
    pub(super) async fn installation_cut(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<u64> {
        self.shared.installation_cut(owner, permit).await
    }
}

impl super::RuntimeShared {
    pub(super) async fn installation_cut(
        &self,
        owner: NetworkStreamOwner,
        permit: NetworkFdPublicationPermit,
    ) -> io::Result<u64> {
        use super::accepted_controller::Effect;
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        if permit.owner != owner {
            return Err(io::Error::other("FD publication cut changed owner"));
        }
        let controller = self
            .controller
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::other("FD publication lacks provider controller"))?
            .map_err(io::Error::other)?;
        let request = controller.prepare(
            Effect::FdPublicationCut(permit.lease),
            owner,
            &Request::ReadFdPublicationCut { permit },
            || Ok(vec![]),
        )?;
        let Reply::FdPublicationCut {
            permit: observed,
            provider,
            status,
        } = controller.response(request).await?
        else {
            return Err(io::Error::other("FD publication cut changed reply kind"));
        };
        if observed != permit
            || provider.status.returned != 0
            || provider.raw.fatal != 0
            || status.status.returned != 0
            || status.raw.problem != 0
        {
            return Err(io::Error::other(
                "FD publication cut failed; raw reply retained",
            ));
        }
        let through = status.raw.next_event;
        if through != 0 {
            self.fd_journal
                .lock()
                .await
                .through(&controller, owner, through)
                .await?;
        }
        Ok(through)
    }

    /// A fresh finite cut belongs to this already-owned Call even when its
    /// original task/table no longer exists. It is observation authority only.
    async fn terminal_allocator_cut(
        &self,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<u64> {
        use super::accepted_controller::Effect;
        use super::accepted_provider::Reply;
        use super::accepted_provider::Request;
        let controller = self
            .controller
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| io::Error::other("terminal allocator lacks provider controller"))?
            .map_err(io::Error::other)?;
        let call = admission.call;
        let request = controller.prepare(
            Effect::OriginalAllocatorCut(call),
            owner,
            &Request::ReadOriginalAllocatorCut {
                call: call.native_command_call(),
            },
            || Ok(vec![]),
        )?;
        let Reply::OriginalAllocatorCut {
            call: observed,
            provider,
            status,
        } = controller.response(request).await?
        else {
            return Err(io::Error::other("terminal allocator cut changed response"));
        };
        if observed != call.native_command_call()
            || provider.status.returned != 0
            || provider.status.errno.is_some()
            || provider.raw.fatal != 0
            || status.status.returned != 0
            || status.status.errno.is_some()
            || status.raw.problem != 0
        {
            return Err(io::Error::other(
                "terminal allocator cut failed; actual response remains retained",
            ));
        }
        let through = status.raw.next_event;
        if through != 0 {
            self.fd_journal
                .lock()
                .await
                .through(&controller, owner, through)
                .await?;
        }
        Ok(through)
    }

    pub(super) async fn reconcile_terminal_allocator(
        self: &Arc<Self>,
        owner: NetworkStreamOwner,
        admission: &crate::network_replay::original_connect::Admission,
    ) -> io::Result<()> {
        use detcore_model::network_trace::NetworkTransportV2;
        use detcore_model::network_trace::StreamSocketKeyV3;

        use crate::network_replay::original_connect::Kind;
        use crate::network_replay::original_installation::FreshStreamEnrollment;
        use crate::network_replay::original_installation::OpenatEnrollment;
        let state = self
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .clone();
        if state.admission != *admission
            || !state.retired
            || !state.close_queued
            || !state.terminal_allocator_started
            || !admission.arguments.kind.allocator()
        {
            return Err(io::Error::other(
                "terminal allocation changed its exact retired Call",
            ));
        }
        let bound = state.installation_owner.clone().ok_or_else(|| {
            io::Error::other("terminal allocation lacks actual Prepared metadata")
        })?;
        let effect = state.completed_allocator_effect()?;
        let raw = i64::from(effect.original.returned);
        let installed = allocator_result(
            &effect.original,
            raw,
            admission.arguments.kind,
            admission.arguments.original_count,
        )?;
        let command = state
            .prepared
            .ok_or_else(|| io::Error::other("terminal allocator lost command"))?
            .1;
        if effect.original.selection.command != command {
            return Err(io::Error::other(
                "terminal allocator changed original command",
            ));
        }
        let publisher = state
            .publication
            .engine
            .lock()
            .unwrap()
            .terminal_allocator_successor(owner, admission)
            .map_err(io::Error::other)?;
        // Join any auxiliary observation already owned by this Call even when
        // there is no surviving table. Never start a getter against a dead PIDFD.
        let opened = if admission.arguments.kind == Kind::Openat
            && installed.is_some()
            && (publisher.is_some() || state.openat_observation.is_some())
        {
            self.observe_original_openat(owner, admission, publisher)
                .await?
        } else {
            None
        };
        let socket = match &effect.socket {
            Some(observation) => observation.checked(&effect)?,
            None if admission.arguments.kind == Kind::Socket
                && installed.is_some()
                && publisher.is_some() =>
            {
                self.observe_terminal_socket(owner, admission, publisher.unwrap(), &effect)
                    .await?
            }
            None => None,
        };
        let through = self.terminal_allocator_cut(owner, admission).await?;
        let receipt = {
            let journal = self.fd_journal.lock().await;
            match installed {
                Some((begin, end, fd)) => {
                    let source = match admission.arguments.kind {
                        Kind::Socket => Source::Socket(admission.call),
                        Kind::Openat => Source::Openat(admission.call),
                        Kind::EpollCreate { .. } => Source::EpollCreate(admission.call),
                        _ => unreachable!("checked allocator"),
                    };
                    let mut receipt = Installation::checked_terminal(
                        bound.clone(),
                        source,
                        command,
                        fd,
                        effect.original.selection.file,
                        begin,
                        end,
                        through,
                        journal.history(),
                    )?;
                    if admission.arguments.kind == Kind::Openat {
                        receipt.allocated = Some(allocated_profile(&effect.original)?);
                    } else if matches!(admission.arguments.kind, Kind::EpollCreate { .. }) {
                        receipt.epoll = Some(epoll_profile(&effect.original)?);
                    }
                    Some(receipt)
                }
                None => {
                    if journal.history().contains_command(command)? {
                        return Err(io::Error::other(
                            "negative terminal allocation contradicts actual journal",
                        ));
                    }
                    let receipt = NoInstallation {
                        owner: bound.clone(),
                        admission: admission.clone(),
                        returned: raw,
                        command,
                        through,
                    };
                    state
                        .publication
                        .engine
                        .lock()
                        .unwrap()
                        .reconcile_terminal_failed_allocator(owner, admission, &receipt)
                        .map_err(io::Error::other)?;
                    None
                }
            }
        };
        if let Some(receipt) = receipt {
            // Retain before any publication attempt; a failed transaction cannot
            // erase the actual install/removal history or pretend retirement.
            self.native_streams
                .lock()
                .unwrap()
                .original(owner, admission.call)?
                .terminal_installation = Some(receipt.clone());
            if let Some(publisher) = publisher {
                let removed = receipt.removed_before_publication();
                let profile = match admission.arguments.kind {
                    Kind::Openat => {
                        let (opened, stat) = if removed {
                            let p = receipt.allocated_profile()?;
                            (
                                OpenatEnrollment {
                                    kind: p.kind()?,
                                    status_flags: p.status_flags,
                                },
                                None,
                            )
                        } else {
                            let observed = opened
                                .as_ref()
                                .filter(|o| receipt.matches_observed_file(o.provider, o.file))
                                .ok_or_else(|| {
                                    io::Error::other(
                                        "live terminal Openat lacks its exact held metadata",
                                    )
                                })?;
                            (
                                OpenatEnrollment {
                                    kind: observed.kind()?,
                                    status_flags: observed.status_flags,
                                },
                                Some(observed.stat),
                            )
                        };
                        TerminalProfile {
                            opened: Some(opened),
                            stat,
                            fresh: None,
                        }
                    }
                    Kind::EpollCreate { .. } => {
                        let p = receipt.epoll_profile()?;
                        TerminalProfile {
                            opened: Some(OpenatEnrollment {
                                kind: crate::fd::FdType::Epoll,
                                status_flags: p.status_flags,
                            }),
                            stat: None,
                            fresh: None,
                        }
                    }
                    Kind::Socket => {
                        let observed = if removed {
                            None
                        } else {
                            Some(
                                socket
                                    .as_ref()
                                    .filter(|o| receipt.matches_observed_file(o.provider, o.file))
                                    .ok_or_else(|| {
                                        io::Error::other(
                                            "live terminal Socket lacks its exact held observation",
                                        )
                                    })?,
                            )
                        };
                        let tcp = matches!(admission.arguments.fd, libc::AF_INET | libc::AF_INET6)
                            && (admission.arguments.address as u32 as i32)
                                & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                                == libc::SOCK_STREAM
                            && matches!(admission.arguments.length, 0 | libc::IPPROTO_TCP);
                        let shadow = state.publication.engine.lock().unwrap().shadow_mode();
                        let fresh = if shadow && tcp && !removed {
                            let observed = observed.unwrap();
                            let namespace_fd = state.allocation_netns.as_ref()
                                .ok_or_else(|| io::Error::other("terminal Socket lost original network namespace capability"))?;
                            let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                            if unsafe {
                                libc::fstat(
                                    std::os::fd::AsRawFd::as_raw_fd(namespace_fd.as_ref()),
                                    stat.as_mut_ptr(),
                                )
                            } != 0
                            {
                                return Err(io::Error::last_os_error());
                            }
                            let stat = unsafe { stat.assume_init() };
                            if stat.st_ino != observed.namespace {
                                return Err(io::Error::other(
                                    "terminal Socket differs from its pinned original namespace",
                                ));
                            }
                            let key = StreamSocketKeyV3 {
                                transport: NetworkTransportV2::Tcp,
                                domain: admission.arguments.fd,
                                socket_type: libc::SOCK_STREAM,
                                protocol: libc::IPPROTO_TCP,
                            };
                            let observed_profile = if state.publication.record_network {
                                let (normalization, _) = super::socket_profile::record_receive_normalization_in_namespace(
                                    namespace_fd.as_fd(), key).map_err(io::Error::other)?;
                                Some(observed.fresh_profile(key, normalization)?)
                            } else {
                                None
                            };
                            Some(FreshStreamEnrollment {
                                key,
                                namespace: crate::network_replay::NetworkStreamNamespace {
                                    device: stat.st_dev,
                                    inode: stat.st_ino,
                                },
                                observed_profile,
                            })
                        } else {
                            None
                        };
                        TerminalProfile {
                            opened: None,
                            stat: observed.map(|o| o.metadata.stat),
                            fresh,
                        }
                    }
                    _ => unreachable!("checked allocator"),
                };
                // Getters and normalization above own no table permit. Acquire
                // the existing mutation lease now, then request the final fresh
                // journal cut under it; the preliminary receipt cannot authorize
                // a stale numeric slot after another writer ran.
                let mutation = loop {
                    let changed = state.publication.changed.notified();
                    tokio::pin!(changed);
                    changed.as_mut().enable();
                    let attempt = {
                        let actual = receipt.metadata();
                        let metadata = actual.lock().unwrap();
                        state
                            .publication
                            .engine
                            .lock()
                            .unwrap()
                            .begin_terminal_allocator_publication(
                                owner, admission, publisher, &receipt, &actual, &metadata,
                            )
                    };
                    match attempt {
                        Ok((mutation, _)) => break mutation,
                        Err(crate::network_replay::NetworkReplayError::StreamOperationBusy(_)) => {
                            changed.await
                        }
                        Err(error) => return Err(io::Error::other(error)),
                    }
                };
                let through = self
                    .installation_cut(publisher, mutation.publication.permit)
                    .await?;
                let mut refreshed = {
                    let journal = self.fd_journal.lock().await;
                    let (begin, end, fd) = installed.expect("positive receipt retained");
                    Installation::checked_terminal(
                        bound.clone(),
                        receipt.source(),
                        command,
                        fd,
                        effect.original.selection.file,
                        begin,
                        end,
                        through,
                        journal.history(),
                    )?
                };
                refreshed.allocated = receipt.allocated;
                refreshed.epoll = receipt.epoll;
                // A newly removed file needs only its authenticated historical
                // profile; it must not enroll a now-absent live TCP stream.
                let profile = if refreshed.removed_before_publication() {
                    let opened = if admission.arguments.kind == Kind::Openat {
                        let p = refreshed.allocated_profile()?;
                        Some(OpenatEnrollment {
                            kind: p.kind()?,
                            status_flags: p.status_flags,
                        })
                    } else if matches!(admission.arguments.kind, Kind::EpollCreate { .. }) {
                        Some(OpenatEnrollment {
                            kind: crate::fd::FdType::Epoll,
                            status_flags: refreshed.epoll_profile()?.status_flags,
                        })
                    } else {
                        None
                    };
                    TerminalProfile {
                        opened,
                        stat: None,
                        fresh: None,
                    }
                } else {
                    profile
                };
                self.native_streams
                    .lock()
                    .unwrap()
                    .original(owner, admission.call)?
                    .terminal_installation = Some(refreshed.clone());
                let publish = state
                    .publication
                    .terminal_allocator
                    .as_ref()
                    .ok_or_else(|| {
                        io::Error::other("terminal allocator lacks its run-owned publisher")
                    })?;
                publish(publisher, admission, &refreshed, profile).map_err(io::Error::other)?;
            } else {
                let actual = receipt.metadata();
                let metadata = actual.lock().unwrap();
                state
                    .publication
                    .engine
                    .lock()
                    .unwrap()
                    .reconcile_terminal_removed_allocator(
                        owner, admission, &receipt, &actual, &metadata,
                    )
                    .map_err(io::Error::other)?;
            }
        }
        self.native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .terminal_allocator_done = true;
        state.publication.changed.notify_waiters();
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn terminal_result_fixture(
    owner: NetworkStreamOwner,
    admission: &crate::network_replay::original_connect::Admission,
    command: u64,
    returned: i32,
) -> crate::network_runtime::OriginalResult {
    use super::accepted_provider_ffi as ffi;
    let mut raw = ffi::OriginalResult {
        selection: ffi::OriginalSelection {
            command,
            call: admission.call.native_command_call(),
            owner_mm: owner.mm.generation(),
            provider: 7,
            task: owner.thread.as_raw() as u64,
            task_start: 101,
            table: 13,
            file: if returned >= 0 { 19 } else { 0 },
            ready: 1,
            requested_fd: admission.arguments.fd,
            user_address: admission.arguments.address,
            address_length: admission.arguments.length,
            original_count: admission.arguments.original_count,
            ..Default::default()
        },
        complete: 1,
        returned,
        ..Default::default()
    };
    if returned >= 0 {
        raw.address[0..8].copy_from_slice(&1u64.to_ne_bytes());
        raw.address[8..16].copy_from_slice(&2u64.to_ne_bytes());
        raw.address[16..20].copy_from_slice(&returned.to_ne_bytes());
        if admission.arguments.kind == crate::network_replay::original_connect::Kind::Openat {
            raw.address[24..28].copy_from_slice(&(libc::S_IFREG | 0o640).to_ne_bytes());
            raw.address[28..32].copy_from_slice(&libc::O_RDWR.to_ne_bytes());
        } else if matches!(
            admission.arguments.kind,
            crate::network_replay::original_connect::Kind::EpollCreate { .. }
        ) {
            raw.address[24..28].copy_from_slice(&libc::O_RDWR.to_ne_bytes());
            raw.address[28..32].copy_from_slice(&libc::FD_CLOEXEC.to_ne_bytes());
            raw.address[32..36].copy_from_slice(&1u32.to_ne_bytes());
        }
    }
    raw.into()
}
#[cfg(test)]
pub(crate) fn epoll_profile_fixture(
    mut receipt: Installation,
    admission: &crate::network_replay::original_connect::Admission,
    descriptor_flags: i32,
) -> Installation {
    let mut result = terminal_result_fixture(
        receipt.original_owner(),
        admission,
        receipt.command,
        receipt.fd,
    );
    result.address[28..32].copy_from_slice(&descriptor_flags.to_ne_bytes());
    receipt.epoll = Some(epoll_profile(&result).unwrap());
    receipt
}

#[cfg(test)]
pub(crate) fn terminal_no_installation_fixture(
    owner: NetworkStreamOwner,
    metadata: Arc<Mutex<FileMetadata>>,
    admission: &crate::network_replay::original_connect::Admission,
    command: u64,
    returned: i64,
) -> NoInstallation {
    NoInstallation {
        owner: Owner {
            owner,
            metadata,
            files: admission.arguments.files,
            provider: 7,
            task: owner.thread.as_raw() as u64,
            start: 101,
            table: 13,
        },
        admission: admission.clone(),
        command,
        returned,
        through: 0,
    }
}

#[cfg(test)]
pub(crate) fn installation_fixture(
    owner: NetworkStreamOwner,
    metadata: Arc<Mutex<FileMetadata>>,
    permit: NetworkFdPublicationPermit,
    source: Source,
    command: u64,
    fd: i32,
    interference: bool,
) -> Installation {
    use super::accepted_provider_ffi as ffi;
    let mut history = History::default();
    let status = ffi::FdStatus {
        next_table: 13,
        next_file: 19,
        next_event: if interference { 4 } else { 2 },
        ..Default::default()
    };
    let first = ffi::FdEvent {
        sequence: 1,
        kind: 1,
        task: 31,
        task_start: 101,
        table: 13,
        file: 19,
        accept_command: command,
        fd,
        complete: 1,
        ..Default::default()
    };
    history.retain(status.into(), first.into()).unwrap();
    history
        .retain(
            status.into(),
            ffi::FdEvent {
                sequence: 2,
                kind: 2,
                dependency: 1,
                ..first
            }
            .into(),
        )
        .unwrap();
    if interference {
        let removed = ffi::FdEvent {
            sequence: 3,
            kind: 10,
            task: 32,
            task_start: 102,
            accept_command: 0,
            file: 0,
            ..first
        };
        history.retain(status.into(), removed.into()).unwrap();
        history
            .retain(
                status.into(),
                ffi::FdEvent {
                    sequence: 4,
                    kind: 3,
                    dependency: 3,
                    file: first.file,
                    ..removed
                }
                .into(),
            )
            .unwrap();
    }
    Installation::checked(
        Owner {
            owner,
            metadata,
            files: permit.files,
            provider: 7,
            task: 31,
            start: 101,
            table: 13,
        },
        permit,
        source,
        command,
        fd,
        19,
        1,
        2,
        status.next_event,
        &history,
    )
    .unwrap()
}

#[cfg(test)]
pub(crate) fn removed_installation_fixture(
    owner: NetworkStreamOwner,
    metadata: Arc<Mutex<FileMetadata>>,
    permit: NetworkFdPublicationPermit,
    source: Source,
    command: u64,
    fd: i32,
    retired: bool,
) -> Installation {
    use super::accepted_provider_ffi as ffi;
    let mut history = History::default();
    let status = ffi::FdStatus {
        next_table: 13,
        next_file: 19,
        next_event: if retired { 5 } else { 4 },
        ..Default::default()
    };
    let install = ffi::FdEvent {
        sequence: 1,
        kind: 1,
        task: 31,
        task_start: 101,
        table: 13,
        file: 19,
        accept_command: command,
        fd,
        complete: 1,
        ..Default::default()
    };
    history.retain(status.into(), install.into()).unwrap();
    history
        .retain(
            status.into(),
            ffi::FdEvent {
                sequence: 2,
                kind: 2,
                dependency: 1,
                ..install
            }
            .into(),
        )
        .unwrap();
    let remove = ffi::FdEvent {
        sequence: 3,
        kind: 10,
        task: 32,
        task_start: 102,
        table: 13,
        fd,
        complete: 1,
        ..Default::default()
    };
    history.retain(status.into(), remove.into()).unwrap();
    history
        .retain(
            status.into(),
            ffi::FdEvent {
                sequence: 4,
                kind: 3,
                dependency: 3,
                file: 19,
                ..remove
            }
            .into(),
        )
        .unwrap();
    if retired {
        history
            .retain(
                status.into(),
                ffi::FdEvent {
                    sequence: 5,
                    kind: 7,
                    task: 32,
                    task_start: 102,
                    file: 19,
                    fd: -1,
                    complete: 1,
                    ..Default::default()
                }
                .into(),
            )
            .unwrap();
    }
    Installation::checked(
        Owner {
            owner,
            metadata,
            files: permit.files,
            provider: 7,
            task: 31,
            start: 101,
            table: 13,
        },
        permit,
        source,
        command,
        fd,
        19,
        1,
        2,
        status.next_event,
        &history,
    )
    .unwrap()
    .reconcile(&history)
    .unwrap()
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TerminalFixture {
    Live,
    Close,
    NonfinalPut,
    FinalPut,
    ExecKeep,
    ExecRemove,
}

#[cfg(test)]
pub(crate) fn terminal_installation_fixture(
    owner: NetworkStreamOwner,
    metadata: Arc<Mutex<FileMetadata>>,
    source: Source,
    command: u64,
    fd: i32,
    disposition: TerminalFixture,
) -> Installation {
    use super::accepted_provider_ffi as ffi;
    let files = metadata.lock().unwrap().files_id;
    let mut rows = vec![ffi::FdEvent {
        sequence: 1,
        kind: 1,
        task: owner.thread.as_raw() as u64,
        task_start: 101,
        table: 13,
        file: 19,
        fd,
        accept_command: command,
        complete: 1,
        ..Default::default()
    }];
    rows.push(ffi::FdEvent {
        sequence: 2,
        kind: 2,
        dependency: 1,
        ..rows[0]
    });
    let event = |sequence, kind, dependency, file, fd, returned| ffi::FdEvent {
        sequence,
        kind,
        dependency,
        file,
        fd,
        returned,
        table: 13,
        task: 32,
        task_start: 102,
        complete: 1,
        ..Default::default()
    };
    match disposition {
        TerminalFixture::Live => {}
        TerminalFixture::Close => {
            rows.push(event(3, 10, 0, 0, fd, 0));
            rows.push(event(4, 3, 3, 19, fd, 0));
        }
        TerminalFixture::NonfinalPut => {
            rows.push(event(3, 12, 0, 0, -1, 0));
            rows.push(event(4, 13, 3, 0, -1, 0));
        }
        TerminalFixture::FinalPut => {
            rows.push(event(3, 12, 0, 0, -1, 0));
            rows.push(ffi::FdEvent {
                table: 0,
                ..event(4, 7, 0, 19, -1, 0)
            });
            rows.push(event(5, 9, 3, 0, -1, 0));
            rows.push(event(6, 13, 5, 0, -1, 1));
        }
        TerminalFixture::ExecKeep => {
            rows.push(event(3, 17, 0, 0, -1, 0));
            rows.push(event(4, 19, 3, 0, -1, 0));
        }
        TerminalFixture::ExecRemove => {
            rows.push(event(3, 17, 0, 0, -1, 0));
            rows.push(event(4, 18, 3, 19, fd, 0));
            rows.push(event(5, 19, 3, 0, -1, 1));
        }
    }
    let through = rows.last().unwrap().sequence;
    let status = ffi::FdStatus {
        next_table: 13,
        next_file: 19,
        next_event: through,
        ..Default::default()
    };
    let mut history = History::default();
    for row in rows {
        history.retain(status.into(), row.into()).unwrap();
    }
    Installation::checked_terminal(
        Owner {
            owner,
            metadata,
            files,
            provider: 7,
            task: owner.thread.as_raw() as u64,
            start: 101,
            table: 13,
        },
        source,
        command,
        fd,
        19,
        1,
        2,
        through,
        &history,
    )
    .unwrap()
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::network_replay::NetworkStreamLeaseId;
    use crate::types::DetTid;
    use crate::types::FilesId;
    use crate::types::MmId;

    #[test]
    fn epoll_receipt_refuses_missing_wrong_class_and_mixed_flag_domains() {
        use super::super::accepted_provider_ffi as ffi;
        use crate::network_replay::original_connect::Kind;
        for (legacy, nr, argument) in [(true, 213u64, 1), (false, 291u64, libc::EPOLL_CLOEXEC)] {
            let kind = Kind::EpollCreate { legacy };
            let mut raw = ffi::OriginalResult {
                selection: ffi::OriginalSelection {
                    file: 19,
                    user_address: nr,
                    requested_fd: argument,
                    ..Default::default()
                },
                returned: 17,
                complete: 1,
                ..Default::default()
            };
            raw.address[0..8].copy_from_slice(&1u64.to_ne_bytes());
            raw.address[8..16].copy_from_slice(&2u64.to_ne_bytes());
            raw.address[16..20].copy_from_slice(&17i32.to_ne_bytes());
            raw.address[24..28].copy_from_slice(&libc::O_RDWR.to_ne_bytes());
            raw.address[28..32].copy_from_slice(&libc::FD_CLOEXEC.to_ne_bytes());
            raw.address[32..36].copy_from_slice(&1u32.to_ne_bytes());
            assert_eq!(
                allocator_result(&raw.into(), 17, kind, 0).unwrap(),
                Some((1, 2, 17))
            );
            for bad in 0..11 {
                let mut changed = raw;
                match bad {
                    0 => changed.selection.file = 0,
                    1 => changed.selection.user_address = 257,
                    2 => changed.selection.requested_fd = if legacy { 0 } else { 1 },
                    3 => changed.address[24..28].copy_from_slice(&libc::O_CLOEXEC.to_ne_bytes()),
                    4 => changed.address[28..32].copy_from_slice(&2i32.to_ne_bytes()),
                    5 => changed.address[32] = 0,
                    6 => changed.address[127] = 1,
                    7 => changed.complete = 0,
                    8 => changed.address[8..16].copy_from_slice(&1u64.to_ne_bytes()),
                    9 => changed.returned = -libc::EINVAL,
                    10 => changed.selection.user_address = if legacy { 291 } else { 213 },
                    _ => unreachable!(),
                }
                assert!(
                    allocator_result(&changed.into(), i64::from(changed.returned), kind, 0)
                        .is_err(),
                    "bad{bad}"
                );
            }
            raw.selection.file = 0;
            raw.selection.requested_fd = if legacy { 0 } else { 1 };
            raw.address = [0; 128];
            raw.returned = -libc::EINVAL;
            assert_eq!(
                allocator_result(&raw.into(), -i64::from(libc::EINVAL), kind, 0).unwrap(),
                None
            );
            raw.complete = 0;
            assert!(allocator_result(&raw.into(), -i64::from(libc::EINVAL), kind, 0).is_err());
        }
    }

    #[test]
    fn resumed_installation_requires_complete_same_effect_identity_and_monotonic_cut() {
        let thread = DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let actual = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
        let permit = NetworkFdPublicationPermit {
            owner,
            files: FilesId::initial(thread),
            lease: NetworkStreamLeaseId::controlled_fixture(9),
        };
        let prior = installation_fixture(
            owner,
            actual.clone(),
            permit,
            Source::Socket(NetworkStreamCallId::controlled_fixture(1)),
            71,
            17,
            false,
        );
        assert!(prior.resumes(&prior));
        let mut later = prior.clone();
        later.through += 1;
        assert!(later.resumes(&prior));
        assert!(!prior.resumes(&later));
        for bad in 0..13 {
            let mut changed = later.clone();
            match bad {
                0 => changed.owner.provider += 1,
                1 => changed.owner.task += 1,
                2 => changed.owner.start += 1,
                3 => changed.owner.table += 1,
                4 => {
                    changed.owner.metadata =
                        Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)))
                }
                5 => {
                    changed.permit.as_mut().unwrap().lease =
                        NetworkStreamLeaseId::controlled_fixture(10)
                }
                6 => changed.source = Source::Socket(NetworkStreamCallId::controlled_fixture(2)),
                7 => changed.command += 1,
                8 => changed.fd += 1,
                9 => changed.file += 1,
                10 => changed.begin += 1,
                11 => changed.end += 1,
                12 => changed.interference.push(3),
                _ => unreachable!(),
            }
            assert!(
                !changed.resumes(&prior),
                "changed original effect field {bad}"
            );
        }
        // Later-cut arithmetic is only a comparison of private checked
        // receipts; production construction still requires the complete History.
        let conflicted = installation_fixture(
            owner,
            actual,
            permit,
            Source::Socket(NetworkStreamCallId::controlled_fixture(1)),
            71,
            17,
            true,
        );
        assert!(!conflicted.resumes(&prior));
        assert!(!conflicted.resumes(&conflicted));
    }
    #[test]
    fn removal_join_refuses_open_begin_foreign_file_reuse_and_retirement_before_remove() {
        use super::super::accepted_provider_ffi as ffi;
        for bad in 0..4 {
            let thread = DetTid::from_raw(31);
            let owner = NetworkStreamOwner {
                thread,
                mm: MmId::initial(thread),
            };
            let metadata = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
            let permit = NetworkFdPublicationPermit {
                owner,
                files: FilesId::initial(thread),
                lease: NetworkStreamLeaseId::controlled_fixture(9),
            };
            let status = ffi::FdStatus {
                next_table: 13,
                next_file: 20,
                next_event: match bad {
                    0 => 3,
                    1 => 4,
                    2 => 6,
                    _ => 5,
                },
                ..Default::default()
            };
            let mut history = History::default();
            let install = ffi::FdEvent {
                sequence: 1,
                kind: 1,
                task: 31,
                task_start: 101,
                table: 13,
                file: 19,
                accept_command: 71,
                fd: 17,
                complete: 1,
                ..Default::default()
            };
            history.retain(status.into(), install.into()).unwrap();
            history
                .retain(
                    status.into(),
                    ffi::FdEvent {
                        sequence: 2,
                        kind: 2,
                        dependency: 1,
                        ..install
                    }
                    .into(),
                )
                .unwrap();
            if bad == 3 {
                history
                    .retain(
                        status.into(),
                        ffi::FdEvent {
                            sequence: 3,
                            kind: 7,
                            task: 32,
                            task_start: 102,
                            file: 19,
                            fd: -1,
                            complete: 1,
                            ..Default::default()
                        }
                        .into(),
                    )
                    .unwrap();
            }
            let begin = ffi::FdEvent {
                sequence: if bad == 3 { 4 } else { 3 },
                kind: 10,
                task: 32,
                task_start: 102,
                table: 13,
                fd: 17,
                complete: 1,
                ..Default::default()
            };
            history.retain(status.into(), begin.into()).unwrap();
            if bad != 0 {
                history
                    .retain(
                        status.into(),
                        ffi::FdEvent {
                            sequence: begin.sequence + 1,
                            kind: 3,
                            file: if bad == 1 { 20 } else { 19 },
                            dependency: begin.sequence,
                            ..begin
                        }
                        .into(),
                    )
                    .unwrap();
            }
            if bad == 2 {
                let reused = ffi::FdEvent {
                    sequence: 5,
                    file: 20,
                    accept_command: 0,
                    task: 32,
                    task_start: 102,
                    ..install
                };
                history.retain(status.into(), reused.into()).unwrap();
                history
                    .retain(
                        status.into(),
                        ffi::FdEvent {
                            sequence: 6,
                            kind: 2,
                            dependency: 5,
                            ..reused
                        }
                        .into(),
                    )
                    .unwrap();
            }
            let receipt = Installation::checked(
                Owner {
                    owner,
                    metadata,
                    files: permit.files,
                    provider: 7,
                    task: 31,
                    start: 101,
                    table: 13,
                },
                permit,
                Source::Socket(NetworkStreamCallId::controlled_fixture(1)),
                71,
                17,
                19,
                1,
                2,
                status.next_event,
                &history,
            )
            .unwrap();
            assert!(
                receipt.reconcile(&history).is_err(),
                "unjoined effect case {bad}"
            );
        }
    }

    #[test]
    fn removal_join_refuses_same_file_alias_install_and_replace_across_slots_and_tables() {
        use super::super::accepted_provider_ffi as ffi;
        for replaced in [false, true] {
            for table in [13, 14] {
                for alias_after_remove in [false, true] {
                    let thread = DetTid::from_raw(31);
                    let owner = NetworkStreamOwner {
                        thread,
                        mm: MmId::initial(thread),
                    };
                    let metadata =
                        Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
                    let permit = NetworkFdPublicationPermit {
                        owner,
                        files: FilesId::initial(thread),
                        lease: NetworkStreamLeaseId::controlled_fixture(9),
                    };
                    let status = ffi::FdStatus {
                        next_table: 14,
                        next_file: 19,
                        next_event: 6,
                        ..Default::default()
                    };
                    let mut history = History::default();
                    let install = ffi::FdEvent {
                        sequence: 1,
                        kind: 1,
                        task: 31,
                        task_start: 101,
                        table: 13,
                        file: 19,
                        accept_command: 71,
                        fd: 17,
                        complete: 1,
                        ..Default::default()
                    };
                    history.retain(status.into(), install.into()).unwrap();
                    history
                        .retain(
                            status.into(),
                            ffi::FdEvent {
                                sequence: 2,
                                kind: 2,
                                dependency: 1,
                                ..install
                            }
                            .into(),
                        )
                        .unwrap();
                    let alias_sequence = if alias_after_remove { 5 } else { 3 };
                    let remove_sequence = if alias_after_remove { 3 } else { 5 };
                    let alias = ffi::FdEvent {
                        sequence: alias_sequence,
                        kind: if replaced { 4 } else { 1 },
                        task: 32,
                        task_start: 102,
                        table,
                        file: 19,
                        fd: 18,
                        complete: 1,
                        ..Default::default()
                    };
                    let remove = ffi::FdEvent {
                        sequence: remove_sequence,
                        kind: 10,
                        task: 32,
                        task_start: 102,
                        table: 13,
                        fd: 17,
                        complete: 1,
                        ..Default::default()
                    };
                    let alias_end = ffi::FdEvent {
                        sequence: alias_sequence + 1,
                        kind: if replaced { 6 } else { 2 },
                        dependency: alias_sequence,
                        returned: if replaced { 18 } else { 0 },
                        ..alias
                    };
                    let remove_end = ffi::FdEvent {
                        sequence: remove_sequence + 1,
                        kind: 3,
                        dependency: remove_sequence,
                        file: 19,
                        ..remove
                    };
                    let mut rows = [alias, alias_end, remove, remove_end];
                    rows.sort_by_key(|row| row.sequence);
                    for row in rows {
                        history.retain(status.into(), row.into()).unwrap();
                    }
                    let receipt = Installation::checked(
                        Owner {
                            owner,
                            metadata,
                            files: permit.files,
                            provider: 7,
                            task: 31,
                            start: 101,
                            table: 13,
                        },
                        permit,
                        Source::Socket(NetworkStreamCallId::controlled_fixture(1)),
                        71,
                        17,
                        19,
                        1,
                        2,
                        status.next_event,
                        &history,
                    )
                    .unwrap();
                    assert!(
                        receipt.reconcile(&history).is_err(),
                        "unpublished alias: replace={replaced} table={table} after_remove={alias_after_remove}"
                    );
                }
            }
        }
    }

    #[test]
    fn terminal_installation_distinguishes_nonfinal_put_exec_and_actual_table_drain() {
        let thread = crate::types::DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        for (kind, removed) in [
            (TerminalFixture::Live, false),
            (TerminalFixture::Close, true),
            (TerminalFixture::NonfinalPut, false),
            (TerminalFixture::FinalPut, true),
            (TerminalFixture::ExecKeep, false),
            (TerminalFixture::ExecRemove, true),
        ] {
            let actual = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
            let receipt = terminal_installation_fixture(
                owner,
                actual.clone(),
                Source::Openat(NetworkStreamCallId::controlled_fixture(1)),
                71,
                17,
                kind,
            );
            assert_eq!(receipt.removed_before_publication(), removed);
            receipt
                .validate_terminal(owner, &actual, &actual.lock().unwrap())
                .unwrap();
            assert!(receipt.permit.is_none());
            assert!(receipt.reconciled.is_some());
            // A copied table-shaped value is not the backend-owned metadata Arc.
            let foreign = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
            assert!(
                receipt
                    .validate_terminal(owner, &foreign, &foreign.lock().unwrap())
                    .is_err()
            );
        }
    }

    #[test]
    fn terminal_installation_requires_complete_table_retirement_and_exact_original_actor() {
        use super::super::accepted_provider_ffi as ffi;
        let thread = crate::types::DetTid::from_raw(31);
        let owner = NetworkStreamOwner {
            thread,
            mm: crate::types::MmId::initial(thread),
        };
        for cut in [3, 4, 5] {
            let actual = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
            let mut history = History::default();
            let first = ffi::FdEvent {
                sequence: 1,
                kind: 1,
                task: 31,
                task_start: 101,
                table: 13,
                file: 19,
                fd: 17,
                accept_command: 71,
                complete: 1,
                ..Default::default()
            };
            let put = ffi::FdEvent {
                sequence: 3,
                kind: 12,
                task: 31,
                task_start: 101,
                table: 13,
                fd: -1,
                complete: 1,
                ..Default::default()
            };
            let rows = [
                first,
                ffi::FdEvent {
                    sequence: 2,
                    kind: 2,
                    dependency: 1,
                    ..first
                },
                put,
                ffi::FdEvent {
                    sequence: 4,
                    kind: 9,
                    dependency: 3,
                    ..put
                },
                ffi::FdEvent {
                    sequence: 5,
                    kind: 13,
                    dependency: 4,
                    returned: 1,
                    ..put
                },
            ];
            let status = ffi::FdStatus {
                next_table: 13,
                next_file: 19,
                next_event: cut,
                ..Default::default()
            };
            for row in rows.into_iter().filter(|r| r.sequence <= cut) {
                history.retain(status.into(), row.into()).unwrap();
            }
            let bound = Owner {
                owner,
                metadata: actual,
                files: crate::types::FilesId::initial(thread),
                provider: 7,
                task: 31,
                start: 101,
                table: 13,
            };
            let good = Installation::checked_terminal(
                bound.clone(),
                Source::Socket(NetworkStreamCallId::controlled_fixture(1)),
                71,
                17,
                19,
                1,
                2,
                cut,
                &history,
            );
            assert_eq!(good.is_ok(), cut == 5, "unfinished table put cut={cut}");
            let mut changed = bound;
            changed.start += 1;
            assert!(
                Installation::checked_terminal(
                    changed,
                    Source::Socket(NetworkStreamCallId::controlled_fixture(1)),
                    71,
                    17,
                    19,
                    1,
                    2,
                    cut,
                    &history
                )
                .is_err()
            );
        }
    }
}

#[cfg(test)]
#[path = "original_installation/native_publication.rs"]
mod native_publication;
