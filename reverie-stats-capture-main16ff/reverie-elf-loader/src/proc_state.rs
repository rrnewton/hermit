/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Inactive proc identity and auxv descriptor building blocks (design LB7).
//!
//! No backend calls this module. Genuine proc objects are recognized by their
//! pinned kernel identities and task generation, never by a caller's pathname
//! text. Each opened virtual auxv is a real kernel OFD on an immutable memfd:
//! another open has a fresh cursor, dup shares the cursor, and replacement of
//! the current snapshot cannot change a previously opened descriptor.
//!
//! This does not virtualize arbitrary proc endpoints or activate Hermit's
//! clock/counter continuation. The inactive clock carrier preserves an actual
//! runtime counter snapshot and opaque owner state for its LB7 fixture. Later
//! exec requiring inherited virtual state
//! remains explicitly refused by [`prepare_exec_continuation`]. Callers must
//! qualify lookup safety and reserve private FD capacity before resolving any
//! supplied pathname; these APIs consume already pinned alias objects.

use std::ffi::CStr;
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const PROC_SUPER_MAGIC: i64 = 0x9fa0;
const TMPFS_MAGIC: i64 = 0x0102_1994;
const SNAPSHOT_SEALS: i32 =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
const CLOCK_STATE_MAGIC: &[u8; 8] = b"REVLBCK\0";
const CLOCK_STATE_VERSION: u64 = 1;
const CLOCK_STATE_HEADER_BYTES: usize = 24;
const MAX_CLOCK_STATE_BYTES: usize = 4096;

/// Proc support refusal and launcher infrastructure error are separate.
#[derive(Debug)]
pub enum ProcStateError {
    InheritedVirtualProcStateUnsupported { reason: &'static str },
    Infrastructure(io::Error),
}

impl ProcStateError {
    pub fn name(&self) -> &'static str {
        match self {
            Self::InheritedVirtualProcStateUnsupported { .. } => {
                "InheritedVirtualProcStateUnsupported"
            }
            Self::Infrastructure(_) => "ProcStateInfrastructure",
        }
    }
}

impl fmt::Display for ProcStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InheritedVirtualProcStateUnsupported { reason } => {
                write!(formatter, "{}: {reason}", self.name())
            }
            Self::Infrastructure(error) => write!(formatter, "{}: {error}", self.name()),
        }
    }
}

impl std::error::Error for ProcStateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Infrastructure(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ProcStateError {
    fn from(error: io::Error) -> Self {
        Self::Infrastructure(error)
    }
}

fn unsupported(reason: &'static str) -> ProcStateError {
    ProcStateError::InheritedVirtualProcStateUnsupported { reason }
}

/// Stable identity of an already pinned kernel object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelObjectIdentity {
    pub mount_id: u64,
    pub inode: u64,
    pub device_major: u32,
    pub device_minor: u32,
}

impl KernelObjectIdentity {
    pub fn of(file: &File) -> Result<Self, ProcStateError> {
        // SAFETY: statx is a valid, writable ABI output buffer.
        let mut stat: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: the empty string is NUL terminated; this queries the existing
        // descriptor and does not resolve its former pathname.
        if unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_DONT_SYNC,
                libc::STATX_INO | libc::STATX_MNT_ID,
                &mut stat,
            )
        } != 0
        {
            return Err(io::Error::last_os_error().into());
        }
        if stat.stx_mask & (libc::STATX_INO | libc::STATX_MNT_ID)
            != libc::STATX_INO | libc::STATX_MNT_ID
        {
            return Err(unsupported("pinned object lacks mount/inode provenance"));
        }
        Ok(Self {
            mount_id: stat.stx_mnt_id,
            inode: stat.stx_ino,
            device_major: stat.stx_dev_major,
            device_minor: stat.stx_dev_minor,
        })
    }
}

fn filesystem_type(file: &File) -> Result<i64, ProcStateError> {
    // SAFETY: statfs is writable for the exact ABI output structure.
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: this is a read-only query on an already owned descriptor.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(stat.f_type)
}

fn o_path(path: &Path) -> Result<File, ProcStateError> {
    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)?)
}

fn openat_file(directory: &File, name: &CStr, flags: i32) -> io::Result<File> {
    // SAFETY: directory is owned, name is NUL terminated, flags need no mode.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new descriptor with sole ownership here.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn sealed_carrier(name: &CStr, bytes: &[u8]) -> Result<File, ProcStateError> {
    // SAFETY: name is NUL terminated and the flags create a fresh kernel memfd.
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: memfd_create returned a new descriptor, owned only here.
    let carrier = unsafe { File::from_raw_fd(fd) };
    carrier.write_all_at(bytes, 0)?;
    // SAFETY: F_ADD_SEALS takes an integer mask on this owned memfd.
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, SNAPSHOT_SEALS) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if filesystem_type(&carrier)? != TMPFS_MAGIC {
        return Err(unsupported("carrier is not the audited memfd object"));
    }
    Ok(carrier)
}

/// Counter snapshot and uninterpreted Tool-owned state for the LB7 carrier.
///
/// `counter_offset` is captured from the runtime's actual guest counter read.
/// The owner must encode, restore and verify its own logical time and committed
/// time in `owner_state`; this crate does not assert Detcore state restoration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClockContinuationState {
    pub counter_offset: u64,
    pub owner_state: Vec<u8>,
}

/// Immutable, versioned clock state held by a genuine sealed kernel carrier.
///
/// This standalone building block has no production caller. It neither admits
/// an exec continuation nor supplies the shared proc/OFD state needed for one.
#[derive(Debug)]
pub struct ClockStateCarrier {
    carrier: File,
}

impl ClockStateCarrier {
    /// Seal the actual saved counter offset and the exact opaque owner bytes.
    pub fn new(counter_offset: u64, owner_state: &[u8]) -> Result<Self, ProcStateError> {
        if owner_state.len() > MAX_CLOCK_STATE_BYTES - CLOCK_STATE_HEADER_BYTES {
            return Err(unsupported("clock carrier exceeds its size bound"));
        }
        let mut bytes = Vec::with_capacity(CLOCK_STATE_HEADER_BYTES + owner_state.len());
        bytes.extend_from_slice(CLOCK_STATE_MAGIC);
        bytes.extend_from_slice(&CLOCK_STATE_VERSION.to_le_bytes());
        bytes.extend_from_slice(&counter_offset.to_le_bytes());
        bytes.extend_from_slice(owner_state);
        Ok(Self {
            carrier: sealed_carrier(c"reverie-inactive-clock-state", &bytes)?,
        })
    }

    /// Borrow the owned carrier for an explicitly managed descriptor transfer.
    pub fn file(&self) -> &File {
        &self.carrier
    }

    /// Decode only a retained, immutable kernel carrier; no pathname is opened.
    pub fn read(file: &File) -> Result<ClockContinuationState, ProcStateError> {
        // SAFETY: F_GET_SEALS is a read-only query on the retained descriptor.
        let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if seals & SNAPSHOT_SEALS != SNAPSHOT_SEALS || filesystem_type(file)? != TMPFS_MAGIC {
            return Err(unsupported("clock state is not an immutable memfd carrier"));
        }
        let length = file.metadata()?.len();
        if !(CLOCK_STATE_HEADER_BYTES as u64..=MAX_CLOCK_STATE_BYTES as u64).contains(&length) {
            return Err(unsupported("clock carrier has an invalid size"));
        }
        let mut bytes = vec![0; length as usize];
        file.read_exact_at(&mut bytes, 0)?;
        if &bytes[..8] != CLOCK_STATE_MAGIC
            || u64::from_le_bytes(bytes[8..16].try_into().unwrap()) != CLOCK_STATE_VERSION
        {
            return Err(unsupported("clock carrier has an unknown format"));
        }
        Ok(ClockContinuationState {
            counter_offset: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            owner_state: bytes[CLOCK_STATE_HEADER_BYTES..].to_vec(),
        })
    }
}

/// Process/thread lifetime binding. mm generation is supplied by its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskGeneration {
    pub pid: u32,
    pub tid: u32,
    pub process_start_time: u64,
    pub thread_start_time: u64,
    pub pid_namespace: String,
    pub mm_generation: u64,
}

impl TaskGeneration {
    /// Read audited self/stat endpoints and reject a mismatched proc PID view.
    pub fn current(mm_generation: u64) -> Result<Self, ProcStateError> {
        if mm_generation == 0 {
            return Err(unsupported("mm generation must be explicitly owned"));
        }
        let (pid, process_start_time) = parse_task_stat(&fs::read("/proc/self/stat")?)?;
        let (tid, thread_start_time) = parse_task_stat(&fs::read("/proc/thread-self/stat")?)?;
        // SAFETY: getpid/gettid are read-only integer queries.
        let actual_pid = unsafe { libc::getpid() } as u32;
        // SAFETY: SYS_gettid takes no arguments.
        let actual_tid = unsafe { libc::syscall(libc::SYS_gettid) } as u32;
        if pid != actual_pid || tid != actual_tid {
            return Err(unsupported(
                "proc PID namespace differs from caller context",
            ));
        }
        let pid_namespace = fs::read_link("/proc/self/ns/pid")?
            .to_str()
            .ok_or_else(|| unsupported("unreadable PID namespace identity"))?
            .to_owned();
        Ok(Self {
            pid,
            tid,
            process_start_time,
            thread_start_time,
            pid_namespace,
            mm_generation,
        })
    }
}

fn parse_task_stat(bytes: &[u8]) -> Result<(u32, u64), ProcStateError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| unsupported("malformed audited task stat endpoint"))?;
    let (pid, _) = text
        .split_once(' ')
        .ok_or_else(|| unsupported("malformed audited task stat endpoint"))?;
    // comm may contain spaces and ')'; kernel appends the remaining fields
    // after its final ')' (field 2). Starttime is field 22, index 19 here.
    let close = text
        .rfind(')')
        .ok_or_else(|| unsupported("malformed audited task stat endpoint"))?;
    let start_time = text[close + 1..]
        .split_ascii_whitespace()
        .nth(19)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| unsupported("malformed audited task start time"))?;
    Ok((
        pid.parse()
            .map_err(|_| unsupported("malformed audited task PID"))?,
        start_time,
    ))
}

/// Resolved endpoint information, with no pathname-based admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAuxv {
    pub task: TaskGeneration,
    pub object: KernelObjectIdentity,
}

/// Narrow resolver for genuine current-task auxv objects only.
#[derive(Debug)]
pub struct ProcAliasResolver {
    task: TaskGeneration,
    // Keeping canonical O_PATH descriptors alive prevents inode-cache reuse.
    objects: Vec<(KernelObjectIdentity, File)>,
}

impl ProcAliasResolver {
    /// Pin the three audited kernel aliases; no arbitrary proc endpoint enters
    /// this registry. Supplied symlink/dirfd aliases are recognized afterward
    /// through their resolved pinned object identities.
    pub fn for_current_task(mm_generation: u64) -> Result<Self, ProcStateError> {
        let task = TaskGeneration::current(mm_generation)?;
        let paths = [
            "/proc/self/auxv".to_owned(),
            "/proc/thread-self/auxv".to_owned(),
            format!("/proc/self/task/{}/auxv", task.tid),
        ];
        let mut objects = Vec::new();
        for path in paths {
            let file = o_path(Path::new(&path))?;
            if filesystem_type(&file)? != PROC_SUPER_MAGIC {
                return Err(unsupported("audited auxv endpoint is not procfs"));
            }
            objects.push((KernelObjectIdentity::of(&file)?, file));
        }
        if TaskGeneration::current(mm_generation)? != task {
            return Err(unsupported(
                "task generation changed while pinning proc aliases",
            ));
        }
        Ok(Self { task, objects })
    }

    pub fn task(&self) -> &TaskGeneration {
        &self.task
    }

    /// Resolve an already pinned O_PATH alias and the exact owner generation.
    pub fn resolve(
        &self,
        alias: &File,
        task: &TaskGeneration,
    ) -> Result<ResolvedAuxv, ProcStateError> {
        if task != &self.task || TaskGeneration::current(task.mm_generation)? != self.task {
            return Err(unsupported(
                "proc alias task/mm generation is not registered",
            ));
        }
        // SAFETY: F_GETFL is a read-only integer query on the alias descriptor.
        let flags = unsafe { libc::fcntl(alias.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if flags & libc::O_PATH == 0 || filesystem_type(alias)? != PROC_SUPER_MAGIC {
            return Err(unsupported("alias is not a registered O_PATH proc object"));
        }
        let object = KernelObjectIdentity::of(alias)?;
        if !self
            .objects
            .iter()
            .any(|(registered, _)| *registered == object)
        {
            return Err(unsupported(
                "proc object is outside the registered auxv endpoints",
            ));
        }
        Ok(ResolvedAuxv {
            task: self.task.clone(),
            object,
        })
    }
}

/// Immutable saved auxv backed by a real sealed kernel object.
#[derive(Debug)]
pub struct AuxvSnapshot {
    mm_generation: u64,
    words: Box<[u64]>,
    carrier: File,
    identity: KernelObjectIdentity,
}

impl AuxvSnapshot {
    /// Preserve the exact original type/value words including terminal AT_NULL.
    pub fn new(mm_generation: u64, original_words: &[u64]) -> Result<Self, ProcStateError> {
        if mm_generation == 0
            || original_words.len() < 2
            || !original_words.len().is_multiple_of(2)
            || original_words.len() > 256
            || original_words[original_words.len() - 2..] != [0, 0]
            || original_words[..original_words.len() - 2]
                .as_chunks::<2>()
                .0
                .iter()
                .any(|pair| pair[0] == 0)
        {
            return Err(unsupported("snapshot lacks a bounded native auxv vector"));
        }
        let bytes: Vec<_> = original_words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect();
        let carrier = sealed_carrier(c"reverie-inactive-auxv-snapshot", &bytes)?;
        let identity = KernelObjectIdentity::of(&carrier)?;
        Ok(Self {
            mm_generation,
            words: original_words.into(),
            carrier,
            identity,
        })
    }

    pub fn mm_generation(&self) -> u64 {
        self.mm_generation
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub fn identity(&self) -> KernelObjectIdentity {
        self.identity
    }

    pub fn seals(&self) -> Result<i32, ProcStateError> {
        // SAFETY: F_GET_SEALS is a read-only query on this owned memfd.
        let seals = unsafe { libc::fcntl(self.carrier.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(seals)
    }

    /// Open the pinned carrier with a fresh kernel OFD/cursor.
    ///
    /// This uses only the audited current proc-fd endpoint and the owned
    /// carrier. The reopened inode, filesystem and seals are checked. A dup
    /// made afterward with File::try_clone shares this OFD's actual cursor.
    pub fn open(&self) -> Result<File, ProcStateError> {
        if self.seals()? & SNAPSHOT_SEALS != SNAPSHOT_SEALS {
            return Err(unsupported("snapshot carrier lost immutable seals"));
        }
        let directory = o_path(Path::new("/proc/self/fd"))?;
        if filesystem_type(&directory)? != PROC_SUPER_MAGIC {
            return Err(unsupported("audited descriptor endpoint is not procfs"));
        }
        let name = CString::new(self.carrier.as_raw_fd().to_string())
            .expect("an integer descriptor has no NUL");
        // First pin the object through the audited descriptor directory. There
        // is no original pathname reopen and no read-open of an unknown object.
        let pin = openat_file(&directory, &name, libc::O_PATH | libc::O_CLOEXEC)?;
        if KernelObjectIdentity::of(&pin)? != self.identity {
            return Err(unsupported(
                "snapshot proc-fd reopen changed object identity",
            ));
        }
        let name = CString::new(pin.as_raw_fd().to_string()).expect("integer has no NUL");
        let file = openat_file(&directory, &name, libc::O_RDONLY | libc::O_CLOEXEC)?;
        if KernelObjectIdentity::of(&file)? != self.identity
            || filesystem_type(&file)? != TMPFS_MAGIC
        {
            return Err(unsupported(
                "snapshot read descriptor changed object identity",
            ));
        }
        Ok(file)
    }
}

/// Current modeled mm snapshot; already-opened OFDs retain the old snapshot.
#[derive(Debug)]
pub struct AuxvState {
    current: AuxvSnapshot,
}

impl AuxvState {
    pub fn new(snapshot: AuxvSnapshot) -> Self {
        Self { current: snapshot }
    }

    pub fn snapshot(&self) -> &AuxvSnapshot {
        &self.current
    }

    pub fn open_current(&self) -> Result<File, ProcStateError> {
        self.current.open()
    }

    pub fn open_alias(
        &self,
        resolver: &ProcAliasResolver,
        alias: &File,
        task: &TaskGeneration,
    ) -> Result<File, ProcStateError> {
        let resolved = resolver.resolve(alias, task)?;
        if resolved.task.mm_generation != self.current.mm_generation() {
            return Err(unsupported(
                "resolved auxv belongs to a different mm generation",
            ));
        }
        self.current.open()
    }

    /// Replacement is a model transition, not runtime exec activation.
    pub fn replace(&mut self, next: AuxvSnapshot) -> Result<(), ProcStateError> {
        if next.mm_generation() <= self.current.mm_generation() {
            return Err(unsupported(
                "snapshot generation must advance on modeled exec",
            ));
        }
        self.current = next;
        Ok(())
    }
}

/// Final script ELF T is pinned independently of original exec filename F.
#[derive(Debug)]
pub struct PinnedFinalElf {
    file: File,
    identity: KernelObjectIdentity,
    original_execfn: CString,
}

impl PinnedFinalElf {
    /// The caller has already performed native format/admission validation and
    /// supplies an immutable readable final ELF. No original-F lookup occurs.
    pub fn new(file: File, original_execfn: CString) -> Result<Self, ProcStateError> {
        if !file.metadata()?.is_file() {
            return Err(unsupported("final executable is not a pinned regular ELF"));
        }
        let mut magic = [0; 4];
        file.read_exact_at(&mut magic, 0)?;
        if magic != *b"\x7fELF" {
            return Err(unsupported("final executable T is not an ELF object"));
        }
        let identity = KernelObjectIdentity::of(&file)?;
        Ok(Self {
            file,
            identity,
            original_execfn,
        })
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn identity(&self) -> KernelObjectIdentity {
        self.identity
    }

    pub fn original_execfn(&self) -> &CStr {
        &self.original_execfn
    }
}

/// Full inherited proc/OFD and Hermit state restoration remains inactive.
///
/// [`ClockStateCarrier`] and the separate actual Reverie counter restoration
/// fixture establish the narrow counter transport, not a complete later exec.
/// Its owner still must implement and qualify inherited proc/OFD transport and
/// full Tool-state restoration before activation.
pub fn prepare_exec_continuation() -> Result<(), ProcStateError> {
    Err(unsupported(
        "actual inherited proc/OFD and clock/counter continuation is not activated",
    ))
}
