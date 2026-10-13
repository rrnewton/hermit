/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Recorded raw observations for a proc subject. This is a child of
//! `syscalls::files`, so it can reuse its existing private capture helpers.
//! These facts do not admit a task, prove a complete view, classify an unknown
//! task as unmanaged, or retain the task referenced by an already opened file.

use std::os::unix::ffi::OsStrExt;

use reverie::syscalls::StatxPtr;
use serde::Deserialize;
use serde::Serialize;

use super::*;

const SCRATCH_BYTES: usize = 64 * 1024;
const PATH_BYTES: usize = 512;
const STATX_OFFSET: usize = 512;
const STATFS_OFFSET: usize = 1024;
const LINK_OFFSET: usize = 2048;
const LINK_BYTES: usize = 128;
const PROC_SUPER_MAGIC: i64 = 0x9fa0;

/// Serializable observation of the existing backend refusal seam. A29's
/// UnsupportedRefusal itself is not serializable; no new backend API is added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcIdentityUnsupportedOperation {
    NonLeaderExec,
    /// A future non-exhaustive operation remains a refusal, never authority.
    Unrecognized(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcIdentityCallResult {
    Returned(i64),
    Errno(i32),
    Tool(String),
    Io { errno: Option<i32> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcIdentityBackendRefusal {
    pub operation: ProcIdentityUnsupportedOperation,
    pub errno: i32,
    pub diagnostic: String,
    /// Keep the actual delegate result as well, including recorder failure.
    pub result: ProcIdentityCallResult,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcIdentityGap {
    /// Low-level errno without a captured typed backend refusal. The Error
    /// variant alone does not certify a kernel operation or its failing phase.
    Errno(i32),
    BackendRefusal(ProcIdentityBackendRefusal),
    Tool(String),
    Io {
        errno: Option<i32>,
    },
    Malformed(ProcIdentityParseError),
    MissingStatxMask(u32),
    NotRequested,
    DependencyUnavailable,
    CaptureFact {
        result: Box<Result<i64, ProcIdentityGap>>,
        primary: Box<ProcIdentityGap>,
    },
    Cleanup {
        primary: Box<ProcIdentityGap>,
        cleanup: Box<ProcCleanupObservation>,
    },
    /// The raw drain completed. A private cleanup failure removes authority,
    /// but is neither the guest read's errno nor a replacement for its bytes.
    CleanupAfterSuccess {
        returned_bytes: usize,
        cleanup: Box<ProcCleanupObservation>,
    },
}

impl ProcIdentityGap {
    pub(super) fn into_error(self) -> Error {
        match self {
            Self::Errno(errno) => Errno::new(errno).into(),
            Self::Io { errno: Some(errno) } => Error::Io(std::io::Error::from_raw_os_error(errno)),
            Self::Io { errno: None } => {
                Error::Io(std::io::Error::other("fdinfo acquisition IO failure"))
            }
            Self::Tool(message) => Error::Tool(anyhow::anyhow!(message)),
            other => Error::Tool(anyhow::anyhow!("fdinfo acquisition refusal: {other:?}")),
        }
    }

    pub(crate) fn from_error(error: Error) -> Self {
        match error {
            Error::Errno(errno) => Self::Errno(errno.into_raw()),
            Error::Tool(error) => Self::Tool(error.to_string()),
            Error::Io(error) => Self::Io {
                errno: error.raw_os_error(),
            },
        }
    }

    /// Only a semantic private cleanup errno/refusal may leave completed raw
    /// bytes available. Fact-hook, codec, ordering and Tool failures remain
    /// fatal even when their nested cause contains a low-level IO errno.
    pub(super) fn cleanup_preserves_completed_read(&self) -> bool {
        match self {
            Self::Errno(_) | Self::Io { errno: Some(_) } => true,
            Self::BackendRefusal(refusal) => matches!(
                &refusal.result,
                ProcIdentityCallResult::Returned(_)
                    | ProcIdentityCallResult::Errno(_)
                    | ProcIdentityCallResult::Io { errno: Some(_) }
            ),
            _ => false,
        }
    }
}

impl From<Error> for ProcIdentityGap {
    fn from(error: Error) -> Self {
        Self::from_error(error)
    }
}

impl From<Errno> for ProcIdentityGap {
    fn from(errno: Errno) -> Self {
        Self::Errno(errno.into_raw())
    }
}

impl ProcIdentityGap {
    fn retry_interrupted(&self) -> bool {
        match self {
            Self::Errno(errno) => {
                *errno == Errno::EINTR.into_raw() || *errno == Errno::ERESTARTSYS.into_raw()
            }
            Self::Cleanup { primary, cleanup }
                if matches!(&cleanup.result, ProcIdentityFact::Observed(0)) =>
            {
                primary.retry_interrupted()
            }
            _ => false,
        }
    }
}

/// `refusal` must be taken immediately after the delegate/injection, before
/// another injection replaces it. It is never inferred from an errno value.
fn proc_identity_injection_result(
    result: Result<i64, Error>,
    refusal: Option<reverie::UnsupportedRefusal>,
) -> Result<i64, ProcIdentityGap> {
    if let Some(refusal) = refusal {
        let operation = match refusal.operation {
            reverie::UnsupportedOperation::NonLeaderExec => {
                ProcIdentityUnsupportedOperation::NonLeaderExec
            }
            other => ProcIdentityUnsupportedOperation::Unrecognized(format!("{other:?}")),
        };
        let result = match result {
            Ok(value) => ProcIdentityCallResult::Returned(value),
            Err(Error::Errno(errno)) => ProcIdentityCallResult::Errno(errno.into_raw()),
            Err(Error::Tool(error)) => ProcIdentityCallResult::Tool(error.to_string()),
            Err(Error::Io(error)) => ProcIdentityCallResult::Io {
                errno: error.raw_os_error(),
            },
        };
        return Err(ProcIdentityGap::BackendRefusal(
            ProcIdentityBackendRefusal {
                operation,
                errno: refusal.errno.into_raw(),
                diagnostic: refusal.diagnostic.to_owned(),
                result,
            },
        ));
    }
    result.map_err(ProcIdentityGap::from_error)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcIdentityFact<T> {
    Observed(T),
    Unavailable(ProcIdentityGap),
}

impl<T> ProcIdentityFact<T> {
    fn pending() -> Self {
        Self::Unavailable(ProcIdentityGap::DependencyUnavailable)
    }

    fn from_result<E: Into<ProcIdentityGap>>(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => Self::Observed(value),
            Err(error) => Self::Unavailable(error.into()),
        }
    }

    fn from_parse(result: Result<T, ProcIdentityParseError>) -> Self {
        match result {
            Ok(value) => Self::Observed(value),
            Err(error) => Self::Unavailable(ProcIdentityGap::Malformed(error)),
        }
    }

    fn value(&self) -> Option<&T> {
        match self {
            Self::Observed(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcIdentityParseError {
    Empty,
    NoncanonicalNumber,
    NumberOutOfRange,
    StatFraming,
    StatFieldCount,
    StatState,
    StatNumericField,
    AliasSyntax,
    WrongObjectType,
    ReadlinkTruncated,
    InvalidSyscallReturn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcRawObject {
    pub inode: RawInode,
    pub mode: u32,
    /// None means Linux did not return STATX_MNT_ID; it is not mount ID zero.
    pub mount_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcRawTaskStat {
    pub pid: i32,
    pub ppid: i32,
    pub start_ticks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcRawAliasName {
    Process { pid: i32 },
    Task { pid: i32, tid: i32 },
}

/// Each member has independent acquisition/parse status. In particular a later
/// namespace lookup failure cannot erase a successfully recorded `stat` read.
/// Missing acquisition fact: this sample does not bind an fdinfo OFD to its
/// retained kernel task reference. The owner must authenticate that open-time
/// binding; PID/start_ticks alone must not redirect a held file after PID reuse.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcRawTaskObservation {
    pub directory: ProcIdentityFact<ProcRawObject>,
    pub stat_bytes: ProcIdentityFact<Vec<u8>>,
    pub stat: ProcIdentityFact<ProcRawTaskStat>,
    pub pid_namespace: ProcIdentityFact<ProcRawObject>,
    pub mount_namespace: ProcIdentityFact<ProcRawObject>,
}

impl ProcRawTaskObservation {
    fn unavailable(gap: ProcIdentityGap) -> Self {
        Self {
            directory: ProcIdentityFact::Unavailable(gap.clone()),
            stat_bytes: ProcIdentityFact::Unavailable(gap.clone()),
            stat: ProcIdentityFact::Unavailable(gap.clone()),
            pid_namespace: ProcIdentityFact::Unavailable(gap.clone()),
            mount_namespace: ProcIdentityFact::Unavailable(gap),
        }
    }
}

/// This requests a capture. It is deliberately not a serializable proof of
/// first-launch admission, and the returned parent remains a candidate only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProcIdentityCaptureRequest {
    Caller,
    CallerAndOriginalInitialParent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcTemporaryResource {
    Descriptor(i32),
    ScratchMapping { bytes: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcCleanupObservation {
    pub resource: ProcTemporaryResource,
    pub result: ProcIdentityFact<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcIdentityAcquisition {
    pub injected_pid: ProcIdentityFact<i32>,
    pub injected_tid: ProcIdentityFact<i32>,
    pub injected_ppid: ProcIdentityFact<i32>,
    pub scratch: ProcIdentityFact<()>,
    pub procfs_root: ProcIdentityFact<ProcRawObject>,
    pub procfs_type: ProcIdentityFact<i64>,
    /// The namespace named by task 1 in this mounted proc view, separately from
    /// the caller's ns/pid. Their equality is not assumed.
    pub mounted_pid_namespace: ProcIdentityFact<ProcRawObject>,
    pub process_link_bytes: ProcIdentityFact<Vec<u8>>,
    pub task_link_bytes: ProcIdentityFact<Vec<u8>>,
    pub process_alias: ProcIdentityFact<ProcRawAliasName>,
    pub task_alias: ProcIdentityFact<ProcRawAliasName>,
    pub process: ProcRawTaskObservation,
    pub task: ProcRawTaskObservation,
    pub original_parent_requested: bool,
    pub original_parent_candidate: ProcRawTaskObservation,
    pub cleanup: Vec<ProcCleanupObservation>,
}

impl ProcIdentityAcquisition {
    fn empty(parent: bool) -> Self {
        Self {
            injected_pid: ProcIdentityFact::pending(),
            injected_tid: ProcIdentityFact::pending(),
            injected_ppid: ProcIdentityFact::pending(),
            scratch: ProcIdentityFact::pending(),
            procfs_root: ProcIdentityFact::pending(),
            procfs_type: ProcIdentityFact::pending(),
            mounted_pid_namespace: ProcIdentityFact::pending(),
            process_link_bytes: ProcIdentityFact::pending(),
            task_link_bytes: ProcIdentityFact::pending(),
            process_alias: ProcIdentityFact::pending(),
            task_alias: ProcIdentityFact::pending(),
            process: ProcRawTaskObservation::unavailable(ProcIdentityGap::DependencyUnavailable),
            task: ProcRawTaskObservation::unavailable(ProcIdentityGap::DependencyUnavailable),
            original_parent_requested: parent,
            original_parent_candidate: ProcRawTaskObservation::unavailable(if parent {
                ProcIdentityGap::DependencyUnavailable
            } else {
                ProcIdentityGap::NotRequested
            }),
            cleanup: Vec::new(),
        }
    }

    /// A returned record must pass cleanup before it is usable for authority.
    /// Cleanup failure is internal acquisition failure, never the guest's errno.
    pub(crate) fn cleanup_complete(&self) -> bool {
        self.scratch.value().is_some()
            && self
                .cleanup
                .iter()
                .any(|item| matches!(item.resource, ProcTemporaryResource::ScratchMapping { .. }))
            && self
                .cleanup
                .iter()
                .all(|item| item.result == ProcIdentityFact::Observed(0))
    }

    /// Positive parent relation observed in one same-namespace proc view. This
    /// does not prove original first-launch status or admit an unmanaged target.
    /// The owner must supply that lifecycle fact and bind the opened proc task.
    pub(crate) fn observed_initial_parent_relation(
        &self,
    ) -> Option<(ProcRawTaskStat, ProcRawTaskStat)> {
        if !self.original_parent_requested
            || !self.cleanup_complete()
            || self.procfs_type.value() != Some(&PROC_SUPER_MAGIC)
            || self.procfs_root.value()?.mode & libc::S_IFMT != libc::S_IFDIR
        {
            return None;
        }
        let pid = *self.injected_pid.value()?;
        let tid = *self.injected_tid.value()?;
        let ppid = *self.injected_ppid.value()?;
        if pid <= 0 || tid != pid || ppid <= 0 {
            return None;
        }
        let process = *self.process.stat.value()?;
        let task = *self.task.stat.value()?;
        let parent = *self.original_parent_candidate.stat.value()?;
        if self.process_alias.value() != Some(&ProcRawAliasName::Process { pid })
            || self.task_alias.value() != Some(&ProcRawAliasName::Task { pid, tid })
            || process.pid != pid
            || process.ppid != ppid
            || task != process
            || parent.pid != ppid
        {
            return None;
        }
        // Different Process and Task directory inodes are valid aliases. Do
        // not compare them for equality; require that both were actual dirs.
        for observation in [&self.process, &self.task, &self.original_parent_candidate] {
            if observation.directory.value()?.mode & libc::S_IFMT != libc::S_IFDIR {
                return None;
            }
        }
        let mounted = self.mounted_pid_namespace.value()?.inode;
        for namespace in [
            &self.mounted_pid_namespace,
            &self.process.pid_namespace,
            &self.task.pid_namespace,
            &self.original_parent_candidate.pid_namespace,
            &self.process.mount_namespace,
            &self.task.mount_namespace,
            &self.original_parent_candidate.mount_namespace,
        ] {
            if namespace.value()?.mode & libc::S_IFMT != libc::S_IFREG {
                return None;
            }
        }
        if self.process.pid_namespace.value()?.inode != mounted
            || self.task.pid_namespace.value()?.inode != mounted
            || self.original_parent_candidate.pid_namespace.value()?.inode != mounted
            || self.process.mount_namespace.value()?.inode
                != self.task.mount_namespace.value()?.inode
        {
            return None;
        }
        Some((process, parent))
    }
}

fn canonical_u64(token: &[u8]) -> Result<u64, ProcIdentityParseError> {
    if token.is_empty() {
        return Err(ProcIdentityParseError::Empty);
    }
    if !token.iter().all(u8::is_ascii_digit) || (token.len() > 1 && token[0] == b'0') {
        return Err(ProcIdentityParseError::NoncanonicalNumber);
    }
    token.iter().try_fold(0u64, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u64::from(digit - b'0')))
            .ok_or(ProcIdentityParseError::NumberOutOfRange)
    })
}

fn canonical_pid(token: &[u8], zero: bool) -> Result<i32, ProcIdentityParseError> {
    let value = canonical_u64(token)?;
    let pid = i32::try_from(value).map_err(|_| ProcIdentityParseError::NumberOutOfRange)?;
    if !zero && pid == 0 {
        return Err(ProcIdentityParseError::NumberOutOfRange);
    }
    Ok(pid)
}

/// `comm` is opaque bytes and may contain spaces, parentheses or non-UTF8.
/// Use the final ") " separator, just as the existing procfs parser does.
pub(crate) fn parse_raw_proc_stat(bytes: &[u8]) -> Result<ProcRawTaskStat, ProcIdentityParseError> {
    let comm_start = bytes
        .windows(2)
        .position(|pair| pair == b" (")
        .ok_or(ProcIdentityParseError::StatFraming)?;
    let comm_end = bytes
        .windows(2)
        .rposition(|pair| pair == b") ")
        .filter(|end| *end > comm_start)
        .ok_or(ProcIdentityParseError::StatFraming)?;
    // Ambiguous multi-line framing is not authority. The original bytes remain
    // retained even if a legitimate task uses a newline in comm.
    if bytes[comm_start..comm_end].contains(&b'\n') || bytes[comm_start..comm_end].contains(&0) {
        return Err(ProcIdentityParseError::StatFraming);
    }
    let pid = canonical_pid(&bytes[..comm_start], false)?;
    let fields = bytes[comm_end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() < 50 {
        return Err(ProcIdentityParseError::StatFieldCount);
    }
    if fields[0].len() != 1 || !b"RSDZTWtXxKPI".contains(&fields[0][0]) {
        return Err(ProcIdentityParseError::StatState);
    }
    // Signed fields occur in Linux stat. Validate framing without pretending
    // all fields are u64; identities below remain strictly unsigned/ranged.
    if fields[1..].iter().any(|field| {
        let digits = field.strip_prefix(b"-").unwrap_or(field);
        digits.is_empty() || !digits.iter().all(u8::is_ascii_digit)
    }) {
        return Err(ProcIdentityParseError::StatNumericField);
    }
    Ok(ProcRawTaskStat {
        pid,
        ppid: canonical_pid(fields[1], true)?,
        start_ticks: canonical_u64(fields[19])?,
    })
}

pub(crate) fn parse_process_alias(
    bytes: &[u8],
) -> Result<ProcRawAliasName, ProcIdentityParseError> {
    Ok(ProcRawAliasName::Process {
        pid: canonical_pid(bytes, false)?,
    })
}

pub(crate) fn parse_task_alias(bytes: &[u8]) -> Result<ProcRawAliasName, ProcIdentityParseError> {
    let parts = bytes.split(|byte| *byte == b'/').collect::<Vec<_>>();
    if parts.len() != 3 || parts[1] != b"task" {
        return Err(ProcIdentityParseError::AliasSyntax);
    }
    Ok(ProcRawAliasName::Task {
        pid: canonical_pid(parts[0], false)?,
        tid: canonical_pid(parts[2], false)?,
    })
}

struct ProcCaptureScratch {
    address: usize,
    held: Vec<i32>,
    cleanup: Vec<ProcCleanupObservation>,
}

impl ProcCaptureScratch {
    fn pointer(&self, offset: usize) -> Result<AddrMut<'static, u8>, Error> {
        self.address
            .checked_add(offset)
            .and_then(AddrMut::from_raw)
            .ok_or_else(|| Errno::EFAULT.into())
    }

    fn path<G: Guest<T>, T: reverie::Tool>(
        &self,
        guest: &mut G,
        path: &str,
    ) -> Result<PathPtr<'static>, Error> {
        if path.len() >= PATH_BYTES || path.as_bytes().contains(&0) {
            return Err(Errno::EINVAL.into());
        }
        let address = self.pointer(0)?;
        let mut bytes = path.as_bytes().to_vec();
        bytes.push(0);
        guest.memory().write_exact(address, &bytes)?;
        PathPtr::from_ptr(address.as_raw() as *const libc::c_char)
            .ok_or_else(|| Errno::EFAULT.into())
    }
}

/// Private acquisition requests retain semantic inputs, not scratch addresses.
/// Descriptor values returned by replay are tokens for these requests only:
/// no replayed helper opens, reads, stats or closes a live descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum ProcIdentityOperation {
    Getpid,
    Gettid,
    Getppid,
    Openat {
        dirfd: i32,
        path: Option<Vec<u8>>,
        flags: i32,
        mode: Option<u32>,
    },
    Close {
        fd: i32,
    },
    Statx {
        dirfd: i32,
        path: Option<Vec<u8>>,
        flags: i32,
        mask: u32,
    },
    Fstatfs {
        fd: i32,
    },
    Readlinkat {
        dirfd: i32,
        path: Option<Vec<u8>>,
        len: u64,
    },
    Read {
        fd: i32,
        len: u64,
    },
    /// Append variants so all existing private-operation discriminants remain.
    DuplicateSource {
        fd: i32,
    },
    RewindSource {
        fd: i32,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ProcIdentityOperationFact {
    version: u8,
    operation: ProcIdentityOperation,
    result: Result<i64, ProcIdentityGap>,
    /// Keep a successful syscall result even when copying its output failed.
    /// A failure/refusal has no valid successful output bytes.
    output: Result<Vec<u8>, ProcIdentityGap>,
}

impl ProcIdentityOperation {
    fn output_bytes(&self, returned: i64) -> Result<usize, ProcIdentityGap> {
        let malformed = || ProcIdentityGap::Tool("invalid private fdinfo result".to_owned());
        match self {
            Self::Read { len, .. } | Self::Readlinkat { len, .. } => {
                let count = usize::try_from(returned).map_err(|_| malformed())?;
                if *len > SCRATCH_BYTES as u64 || count as u64 > *len {
                    return Err(malformed());
                }
                Ok(count)
            }
            Self::Statx { .. } if returned == 0 => Ok(std::mem::size_of::<libc::statx>()),
            Self::Fstatfs { .. } if returned == 0 => Ok(std::mem::size_of::<libc::statfs>()),
            Self::Close { .. } | Self::RewindSource { .. } if returned == 0 => Ok(0),
            Self::Getpid | Self::Gettid if (1..=i64::from(i32::MAX)).contains(&returned) => Ok(0),
            Self::Getppid | Self::Openat { .. } | Self::DuplicateSource { .. }
                if (0..=i64::from(i32::MAX)).contains(&returned) =>
            {
                Ok(0)
            }
            _ => Err(malformed()),
        }
    }
}

fn proc_identity_returned(result: &Result<i64, ProcIdentityGap>) -> Option<i64> {
    match result {
        Ok(returned) => Some(*returned),
        Err(ProcIdentityGap::BackendRefusal(refusal)) => match &refusal.result {
            ProcIdentityCallResult::Returned(returned) => Some(*returned),
            _ => None,
        },
        _ => None,
    }
}

impl ProcIdentityOperationFact {
    fn validate(&self, expected: &ProcIdentityOperation) -> Result<(), ProcIdentityGap> {
        if self.version != 1 || self.operation != *expected {
            return Err(ProcIdentityGap::Tool(
                "private fdinfo operation/order mismatch".to_owned(),
            ));
        }
        // A refusal preserves the actual call class, including a successful
        // open which still owns a cleanup token. Validate that return too.
        if let Some(returned) = proc_identity_returned(&self.result) {
            let _ = self.operation.output_bytes(returned)?;
        }
        match (&self.result, &self.output) {
            (Ok(returned), Ok(bytes)) => {
                if self.operation.output_bytes(*returned)? != bytes.len() {
                    return Err(ProcIdentityGap::Tool(
                        "private fdinfo output length mismatch".to_owned(),
                    ));
                }
            }
            (Ok(returned), Err(_)) => {
                let _ = self.operation.output_bytes(*returned)?;
            }
            (Err(_), Ok(bytes)) if bytes.is_empty() => {}
            (Err(_), _) => {
                return Err(ProcIdentityGap::Tool(
                    "failed private fdinfo operation has successful output".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// This address belongs exclusively to the current execution's owned scratch.
/// It is never serialized, and cannot be obtained from the recorded fact.
#[derive(Clone, Copy)]
struct ProcIdentityOutputBuffer {
    address: Option<usize>,
}

fn proc_identity_operation<G: Guest<T>, T: reverie::Tool>(
    guest: &G,
    call: Syscall,
) -> Result<(ProcIdentityOperation, ProcIdentityOutputBuffer), ProcIdentityGap> {
    let path = |pointer: Option<PathPtr<'_>>| {
        pointer
            .map(|pointer| {
                pointer
                    .read(&guest.memory())
                    .map(|path| path.as_os_str().as_bytes().to_vec())
                    .map_err(ProcIdentityGap::from)
            })
            .transpose()
    };
    let (operation, address) = match call {
        Syscall::Getpid(_) => (ProcIdentityOperation::Getpid, None),
        Syscall::Gettid(_) => (ProcIdentityOperation::Gettid, None),
        Syscall::Getppid(_) => (ProcIdentityOperation::Getppid, None),
        Syscall::Openat(call) => (
            ProcIdentityOperation::Openat {
                dirfd: call.dirfd(),
                path: path(call.path())?,
                flags: call.flags().bits(),
                mode: call.mode().map(|mode| mode.bits()),
            },
            None,
        ),
        Syscall::Close(call) => (ProcIdentityOperation::Close { fd: call.fd() }, None),
        Syscall::Fcntl(call) if matches!(call.cmd(), F_DUPFD_CLOEXEC(0)) => (
            ProcIdentityOperation::DuplicateSource { fd: call.fd() },
            None,
        ),
        Syscall::Lseek(call) if call.offset() == 0 && call.whence() == Whence::SEEK_SET => {
            (ProcIdentityOperation::RewindSource { fd: call.fd() }, None)
        }
        Syscall::Statx(call) => (
            ProcIdentityOperation::Statx {
                dirfd: call.dirfd(),
                path: path(call.path())?,
                flags: call.flags().bits(),
                mask: call.mask().bits(),
            },
            call.statx().map(|pointer| pointer.0.as_raw()),
        ),
        Syscall::Fstatfs(call) => (
            ProcIdentityOperation::Fstatfs { fd: call.fd() },
            call.buf().map(|pointer| pointer.as_raw()),
        ),
        Syscall::Readlinkat(call) => (
            ProcIdentityOperation::Readlinkat {
                dirfd: call.dirfd(),
                path: path(call.path())?,
                len: call.buf_len() as u64,
            },
            call.buf().map(|pointer| pointer.as_raw()),
        ),
        Syscall::Read(call) => (
            ProcIdentityOperation::Read {
                fd: call.fd(),
                len: call.len() as u64,
            },
            call.buf().map(|pointer| pointer.as_raw()),
        ),
        _ => {
            return Err(ProcIdentityGap::Tool(
                "unsupported private fdinfo acquisition operation".to_owned(),
            ));
        }
    };
    Ok((operation, ProcIdentityOutputBuffer { address }))
}

impl ProcIdentityOutputBuffer {
    fn capture<G: Guest<T>, T: reverie::Tool>(
        self,
        guest: &G,
        operation: &ProcIdentityOperation,
        result: &Result<i64, ProcIdentityGap>,
    ) -> Result<Vec<u8>, ProcIdentityGap> {
        let count = match result {
            Ok(returned) => operation.output_bytes(*returned)?,
            Err(_) => return Ok(Vec::new()),
        };
        let mut bytes = vec![0; count];
        if count != 0 {
            let pointer = self
                .address
                .and_then(AddrMut::<u8>::from_raw)
                .ok_or(Errno::EFAULT)?;
            read_guest_exact(&guest.memory(), pointer, &mut bytes)?;
        }
        Ok(bytes)
    }

    fn finish<G: Guest<T>, T: reverie::Tool>(
        self,
        guest: &G,
        fact: ProcIdentityOperationFact,
        restore: bool,
    ) -> Result<i64, ProcIdentityGap> {
        let returned = fact.result?;
        let bytes = fact.output?;
        if restore && !bytes.is_empty() {
            let pointer = self
                .address
                .and_then(AddrMut::<u8>::from_raw)
                .ok_or(Errno::EFAULT)?;
            guest.memory().write_exact(pointer, &bytes)?;
        }
        Ok(returned)
    }
}

fn hold_proc_identity_descriptor(
    held: &mut Vec<i32>,
    returned: i64,
) -> Result<(), ProcIdentityGap> {
    let fd = i32::try_from(returned)
        .ok()
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| {
            ProcIdentityGap::Tool("private proc open returned an invalid descriptor".to_owned())
        })?;
    held.push(fd);
    Ok(())
}

fn proc_identity_scratch_failure(
    actual: Result<i64, ProcIdentityGap>,
    recorded: ProcIdentityGap,
) -> ProcIdentityGap {
    match actual {
        Err(actual) if actual != recorded => ProcIdentityGap::CaptureFact {
            result: Box::new(Err(actual)),
            primary: Box::new(recorded),
        },
        _ => recorded,
    }
}

pub(super) struct HeldFdinfoSnapshot {
    pub(super) contents: Vec<u8>,
    pub(super) cleanup: Option<ProcIdentityGap>,
}

fn finish_held_fdinfo_snapshot(
    result: Result<Vec<u8>, ProcIdentityGap>,
    cleanup: Result<i64, ProcIdentityGap>,
) -> Result<HeldFdinfoSnapshot, ProcIdentityGap> {
    let cleanup_observation = |error| ProcCleanupObservation {
        resource: ProcTemporaryResource::ScratchMapping {
            bytes: SCRATCH_BYTES as u64,
        },
        result: ProcIdentityFact::Unavailable(error),
    };
    match (result, cleanup) {
        (Ok(contents), Ok(_)) => Ok(HeldFdinfoSnapshot {
            contents,
            cleanup: None,
        }),
        (Ok(contents), Err(error)) if error.cleanup_preserves_completed_read() => {
            let returned_bytes = contents.len();
            Ok(HeldFdinfoSnapshot {
                contents,
                cleanup: Some(ProcIdentityGap::CleanupAfterSuccess {
                    returned_bytes,
                    cleanup: Box::new(cleanup_observation(error)),
                }),
            })
        }
        (Ok(contents), Err(error)) => Err(ProcIdentityGap::CleanupAfterSuccess {
            returned_bytes: contents.len(),
            cleanup: Box::new(cleanup_observation(error)),
        }),
        (Err(primary), Ok(_)) => Err(primary),
        (Err(primary), Err(cleanup)) => Err(ProcIdentityGap::Cleanup {
            primary: Box::new(primary),
            cleanup: Box::new(cleanup_observation(cleanup)),
        }),
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    pub(super) async fn proc_identity_recorded<G: Guest<Self>, S: Into<Syscall>>(
        &self,
        guest: &mut G,
        call: S,
    ) -> Result<i64, ProcIdentityGap> {
        self.proc_identity_recorded_operation(guest, call.into(), None)
            .await
    }

    async fn proc_identity_recorded_operation<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        open_owner: Option<&mut Vec<i32>>,
    ) -> Result<i64, ProcIdentityGap> {
        let (operation, output) = proc_identity_operation(guest, call).inspect_err(|error| {
            tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                mm = ?guest.thread_state().mm_id, kind = 1, phase = "describe", ?error,
                "private fdinfo helper description failed");
        })?;
        tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
            mm = ?guest.thread_state().mm_id, kind = 1, ?operation,
            "private fdinfo helper begin");
        if matches!(
            operation,
            ProcIdentityOperation::Openat { .. } | ProcIdentityOperation::DuplicateSource { .. }
        ) && open_owner.is_none()
        {
            return Err(ProcIdentityGap::Tool(
                "private proc open has no resource owner".to_owned(),
            ));
        }
        if let Some(recorded) = self
            .fdinfo_replay_capture_fact::<_, ProcIdentityOperationFact>(guest, 1)
            .await
            .map_err(|error| ProcIdentityGap::CaptureFact {
                // The hook failed before any physical delegate was requested.
                result: Box::new(Err(ProcIdentityGap::NotRequested)),
                primary: Box::new(ProcIdentityGap::from_error(error)),
            })?
        {
            recorded.validate(&operation).inspect_err(|error| {
                tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                    mm = ?guest.thread_state().mm_id, kind = 1, mode = "replayed",
                    phase = "validate", ?operation, ?error,
                    "private fdinfo helper fact validation failed");
            })?;
            tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                mm = ?guest.thread_state().mm_id, kind = 1, mode = "replayed", ?operation,
                result = ?recorded.result, output = ?recorded.output.as_ref().map(Vec::len),
                "private fdinfo helper recorded result");
            if let (Some(held), Some(fd)) = (open_owner, proc_identity_returned(&recorded.result)) {
                // Register a nonphysical cleanup token even when the actual
                // successful open was masked by a refusal. Its Close consumes
                // the recorded fact and never injects this number into Linux.
                hold_proc_identity_descriptor(held, fd)?;
            }
            // Restore bytes only to the current call's scratch, preserving the
            // refusal after registering any owned cleanup token.
            return output.finish(guest, recorded, true).inspect_err(|error| {
                tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                    mm = ?guest.thread_state().mm_id, kind = 1, mode = "replayed",
                    phase = "finish", ?operation, ?error,
                    "private fdinfo helper completion returned error");
            });
        }
        // Private physical acquisition is outside ordinary guest syscall and
        // debug-argument recording. Preserve its full result in the typed fact.
        let _ = guest.take_unsupported_refusal();
        let result = guest.inject(call).await.map_err(Error::from);
        let refusal = guest.take_unsupported_refusal();
        tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
            mm = ?guest.thread_state().mm_id, kind = 1, mode = "physical", ?operation,
            actual_result = ?result, immediate_refusal = ?refusal,
            "private fdinfo helper physical return");
        if let (Some(held), Ok(fd)) = (open_owner, &result) {
            // Register the actual physical descriptor before fact encoding,
            // writing, decoding or validation can fail, including a refusal
            // marker paired with a physically successful operation.
            hold_proc_identity_descriptor(held, *fd)?;
        }
        let result = proc_identity_injection_result(result, refusal);
        let bytes = output.capture(guest, &operation, &result);
        tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
            mm = ?guest.thread_state().mm_id, kind = 1, mode = "physical", ?operation,
            result = ?result, output = ?bytes.as_ref().map(Vec::len),
            "private fdinfo helper output copy result");
        let observed = ProcIdentityOperationFact {
            version: 1,
            operation: operation.clone(),
            result,
            output: bytes,
        };
        let recorded: ProcIdentityOperationFact = self
            .fdinfo_capture_fact(guest, 1, &observed)
            .await
            .map_err(|error| ProcIdentityGap::CaptureFact {
                result: Box::new(observed.result.clone()),
                primary: Box::new(ProcIdentityGap::from_error(error)),
            })?;
        recorded.validate(&operation).map_err(|error| {
            tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                    mm = ?guest.thread_state().mm_id, kind = 1, mode = "physical",
                    phase = "validate", ?operation, ?error,
                    "private fdinfo helper fact validation failed");
            ProcIdentityGap::CaptureFact {
                result: Box::new(observed.result.clone()),
                primary: Box::new(error),
            }
        })?;
        output.finish(guest, recorded, false).inspect_err(|error| {
            tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                mm = ?guest.thread_state().mm_id, kind = 1, mode = "physical",
                phase = "finish", ?operation, ?error,
                "private fdinfo helper completion returned error");
        })
    }

    async fn rollback_proc_identity_scratch<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
        physical_returned: Option<i64>,
        primary: ProcIdentityGap,
    ) -> ProcIdentityGap {
        let (Some(address), Syscall::Mmap(mapping)) = (physical_returned, call) else {
            return primary;
        };
        // Only this execution's successful physical Mmap result is eligible.
        // No address is taken from the recorded outcome or a helper FD token.
        let cleanup = match usize::try_from(address).ok().and_then(Addr::<u8>::from_raw) {
            None => ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                ProcIdentityParseError::InvalidSyscallReturn,
            )),
            Some(address) => {
                let result = guest
                    .inject(
                        syscalls::Munmap::new()
                            .with_addr(Some(address.cast()))
                            .with_len(mapping.len()),
                    )
                    .await
                    .map_err(Error::from);
                let refusal = guest.take_unsupported_refusal();
                ProcIdentityFact::from_result(proc_identity_injection_result(result, refusal))
            }
        };
        tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
            mm = ?guest.thread_state().mm_id, kind = 2, physical_owned_mapping = true,
            ?primary, cleanup = ?cleanup,
            "private fdinfo scratch rollback result");
        ProcIdentityGap::Cleanup {
            primary: Box::new(primary),
            cleanup: Box::new(ProcCleanupObservation {
                resource: ProcTemporaryResource::ScratchMapping {
                    bytes: mapping.len() as u64,
                },
                result: cleanup,
            }),
        }
    }

    pub(super) async fn proc_identity_inject_with_retry<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: Syscall,
    ) -> Result<i64, ProcIdentityGap> {
        loop {
            let result = guest.inject(call).await.map_err(Error::from);
            let refusal = guest.take_unsupported_refusal();
            tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                mm = ?guest.thread_state().mm_id, kind = 2, physical_returned = result.is_ok(),
                physical_error = ?result.as_ref().err(), immediate_refusal = ?refusal,
                "private fdinfo scratch physical return");
            let physical_returned = result.as_ref().ok().copied();
            let actual = proc_identity_injection_result(result, refusal);
            // Scratch addresses belong to this execution. Only the outcome
            // class/refusal is historical; never map a replayed host address.
            let observed = actual.as_ref().map(|_| ()).map_err(Clone::clone);
            let recorded: Result<(), ProcIdentityGap> =
                match self.fdinfo_capture_fact(guest, 2, &observed).await {
                    Ok(recorded) => recorded,
                    Err(error) => {
                        let primary = ProcIdentityGap::CaptureFact {
                            result: Box::new(actual),
                            primary: Box::new(ProcIdentityGap::from_error(error)),
                        };
                        return Err(self
                            .rollback_proc_identity_scratch(guest, call, physical_returned, primary)
                            .await);
                    }
                };
            let result = match (actual, recorded) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(actual), Ok(())) => {
                    let primary = ProcIdentityGap::CaptureFact {
                        result: Box::new(Err(actual)),
                        primary: Box::new(ProcIdentityGap::Tool(
                            "fdinfo scratch acquisition failed during replay".to_owned(),
                        )),
                    };
                    Err(self
                        .rollback_proc_identity_scratch(guest, call, physical_returned, primary)
                        .await)
                }
                (actual, Err(gap)) => {
                    // Historical interruption cannot erase a current refusal
                    // or another changed failure class and make it retryable.
                    let primary = proc_identity_scratch_failure(actual, gap);
                    Err(self
                        .rollback_proc_identity_scratch(guest, call, physical_returned, primary)
                        .await)
                }
            };
            tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
                mm = ?guest.thread_state().mm_id, kind = 2,
                result_class = ?result.as_ref().map(|_| ()),
                "private fdinfo scratch selected outcome");
            match result {
                // The pre-existing mapping helper retries these interruptions.
                // Consume the marker before a retry; never retry a refusal.
                Err(gap) if gap.retry_interrupted() => continue,
                result => return result,
            }
        }
    }

    /// Select the actual source OFD once. A physical success is registered in
    /// `held` before any fact-hook failure; replay registers only a token.
    pub(super) async fn hold_fdinfo_source<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        held: &mut Vec<i32>,
    ) -> Result<i32, ProcIdentityGap> {
        let returned = self
            .proc_identity_recorded_operation(
                guest,
                syscalls::Fcntl::new()
                    .with_fd(fd)
                    .with_cmd(F_DUPFD_CLOEXEC(0))
                    .into(),
                Some(held),
            )
            .await?;
        i32::try_from(returned)
            .map_err(|_| ProcIdentityGap::Malformed(ProcIdentityParseError::InvalidSyscallReturn))
    }

    /// Close each owned hold once, including a success masked by a refusal or
    /// fact failure. Do not retry Close on EINTR or inject replay token numbers.
    pub(super) async fn close_fdinfo_source<G: Guest<Self>>(
        &self,
        guest: &mut G,
        held: &mut Vec<i32>,
    ) -> Result<(), ProcIdentityGap> {
        let mut failure = None;
        while let Some(fd) = held.pop() {
            if let Err(error) = self
                .proc_identity_recorded(guest, syscalls::Close::new().with_fd(fd))
                .await
            {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Take the initial fdinfo bytes from the held source, never by looking up
    /// the guest's slot again. Scratch belongs to this execution; each semantic
    /// read and rewind remains in the existing private fact stream.
    pub(super) async fn snapshot_held_fdinfo<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
    ) -> Result<HeldFdinfoSnapshot, ProcIdentityGap> {
        match self
            .proc_identity_recorded(
                guest,
                syscalls::Lseek::new()
                    .with_fd(fd)
                    .with_offset(0)
                    .with_whence(Whence::SEEK_SET),
            )
            .await
        {
            Ok(_) => {}
            Err(ProcIdentityGap::Errno(errno)) if errno == libc::ESPIPE => {}
            Err(error) => return Err(error),
        }
        let mapped = self
            .proc_identity_inject_with_retry(
                guest,
                Syscall::Mmap(
                    syscalls::Mmap::new()
                        .with_addr(None)
                        .with_len(SCRATCH_BYTES)
                        .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                        .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                        .with_fd(-1)
                        .with_offset(0),
                ),
            )
            .await?;
        let address = usize::try_from(mapped).map_err(|_| {
            ProcIdentityGap::Malformed(ProcIdentityParseError::InvalidSyscallReturn)
        })?;
        let result = match AddrMut::<u8>::from_raw(address) {
            Some(buffer) => self.proc_identity_drain(guest, fd, buffer).await,
            None => Err(ProcIdentityGap::Errno(libc::EFAULT)),
        };
        let cleanup = self
            .proc_identity_inject_with_retry(
                guest,
                Syscall::Munmap(
                    syscalls::Munmap::new()
                        .with_addr(Addr::<libc::c_void>::from_raw(address))
                        .with_len(SCRATCH_BYTES),
                ),
            )
            .await;
        finish_held_fdinfo_snapshot(result, cleanup)
    }

    async fn proc_identity_open<G: Guest<Self>>(
        &self,
        guest: &mut G,
        scratch: &mut ProcCaptureScratch,
        dirfd: i32,
        path: &str,
        flags: OFlag,
    ) -> Result<i32, ProcIdentityGap> {
        let path = scratch.path(guest, path)?;
        let fd = self
            .proc_identity_recorded_operation(
                guest,
                syscalls::Openat::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(path))
                    .with_flags(flags)
                    .into(),
                Some(&mut scratch.held),
            )
            .await?;
        let fd = i32::try_from(fd).map_err(|_| {
            Error::Tool(anyhow::anyhow!(
                "raw proc acquisition open returned an invalid descriptor"
            ))
        })?;
        if fd < 0 {
            return Err(Error::Tool(anyhow::anyhow!(
                "raw proc acquisition open returned a negative descriptor"
            ))
            .into());
        }
        Ok(fd)
    }

    async fn proc_identity_close<G: Guest<Self>>(
        &self,
        guest: &mut G,
        scratch: &mut ProcCaptureScratch,
        fd: i32,
    ) {
        // Linux close is not retried: a retry after EINTR could close a reused N.
        if let Some(index) = scratch.held.iter().position(|held| *held == fd) {
            scratch.held.remove(index);
            let result = ProcIdentityFact::from_result(
                self.proc_identity_recorded(guest, syscalls::Close::new().with_fd(fd))
                    .await,
            );
            scratch.cleanup.push(ProcCleanupObservation {
                resource: ProcTemporaryResource::Descriptor(fd),
                result,
            });
        }
    }

    async fn proc_identity_object<G: Guest<Self>>(
        &self,
        guest: &mut G,
        scratch: &ProcCaptureScratch,
        dirfd: i32,
        path: &str,
    ) -> ProcIdentityFact<ProcRawObject> {
        // The exact raw Statx/RR sequence from CLOSED capture_fdinfo_statx,
        // using this helper's one owned mapping. This exposes its cleanup
        // separately instead of copying the old wrapper's merged error policy.
        let captured: Result<reverie::syscalls::StatxBuf, ProcIdentityGap> = async {
            let pathptr = scratch.path(guest, path)?;
            let statptr = StatxPtr(scratch.pointer(STATX_OFFSET)?.cast());
            guest
                .memory()
                .write_exact(statptr.0.cast(), &[0; std::mem::size_of::<libc::statx>()])?;
            self.proc_identity_recorded(
                guest,
                syscalls::Statx::new()
                    .with_dirfd(dirfd)
                    .with_path(Some(pathptr))
                    .with_flags(if path.is_empty() {
                        AtFlags::AT_EMPTY_PATH
                    } else {
                        AtFlags::empty()
                    })
                    .with_mask(StatxMask::from_bits_retain(
                        libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
                    ))
                    .with_statx(Some(statptr)),
            )
            .await?;
            Ok(reverie::syscalls::StatxBuf::from(
                statptr.read(&guest.memory())?,
            ))
        }
        .await;
        match captured {
            Err(error) => ProcIdentityFact::from_result(Err(error)),
            Ok(stat) => {
                let raw = libc::statx::from(stat);
                let needed = libc::STATX_TYPE | libc::STATX_INO;
                if raw.stx_mask & needed != needed {
                    return ProcIdentityFact::Unavailable(ProcIdentityGap::MissingStatxMask(
                        needed & !raw.stx_mask,
                    ));
                }
                ProcIdentityFact::Observed(ProcRawObject {
                    inode: RawInode::new(
                        libc::makedev(raw.stx_dev_major, raw.stx_dev_minor),
                        raw.stx_ino,
                    ),
                    mode: u32::from(raw.stx_mode),
                    mount_id: (raw.stx_mask & libc::STATX_MNT_ID != 0).then_some(raw.stx_mnt_id),
                })
            }
        }
    }

    async fn proc_identity_link<G: Guest<Self>>(
        &self,
        guest: &mut G,
        scratch: &ProcCaptureScratch,
        root: i32,
        name: &str,
    ) -> ProcIdentityFact<Vec<u8>> {
        let result: Result<ProcIdentityFact<Vec<u8>>, ProcIdentityGap> = async {
            let path = scratch.path(guest, name)?;
            let buffer = scratch.pointer(LINK_OFFSET)?;
            let len = self
                .proc_identity_recorded(
                    guest,
                    syscalls::Readlinkat::new()
                        .with_dirfd(root)
                        .with_path(Some(path))
                        .with_buf(Some(buffer.cast()))
                        .with_buf_len(LINK_BYTES),
                )
                .await?;
            let len = usize::try_from(len).map_err(|_| Errno::EOVERFLOW)?;
            if len >= LINK_BYTES {
                return Ok(ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::ReadlinkTruncated,
                )));
            }
            let mut bytes = vec![0; len];
            read_guest_exact(&guest.memory(), buffer, &mut bytes)?;
            Ok(ProcIdentityFact::Observed(bytes))
        }
        .await;
        match result {
            Ok(value) => value,
            Err(error) => ProcIdentityFact::from_result(Err(error)),
        }
    }

    /// The existing drain_procfs EOF/size-limit algorithm with private typed
    /// acquisition facts. Replay restores each chunk without a physical read.
    async fn proc_identity_drain<G: Guest<Self>>(
        &self,
        guest: &mut G,
        fd: i32,
        buffer: AddrMut<'_, u8>,
    ) -> Result<Vec<u8>, ProcIdentityGap> {
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
        let call = syscalls::Read::new()
            .with_fd(fd)
            .with_buf(Some(buffer))
            .with_len(SCRATCH_BYTES);
        let mut contents = Vec::new();
        loop {
            let count = self.proc_identity_recorded(guest, call).await?;
            let count = usize::try_from(count).map_err(|_| {
                Error::Tool(anyhow::anyhow!(
                    "raw proc acquisition read returned an invalid byte count"
                ))
            })?;
            if count == 0 {
                return Ok(contents);
            }
            if count > SCRATCH_BYTES {
                return Err(Error::Tool(anyhow::anyhow!(
                    "raw proc acquisition read exceeded its buffer"
                ))
                .into());
            }
            if contents.len() + count > MAX_SNAPSHOT_BYTES {
                return Err(Errno::EFBIG.into());
            }
            let start = contents.len();
            contents.resize(start + count, 0);
            read_guest_exact(&guest.memory(), buffer, &mut contents[start..])?;
        }
    }

    async fn proc_identity_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        scratch: &mut ProcCaptureScratch,
        root: i32,
        name: &str,
    ) -> ProcRawTaskObservation {
        let directory = match self
            .proc_identity_open(
                guest,
                scratch,
                root,
                name,
                OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            )
            .await
        {
            Ok(fd) => fd,
            Err(error) => {
                return ProcRawTaskObservation::unavailable(error);
            }
        };
        // AT_EMPTY_PATH binds metadata to the held directory, rather than a
        // second PID lookup that might have been redirected by PID reuse.
        let object = self
            .proc_identity_object(guest, scratch, directory, "")
            .await;
        let stat_bytes = match self
            .proc_identity_open(
                guest,
                scratch,
                directory,
                "stat",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            )
            .await
        {
            Err(error) => ProcIdentityFact::from_result(Err(error)),
            Ok(fd) => {
                let result = match scratch.pointer(0) {
                    Ok(buffer) => self.proc_identity_drain(guest, fd, buffer).await,
                    Err(error) => Err(error.into()),
                };
                self.proc_identity_close(guest, scratch, fd).await;
                ProcIdentityFact::from_result(result)
            }
        };
        let stat = match &stat_bytes {
            ProcIdentityFact::Observed(bytes) => {
                ProcIdentityFact::from_parse(parse_raw_proc_stat(bytes))
            }
            ProcIdentityFact::Unavailable(gap) => ProcIdentityFact::Unavailable(gap.clone()),
        };
        let pid_namespace = self
            .proc_identity_object(guest, scratch, directory, "ns/pid")
            .await;
        let mount_namespace = self
            .proc_identity_object(guest, scratch, directory, "ns/mnt")
            .await;
        self.proc_identity_close(guest, scratch, directory).await;
        ProcRawTaskObservation {
            directory: object,
            stat_bytes,
            stat,
            pid_namespace,
            mount_namespace,
        }
    }

    /// Call at the existing serialized lifecycle/capture seam, before publishing
    /// facts. No RPC, turn or guest-visible fdinfo error is introduced here.
    /// Private operations use complete typed facts, never ordinary guest
    /// syscall argument recording. Scratch mmap/munmap remain execution-owned.
    pub(crate) async fn capture_fdinfo_proc_identity<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: ProcIdentityCaptureRequest,
    ) -> Result<ProcIdentityAcquisition, Error> {
        let actual = self.capture_fdinfo_proc_identity_raw(guest, request).await;
        let recorded = self.fdinfo_capture_fact(guest, 3, &actual).await?;
        let selected = if actual.cleanup_complete() {
            recorded
        } else {
            actual
        };
        tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
            mm = ?guest.thread_state().mm_id, kind = 3, ?request,
            pid = ?selected.injected_pid, tid = ?selected.injected_tid, ppid = ?selected.injected_ppid,
            scratch = ?selected.scratch, procfs_root = ?selected.procfs_root,
            procfs_type = ?selected.procfs_type, mounted_pid_namespace = ?selected.mounted_pid_namespace,
            process_alias = ?selected.process_alias, task_alias = ?selected.task_alias,
            task_directory = ?selected.task.directory, task_stat = ?selected.task.stat,
            task_pid_namespace = ?selected.task.pid_namespace, task_mount_namespace = ?selected.task.mount_namespace,
            process_directory = ?selected.process.directory, process_stat = ?selected.process.stat,
            process_pid_namespace = ?selected.process.pid_namespace, process_mount_namespace = ?selected.process.mount_namespace,
            cleanup_complete = selected.cleanup_complete(), cleanup = ?selected.cleanup,
            "private fdinfo own acquisition selected facts");
        Ok(selected)
    }

    async fn capture_fdinfo_proc_identity_raw<G: Guest<Self>>(
        &self,
        guest: &mut G,
        request: ProcIdentityCaptureRequest,
    ) -> ProcIdentityAcquisition {
        let mut result = ProcIdentityAcquisition::empty(
            request == ProcIdentityCaptureRequest::CallerAndOriginalInitialParent,
        );
        for (call, output, allow_zero) in [
            (
                Syscall::Getpid(syscalls::Getpid::new()),
                &mut result.injected_pid,
                false,
            ),
            (
                Syscall::Gettid(syscalls::Gettid::new()),
                &mut result.injected_tid,
                false,
            ),
            (
                Syscall::Getppid(syscalls::Getppid::new()),
                &mut result.injected_ppid,
                true,
            ),
        ] {
            *output = match self.proc_identity_recorded(guest, call).await {
                Ok(value) if value >= i64::from(!allow_zero) && value <= i64::from(i32::MAX) => {
                    ProcIdentityFact::Observed(value as i32)
                }
                Ok(_) => ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::InvalidSyscallReturn,
                )),
                Err(error) => ProcIdentityFact::from_result(Err(error)),
            };
        }
        let mapped = self
            .proc_identity_inject_with_retry(
                guest,
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(SCRATCH_BYTES)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0)
                    .into(),
            )
            .await;
        let address = match mapped {
            Ok(address) if address >= 0 => address as usize,
            Ok(_) => {
                result.scratch = ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::InvalidSyscallReturn,
                ));
                return result;
            }
            Err(error) => {
                result.scratch = ProcIdentityFact::from_result(Err(error));
                return result;
            }
        };
        result.scratch = ProcIdentityFact::Observed(());
        let mut scratch = ProcCaptureScratch {
            address,
            held: Vec::new(),
            cleanup: Vec::new(),
        };
        let root = self
            .proc_identity_open(
                guest,
                &mut scratch,
                libc::AT_FDCWD,
                "/proc",
                OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            )
            .await;
        match root {
            Err(error) => result.procfs_root = ProcIdentityFact::from_result(Err(error)),
            Ok(root) => {
                result.procfs_root = self.proc_identity_object(guest, &scratch, root, "").await;
                result.procfs_type = ProcIdentityFact::from_result::<ProcIdentityGap>(
                    async {
                        let buffer = scratch.pointer(STATFS_OFFSET)?.cast::<libc::statfs>();
                        guest.memory().write_exact(
                            buffer.cast(),
                            &[0; std::mem::size_of::<libc::statfs>()],
                        )?;
                        self.proc_identity_recorded(
                            guest,
                            syscalls::Fstatfs::new()
                                .with_fd(root)
                                .with_buf(Some(buffer)),
                        )
                        .await?;
                        let raw: libc::statfs = guest.memory().read_value(buffer)?;
                        Ok(raw.f_type as i64)
                    }
                    .await,
                );
                result.mounted_pid_namespace = self
                    .proc_identity_object(guest, &scratch, root, "1/ns/pid")
                    .await;
                result.process_link_bytes =
                    self.proc_identity_link(guest, &scratch, root, "self").await;
                result.task_link_bytes = self
                    .proc_identity_link(guest, &scratch, root, "thread-self")
                    .await;
                result.process_alias = match &result.process_link_bytes {
                    ProcIdentityFact::Observed(bytes) => {
                        ProcIdentityFact::from_parse(parse_process_alias(bytes))
                    }
                    ProcIdentityFact::Unavailable(gap) => {
                        ProcIdentityFact::Unavailable(gap.clone())
                    }
                };
                result.task_alias = match &result.task_link_bytes {
                    ProcIdentityFact::Observed(bytes) => {
                        ProcIdentityFact::from_parse(parse_task_alias(bytes))
                    }
                    ProcIdentityFact::Unavailable(gap) => {
                        ProcIdentityFact::Unavailable(gap.clone())
                    }
                };
                result.process = self
                    .proc_identity_task(guest, &mut scratch, root, "self")
                    .await;
                result.task = self
                    .proc_identity_task(guest, &mut scratch, root, "thread-self")
                    .await;
                if result.original_parent_requested
                    && let (Some(ppid), Some(ProcRawAliasName::Process { pid })) =
                        (result.injected_ppid.value(), result.process_alias.value())
                {
                    // Only this matching PID view allows the injected PPID
                    // to name a directory under this proc root. A different
                    // view is not guessed or classified as an outside task.
                    if *ppid > 0 && result.injected_pid.value() == Some(pid) {
                        result.original_parent_candidate = self
                            .proc_identity_task(guest, &mut scratch, root, &ppid.to_string())
                            .await;
                    }
                }
                self.proc_identity_close(guest, &mut scratch, root).await;
            }
        }
        // A failed inner operation never skips owned descriptor cleanup.
        while let Some(fd) = scratch.held.last().copied() {
            self.proc_identity_close(guest, &mut scratch, fd).await;
        }
        let unmapped = self
            .proc_identity_inject_with_retry(
                guest,
                syscalls::Munmap::new()
                    .with_addr(Addr::<u8>::from_raw(address).map(|address| address.cast()))
                    .with_len(SCRATCH_BYTES)
                    .into(),
            )
            .await;
        scratch.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::ScratchMapping {
                bytes: SCRATCH_BYTES as u64,
            },
            result: ProcIdentityFact::from_result(unmapped),
        });
        result.cleanup = scratch.cleanup;
        result
    }
}

/// The exact kernel-proc alias used for the already opened source. These are
/// raw proc-view numbers, never DetTid, reader-N or a registration heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ProcFdinfoSubjectSelector {
    SelfProcess,
    ThreadSelf,
    Process { pid: i32 },
    Task { pid: i32, tid: i32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcFdinfoOpenSubjectAcquisition {
    pub selector: ProcFdinfoSubjectSelector,
    pub source_local_fd: i32,
    pub target_fd: i32,
    pub injected_pid: ProcIdentityFact<i32>,
    pub injected_tid: ProcIdentityFact<i32>,
    pub injected_ppid: ProcIdentityFact<i32>,
    pub scratch: ProcIdentityFact<()>,
    pub procfs_root: ProcIdentityFact<ProcRawObject>,
    pub procfs_type: ProcIdentityFact<i64>,
    pub mounted_pid_namespace_before: ProcIdentityFact<ProcRawObject>,
    pub mounted_pid_namespace_after: ProcIdentityFact<ProcRawObject>,
    pub caller_pid_namespace_before: ProcIdentityFact<ProcRawObject>,
    pub caller_pid_namespace_after: ProcIdentityFact<ProcRawObject>,
    pub caller_mount_namespace_before: ProcIdentityFact<ProcRawObject>,
    pub caller_mount_namespace_after: ProcIdentityFact<ProcRawObject>,
    pub process_link_bytes: ProcIdentityFact<Vec<u8>>,
    pub task_link_bytes: ProcIdentityFact<Vec<u8>>,
    pub process_alias: ProcIdentityFact<ProcRawAliasName>,
    pub task_alias: ProcIdentityFact<ProcRawAliasName>,
    pub resolved_alias: ProcIdentityFact<ProcRawAliasName>,
    pub source_object_before: ProcIdentityFact<ProcRawObject>,
    pub source_object_after: ProcIdentityFact<ProcRawObject>,
    pub path_object_before: ProcIdentityFact<ProcRawObject>,
    pub path_object_after: ProcIdentityFact<ProcRawObject>,
    pub target_before: ProcRawTaskObservation,
    pub target_after: ProcRawTaskObservation,
    /// Process incarnation is sampled independently of a task alias. A task
    /// path establishes membership, never that its files table is shared.
    pub process_before: ProcRawTaskObservation,
    pub process_after: ProcRawTaskObservation,
    pub cleanup: Vec<ProcCleanupObservation>,
}

/// Positive sampled kernel-file/path relation only. This is not managed,
/// outside-tool, initial-parent, FilesId, slot/OFD-generation or guest authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcFdinfoOpenSubjectRelation {
    pub alias: ProcRawAliasName,
    pub target_directory: ProcRawObject,
    pub target_start_ticks: u64,
    pub target_fd: i32,
    pub opened_proc_file: ProcRawObject,
    pub procfs_root: ProcRawObject,
    pub mounted_pid_namespace: ProcRawObject,
    pub caller_pid_namespace: ProcRawObject,
    pub caller_mount_namespace: ProcRawObject,
    pub target_pid_namespace: ProcRawObject,
    pub target_mount_namespace: ProcRawObject,
    pub process_incarnation: Option<(ProcRawObject, u64)>,
}

/// A positive self-source/proc/mount-view relation only. This deliberately
/// supplies neither mounted-PID-namespace nor target FilesId/slot/OFD authority.
/// It may normalize one raw mount number, never an inode, flags or cookies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcFdinfoMountSubjectRelation {
    pub selector: ProcFdinfoSubjectSelector,
    pub alias: ProcRawAliasName,
    pub target_fd: i32,
    pub opened_proc_file: ProcRawObject,
    pub procfs_root: ProcRawObject,
    pub target_directory: ProcRawObject,
    pub target_start_ticks: u64,
    pub caller_pid_namespace: ProcRawObject,
    pub mount_namespace: ProcRawObject,
}

impl ProcFdinfoMountSubjectRelation {
    pub(crate) fn matches_mount_view(&self, order: &[u64]) -> bool {
        self.procfs_root
            .mount_id
            .is_some_and(|id| id != 0 && order.contains(&id))
    }
}

fn required_proc_fact<T>(fact: &ProcIdentityFact<T>) -> Result<&T, ProcIdentityGap> {
    match fact {
        ProcIdentityFact::Observed(value) => Ok(value),
        ProcIdentityFact::Unavailable(gap) => Err(gap.clone()),
    }
}

/// Names returned by procfs self links belong to that held proc view, not
/// necessarily to the backend's application-facing Getpid/Gettid domain.
/// This parses names only. The caller must still authenticate the held proc
/// root, task directories/stat/lifetime, opened source and namespace relation.
pub(crate) fn proc_view_self_ids(
    process_link: &ProcIdentityFact<Vec<u8>>,
    task_link: &ProcIdentityFact<Vec<u8>>,
) -> (ProcIdentityFact<i32>, ProcIdentityFact<i32>) {
    let resolved: Result<(i32, i32), ProcIdentityGap> = (|| {
        let process = parse_process_alias(required_proc_fact(process_link)?)
            .map_err(ProcIdentityGap::Malformed)?;
        let task =
            parse_task_alias(required_proc_fact(task_link)?).map_err(ProcIdentityGap::Malformed)?;
        match (process, task) {
            (ProcRawAliasName::Process { pid }, ProcRawAliasName::Task { pid: group, tid })
                if pid == group =>
            {
                Ok((pid, tid))
            }
            _ => Err(ProcIdentityGap::Malformed(
                ProcIdentityParseError::AliasSyntax,
            )),
        }
    })();
    match resolved {
        Ok((pid, tid)) => (
            ProcIdentityFact::Observed(pid),
            ProcIdentityFact::Observed(tid),
        ),
        Err(gap) => (
            ProcIdentityFact::Unavailable(gap.clone()),
            ProcIdentityFact::Unavailable(gap),
        ),
    }
}

fn resolve_fdinfo_subject(
    selector: ProcFdinfoSubjectSelector,
    process: &ProcIdentityFact<ProcRawAliasName>,
    task: &ProcIdentityFact<ProcRawAliasName>,
    pid: &ProcIdentityFact<i32>,
    tid: &ProcIdentityFact<i32>,
) -> ProcIdentityFact<ProcRawAliasName> {
    let resolved: Result<ProcRawAliasName, ProcIdentityGap> = (|| match selector {
        ProcFdinfoSubjectSelector::SelfProcess => {
            let alias = *required_proc_fact(process)?;
            let current = *required_proc_fact(pid)?;
            match alias {
                ProcRawAliasName::Process { pid } if pid > 0 && pid == current => Ok(alias),
                _ => Err(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::AliasSyntax,
                )),
            }
        }
        ProcFdinfoSubjectSelector::ThreadSelf => {
            let alias = *required_proc_fact(task)?;
            let current_pid = *required_proc_fact(pid)?;
            let current_tid = *required_proc_fact(tid)?;
            match alias {
                ProcRawAliasName::Task { pid, tid }
                    if pid > 0 && tid > 0 && pid == current_pid && tid == current_tid =>
                {
                    Ok(alias)
                }
                _ => Err(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::AliasSyntax,
                )),
            }
        }
        ProcFdinfoSubjectSelector::Process { pid } if pid > 0 => {
            Ok(ProcRawAliasName::Process { pid })
        }
        ProcFdinfoSubjectSelector::Task { pid, tid } if pid > 0 && tid > 0 => {
            Ok(ProcRawAliasName::Task { pid, tid })
        }
        _ => Err(ProcIdentityGap::Malformed(
            ProcIdentityParseError::NumberOutOfRange,
        )),
    })();
    ProcIdentityFact::from_result(resolved)
}

fn same_observed_proc_object(left: ProcRawObject, right: ProcRawObject, ty: u32) -> bool {
    left.mode & libc::S_IFMT == ty
        && right.mode & libc::S_IFMT == ty
        && left.inode == right.inode
        && left.mount_id.is_some()
        && left.mount_id == right.mount_id
}

impl ProcFdinfoOpenSubjectAcquisition {
    fn empty(selector: ProcFdinfoSubjectSelector, source_local_fd: i32, target_fd: i32) -> Self {
        Self {
            selector,
            source_local_fd,
            target_fd,
            injected_pid: ProcIdentityFact::pending(),
            injected_tid: ProcIdentityFact::pending(),
            injected_ppid: ProcIdentityFact::pending(),
            scratch: ProcIdentityFact::pending(),
            procfs_root: ProcIdentityFact::pending(),
            procfs_type: ProcIdentityFact::pending(),
            mounted_pid_namespace_before: ProcIdentityFact::pending(),
            mounted_pid_namespace_after: ProcIdentityFact::pending(),
            caller_pid_namespace_before: ProcIdentityFact::pending(),
            caller_pid_namespace_after: ProcIdentityFact::pending(),
            caller_mount_namespace_before: ProcIdentityFact::pending(),
            caller_mount_namespace_after: ProcIdentityFact::pending(),
            process_link_bytes: ProcIdentityFact::pending(),
            task_link_bytes: ProcIdentityFact::pending(),
            process_alias: ProcIdentityFact::pending(),
            task_alias: ProcIdentityFact::pending(),
            resolved_alias: ProcIdentityFact::pending(),
            source_object_before: ProcIdentityFact::pending(),
            source_object_after: ProcIdentityFact::pending(),
            path_object_before: ProcIdentityFact::pending(),
            path_object_after: ProcIdentityFact::pending(),
            target_before: ProcRawTaskObservation::unavailable(
                ProcIdentityGap::DependencyUnavailable,
            ),
            target_after: ProcRawTaskObservation::unavailable(
                ProcIdentityGap::DependencyUnavailable,
            ),
            process_before: ProcRawTaskObservation::unavailable(
                ProcIdentityGap::DependencyUnavailable,
            ),
            process_after: ProcRawTaskObservation::unavailable(
                ProcIdentityGap::DependencyUnavailable,
            ),
            cleanup: Vec::new(),
        }
    }

    pub(crate) fn cleanup_complete(&self) -> bool {
        self.scratch.value().is_some()
            && self
                .cleanup
                .iter()
                .any(|item| matches!(item.resource, ProcTemporaryResource::ScratchMapping { .. }))
            && self
                .cleanup
                .iter()
                .all(|item| item.result == ProcIdentityFact::Observed(0))
    }

    /// Independently authenticate a current self/thread-self proc source and
    /// its mount view. A denied mounted-PID-namespace probe remains denied;
    /// this relation cannot authorize whole target projection or a foreign FD.
    pub(crate) fn observed_mount_subject_relation(&self) -> Option<ProcFdinfoMountSubjectRelation> {
        if self.source_local_fd < 0
            || self.target_fd < 0
            || !self.cleanup_complete()
            || self.procfs_type.value() != Some(&PROC_SUPER_MAGIC)
        {
            return None;
        }
        let process = parse_process_alias(self.process_link_bytes.value()?).ok()?;
        let task = parse_task_alias(self.task_link_bytes.value()?).ok()?;
        let (pid, tid) = proc_view_self_ids(&self.process_link_bytes, &self.task_link_bytes);
        if self.process_alias.value() != Some(&process) || self.task_alias.value() != Some(&task) {
            return None;
        }
        let alias = match self.selector {
            ProcFdinfoSubjectSelector::SelfProcess => process,
            ProcFdinfoSubjectSelector::ThreadSelf => task,
            ProcFdinfoSubjectSelector::Process { .. } | ProcFdinfoSubjectSelector::Task { .. } => {
                return None;
            }
        };
        if self.resolved_alias.value() != Some(&alias)
            || resolve_fdinfo_subject(
                self.selector,
                &self.process_alias,
                &self.task_alias,
                &pid,
                &tid,
            )
            .value()
                != Some(&alias)
        {
            return None;
        }
        let root = *self.procfs_root.value()?;
        let source = *self.source_object_before.value()?;
        if root.mode & libc::S_IFMT != libc::S_IFDIR
            || root.mount_id.is_none()
            || source.mount_id != root.mount_id
        {
            return None;
        }
        for object in [
            self.source_object_after.value()?,
            self.path_object_before.value()?,
            self.path_object_after.value()?,
        ] {
            if !same_observed_proc_object(source, *object, libc::S_IFREG) {
                return None;
            }
        }
        let directory = *self.target_before.directory.value()?;
        if directory.mount_id != root.mount_id
            || !same_observed_proc_object(
                directory,
                *self.target_after.directory.value()?,
                libc::S_IFDIR,
            )
        {
            return None;
        }
        let before = *self.target_before.stat.value()?;
        let after = *self.target_after.stat.value()?;
        let expected = match alias {
            ProcRawAliasName::Process { pid } => pid,
            ProcRawAliasName::Task { tid, .. } => tid,
        };
        if expected <= 0
            || before.pid != expected
            || after.pid != expected
            || before.start_ticks != after.start_ticks
            || parse_raw_proc_stat(self.target_before.stat_bytes.value()?).ok() != Some(before)
            || parse_raw_proc_stat(self.target_after.stat_bytes.value()?).ok() != Some(after)
        {
            return None;
        }
        let caller_pid = *self.caller_pid_namespace_before.value()?;
        let caller_mount = *self.caller_mount_namespace_before.value()?;
        let target_pid = *self.target_before.pid_namespace.value()?;
        let target_mount = *self.target_before.mount_namespace.value()?;
        for (before, after) in [
            (caller_pid, *self.caller_pid_namespace_after.value()?),
            (caller_mount, *self.caller_mount_namespace_after.value()?),
            (target_pid, *self.target_after.pid_namespace.value()?),
            (target_mount, *self.target_after.mount_namespace.value()?),
            (caller_pid, target_pid),
            (caller_mount, target_mount),
        ] {
            if !same_observed_proc_object(before, after, libc::S_IFREG) {
                return None;
            }
        }
        Some(ProcFdinfoMountSubjectRelation {
            selector: self.selector,
            alias,
            target_fd: self.target_fd,
            opened_proc_file: source,
            procfs_root: root,
            target_directory: directory,
            target_start_ticks: before.start_ticks,
            caller_pid_namespace: caller_pid,
            mount_namespace: caller_mount,
        })
    }

    /// Integration calls acquisition at the actual proc-open seam and retains
    /// the already-opened source OFD. Numbers alone cannot bind an old file to
    /// another task. No later failed path lookup changes a successful read.
    pub(crate) fn observed_open_subject_relation(&self) -> Option<ProcFdinfoOpenSubjectRelation> {
        if self.source_local_fd < 0
            || self.target_fd < 0
            || !self.cleanup_complete()
            || self.procfs_type.value() != Some(&PROC_SUPER_MAGIC)
        {
            return None;
        }
        let alias = *self.resolved_alias.value()?;
        let (proc_pid, proc_tid) =
            proc_view_self_ids(&self.process_link_bytes, &self.task_link_bytes);
        if resolve_fdinfo_subject(
            self.selector,
            &self.process_alias,
            &self.task_alias,
            &proc_pid,
            &proc_tid,
        )
        .value()
            != Some(&alias)
        {
            return None;
        }
        match self.selector {
            ProcFdinfoSubjectSelector::SelfProcess => {
                if parse_process_alias(self.process_link_bytes.value()?).ok() != Some(alias) {
                    return None;
                }
            }
            ProcFdinfoSubjectSelector::ThreadSelf => {
                if parse_task_alias(self.task_link_bytes.value()?).ok() != Some(alias) {
                    return None;
                }
            }
            ProcFdinfoSubjectSelector::Process { .. } | ProcFdinfoSubjectSelector::Task { .. } => {}
        }
        let root = *self.procfs_root.value()?;
        if root.mode & libc::S_IFMT != libc::S_IFDIR || root.mount_id.is_none() {
            return None;
        }
        let source = *self.source_object_before.value()?;
        if source.mount_id != root.mount_id {
            return None;
        }
        for object in [
            self.source_object_after.value()?,
            self.path_object_before.value()?,
            self.path_object_after.value()?,
        ] {
            if !same_observed_proc_object(source, *object, libc::S_IFREG) {
                return None;
            }
        }
        let directory = *self.target_before.directory.value()?;
        if directory.mount_id != root.mount_id {
            return None;
        }
        if !same_observed_proc_object(
            directory,
            *self.target_after.directory.value()?,
            libc::S_IFDIR,
        ) {
            return None;
        }
        let before = *self.target_before.stat.value()?;
        let after = *self.target_after.stat.value()?;
        if parse_raw_proc_stat(self.target_before.stat_bytes.value()?).ok() != Some(before)
            || parse_raw_proc_stat(self.target_after.stat_bytes.value()?).ok() != Some(after)
        {
            return None;
        }
        let expected_pid = match alias {
            ProcRawAliasName::Process { pid } => pid,
            ProcRawAliasName::Task { tid, .. } => tid,
        };
        if expected_pid <= 0
            || before.pid != expected_pid
            || after.pid != expected_pid
            || before.start_ticks != after.start_ticks
        {
            return None;
        }
        let mounted = *self.mounted_pid_namespace_before.value()?;
        let caller_pid = *self.caller_pid_namespace_before.value()?;
        let caller_mount = *self.caller_mount_namespace_before.value()?;
        let target_pid = *self.target_before.pid_namespace.value()?;
        let target_mount = *self.target_before.mount_namespace.value()?;
        for (before, after) in [
            (mounted, *self.mounted_pid_namespace_after.value()?),
            (caller_pid, *self.caller_pid_namespace_after.value()?),
            (caller_mount, *self.caller_mount_namespace_after.value()?),
            (target_pid, *self.target_after.pid_namespace.value()?),
            (target_mount, *self.target_after.mount_namespace.value()?),
        ] {
            if !same_observed_proc_object(before, after, libc::S_IFREG) {
                return None;
            }
        }
        // Do not guess an injected self ID in a differently mounted PID view.
        // Explicit foreign aliases remain only a kernel-file/path relation.
        if matches!(
            self.selector,
            ProcFdinfoSubjectSelector::SelfProcess | ProcFdinfoSubjectSelector::ThreadSelf
        ) && mounted.inode != caller_pid.inode
        {
            return None;
        }
        let process_incarnation = (|| {
            let pid = match alias {
                ProcRawAliasName::Process { pid } | ProcRawAliasName::Task { pid, .. } => pid,
            };
            let directory = *self.process_before.directory.value()?;
            let after_dir = *self.process_after.directory.value()?;
            let stat = *self.process_before.stat.value()?;
            let after_stat = *self.process_after.stat.value()?;
            if directory.mount_id != root.mount_id
                || !same_observed_proc_object(directory, after_dir, libc::S_IFDIR)
                || stat.pid != pid
                || stat != after_stat
                || parse_raw_proc_stat(self.process_before.stat_bytes.value()?).ok() != Some(stat)
                || parse_raw_proc_stat(self.process_after.stat_bytes.value()?).ok()
                    != Some(after_stat)
                || !same_observed_proc_object(
                    *self.process_before.pid_namespace.value()?,
                    *self.process_after.pid_namespace.value()?,
                    libc::S_IFREG,
                )
                || !same_observed_proc_object(
                    *self.process_before.mount_namespace.value()?,
                    *self.process_after.mount_namespace.value()?,
                    libc::S_IFREG,
                )
                || self.process_before.pid_namespace.value()?.inode != target_pid.inode
            {
                return None;
            }
            Some((directory, stat.start_ticks))
        })();
        Some(ProcFdinfoOpenSubjectRelation {
            alias,
            target_directory: directory,
            target_start_ticks: before.start_ticks,
            target_fd: self.target_fd,
            opened_proc_file: source,
            procfs_root: root,
            mounted_pid_namespace: mounted,
            caller_pid_namespace: caller_pid,
            caller_mount_namespace: caller_mount,
            target_pid_namespace: target_pid,
            target_mount_namespace: target_mount,
            process_incarnation,
        })
    }
}

impl<T: RecordOrReplay> Detcore<T> {
    /// Sample a held task directory without closing it. This keeps all name
    /// probes rooted at the same task reference until both samples finish.
    async fn proc_identity_held_task<G: Guest<Self>>(
        &self,
        guest: &mut G,
        scratch: &mut ProcCaptureScratch,
        directory: i32,
    ) -> ProcRawTaskObservation {
        let object = self
            .proc_identity_object(guest, scratch, directory, "")
            .await;
        let stat_bytes = match self
            .proc_identity_open(
                guest,
                scratch,
                directory,
                "stat",
                OFlag::O_RDONLY | OFlag::O_CLOEXEC,
            )
            .await
        {
            Err(gap) => ProcIdentityFact::Unavailable(gap),
            Ok(fd) => {
                let bytes = match scratch.pointer(0) {
                    Ok(buffer) => self.proc_identity_drain(guest, fd, buffer).await,
                    Err(error) => Err(error.into()),
                };
                self.proc_identity_close(guest, scratch, fd).await;
                ProcIdentityFact::from_result(bytes)
            }
        };
        let stat = match &stat_bytes {
            ProcIdentityFact::Observed(bytes) => {
                ProcIdentityFact::from_parse(parse_raw_proc_stat(bytes))
            }
            ProcIdentityFact::Unavailable(gap) => ProcIdentityFact::Unavailable(gap.clone()),
        };
        let pid_namespace = self
            .proc_identity_object(guest, scratch, directory, "ns/pid")
            .await;
        let mount_namespace = self
            .proc_identity_object(guest, scratch, directory, "ns/mnt")
            .await;
        ProcRawTaskObservation {
            directory: object,
            stat_bytes,
            stat,
            pid_namespace,
            mount_namespace,
        }
    }

    /// `source_local_fd` is the actual already-opened proc-file FD, not target N.
    /// Call at its original open seam under the existing held serialized turn.
    /// Source FD/OFD ownership stays with the caller; only temporary acquisitions
    /// are closed here. No classification, target wake or retained FD is added.
    pub(crate) async fn capture_fdinfo_open_subject<G: Guest<Self>>(
        &self,
        guest: &mut G,
        source_local_fd: i32,
        selector: ProcFdinfoSubjectSelector,
        target_fd: i32,
    ) -> Result<ProcFdinfoOpenSubjectAcquisition, Error> {
        let actual = self
            .capture_fdinfo_open_subject_raw(guest, source_local_fd, selector, target_fd)
            .await;
        let recorded = self.fdinfo_capture_fact(guest, 4, &actual).await?;
        let selected = if actual.cleanup_complete() {
            recorded
        } else {
            actual
        };
        tracing::trace!(target: "fdinfo_diagnostic", dtid = ?guest.thread_state().dettid,
            mm = ?guest.thread_state().mm_id, kind = 4, ?selector, source_local_fd, target_fd,
            pid = ?selected.injected_pid, tid = ?selected.injected_tid,
            scratch = ?selected.scratch, procfs_root = ?selected.procfs_root, procfs_type = ?selected.procfs_type,
            mounted_pid_namespace_before = ?selected.mounted_pid_namespace_before,
            mounted_pid_namespace_after = ?selected.mounted_pid_namespace_after,
            caller_pid_namespace_before = ?selected.caller_pid_namespace_before,
            caller_pid_namespace_after = ?selected.caller_pid_namespace_after,
            caller_mount_namespace_before = ?selected.caller_mount_namespace_before,
            caller_mount_namespace_after = ?selected.caller_mount_namespace_after,
            resolved_alias = ?selected.resolved_alias,
            source_object_before = ?selected.source_object_before, source_object_after = ?selected.source_object_after,
            path_object_before = ?selected.path_object_before, path_object_after = ?selected.path_object_after,
            target_directory_before = ?selected.target_before.directory, target_directory_after = ?selected.target_after.directory,
            target_stat_before = ?selected.target_before.stat, target_stat_after = ?selected.target_after.stat,
            target_pid_namespace_before = ?selected.target_before.pid_namespace, target_pid_namespace_after = ?selected.target_after.pid_namespace,
            target_mount_namespace_before = ?selected.target_before.mount_namespace, target_mount_namespace_after = ?selected.target_after.mount_namespace,
            process_directory_before = ?selected.process_before.directory, process_directory_after = ?selected.process_after.directory,
            process_stat_before = ?selected.process_before.stat, process_stat_after = ?selected.process_after.stat,
            cleanup_complete = selected.cleanup_complete(), cleanup = ?selected.cleanup,
            "private fdinfo opened-subject acquisition selected facts");
        Ok(selected)
    }

    async fn capture_fdinfo_open_subject_raw<G: Guest<Self>>(
        &self,
        guest: &mut G,
        source_local_fd: i32,
        selector: ProcFdinfoSubjectSelector,
        target_fd: i32,
    ) -> ProcFdinfoOpenSubjectAcquisition {
        let mut result =
            ProcFdinfoOpenSubjectAcquisition::empty(selector, source_local_fd, target_fd);
        if source_local_fd < 0 || target_fd < 0 {
            result.resolved_alias = ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                ProcIdentityParseError::NumberOutOfRange,
            ));
            return result;
        }
        for (call, output, allow_zero) in [
            (
                Syscall::Getpid(syscalls::Getpid::new()),
                &mut result.injected_pid,
                false,
            ),
            (
                Syscall::Gettid(syscalls::Gettid::new()),
                &mut result.injected_tid,
                false,
            ),
            (
                Syscall::Getppid(syscalls::Getppid::new()),
                &mut result.injected_ppid,
                true,
            ),
        ] {
            *output = match self.proc_identity_recorded(guest, call).await {
                Ok(value) if value >= i64::from(!allow_zero) && value <= i64::from(i32::MAX) => {
                    ProcIdentityFact::Observed(value as i32)
                }
                Ok(_) => ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::InvalidSyscallReturn,
                )),
                Err(gap) => ProcIdentityFact::Unavailable(gap),
            };
        }
        let mapped = self
            .proc_identity_inject_with_retry(
                guest,
                syscalls::Mmap::new()
                    .with_addr(None)
                    .with_len(SCRATCH_BYTES)
                    .with_prot(ProtFlags::PROT_READ | ProtFlags::PROT_WRITE)
                    .with_flags(MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS)
                    .with_fd(-1)
                    .with_offset(0)
                    .into(),
            )
            .await;
        let address = match mapped {
            Ok(value) if value >= 0 => value as usize,
            Ok(_) => {
                result.scratch = ProcIdentityFact::Unavailable(ProcIdentityGap::Malformed(
                    ProcIdentityParseError::InvalidSyscallReturn,
                ));
                return result;
            }
            Err(gap) => {
                result.scratch = ProcIdentityFact::Unavailable(gap);
                return result;
            }
        };
        result.scratch = ProcIdentityFact::Observed(());
        let mut scratch = ProcCaptureScratch {
            address,
            held: Vec::new(),
            cleanup: Vec::new(),
        };
        // AT_EMPTY_PATH on the already-opened source is raw fstat-equivalent
        // object evidence; it never invokes handle_statx or a DetFd lookup.
        result.source_object_before = self
            .proc_identity_object(guest, &scratch, source_local_fd, "")
            .await;
        match self
            .proc_identity_open(
                guest,
                &mut scratch,
                libc::AT_FDCWD,
                "/proc",
                OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            )
            .await
        {
            Err(gap) => result.procfs_root = ProcIdentityFact::Unavailable(gap),
            Ok(root) => {
                result.procfs_root = self.proc_identity_object(guest, &scratch, root, "").await;
                result.procfs_type = ProcIdentityFact::from_result::<ProcIdentityGap>(
                    async {
                        let buffer = scratch.pointer(STATFS_OFFSET)?.cast::<libc::statfs>();
                        guest.memory().write_exact(
                            buffer.cast(),
                            &[0; std::mem::size_of::<libc::statfs>()],
                        )?;
                        self.proc_identity_recorded(
                            guest,
                            syscalls::Fstatfs::new()
                                .with_fd(root)
                                .with_buf(Some(buffer)),
                        )
                        .await?;
                        let value: libc::statfs = guest.memory().read_value(buffer)?;
                        Ok(value.f_type as i64)
                    }
                    .await,
                );
                result.mounted_pid_namespace_before = self
                    .proc_identity_object(guest, &scratch, root, "1/ns/pid")
                    .await;
                result.caller_pid_namespace_before = self
                    .proc_identity_object(guest, &scratch, root, "thread-self/ns/pid")
                    .await;
                result.caller_mount_namespace_before = self
                    .proc_identity_object(guest, &scratch, root, "thread-self/ns/mnt")
                    .await;
                result.process_link_bytes =
                    self.proc_identity_link(guest, &scratch, root, "self").await;
                result.task_link_bytes = self
                    .proc_identity_link(guest, &scratch, root, "thread-self")
                    .await;
                result.process_alias = match &result.process_link_bytes {
                    ProcIdentityFact::Observed(bytes) => {
                        ProcIdentityFact::from_parse(parse_process_alias(bytes))
                    }
                    ProcIdentityFact::Unavailable(gap) => {
                        ProcIdentityFact::Unavailable(gap.clone())
                    }
                };
                result.task_alias = match &result.task_link_bytes {
                    ProcIdentityFact::Observed(bytes) => {
                        ProcIdentityFact::from_parse(parse_task_alias(bytes))
                    }
                    ProcIdentityFact::Unavailable(gap) => {
                        ProcIdentityFact::Unavailable(gap.clone())
                    }
                };
                let (proc_pid, proc_tid) =
                    proc_view_self_ids(&result.process_link_bytes, &result.task_link_bytes);
                result.resolved_alias = resolve_fdinfo_subject(
                    selector,
                    &result.process_alias,
                    &result.task_alias,
                    &proc_pid,
                    &proc_tid,
                );
                if let Some(alias) = result.resolved_alias.value().copied() {
                    let path = match alias {
                        ProcRawAliasName::Process { pid } => pid.to_string(),
                        ProcRawAliasName::Task { pid, tid } => format!("{pid}/task/{tid}"),
                    };
                    match self
                        .proc_identity_open(
                            guest,
                            &mut scratch,
                            root,
                            &path,
                            OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                        )
                        .await
                    {
                        Err(gap) => {
                            result.target_before = ProcRawTaskObservation::unavailable(gap.clone());
                            result.target_after = ProcRawTaskObservation::unavailable(gap);
                        }
                        Ok(directory) => {
                            let name = format!("fdinfo/{target_fd}");
                            result.path_object_before = self
                                .proc_identity_object(guest, &scratch, directory, &name)
                                .await;
                            result.target_before = self
                                .proc_identity_held_task(guest, &mut scratch, directory)
                                .await;
                            let process_directory = match alias {
                                ProcRawAliasName::Process { .. } => None,
                                ProcRawAliasName::Task { pid, .. } => match self
                                    .proc_identity_open(
                                        guest,
                                        &mut scratch,
                                        root,
                                        &pid.to_string(),
                                        OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
                                    )
                                    .await
                                {
                                    Ok(directory) => Some(directory),
                                    Err(gap) => {
                                        result.process_before =
                                            ProcRawTaskObservation::unavailable(gap.clone());
                                        result.process_after =
                                            ProcRawTaskObservation::unavailable(gap);
                                        None
                                    }
                                },
                            };
                            result.process_before = if let Some(process) = process_directory {
                                self.proc_identity_held_task(guest, &mut scratch, process)
                                    .await
                            } else if matches!(alias, ProcRawAliasName::Process { .. }) {
                                result.target_before.clone()
                            } else {
                                result.process_before.clone()
                            };
                            result.target_after = self
                                .proc_identity_held_task(guest, &mut scratch, directory)
                                .await;
                            result.process_after = if let Some(process) = process_directory {
                                self.proc_identity_held_task(guest, &mut scratch, process)
                                    .await
                            } else if matches!(alias, ProcRawAliasName::Process { .. }) {
                                result.target_after.clone()
                            } else {
                                result.process_after.clone()
                            };
                            result.path_object_after = self
                                .proc_identity_object(guest, &scratch, directory, &name)
                                .await;
                            if let Some(process) = process_directory {
                                self.proc_identity_close(guest, &mut scratch, process).await;
                            }
                            self.proc_identity_close(guest, &mut scratch, directory)
                                .await;
                        }
                    }
                }
                result.mounted_pid_namespace_after = self
                    .proc_identity_object(guest, &scratch, root, "1/ns/pid")
                    .await;
                result.caller_pid_namespace_after = self
                    .proc_identity_object(guest, &scratch, root, "thread-self/ns/pid")
                    .await;
                result.caller_mount_namespace_after = self
                    .proc_identity_object(guest, &scratch, root, "thread-self/ns/mnt")
                    .await;
                self.proc_identity_close(guest, &mut scratch, root).await;
            }
        }
        result.source_object_after = self
            .proc_identity_object(guest, &scratch, source_local_fd, "")
            .await;
        while let Some(fd) = scratch.held.last().copied() {
            self.proc_identity_close(guest, &mut scratch, fd).await;
        }
        let unmapped = self
            .proc_identity_inject_with_retry(
                guest,
                syscalls::Munmap::new()
                    .with_addr(Addr::<u8>::from_raw(address).map(|address| address.cast()))
                    .with_len(SCRATCH_BYTES)
                    .into(),
            )
            .await;
        scratch.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::ScratchMapping {
                bytes: SCRATCH_BYTES as u64,
            },
            result: ProcIdentityFact::from_result(unmapped),
        });
        result.cleanup = scratch.cleanup;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(pid: &str, ppid: &str, ticks: &str, comm: &[u8]) -> Vec<u8> {
        let mut fields = vec!["0".to_owned(); 50];
        fields[0] = "S".to_owned();
        fields[1] = ppid.to_owned();
        fields[19] = ticks.to_owned();
        let mut bytes = format!("{pid} (").into_bytes();
        bytes.extend_from_slice(comm);
        bytes.extend_from_slice(format!(") {}\n", fields.join(" ")).as_bytes());
        bytes
    }

    #[test]
    fn raw_stat_retains_real_birth_field_and_opaque_comm() {
        assert_eq!(
            parse_raw_proc_stat(&stat("7", "3", "18446744073709551615", b"x) (y\xff")),
            Ok(ProcRawTaskStat {
                pid: 7,
                ppid: 3,
                start_ticks: u64::MAX
            })
        );
        assert_eq!(
            parse_raw_proc_stat(&stat("1", "0", "0", b"init")),
            Ok(ProcRawTaskStat {
                pid: 1,
                ppid: 0,
                start_ticks: 0
            })
        );
    }

    #[test]
    fn malformed_or_truncated_stat_has_no_default_identity() {
        for bytes in [
            stat("07", "3", "9", b"name"),
            stat("7", "-1", "9", b"name"),
            stat("7", "3", "18446744073709551616", b"name"),
            stat("7", "3", "+9", b"name"),
            b"7 (name) S 3 0\n".to_vec(),
            b"7 (name S 3 0\n".to_vec(),
        ] {
            assert!(parse_raw_proc_stat(&bytes).is_err());
        }
        let mut multiple = stat("7", "3", "9", b"name");
        multiple.extend(stat("8", "3", "9", b"name"));
        assert!(parse_raw_proc_stat(&multiple).is_err());
    }

    #[test]
    fn proc_aliases_are_exact_and_not_normalized_to_reader_ids() {
        assert_eq!(
            parse_process_alias(b"47"),
            Ok(ProcRawAliasName::Process { pid: 47 })
        );
        assert_eq!(
            parse_task_alias(b"47/task/51"),
            Ok(ProcRawAliasName::Task { pid: 47, tid: 51 })
        );
        for bytes in [
            b"47/task/051".as_slice(),
            b"47/task/0",
            b"/47/task/51",
            b"47/task/51/",
            b"47/task/51\n",
        ] {
            assert!(parse_task_alias(bytes).is_err());
        }
        assert!(parse_process_alias(b"2147483648").is_err());
    }

    fn observed_parent() -> ProcIdentityAcquisition {
        let mut value = ProcIdentityAcquisition::empty(true);
        let object = |inode: u64, mode: u32| {
            ProcIdentityFact::Observed(ProcRawObject {
                inode: RawInode::new(3, inode),
                mode,
                mount_id: Some(8),
            })
        };
        let task = |pid: i32, ppid: i32, inode: u64| ProcRawTaskObservation {
            directory: object(inode, libc::S_IFDIR),
            stat_bytes: ProcIdentityFact::Observed(stat(
                &pid.to_string(),
                &ppid.to_string(),
                "99",
                b"task",
            )),
            stat: ProcIdentityFact::Observed(ProcRawTaskStat {
                pid,
                ppid,
                start_ticks: 99,
            }),
            pid_namespace: object(80, libc::S_IFREG),
            mount_namespace: object(81, libc::S_IFREG),
        };
        value.injected_pid = ProcIdentityFact::Observed(3);
        value.injected_tid = ProcIdentityFact::Observed(3);
        value.injected_ppid = ProcIdentityFact::Observed(1);
        value.scratch = ProcIdentityFact::Observed(());
        value.procfs_root = object(1, libc::S_IFDIR);
        value.procfs_type = ProcIdentityFact::Observed(PROC_SUPER_MAGIC);
        value.mounted_pid_namespace = object(80, libc::S_IFREG);
        value.process_alias = ProcIdentityFact::Observed(ProcRawAliasName::Process { pid: 3 });
        value.task_alias = ProcIdentityFact::Observed(ProcRawAliasName::Task { pid: 3, tid: 3 });
        value.process = task(3, 1, 20);
        value.task = task(3, 1, 21); // same subject, different proc aliases
        value.original_parent_candidate = task(1, 0, 10);
        value.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::ScratchMapping {
                bytes: SCRATCH_BYTES as u64,
            },
            result: ProcIdentityFact::Observed(0),
        });
        value
    }

    #[test]
    fn initial_parent_needs_positive_view_relation_not_a_missing_registration() {
        let valid = observed_parent();
        assert!(valid.observed_initial_parent_relation().is_some());
        let mut value = valid.clone();
        value.original_parent_requested = false;
        assert_eq!(value.observed_initial_parent_relation(), None);
        let mut value = valid.clone();
        value.injected_ppid = ProcIdentityFact::Observed(0);
        assert_eq!(value.observed_initial_parent_relation(), None);
        let mut value = valid.clone();
        value.injected_ppid = ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(value.observed_initial_parent_relation(), None);
        let mut value = valid.clone();
        value.injected_pid = ProcIdentityFact::Observed(13);
        assert_eq!(value.observed_initial_parent_relation(), None);
        let mut value = valid.clone();
        value.mounted_pid_namespace =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::ENOENT));
        assert_eq!(value.observed_initial_parent_relation(), None);
        let mut value = valid.clone();
        value.procfs_root = ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(value.observed_initial_parent_relation(), None);
        let mut value = valid;
        value.procfs_type = ProcIdentityFact::Observed(0x01021994);
        assert_eq!(value.observed_initial_parent_relation(), None);
    }

    #[test]
    fn later_acquisition_or_cleanup_failure_does_not_erase_recorded_stat() {
        let mut value = observed_parent();
        let original = value.process.stat_bytes.clone();
        value.process.mount_namespace =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        value.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::Descriptor(7),
            result: ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO)),
        });
        assert_eq!(value.process.stat_bytes, original);
        assert_eq!(value.observed_initial_parent_relation(), None);
        assert!(!value.cleanup_complete());
    }

    #[test]
    fn real_errno_recorder_failure_and_unrequested_capture_remain_distinct() {
        assert_eq!(
            ProcIdentityFact::<()>::from_result(Err(Error::Errno(Errno::EBADF))),
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EBADF))
        );
        let tool = ProcIdentityFact::<()>::from_result(Err(Error::Tool(anyhow::anyhow!(
            "recording failed"
        ))));
        assert!(matches!(
            tool,
            ProcIdentityFact::Unavailable(ProcIdentityGap::Tool(_))
        ));
        let idle = ProcIdentityAcquisition::empty(false);
        assert_eq!(
            idle.original_parent_candidate.stat,
            ProcIdentityFact::Unavailable(ProcIdentityGap::NotRequested)
        );
        assert_eq!(idle.observed_initial_parent_relation(), None);
    }

    #[test]
    fn typed_refusal_survives_shared_errno_and_never_retries_as_linux_interruption() {
        let refusal = |errno| {
            reverie::UnsupportedRefusal::new(
                reverie::UnsupportedOperation::NonLeaderExec,
                errno,
                "backend explicitly refused acquisition",
            )
        };
        let plain = proc_identity_injection_result(Err(Error::Errno(Errno::EOPNOTSUPP)), None);
        assert_eq!(plain, Err(ProcIdentityGap::Errno(libc::EOPNOTSUPP)));
        let marked = proc_identity_injection_result(
            Err(Error::Errno(Errno::EOPNOTSUPP)),
            Some(refusal(Errno::EOPNOTSUPP)),
        );
        assert_eq!(
            marked,
            Err(ProcIdentityGap::BackendRefusal(
                ProcIdentityBackendRefusal {
                    operation: ProcIdentityUnsupportedOperation::NonLeaderExec,
                    errno: libc::EOPNOTSUPP,
                    diagnostic: "backend explicitly refused acquisition".to_owned(),
                    result: ProcIdentityCallResult::Errno(libc::EOPNOTSUPP),
                }
            ))
        );
        for errno in [Errno::EINTR, Errno::ERESTARTSYS] {
            let plain = proc_identity_injection_result(Err(Error::Errno(errno)), None).unwrap_err();
            assert!(plain.retry_interrupted());
            let marked =
                proc_identity_injection_result(Err(Error::Errno(errno)), Some(refusal(errno)))
                    .unwrap_err();
            assert!(!marked.retry_interrupted());
        }
        let tool = proc_identity_injection_result(
            Err(Error::Tool(anyhow::anyhow!("recording failed"))),
            Some(refusal(Errno::EOPNOTSUPP)),
        )
        .unwrap_err();
        assert!(
            matches!(&tool, ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
            result: ProcIdentityCallResult::Tool(message), ..
        }) if message == "recording failed")
        );
        let io = proc_identity_injection_result(
            Err(Error::Io(std::io::Error::from_raw_os_error(libc::EIO))),
            Some(refusal(Errno::EOPNOTSUPP)),
        )
        .unwrap_err();
        assert!(matches!(
            io,
            ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
                result: ProcIdentityCallResult::Io {
                    errno: Some(libc::EIO)
                },
                ..
            })
        ));
        // Even an inconsistent success+marker cannot supply identity authority.
        let returned =
            proc_identity_injection_result(Ok(3), Some(refusal(Errno::EOPNOTSUPP))).unwrap_err();
        assert!(matches!(
            returned,
            ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
                result: ProcIdentityCallResult::Returned(3),
                ..
            })
        ));
        let mut value = observed_parent();
        let bytes = value.process.stat_bytes.clone();
        value.process.mount_namespace = ProcIdentityFact::Unavailable(tool);
        value.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::Descriptor(7),
            result: ProcIdentityFact::from_result(marked),
        });
        assert_eq!(value.process.stat_bytes, bytes);
        assert_eq!(value.observed_initial_parent_relation(), None);
        assert!(!value.cleanup_complete());
    }

    #[test]
    fn opened_subject_selector_requires_exact_positive_roles_and_self_view_ids() {
        let process = ProcIdentityFact::Observed(ProcRawAliasName::Process { pid: 17 });
        let task = ProcIdentityFact::Observed(ProcRawAliasName::Task { pid: 17, tid: 19 });
        let pid = ProcIdentityFact::Observed(17);
        let tid = ProcIdentityFact::Observed(19);
        assert_eq!(
            resolve_fdinfo_subject(
                ProcFdinfoSubjectSelector::SelfProcess,
                &process,
                &task,
                &pid,
                &tid
            ),
            process
        );
        assert_eq!(
            resolve_fdinfo_subject(
                ProcFdinfoSubjectSelector::ThreadSelf,
                &process,
                &task,
                &pid,
                &tid
            ),
            task
        );
        assert_eq!(
            resolve_fdinfo_subject(
                ProcFdinfoSubjectSelector::Process { pid: 47 },
                &process,
                &task,
                &pid,
                &tid
            ),
            ProcIdentityFact::Observed(ProcRawAliasName::Process { pid: 47 })
        );
        assert_eq!(
            resolve_fdinfo_subject(
                ProcFdinfoSubjectSelector::Task { pid: 47, tid: 51 },
                &process,
                &task,
                &pid,
                &tid
            ),
            ProcIdentityFact::Observed(ProcRawAliasName::Task { pid: 47, tid: 51 })
        );
        for selector in [
            ProcFdinfoSubjectSelector::Process { pid: 0 },
            ProcFdinfoSubjectSelector::Process { pid: -1 },
            ProcFdinfoSubjectSelector::Task { pid: 47, tid: 0 },
            ProcFdinfoSubjectSelector::Task { pid: 0, tid: 51 },
        ] {
            assert!(matches!(
                resolve_fdinfo_subject(selector, &process, &task, &pid, &tid),
                ProcIdentityFact::Unavailable(_)
            ));
        }
        assert!(matches!(
            resolve_fdinfo_subject(
                ProcFdinfoSubjectSelector::SelfProcess,
                &process,
                &task,
                &ProcIdentityFact::Observed(3),
                &tid
            ),
            ProcIdentityFact::Unavailable(_)
        ));
        let missing = ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(
            resolve_fdinfo_subject(
                ProcFdinfoSubjectSelector::ThreadSelf,
                &process,
                &missing,
                &pid,
                &tid
            ),
            missing
        );
    }

    fn observed_open_subject() -> ProcFdinfoOpenSubjectAcquisition {
        let mut value =
            ProcFdinfoOpenSubjectAcquisition::empty(ProcFdinfoSubjectSelector::ThreadSelf, 5, 7);
        let object = |ino, mode| {
            ProcIdentityFact::Observed(ProcRawObject {
                inode: RawInode::new(9, ino),
                mode,
                mount_id: Some(8),
            })
        };
        let target = ProcRawTaskObservation {
            directory: object(71, libc::S_IFDIR),
            stat_bytes: ProcIdentityFact::Observed(stat("19", "1", "37", b"task")),
            stat: ProcIdentityFact::Observed(ProcRawTaskStat {
                pid: 19,
                ppid: 1,
                start_ticks: 37,
            }),
            pid_namespace: object(80, libc::S_IFREG),
            mount_namespace: object(81, libc::S_IFREG),
        };
        value.injected_pid = ProcIdentityFact::Observed(17);
        value.injected_tid = ProcIdentityFact::Observed(19);
        value.injected_ppid = ProcIdentityFact::Observed(1);
        value.scratch = ProcIdentityFact::Observed(());
        value.procfs_root = object(1, libc::S_IFDIR);
        value.procfs_type = ProcIdentityFact::Observed(PROC_SUPER_MAGIC);
        value.mounted_pid_namespace_before = object(80, libc::S_IFREG);
        value.mounted_pid_namespace_after = object(80, libc::S_IFREG);
        value.caller_pid_namespace_before = object(80, libc::S_IFREG);
        value.caller_pid_namespace_after = object(80, libc::S_IFREG);
        value.caller_mount_namespace_before = object(81, libc::S_IFREG);
        value.caller_mount_namespace_after = object(81, libc::S_IFREG);
        value.process_link_bytes = ProcIdentityFact::Observed(b"17".to_vec());
        value.task_link_bytes = ProcIdentityFact::Observed(b"17/task/19".to_vec());
        value.process_alias = ProcIdentityFact::Observed(ProcRawAliasName::Process { pid: 17 });
        value.task_alias = ProcIdentityFact::Observed(ProcRawAliasName::Task { pid: 17, tid: 19 });
        value.resolved_alias = value.task_alias.clone();
        value.source_object_before = object(101, libc::S_IFREG);
        value.source_object_after = object(101, libc::S_IFREG);
        value.path_object_before = object(101, libc::S_IFREG);
        value.path_object_after = object(101, libc::S_IFREG);
        value.target_before = target.clone();
        value.target_after = target;
        value.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::ScratchMapping {
                bytes: SCRATCH_BYTES as u64,
            },
            result: ProcIdentityFact::Observed(0),
        });
        value
    }

    #[test]
    fn opened_subject_relation_requires_actual_file_path_lifetime_view_and_cleanup() {
        let valid = observed_open_subject();
        let relation = valid.observed_open_subject_relation().unwrap();
        assert_eq!(relation.alias, ProcRawAliasName::Task { pid: 17, tid: 19 });
        assert_eq!(relation.target_start_ticks, 37);
        assert_eq!(relation.target_fd, 7);
        assert_eq!(relation.opened_proc_file.inode, RawInode::new(9, 101));
        let mut value = valid.clone();
        value.path_object_after =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::ENOENT));
        assert_eq!(value.observed_open_subject_relation(), None);
        assert_eq!(value.source_object_before, valid.source_object_before); // Successful facts survive late lookup failure.
        let mut value = valid.clone();
        value.source_object_after = ProcIdentityFact::Observed(ProcRawObject {
            inode: RawInode::new(9, 102),
            mode: libc::S_IFREG,
            mount_id: Some(8),
        });
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.target_after.stat = ProcIdentityFact::Observed(ProcRawTaskStat {
            pid: 19,
            ppid: 1,
            start_ticks: 38,
        });
        value.target_after.stat_bytes = ProcIdentityFact::Observed(stat("19", "1", "38", b"task"));
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.target_after.stat_bytes = ProcIdentityFact::Observed(b"malformed".to_vec());
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.target_after.directory = ProcIdentityFact::Observed(ProcRawObject {
            inode: RawInode::new(9, 72),
            mode: libc::S_IFDIR,
            mount_id: Some(8),
        });
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.procfs_type = ProcIdentityFact::Observed(0x01021994);
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.task_link_bytes = ProcIdentityFact::Observed(b"17/task/20".to_vec());
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        for object in [
            &mut value.source_object_before,
            &mut value.source_object_after,
            &mut value.path_object_before,
            &mut value.path_object_after,
        ] {
            *object = ProcIdentityFact::Observed(ProcRawObject {
                inode: RawInode::new(9, 101),
                mode: libc::S_IFREG,
                mount_id: Some(10),
            });
        }
        assert_eq!(value.observed_open_subject_relation(), None); // Matching override outside the held proc mount is not a proc subject.
        let mut value = valid.clone();
        value.caller_pid_namespace_after =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::Descriptor(6),
            result: ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO)),
        });
        assert_eq!(value.observed_open_subject_relation(), None);
        let mut value = valid.clone();
        value.source_local_fd = -1;
        assert_eq!(value.observed_open_subject_relation(), None);
    }

    #[test]
    fn own_proc_identity_keeps_application_getters_out_of_proc_view_authority() {
        let mut value = observed_parent();
        value.process_link_bytes = ProcIdentityFact::Observed(b"3".to_vec());
        value.task_link_bytes = ProcIdentityFact::Observed(b"3/task/3".to_vec());
        value.injected_pid = ProcIdentityFact::Observed(17);
        value.injected_tid = ProcIdentityFact::Observed(19);
        let own = crate::fdinfo_rpc::FdinfoOwnIdentity::from_acquisition(&value).unwrap();
        assert_eq!(own.aliases.len(), 2);
        assert!(own.aliases.iter().any(|alias| alias.kind
            == crate::fdinfo_projection::FdinfoProcAliasKind::Task { pid: 3, tid: 3 }));
        assert!(
            own.aliases.iter().any(|alias| alias.kind
                == crate::fdinfo_projection::FdinfoProcAliasKind::Process { pid: 3 })
        );
        assert_eq!(own.original_parent_candidate, None);

        let valid = value.clone();
        value.task_alias = ProcIdentityFact::Observed(ProcRawAliasName::Task { pid: 4, tid: 3 });
        assert_eq!(
            crate::fdinfo_rpc::FdinfoOwnIdentity::from_acquisition(&value),
            None
        );
        value = valid.clone();
        value.process_link_bytes = ProcIdentityFact::Observed(b"03".to_vec());
        assert_eq!(
            crate::fdinfo_rpc::FdinfoOwnIdentity::from_acquisition(&value),
            None
        );
        value = valid;
        value.mounted_pid_namespace =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(
            crate::fdinfo_rpc::FdinfoOwnIdentity::from_acquisition(&value),
            None
        );
    }

    #[test]
    fn opened_proc_subject_keeps_application_getters_out_of_proc_view_authority() {
        let mut value = observed_open_subject();
        value.injected_pid = ProcIdentityFact::Observed(3);
        value.injected_tid = ProcIdentityFact::Observed(3);
        let relation = value.observed_open_subject_relation().unwrap();
        assert_eq!(relation.alias, ProcRawAliasName::Task { pid: 17, tid: 19 });
        assert_eq!(relation.target_start_ticks, 37);
        assert_eq!(relation.opened_proc_file.inode, RawInode::new(9, 101));

        let valid = value.clone();
        value.task_link_bytes = ProcIdentityFact::Observed(b"18/task/19".to_vec());
        assert_eq!(value.observed_open_subject_relation(), None);
        value = valid;
        value.caller_pid_namespace_after =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(value.observed_open_subject_relation(), None);
    }

    #[test]
    fn mount_subject_proves_self_view_without_claiming_missing_pid_or_target_authority() {
        let mut valid = observed_open_subject();
        valid.mounted_pid_namespace_before =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        valid.mounted_pid_namespace_after = valid.mounted_pid_namespace_before.clone();
        assert_eq!(valid.observed_open_subject_relation(), None);
        let relation = valid.observed_mount_subject_relation().unwrap();
        assert!(relation.matches_mount_view(&[8, 17]));
        assert!(!relation.matches_mount_view(&[17]));
        // Runtime provenance is a strictly parsed view of the same task, not
        // a requirement that the caller supplied mount IDs before launch.
        let rows = crate::procfs::parse_mountinfo(
            b"17 17 8:1 / / rw - ext4 /dev/root rw\n8 17 0:8 / /proc rw - proc proc rw\n",
        )
        .unwrap();
        let runtime_view =
            MountInfoSnapshot::new(rows, &[], false, BTreeMap::new(), BTreeMap::new()).unwrap();
        assert!(relation.matches_mount_view(&runtime_view.raw_mount_id_order()));
        assert!(!relation.matches_mount_view(&[]));
        let mut missing_root = relation;
        missing_root.procfs_root.mount_id = None;
        assert!(!missing_root.matches_mount_view(&runtime_view.raw_mount_id_order()));
        let mut zero_root = relation;
        zero_root.procfs_root.mount_id = Some(0);
        assert!(!zero_root.matches_mount_view(&runtime_view.raw_mount_id_order()));
        assert_eq!(relation.opened_proc_file.inode, RawInode::new(9, 101));
        assert_eq!(relation.target_start_ticks, 37);

        let mut value = valid.clone();
        value.selector = ProcFdinfoSubjectSelector::Task { pid: 17, tid: 19 };
        assert_eq!(value.observed_mount_subject_relation(), None);
        let mut value = valid.clone();
        value.task_link_bytes = ProcIdentityFact::Observed(b"18/task/19".to_vec());
        assert_eq!(value.observed_mount_subject_relation(), None);
        let mut value = valid.clone();
        value.source_object_after = ProcIdentityFact::Observed(ProcRawObject {
            inode: RawInode::new(9, 102),
            mode: libc::S_IFREG,
            mount_id: Some(8),
        });
        assert_eq!(value.observed_mount_subject_relation(), None);
        let mut value = valid.clone();
        value.target_after.stat = ProcIdentityFact::Observed(ProcRawTaskStat {
            pid: 19,
            ppid: 1,
            start_ticks: 38,
        });
        assert_eq!(value.observed_mount_subject_relation(), None);
        let mut value = valid.clone();
        value.caller_mount_namespace_after =
            ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EACCES));
        assert_eq!(value.observed_mount_subject_relation(), None);
        let mut value = valid;
        value.cleanup.push(ProcCleanupObservation {
            resource: ProcTemporaryResource::Descriptor(6),
            result: ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO)),
        });
        assert_eq!(value.observed_mount_subject_relation(), None);
    }

    #[test]
    fn private_fdinfo_operation_facts_preserve_full_buffers_and_refusals() {
        let operations = [
            (
                ProcIdentityOperation::Read { fd: 7, len: 64 },
                5,
                b"hello".to_vec(),
            ),
            (
                ProcIdentityOperation::Read { fd: 7, len: 64 },
                0,
                Vec::new(),
            ),
            (
                ProcIdentityOperation::Readlinkat {
                    dirfd: 9,
                    path: Some(b"thread-self".to_vec()),
                    len: 128,
                },
                9,
                b"3/task/11".to_vec(),
            ),
            (
                ProcIdentityOperation::Statx {
                    dirfd: 9,
                    path: Some(Vec::new()),
                    flags: libc::AT_EMPTY_PATH,
                    mask: libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
                },
                0,
                (0..std::mem::size_of::<libc::statx>())
                    .map(|index| index as u8)
                    .collect(),
            ),
            (
                ProcIdentityOperation::Fstatfs { fd: 9 },
                0,
                vec![0x5a; std::mem::size_of::<libc::statfs>()],
            ),
            (
                ProcIdentityOperation::DuplicateSource { fd: 7 },
                9,
                Vec::new(),
            ),
            (ProcIdentityOperation::RewindSource { fd: 9 }, 0, Vec::new()),
        ];
        for (operation, returned, bytes) in operations {
            let fact = ProcIdentityOperationFact {
                version: 1,
                operation: operation.clone(),
                result: Ok(returned),
                output: Ok(bytes.clone()),
            };
            let encoded = bincode::serde::encode_to_vec(&fact, bincode::config::legacy()).unwrap();
            let (decoded, consumed): (ProcIdentityOperationFact, usize) =
                bincode::serde::decode_from_slice(&encoded, bincode::config::legacy()).unwrap();
            assert_eq!(consumed, encoded.len());
            decoded.validate(&operation).unwrap();
            assert_eq!(decoded, fact);
            assert_eq!(decoded.output.unwrap(), bytes);
        }
        for gap in [
            ProcIdentityGap::Errno(libc::EBADF),
            ProcIdentityGap::Tool("recorder unavailable".to_owned()),
            ProcIdentityGap::Io {
                errno: Some(libc::EIO),
            },
            ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
                operation: ProcIdentityUnsupportedOperation::NonLeaderExec,
                errno: libc::EOPNOTSUPP,
                diagnostic: "explicit backend refusal".to_owned(),
                result: ProcIdentityCallResult::Errno(libc::EOPNOTSUPP),
            }),
        ] {
            let operation = ProcIdentityOperation::Read { fd: 7, len: 64 };
            let fact = ProcIdentityOperationFact {
                version: 1,
                operation: operation.clone(),
                result: Err(gap.clone()),
                output: Ok(Vec::new()),
            };
            fact.validate(&operation).unwrap();
            assert_eq!(fact.result, Err(gap));
        }
        let operation = ProcIdentityOperation::Read { fd: 7, len: 64 };
        let copied = ProcIdentityOperationFact {
            version: 1,
            operation: operation.clone(),
            result: Ok(5),
            output: Err(ProcIdentityGap::Errno(libc::EFAULT)),
        };
        copied.validate(&operation).unwrap();
        assert_eq!(copied.result, Ok(5));
        assert_eq!(copied.output, Err(ProcIdentityGap::Errno(libc::EFAULT)));
    }

    #[test]
    fn private_fdinfo_operation_replay_rejects_changed_semantics_and_truncated_outputs() {
        let operation = ProcIdentityOperation::Openat {
            dirfd: libc::AT_FDCWD,
            path: Some(b"/proc".to_vec()),
            flags: (OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC).bits(),
            mode: None,
        };
        let fact = ProcIdentityOperationFact {
            version: 1,
            operation: operation.clone(),
            result: Ok(9),
            output: Ok(Vec::new()),
        };
        fact.validate(&operation).unwrap();
        for changed in [
            ProcIdentityOperation::Openat {
                dirfd: 9,
                path: Some(b"/proc".to_vec()),
                flags: (OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC).bits(),
                mode: None,
            },
            ProcIdentityOperation::Openat {
                dirfd: libc::AT_FDCWD,
                path: Some(b"/proc/1".to_vec()),
                flags: (OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC).bits(),
                mode: None,
            },
            ProcIdentityOperation::Close { fd: 9 },
            ProcIdentityOperation::DuplicateSource { fd: 9 },
            ProcIdentityOperation::RewindSource { fd: 9 },
        ] {
            assert!(matches!(
                fact.validate(&changed),
                Err(ProcIdentityGap::Tool(_))
            ));
        }
        let operation = ProcIdentityOperation::Read { fd: 7, len: 64 };
        for (returned, output) in [(5, vec![0; 4]), (65, vec![0; 65]), (-1, Vec::new())] {
            let fact = ProcIdentityOperationFact {
                version: 1,
                operation: operation.clone(),
                result: Ok(returned),
                output: Ok(output),
            };
            assert!(matches!(
                fact.validate(&operation),
                Err(ProcIdentityGap::Tool(_))
            ));
        }
        let operation = ProcIdentityOperation::Statx {
            dirfd: 9,
            path: Some(Vec::new()),
            flags: libc::AT_EMPTY_PATH,
            mask: libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
        };
        let fact = ProcIdentityOperationFact {
            version: 1,
            operation: operation.clone(),
            result: Ok(0),
            output: Ok(vec![0; std::mem::size_of::<libc::statx>() - 1]),
        };
        assert!(matches!(
            fact.validate(&operation),
            Err(ProcIdentityGap::Tool(_))
        ));
        let operation = ProcIdentityOperation::Close { fd: 9 };
        let fact = ProcIdentityOperationFact {
            version: 1,
            operation: operation.clone(),
            result: Err(ProcIdentityGap::Errno(libc::EBADF)),
            output: Ok(vec![0]),
        };
        assert!(matches!(
            fact.validate(&operation),
            Err(ProcIdentityGap::Tool(_))
        ));
    }

    #[test]
    fn private_fdinfo_fact_errors_retain_primary_resource_and_cleanup_outcomes() {
        let mut held = Vec::new();
        hold_proc_identity_descriptor(&mut held, 9).unwrap();
        let primary = ProcIdentityGap::CaptureFact {
            result: Box::new(Ok(9)),
            primary: Box::new(ProcIdentityGap::Io {
                errno: Some(libc::ENOSPC),
            }),
        };
        assert_eq!(held, [9]);
        assert!(
            matches!(&primary, ProcIdentityGap::CaptureFact { result, primary }
            if **result == Ok(9) && **primary == ProcIdentityGap::Io { errno: Some(libc::ENOSPC) })
        );
        for returned in [-1, i64::from(i32::MAX) + 1] {
            assert!(hold_proc_identity_descriptor(&mut held, returned).is_err());
            assert_eq!(held, [9]);
        }
        let cleanup = ProcIdentityGap::Cleanup {
            primary: Box::new(primary.clone()),
            cleanup: Box::new(ProcCleanupObservation {
                resource: ProcTemporaryResource::ScratchMapping { bytes: 4096 },
                result: ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO)),
            }),
        };
        assert!(!cleanup.retry_interrupted());
        assert!(
            matches!(&cleanup, ProcIdentityGap::Cleanup { primary: original, cleanup }
            if **original == primary && cleanup.result == ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO)))
        );
        for (cleanup_result, retry) in [
            (ProcIdentityFact::Observed(0), true),
            (
                ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO)),
                false,
            ),
        ] {
            let gap = ProcIdentityGap::Cleanup {
                primary: Box::new(ProcIdentityGap::Errno(libc::EINTR)),
                cleanup: Box::new(ProcCleanupObservation {
                    resource: ProcTemporaryResource::ScratchMapping { bytes: 4096 },
                    result: cleanup_result,
                }),
            };
            assert_eq!(gap.retry_interrupted(), retry);
        }

        let bytes = b"ino: 71\nmnt_id: 419\nopaque: \xff\n".to_vec();
        let completed =
            finish_held_fdinfo_snapshot(Ok(bytes.clone()), Err(ProcIdentityGap::Errno(libc::EIO)))
                .unwrap();
        assert_eq!(completed.contents, bytes);
        let cleanup = completed.cleanup.unwrap();
        assert!(matches!(&cleanup,
            ProcIdentityGap::CleanupAfterSuccess { returned_bytes, cleanup }
            if *returned_bytes == bytes.len()
                && cleanup.result == ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO))));
        assert!(!cleanup.retry_interrupted());
        assert!(matches!(cleanup.into_error(), Error::Tool(_)));
        assert!(matches!(finish_held_fdinfo_snapshot(
            Err(ProcIdentityGap::Errno(libc::EBADF)), Err(ProcIdentityGap::Errno(libc::EIO)),
        ), Err(ProcIdentityGap::Cleanup { primary, cleanup })
            if *primary == ProcIdentityGap::Errno(libc::EBADF)
                && cleanup.result == ProcIdentityFact::Unavailable(ProcIdentityGap::Errno(libc::EIO))));

        let semantic_refusal = ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
            operation: ProcIdentityUnsupportedOperation::NonLeaderExec,
            errno: libc::EOPNOTSUPP,
            diagnostic: "private cleanup refused".to_owned(),
            result: ProcIdentityCallResult::Errno(libc::EOPNOTSUPP),
        });
        for semantic in [
            ProcIdentityGap::Errno(libc::EIO),
            ProcIdentityGap::Io {
                errno: Some(libc::EIO),
            },
            semantic_refusal.clone(),
        ] {
            assert!(semantic.cleanup_preserves_completed_read());
            let completed =
                finish_held_fdinfo_snapshot(Ok(bytes.clone()), Err(semantic.clone())).unwrap();
            assert_eq!(completed.contents, bytes);
            assert!(matches!(completed.cleanup,
                Some(ProcIdentityGap::CleanupAfterSuccess { returned_bytes, cleanup })
                if returned_bytes == bytes.len()
                    && cleanup.result == ProcIdentityFact::Unavailable(semantic)));
        }
        let mut failed_refusal = semantic_refusal;
        if let ProcIdentityGap::BackendRefusal(refusal) = &mut failed_refusal {
            refusal.result = ProcIdentityCallResult::Tool("replay hook failed".to_owned());
        }
        for fatal in [
            primary,
            ProcIdentityGap::CaptureFact {
                result: Box::new(Err(ProcIdentityGap::NotRequested)),
                primary: Box::new(ProcIdentityGap::Io {
                    errno: Some(libc::EIO),
                }),
            },
            ProcIdentityGap::Tool("replay kind mismatch".to_owned()),
            ProcIdentityGap::Malformed(ProcIdentityParseError::InvalidSyscallReturn),
            ProcIdentityGap::Io { errno: None },
            failed_refusal,
        ] {
            assert!(!fatal.cleanup_preserves_completed_read());
            let rejected = finish_held_fdinfo_snapshot(Ok(bytes.clone()), Err(fatal.clone()))
                .err()
                .unwrap();
            assert!(matches!(&rejected,
                ProcIdentityGap::CleanupAfterSuccess { returned_bytes, cleanup }
                if *returned_bytes == bytes.len()
                    && cleanup.result == ProcIdentityFact::Unavailable(fatal)));
            assert!(!rejected.retry_interrupted());
            assert!(matches!(rejected.into_error(), Error::Tool(_)));
        }
    }

    #[test]
    fn private_fdinfo_refused_open_replay_keeps_the_recorded_cleanup_token() {
        let operation = ProcIdentityOperation::Openat {
            dirfd: libc::AT_FDCWD,
            path: Some(b"/proc".to_vec()),
            flags: (OFlag::O_PATH | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC).bits(),
            mode: None,
        };
        let refusal = ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
            operation: ProcIdentityUnsupportedOperation::NonLeaderExec,
            errno: libc::EOPNOTSUPP,
            diagnostic: "actual open succeeded with a refusal marker".to_owned(),
            result: ProcIdentityCallResult::Returned(9),
        });
        let fact = ProcIdentityOperationFact {
            version: 1,
            operation: operation.clone(),
            result: Err(refusal.clone()),
            output: Ok(Vec::new()),
        };
        fact.validate(&operation).unwrap();
        let mut held = Vec::new();
        hold_proc_identity_descriptor(&mut held, proc_identity_returned(&fact.result).unwrap())
            .unwrap();
        assert_eq!(fact.result, Err(refusal.clone()));
        let close = ProcIdentityOperation::Close {
            fd: held.pop().unwrap(),
        };
        let close_fact = ProcIdentityOperationFact {
            version: 1,
            operation: close.clone(),
            result: Ok(0),
            output: Ok(Vec::new()),
        };
        close_fact
            .validate(&ProcIdentityOperation::Close { fd: 9 })
            .unwrap();
        assert_eq!(close, ProcIdentityOperation::Close { fd: 9 });
        assert!(held.is_empty());
        for returned in [-1, i64::from(i32::MAX) + 1] {
            let mut malformed = fact.clone();
            let Err(ProcIdentityGap::BackendRefusal(marker)) = &mut malformed.result else {
                panic!("missing refusal");
            };
            marker.result = ProcIdentityCallResult::Returned(returned);
            assert!(malformed.validate(&operation).is_err());
        }
        assert_eq!(
            proc_identity_returned(&Err(ProcIdentityGap::Errno(libc::EACCES))),
            None
        );
    }

    #[test]
    fn private_fdinfo_current_scratch_refusal_blocks_historical_interruption_retry() {
        let refusal = ProcIdentityGap::BackendRefusal(ProcIdentityBackendRefusal {
            operation: ProcIdentityUnsupportedOperation::NonLeaderExec,
            errno: libc::EOPNOTSUPP,
            diagnostic: "current mapping refusal".to_owned(),
            result: ProcIdentityCallResult::Returned(0x10000),
        });
        let historical = ProcIdentityGap::Errno(libc::EINTR);
        let primary = proc_identity_scratch_failure(Err(refusal.clone()), historical.clone());
        assert!(
            matches!(&primary, ProcIdentityGap::CaptureFact { result, primary }
            if **result == Err(refusal) && **primary == historical)
        );
        let cleaned = ProcIdentityGap::Cleanup {
            primary: Box::new(primary),
            cleanup: Box::new(ProcCleanupObservation {
                resource: ProcTemporaryResource::ScratchMapping { bytes: 4096 },
                result: ProcIdentityFact::Observed(0),
            }),
        };
        assert!(!cleaned.retry_interrupted());
        // A normal recorded interruption with a current successful owned map
        // still retries only after the real rollback succeeds.
        for actual in [Ok(0x10000), Err(historical.clone())] {
            let primary = proc_identity_scratch_failure(actual, historical.clone());
            let cleaned = ProcIdentityGap::Cleanup {
                primary: Box::new(primary),
                cleanup: Box::new(ProcCleanupObservation {
                    resource: ProcTemporaryResource::ScratchMapping { bytes: 4096 },
                    result: ProcIdentityFact::Observed(0),
                }),
            };
            assert!(cleaned.retry_interrupted());
        }
    }
}
