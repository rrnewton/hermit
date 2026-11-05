/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Host evidence for the inactive exec preparation API.
//!
//! A pathname's final filesystem does not establish the safety of its lookup.
//! Qualification therefore checks a complete mount namespace and inherited
//! lookup descriptors before a caller traverses its first executable pathname.
//! Likewise, CHECK is not evidence for the security hooks that it omits.
//!
//! The pure classifiers below are also used by explicitly **modeled** policy
//! tests. Those tests establish classifier behavior; they do not attest this
//! machine's privileged security, mount, integrity, or binfmt configuration.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::CStr;
use std::ffi::CString;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

/// Linux's zero-filled prefix presented to binary format handlers.
pub const BINFMT_HEADER_SIZE: usize = 256;

/// A host-policy refusal, distinct from an error returned by native exec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostPolicyRefusal {
    UnsafeLookupMount {
        mount_id: u64,
        filesystem: String,
    },
    LookupMountUnverified {
        fd: Option<RawFd>,
        mount_id: Option<u64>,
    },
    ProcEndpointUnverified {
        endpoint: String,
        reason: String,
    },
    SecurityPolicyUnverified {
        reason: String,
    },
    IntegrityAppraisalUnsupported {
        reason: String,
    },
    BpfLsmUnattested {
        reason: String,
    },
    BinfmtRegistryUnverified {
        reason: String,
    },
    BinfmtHandlerUnsupported {
        handler: String,
        flags: BinfmtFlags,
    },
    InterpreterCheckStronger {
        errno: i32,
    },
    SecureExecUnsupported,
    CapabilityPersonalityClearing,
    NamespaceInitUnsupported,
    PreContentWatchUnverified,
}

impl HostPolicyRefusal {
    /// Stable design name; reporting must retain refusal/error distinctions.
    pub fn name(&self) -> &'static str {
        match self {
            Self::UnsafeLookupMount { .. } => "UnsafeLookupMount",
            Self::LookupMountUnverified { .. } => "LookupMountUnverified",
            Self::ProcEndpointUnverified { .. } => "ProcEndpointUnverified",
            Self::SecurityPolicyUnverified { .. } => "SecurityPolicyUnverified",
            Self::IntegrityAppraisalUnsupported { .. } => "IntegrityAppraisalUnsupported",
            Self::BpfLsmUnattested { .. } => "BpfLsmUnattested",
            Self::BinfmtRegistryUnverified { .. } => "BinfmtRegistryUnverified",
            Self::BinfmtHandlerUnsupported { .. } => "BinfmtHandlerUnsupported",
            Self::InterpreterCheckStronger { .. } => "InterpreterCheckStronger",
            Self::SecureExecUnsupported => "SecureExecUnsupported",
            Self::CapabilityPersonalityClearing => "CapabilityPersonalityClearing",
            Self::NamespaceInitUnsupported => "NamespaceInitUnsupported",
            Self::PreContentWatchUnverified => "PreContentWatchUnverified",
        }
    }
}

impl fmt::Display for HostPolicyRefusal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Retain the design's refusal name, including its discriminating data.
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for HostPolicyRefusal {}

/// A decoded row of the complete `/proc/self/mountinfo` snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRecord {
    pub mount_id: u64,
    pub parent_id: u64,
    pub mount_point: Vec<u8>,
    pub filesystem: String,
}

/// A directory or O_PATH descriptor inherited by the would-be exec caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InheritedLookupFd {
    pub fd: RawFd,
    pub mount_id: u64,
}

/// An entire namespace snapshot; a target-prefix subset is not accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEvidence {
    records: Vec<MountRecord>,
    inherited_lookup_fds: Vec<InheritedLookupFd>,
    proc_namespaces: Vec<ProcNamespaceEvidence>,
}

impl MountEvidence {
    /// Parse every row from a complete mountinfo read, without target lookup.
    ///
    /// Completeness and stability of supplied bytes are part of the frozen
    /// caller contract. Parsing alone cannot prove that a caller did not remove
    /// a row from a snapshot or change its namespace afterward.
    pub fn parse_complete(
        mountinfo: &[u8],
        inherited_lookup_fds: Vec<InheritedLookupFd>,
    ) -> Result<Self, HostPolicyRefusal> {
        let unverified = || HostPolicyRefusal::LookupMountUnverified {
            fd: None,
            mount_id: None,
        };
        if mountinfo.is_empty() || !mountinfo.ends_with(b"\n") {
            return Err(unverified());
        }
        let text = std::str::from_utf8(mountinfo).map_err(|_| unverified())?;
        let mut records = Vec::new();
        let mut identifiers = BTreeSet::new();
        for row in text.lines() {
            let fields: Vec<_> = row.split_ascii_whitespace().collect();
            let separator = fields
                .iter()
                .position(|field| *field == "-")
                .ok_or_else(unverified)?;
            if separator < 6 || fields.len() != separator + 4 {
                return Err(unverified());
            }
            let mount_id = fields[0].parse::<u64>().map_err(|_| unverified())?;
            let parent_id = fields[1].parse::<u64>().map_err(|_| unverified())?;
            if mount_id == 0 || !identifiers.insert(mount_id) {
                return Err(unverified());
            }
            let mount_point = decode_mountinfo_path(fields[4]).ok_or_else(unverified)?;
            if !mount_point.starts_with(b"/") {
                return Err(unverified());
            }
            let filesystem = fields[separator + 1];
            if filesystem.is_empty() {
                return Err(unverified());
            }
            records.push(MountRecord {
                mount_id,
                parent_id,
                mount_point,
                filesystem: filesystem.to_owned(),
            });
        }
        if records.is_empty() {
            return Err(unverified());
        }
        let mut descriptors = BTreeSet::new();
        if inherited_lookup_fds
            .iter()
            .any(|entry| entry.fd < 0 || !descriptors.insert(entry.fd))
        {
            return Err(unverified());
        }
        Ok(Self {
            records,
            inherited_lookup_fds,
            proc_namespaces: Vec::new(),
        })
    }

    pub fn records(&self) -> &[MountRecord] {
        &self.records
    }

    pub fn inherited_lookup_fds(&self) -> &[InheritedLookupFd] {
        &self.inherited_lookup_fds
    }

    /// Attach individually audited proc authority to the complete snapshot.
    ///
    /// This never removes a namespace row or grants generic proc lookup. The
    /// authority must bind the exact retained endpoints to the process and
    /// procfs PID namespace. Ordinary proc objects remain unqualified.
    pub fn with_proc_namespaces(mut self, evidence: Vec<ProcNamespaceEvidence>) -> Self {
        self.proc_namespaces = evidence;
        self
    }

    pub fn proc_namespaces(&self) -> &[ProcNamespaceEvidence] {
        &self.proc_namespaces
    }

    /// Check all mounts, including mounts outside the target's textual prefix.
    pub fn qualify(&self) -> Result<(), HostPolicyRefusal> {
        let mut proc_mount_ids = BTreeSet::new();
        for evidence in &self.proc_namespaces {
            evidence.qualify()?;
            if !proc_mount_ids.insert(evidence.mount_id)
                || !self.records.iter().any(|record| {
                    record.mount_id == evidence.mount_id && record.filesystem == "proc"
                })
            {
                return Err(proc_unverified(
                    "namespace",
                    "proc mount authority is not unique or is absent from the complete snapshot",
                ));
            }
        }
        for record in &self.records {
            if !admitted_lookup_filesystem(&record.filesystem)
                && !(record.filesystem == "proc" && proc_mount_ids.contains(&record.mount_id))
            {
                return Err(HostPolicyRefusal::UnsafeLookupMount {
                    mount_id: record.mount_id,
                    filesystem: record.filesystem.clone(),
                });
            }
        }
        for entry in &self.inherited_lookup_fds {
            if self.verify_mount_id(entry.mount_id).is_err() {
                return Err(HostPolicyRefusal::LookupMountUnverified {
                    fd: Some(entry.fd),
                    mount_id: Some(entry.mount_id),
                });
            }
        }
        Ok(())
    }

    /// Check an object against the already-qualified namespace membership.
    pub fn verify_mount_id(&self, mount_id: u64) -> Result<(), HostPolicyRefusal> {
        let record = self
            .records
            .iter()
            .find(|entry| entry.mount_id == mount_id)
            .ok_or(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: Some(mount_id),
            })?;
        if !admitted_lookup_filesystem(&record.filesystem) {
            return Err(HostPolicyRefusal::UnsafeLookupMount {
                mount_id,
                filesystem: record.filesystem.clone(),
            });
        }
        Ok(())
    }
}

/// The complete audited generic-lookup allowlist in approved design F2.
pub fn admitted_lookup_filesystem(filesystem: &str) -> bool {
    matches!(
        filesystem,
        "ext4" | "xfs" | "btrfs" | "tmpfs" | "squashfs" | "erofs"
    )
}

fn decode_mountinfo_path(path: &str) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut remaining = path.as_bytes();
    while let Some((&first, tail)) = remaining.split_first() {
        if first == b'\\' {
            let escape = remaining.get(1..4)?;
            let byte = match escape {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return None,
            };
            decoded.push(byte);
            remaining = &remaining[4..];
        } else {
            decoded.push(first);
            remaining = tail;
        }
    }
    Some(decoded)
}

/// Exact proc identities admitted by the F2 bootstrap exception.
///
/// A genuine procfs superblock alone does not identify the caller or the PID
/// namespace used by `self`. This evidence binds both, the current mount/user
/// namespaces, and every retained endpoint. The receipt must establish those
/// facts for the frozen lifetime; no generic proc pathname is authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcNamespaceEvidence {
    pub mount_id: u64,
    pub root_inode: u64,
    pub process_inode: u64,
    pub fd_directory_inode: u64,
    pub fdinfo_directory_inode: u64,
    pub executable_link_inode: u64,
    pub mountinfo_inode: u64,
    pub status_inode: u64,
    pub mount_namespace: u64,
    pub user_namespace: u64,
    pub pid_namespace: u64,
    /// PID namespace of the procfs superblock, proved by bootstrap authority.
    pub proc_pid_namespace: u64,
    /// Caller PID as seen in that namespace, rather than an outer host PID.
    pub namespace_pid: u32,
    pub receipt: PolicyReceipt,
}

impl ProcNamespaceEvidence {
    fn qualify(&self) -> Result<(), HostPolicyRefusal> {
        if self.mount_id == 0
            || self.namespace_pid == 0
            || [
                self.root_inode,
                self.process_inode,
                self.fd_directory_inode,
                self.fdinfo_directory_inode,
                self.executable_link_inode,
                self.mountinfo_inode,
                self.status_inode,
                self.mount_namespace,
                self.user_namespace,
                self.pid_namespace,
            ]
            .contains(&0)
            || self.proc_pid_namespace != self.pid_namespace
            || !self.receipt.has_provenance()
        {
            return Err(proc_unverified(
                "namespace",
                "proc/task/namespace identities or lifetime provenance are incomplete",
            ));
        }
        Ok(())
    }

    fn matches_context(&self, context: &ProcessContext) -> bool {
        self.namespace_pid == context.namespace_pid
            && self.mount_namespace == context.mount_namespace
            && self.user_namespace == context.user_namespace
            && self.pid_namespace == context.pid_namespace
    }
}

/// Bootstrap-owned descriptors opened before the preparation window.
///
/// Their path lookup and open must already have been qualified by the trusted
/// bootstrap. Passing an arbitrary descriptor never qualifies its old lookup.
/// The library consumes these descriptors and does not discover them by path.
#[derive(Debug)]
pub struct RetainedProcEndpoints {
    pub root: File,
    pub process: File,
    pub fd_directory: File,
    pub fdinfo_directory: File,
    /// O_PATH|O_NOFOLLOW pin of the genuine proc `self/exe` magic link.
    pub executable_link: File,
    /// O_PATH pin of the executable reached by that exact audited magic link.
    pub executable: File,
    pub mountinfo: File,
    pub status: File,
    /// Retained namespace handles reached from the genuine caller proc task.
    pub mount_namespace: File,
    pub user_namespace: File,
    pub pid_namespace: File,
}

/// Retained, already-qualified exact proc endpoints; no pathname constructor.
#[derive(Debug)]
pub struct QualifiedProcEndpoints {
    descriptors: RetainedProcEndpoints,
    evidence: ProcNamespaceEvidence,
}

impl QualifiedProcEndpoints {
    /// Verify retained endpoint facts without opening any pathname.
    ///
    /// # Safety
    ///
    /// Every descriptor must already have qualified lookup/open provenance in
    /// the actual frozen namespace. `root` is genuine procfs; `process` is its
    /// exact directory for the current task; `fd_directory`, `fdinfo_directory`,
    /// `executable_link`, `mountinfo`, `status` and the namespace handles are
    /// the genuine corresponding children, with no replacement bind mount.
    /// `executable` was reached through that exact magic link. The caller must
    /// prove the proc superblock's PID namespace, the task/PID mapping and the
    /// frozen lifetime using the receipt. It must retain the same task, root,
    /// namespaces and serialized descriptor table for every use. A modeled
    /// receipt is only evidence for an explicitly labeled inactive fixture.
    pub unsafe fn from_retained(
        descriptors: RetainedProcEndpoints,
        namespace_pid: u32,
        proc_pid_namespace: u64,
        receipt: PolicyReceipt,
    ) -> Result<Self, HostPolicyRefusal> {
        if !receipt.has_provenance() || namespace_pid == 0 || proc_pid_namespace == 0 {
            return Err(proc_unverified(
                "bootstrap",
                "missing qualified endpoint provenance",
            ));
        }
        let root = retained_metadata(&descriptors.root, "root")?;
        let process = retained_metadata(&descriptors.process, "self")?;
        let directory = retained_metadata(&descriptors.fd_directory, "self/fd")?;
        let fdinfo = retained_metadata(&descriptors.fdinfo_directory, "self/fdinfo")?;
        let executable_link = retained_metadata(&descriptors.executable_link, "self/exe link")?;
        let mountinfo = retained_metadata(&descriptors.mountinfo, "self/mountinfo")?;
        let status = retained_metadata(&descriptors.status, "self/status")?;
        for (name, metadata, expected_type) in [
            ("root", &root, libc::S_IFDIR),
            ("self", &process, libc::S_IFDIR),
            ("self/fd", &directory, libc::S_IFDIR),
            ("self/fdinfo", &fdinfo, libc::S_IFDIR),
            ("self/exe link", &executable_link, libc::S_IFLNK),
            ("self/mountinfo", &mountinfo, libc::S_IFREG),
            ("self/status", &status, libc::S_IFREG),
        ] {
            if metadata.filesystem != PROC_SUPER_MAGIC
                || metadata.mount_id != root.mount_id
                || metadata.mode & libc::S_IFMT != expected_type
            {
                return Err(proc_unverified(
                    name,
                    "descriptor is not its attested genuine proc endpoint",
                ));
            }
        }
        let namespace = |file: &File, name: &str| -> Result<u64, HostPolicyRefusal> {
            let metadata = retained_metadata(file, name)?;
            if metadata.filesystem != NSFS_MAGIC {
                return Err(proc_unverified(
                    name,
                    "descriptor is not a genuine namespace handle",
                ));
            }
            Ok(metadata.inode)
        };
        let evidence = ProcNamespaceEvidence {
            mount_id: root.mount_id,
            root_inode: root.inode,
            process_inode: process.inode,
            fd_directory_inode: directory.inode,
            fdinfo_directory_inode: fdinfo.inode,
            executable_link_inode: executable_link.inode,
            mountinfo_inode: mountinfo.inode,
            status_inode: status.inode,
            mount_namespace: namespace(&descriptors.mount_namespace, "self/ns/mnt")?,
            user_namespace: namespace(&descriptors.user_namespace, "self/ns/user")?,
            pid_namespace: namespace(&descriptors.pid_namespace, "self/ns/pid")?,
            proc_pid_namespace,
            namespace_pid,
            receipt,
        };
        evidence.qualify()?;
        let executable = retained_metadata(&descriptors.executable, "self/exe target")?;
        if executable.mode & libc::S_IFMT != libc::S_IFREG {
            return Err(proc_unverified(
                "self/exe target",
                "current executable is not a regular file",
            ));
        }
        // Reading is allowed only after every retained endpoint has been
        // classified. This status read cannot traverse or open another path.
        let status = read_retained(&descriptors.status, "self/status")?;
        let actual_pid = std::str::from_utf8(&status)
            .ok()
            .and_then(|text| text.lines().find_map(|line| line.strip_prefix("NSpid:")))
            .and_then(|value| value.split_ascii_whitespace().last())
            .and_then(|value| value.parse::<u32>().ok());
        if actual_pid != Some(namespace_pid) {
            return Err(proc_unverified(
                "self/status",
                "retained status names a different namespace PID",
            ));
        }
        Ok(Self {
            descriptors,
            evidence,
        })
    }

    pub fn evidence(&self) -> &ProcNamespaceEvidence {
        &self.evidence
    }

    pub fn fd_directory(&self) -> RawFd {
        self.descriptors.fd_directory.as_raw_fd()
    }

    pub fn executable(&self) -> RawFd {
        self.descriptors.executable.as_raw_fd()
    }

    fn owned_fds(&self) -> BTreeSet<RawFd> {
        [
            &self.descriptors.root,
            &self.descriptors.process,
            &self.descriptors.fd_directory,
            &self.descriptors.fdinfo_directory,
            &self.descriptors.executable_link,
            &self.descriptors.executable,
            &self.descriptors.mountinfo,
            &self.descriptors.status,
            &self.descriptors.mount_namespace,
            &self.descriptors.user_namespace,
            &self.descriptors.pid_namespace,
        ]
        .iter()
        .map(|file| file.as_raw_fd())
        .collect()
    }
}

const PROC_SUPER_MAGIC: i64 = 0x9fa0;
const NSFS_MAGIC: i64 = 0x6e73_6673;

struct RetainedMetadata {
    mount_id: u64,
    inode: u64,
    mode: u32,
    filesystem: i64,
}

fn proc_unverified(endpoint: &str, reason: &str) -> HostPolicyRefusal {
    HostPolicyRefusal::ProcEndpointUnverified {
        endpoint: endpoint.to_owned(),
        reason: reason.to_owned(),
    }
}

fn retained_metadata(file: &File, endpoint: &str) -> Result<RetainedMetadata, HostPolicyRefusal> {
    // SAFETY: exact ABI output buffers and NUL-terminated empty pathname.
    let mut metadata: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: read-only metadata of an already-qualified retained descriptor.
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_DONT_SYNC,
            libc::STATX_TYPE | libc::STATX_INO | libc::STATX_MNT_ID,
            &mut metadata,
        )
    } != 0
        || metadata.stx_mask & (libc::STATX_TYPE | libc::STATX_INO | libc::STATX_MNT_ID)
            != libc::STATX_TYPE | libc::STATX_INO | libc::STATX_MNT_ID
    {
        return Err(proc_unverified(
            endpoint,
            "retained descriptor identity is unreadable",
        ));
    }
    // SAFETY: exact ABI output buffer; fstatfs queries only the retained FD.
    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: no pathname lookup and file remains owned by the bootstrap.
    if unsafe { libc::fstatfs(file.as_raw_fd(), &mut filesystem) } != 0 {
        return Err(proc_unverified(
            endpoint,
            "retained descriptor filesystem is unreadable",
        ));
    }
    Ok(RetainedMetadata {
        mount_id: metadata.stx_mnt_id,
        inode: metadata.stx_ino,
        mode: metadata.stx_mode as u32,
        filesystem: filesystem.f_type,
    })
}

fn read_retained(file: &File, endpoint: &str) -> Result<Vec<u8>, HostPolicyRefusal> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = file
            .read_at(&mut buffer, bytes.len() as u64)
            .map_err(|_| proc_unverified(endpoint, "retained endpoint read failed"))?;
        if count == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

/// Whether a receipt describes actual host facts or a declared test model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EvidenceOrigin {
    Live,
    Modeled { fixture: String },
}

/// Provenance and the lifetime boundary of an externally approved policy.
///
/// A one-time listing is insufficient. The receipt's scope must cover policy
/// changes, attachments, cgroup scope and policy-map updates for the entire run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyReceipt {
    pub approval_id: String,
    pub scope: String,
    pub lifetime: String,
    pub policy_digest: [u8; 32],
    pub generation: u64,
}

impl PolicyReceipt {
    fn has_provenance(&self) -> bool {
        !self.approval_id.trim().is_empty()
            && !self.scope.trim().is_empty()
            && !self.lifetime.trim().is_empty()
            && self.policy_digest != [0; 32]
    }
}

/// Omitted and additional exec hooks requiring target/launcher equivalence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecSecurityHook {
    FileOpen,
    FilePermission,
    BprmCredsForExec,
    BprmCheck,
    BprmCredsFromFile,
    BprmCommittingCreds,
    BprmCommittedCreds,
    MmapFile,
}

const REQUIRED_EXEC_HOOKS: [ExecSecurityHook; 8] = [
    ExecSecurityHook::FileOpen,
    ExecSecurityHook::FilePermission,
    ExecSecurityHook::BprmCredsForExec,
    ExecSecurityHook::BprmCheck,
    ExecSecurityHook::BprmCredsFromFile,
    ExecSecurityHook::BprmCommittingCreds,
    ExecSecurityHook::BprmCommittedCreds,
    ExecSecurityHook::MmapFile,
];

/// Stable privileged evidence covering every relevant BPF exec hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BpfAttestation {
    pub receipt: PolicyReceipt,
    pub context_digest: [u8; 32],
    pub covered_hooks: Vec<ExecSecurityHook>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BpfEvidence {
    Inactive,
    Unattested,
    Attested(BpfAttestation),
}

/// Measurement is disclosed separately; enforcing appraisal is not emulated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntegrityEvidence {
    Inactive,
    /// Receipt proves no enforcing IMA/EVM appraisal for the run lifetime.
    MeasurementOnly {
        receipt: PolicyReceipt,
    },
    /// Includes BPRM-only appraisal, even if a prior CHECK appraised the inode.
    EnforcingAppraisal {
        policy: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecurityEvidence {
    /// `None` records unreadable or malformed LSM authority.
    pub active_lsms: Option<Vec<String>>,
    pub bpf: BpfEvidence,
    pub integrity: IntegrityEvidence,
}

impl SecurityEvidence {
    pub fn qualify(&self, context_digest: &[u8; 32]) -> Result<(), HostPolicyRefusal> {
        let lsms = self.active_lsms.as_ref().ok_or_else(|| {
            HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "active LSM authority is unreadable".to_owned(),
            }
        })?;
        if lsms.is_empty() || lsms.iter().collect::<BTreeSet<_>>().len() != lsms.len() {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "active LSM authority is empty or repeats a module".to_owned(),
            });
        }
        if let Some(module) = lsms
            .iter()
            .find(|module| !matches!(module.as_str(), "capability" | "bpf" | "ima"))
        {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: format!("omitted exec hooks of active module {module} are unproved"),
            });
        }
        if lsms.iter().any(|module| module == "bpf") {
            let valid = match &self.bpf {
                BpfEvidence::Attested(proof) => {
                    proof.receipt.has_provenance()
                        && proof.context_digest != [0; 32]
                        && &proof.context_digest == context_digest
                        && REQUIRED_EXEC_HOOKS
                            .iter()
                            .all(|hook| proof.covered_hooks.contains(hook))
                }
                _ => false,
            };
            if !valid {
                return Err(HostPolicyRefusal::BpfLsmUnattested {
                    reason: "stable approved hook/context attestation is missing".to_owned(),
                });
            }
        } else if !matches!(self.bpf, BpfEvidence::Inactive) {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "BPF evidence disagrees with active LSM authority".to_owned(),
            });
        }
        match &self.integrity {
            IntegrityEvidence::Inactive if !lsms.iter().any(|module| module == "ima") => {}
            IntegrityEvidence::MeasurementOnly { receipt } if receipt.has_provenance() => {}
            IntegrityEvidence::EnforcingAppraisal { policy } => {
                return Err(HostPolicyRefusal::IntegrityAppraisalUnsupported {
                    reason: format!(
                        "appraisal continuity and native transitions are unproved: {policy}"
                    ),
                });
            }
            _ => {
                return Err(HostPolicyRefusal::IntegrityAppraisalUnsupported {
                    reason: "integrity authority or absence of appraisal is unproved".to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// Calling-process facts, interpreted in its actual guest namespaces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessContext {
    pub namespace_pid: u32,
    pub mount_namespace: u64,
    pub user_namespace: u64,
    pub pid_namespace: u64,
    pub real_uid: u32,
    pub effective_uid: u32,
    pub real_gid: u32,
    pub effective_gid: u32,
    pub securebits: u32,
    pub inheritable_capabilities: u64,
    pub bounding_capabilities: u64,
    pub permitted_capabilities: u64,
    pub no_new_privs: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecContextEvidence {
    Current(ProcessContext),
    Unreadable { reason: String },
}

impl ProcessContext {
    pub fn qualify(&self) -> Result<(), HostPolicyRefusal> {
        if self.namespace_pid == 1 {
            return Err(HostPolicyRefusal::NamespaceInitUnsupported);
        }
        if self.namespace_pid == 0
            || self.mount_namespace == 0
            || self.user_namespace == 0
            || self.pid_namespace == 0
        {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "process namespace context is incomplete".to_owned(),
            });
        }
        if self.real_uid != self.effective_uid || self.real_gid != self.effective_gid {
            return Err(HostPolicyRefusal::SecureExecUnsupported);
        }
        if capability_personality_clearing(self) {
            return Err(HostPolicyRefusal::CapabilityPersonalityClearing);
        }
        Ok(())
    }
}

/// Linux commoncap sets PER_CLEAR_ON_SETID before applying NNP clipping.
///
/// UID values are those in the current user namespace. NNP deliberately does
/// not enter this formula: it cannot undo an already selected personality bit.
pub fn capability_personality_clearing(context: &ProcessContext) -> bool {
    const SECURE_NOROOT_MASK: u32 = 1;
    (context.real_uid == 0 || context.effective_uid == 0)
        && context.securebits & SECURE_NOROOT_MASK == 0
        && (context.inheritable_capabilities | context.bounding_capabilities)
            & !context.permitted_capabilities
            != 0
}

/// Setid/filecap refusal is conservative under nosuid, NNP and idmapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityXattrEvidence {
    Absent,
    /// An empty xattr is still present; its contents do not waive refusal.
    Present {
        bytes: Vec<u8>,
        namespaced: bool,
    },
    Unreadable,
}

pub fn qualify_executable_privileges(
    mode: u32,
    capabilities: &CapabilityXattrEvidence,
) -> Result<(), HostPolicyRefusal> {
    if mode & (libc::S_ISUID | libc::S_ISGID) != 0
        || matches!(capabilities, CapabilityXattrEvidence::Present { .. })
    {
        return Err(HostPolicyRefusal::SecureExecUnsupported);
    }
    if matches!(capabilities, CapabilityXattrEvidence::Unreadable) {
        return Err(HostPolicyRefusal::SecurityPolicyUnverified {
            reason: "capability xattr authority is unreadable".to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreContentWatchEvidence {
    AbsentForLifetime(PolicyReceipt),
    NativeEquivalentForLifetime(PolicyReceipt),
    Unverified,
}

impl PreContentWatchEvidence {
    fn qualify(&self) -> Result<(), HostPolicyRefusal> {
        match self {
            Self::AbsentForLifetime(receipt) | Self::NativeEquivalentForLifetime(receipt)
                if receipt.has_provenance() =>
            {
                Ok(())
            }
            _ => Err(HostPolicyRefusal::PreContentWatchUnverified),
        }
    }
}

/// The kernel flags are retained even though matching handlers are refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BinfmtFlags {
    pub preserve_argv0: bool,
    pub open_binary: bool,
    pub credentials: bool,
    pub fixed_interpreter: bool,
}

impl BinfmtFlags {
    pub fn parse(flags: &str) -> Result<Self, HostPolicyRefusal> {
        let mut result = Self::default();
        for flag in flags.bytes() {
            match flag {
                b'P' => result.preserve_argv0 = true,
                b'O' => result.open_binary = true,
                b'C' => {
                    result.credentials = true;
                    result.open_binary = true;
                }
                b'F' => result.fixed_interpreter = true,
                _ => {
                    return Err(HostPolicyRefusal::BinfmtRegistryUnverified {
                        reason: "unknown binfmt flag".to_owned(),
                    });
                }
            }
        }
        Ok(result)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinfmtMatch {
    Magic {
        offset: usize,
        magic: Vec<u8>,
        mask: Option<Vec<u8>>,
    },
    Extension(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinfmtEntry {
    pub name: String,
    pub enabled: bool,
    pub rule: BinfmtMatch,
    pub interpreter: Vec<u8>,
    pub flags: BinfmtFlags,
}

impl BinfmtEntry {
    fn validate(&self) -> Result<(), HostPolicyRefusal> {
        let malformed = || HostPolicyRefusal::BinfmtRegistryUnverified {
            reason: format!("malformed binfmt entry {}", self.name),
        };
        if self.name.is_empty()
            || self.interpreter.is_empty()
            || self.interpreter.contains(&0)
            || (self.flags.credentials && !self.flags.open_binary)
        {
            return Err(malformed());
        }
        match &self.rule {
            BinfmtMatch::Magic {
                offset,
                magic,
                mask,
            } => {
                if magic.is_empty()
                    || offset
                        .checked_add(magic.len())
                        .is_none_or(|end| end > BINFMT_HEADER_SIZE)
                    || mask.as_ref().is_some_and(|mask| mask.len() != magic.len())
                {
                    return Err(malformed());
                }
            }
            BinfmtMatch::Extension(extension) => {
                if extension.is_empty() || extension.contains(&0) || extension.contains(&b'/') {
                    return Err(malformed());
                }
            }
        }
        Ok(())
    }

    /// Match the current bprm interp, not the original exec filename F.
    pub fn matches(&self, header: &[u8], bprm_interp: &[u8]) -> bool {
        if !self.enabled || self.validate().is_err() {
            return false;
        }
        match &self.rule {
            BinfmtMatch::Extension(extension) => bprm_interp
                .iter()
                .rposition(|byte| *byte == b'.')
                .is_some_and(|dot| &bprm_interp[dot + 1..] == extension),
            BinfmtMatch::Magic {
                offset,
                magic,
                mask,
            } => magic.iter().enumerate().all(|(index, expected)| {
                let actual = header.get(offset + index).copied().unwrap_or(0);
                let selected = mask.as_ref().map_or(0xff, |mask| mask[index]);
                (actual ^ expected) & selected == 0
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinfmtRegistryIdentity {
    pub owner_user_namespace: u64,
    pub mount_id: u64,
    pub policy_digest: [u8; 32],
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BinfmtRegistry {
    pub identity: BinfmtRegistryIdentity,
    pub enabled: bool,
    pub entries: Vec<BinfmtEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceBinfmtEvidence {
    pub user_namespace: u64,
    pub parent: Option<u64>,
    /// `None` means proven absent, not merely absent from a visible path.
    pub registry: Option<BinfmtRegistry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BinfmtAuthority {
    Unverified {
        reason: String,
    },
    /// Full caller-to-initial-userns ancestry and the visible registry binding.
    NearestAncestor {
        ancestry: Vec<NamespaceBinfmtEvidence>,
        visible_registry: BinfmtRegistryIdentity,
    },
}

impl BinfmtAuthority {
    pub fn registry(&self, user_namespace: u64) -> Result<&BinfmtRegistry, HostPolicyRefusal> {
        let unverified = |reason: &str| HostPolicyRefusal::BinfmtRegistryUnverified {
            reason: reason.to_owned(),
        };
        let Self::NearestAncestor {
            ancestry,
            visible_registry,
        } = self
        else {
            return Err(unverified("authoritative registry ancestry is unreadable"));
        };
        if ancestry.first().map(|entry| entry.user_namespace) != Some(user_namespace)
            || ancestry.last().is_none_or(|entry| entry.parent.is_some())
        {
            return Err(unverified("incomplete user namespace ancestry"));
        }
        let mut identifiers = BTreeSet::new();
        for (index, entry) in ancestry.iter().enumerate() {
            if entry.user_namespace == 0 || !identifiers.insert(entry.user_namespace) {
                return Err(unverified("invalid or repeated namespace identity"));
            }
            if let Some(next) = ancestry.get(index + 1)
                && entry.parent != Some(next.user_namespace)
            {
                return Err(unverified("namespace parent relation changed"));
            }
            if let Some(registry) = &entry.registry {
                if registry.identity.owner_user_namespace != entry.user_namespace
                    || registry.identity.mount_id == 0
                    || registry.identity.policy_digest == [0; 32]
                {
                    return Err(unverified("registry owner/provenance is unproved"));
                }
                let mut names = BTreeSet::new();
                for handler in &registry.entries {
                    handler.validate()?;
                    if !names.insert(&handler.name) {
                        return Err(unverified("registry repeats an entry"));
                    }
                }
            }
        }
        // Linux selects the nearest ancestor with an instantiated registry,
        // even when that registry is globally disabled or has no entries.
        let registry = ancestry
            .iter()
            .find_map(|entry| entry.registry.as_ref())
            .ok_or_else(|| unverified("initial registry state is unproved"))?;
        if &registry.identity != visible_registry {
            return Err(unverified(
                "visible registry is hidden or mismatches authority",
            ));
        }
        Ok(registry)
    }

    /// Call this again after every #! rewrite and for the final ELF.
    pub fn check_stage(
        &self,
        user_namespace: u64,
        header: &[u8],
        bprm_interp: &[u8],
    ) -> Result<(), HostPolicyRefusal> {
        let registry = self.registry(user_namespace)?;
        if registry.enabled {
            for entry in &registry.entries {
                if entry.matches(header, bprm_interp) {
                    return Err(HostPolicyRefusal::BinfmtHandlerUnsupported {
                        handler: entry.name.clone(),
                        flags: entry.flags,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Proven native provenance of an interpreter fd-CHECK denial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterpreterDenialSource {
    Regularity,
    ExecuteDac,
    NoexecMount,
    WriterHeld,
    BprmOrUnknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpreterAuthorizationOutcome {
    NativeErrno(i32),
    Refuse(HostPolicyRefusal),
}

/// PT_INTERP native open_exec does not run an independent bprm sequence.
pub fn classify_interpreter_denial(
    errno: i32,
    source: InterpreterDenialSource,
) -> InterpreterAuthorizationOutcome {
    let native = match source {
        InterpreterDenialSource::Regularity
        | InterpreterDenialSource::ExecuteDac
        | InterpreterDenialSource::NoexecMount => errno == libc::EACCES,
        InterpreterDenialSource::WriterHeld => errno == libc::ETXTBSY,
        InterpreterDenialSource::BprmOrUnknown => false,
    };
    if native {
        InterpreterAuthorizationOutcome::NativeErrno(errno)
    } else {
        InterpreterAuthorizationOutcome::Refuse(HostPolicyRefusal::InterpreterCheckStronger {
            errno,
        })
    }
}

/// Pure snapshots needed before an executable's first pathname traversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostEvidence {
    pub mounts: MountEvidence,
    pub security: SecurityEvidence,
    pub binfmt: BinfmtAuthority,
    pub context: ExecContextEvidence,
    pub watches: PreContentWatchEvidence,
    /// Individually audited anonymous endpoints, not generic detached mounts.
    pub sealed_memfds: Vec<SealedMemfdEvidence>,
}

/// An immutable anonymous tmpfs inode explicitly bound to the host receipt.
///
/// Namespace membership cannot describe an anonymous memfd. Admitting its
/// inode/seals separately does not admit detached directory or O_PATH mounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedMemfdEvidence {
    /// Anonymous memfds can report a present STATX_MNT_ID of zero. This is
    /// meaningful only with this individually attested inode and seal set;
    /// it never admits mount ID zero to the ordinary namespace lookup policy.
    pub mount_id: u64,
    pub inode: u64,
    pub seals: i32,
    pub receipt: PolicyReceipt,
}

const IMMUTABLE_MEMFD_SEALS: i32 =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// An external lifetime proof, never inferred from a successful CHECK.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrozenHostAttestation {
    pub origin: EvidenceOrigin,
    pub receipt: PolicyReceipt,
    pub context_digest: [u8; 32],
}

/// An already-qualified O_PATH object for an admitted namespace mount point.
///
/// Bootstrap must pin the exact mount root, not another inode in the same
/// mount. A directory permits later resolution below it with NO_XDEV. A bind
/// mount of a leaf object permits only that exact object, without traversal.
/// Exception-only proc objects and symlink roots are never admitted here.
#[derive(Debug)]
pub struct RetainedLookupRoot {
    pub mount_point: Vec<u8>,
    /// O_PATH handle to the mount root, which may be a directory or a leaf.
    /// The field name is retained for callers of the directory-only API.
    pub directory: File,
}

/// Qualified frozen host evidence. No constructor silently waives a refusal.
#[derive(Clone, Debug)]
pub struct HostQualification {
    evidence: HostEvidence,
    attestation: FrozenHostAttestation,
    proc_endpoints: Option<Arc<QualifiedProcEndpoints>>,
    lookup_roots: Option<Arc<Vec<RetainedLookupRoot>>>,
    lookup_root_modes: BTreeMap<Vec<u8>, (u64, u32)>,
}

impl HostQualification {
    /// Validate an externally frozen and attested evidence snapshot.
    ///
    /// # Safety
    ///
    /// For real preparation, the caller must supply complete genuine evidence
    /// for the actual caller's mount/user/PID namespaces, inherited lookup FDs,
    /// credentials, security/integrity/binfmt policy and watch state. It must
    /// bind the receipt to these facts and guarantee they remain unchanged
    /// through the run, including external policy-map and attachment changes.
    /// The receipt must identify the coordinator approval that authorizes any
    /// BPF policy equivalence claim. A modeled origin may only be used by an
    /// explicitly labeled inactive model/test; it is not an activation proof.
    /// The launcher must separately prove the thread-group-leader, signal and
    /// foreign-seccomp exclusions documented on [`crate::prepare_exec`]; this
    /// host-policy receipt does not establish them.
    pub unsafe fn from_frozen_evidence(
        evidence: HostEvidence,
        attestation: FrozenHostAttestation,
    ) -> Result<Self, HostPolicyRefusal> {
        if !attestation.receipt.has_provenance()
            || attestation.context_digest == [0; 32]
            || matches!(&attestation.origin, EvidenceOrigin::Modeled { fixture } if fixture.is_empty())
        {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "frozen evidence lacks approval, digest, context or lifetime provenance"
                    .to_owned(),
            });
        }
        qualify_host_evidence(&evidence, &attestation.context_digest)?;
        Ok(Self {
            evidence,
            attestation,
            proc_endpoints: None,
            lookup_roots: None,
            lookup_root_modes: BTreeMap::new(),
        })
    }

    /// Bind the retained bootstrap authority to this exact frozen snapshot.
    ///
    /// Retention prevents rebinding `/proc`, `self`, or `fd` from redirecting
    /// helper operations. Only the audited numeric FD child and executable
    /// endpoint are exposed; the proc mount never becomes a generic allowlist.
    pub fn with_proc_endpoints(
        mut self,
        endpoints: Arc<QualifiedProcEndpoints>,
    ) -> Result<Self, HostPolicyRefusal> {
        if !self
            .evidence
            .mounts
            .proc_namespaces()
            .contains(endpoints.evidence())
            || !endpoints.evidence().matches_context(self.context())
        {
            return Err(proc_unverified(
                "bootstrap",
                "retained endpoints differ from the frozen namespace authority",
            ));
        }
        let executable = retained_metadata(&endpoints.descriptors.executable, "self/exe target")?;
        self.check_pinned_mount(executable.mount_id, executable.filesystem)?;
        self.proc_endpoints = Some(endpoints);
        Ok(self)
    }

    /// Genuine retained `/proc/self/fd`, exclusively for numeric helper FDs.
    pub fn proc_fd_directory(&self) -> Result<RawFd, HostPolicyRefusal> {
        self.proc_endpoints
            .as_ref()
            .map(|endpoints| endpoints.fd_directory())
            .ok_or_else(|| proc_unverified("self/fd", "qualified retained endpoint is missing"))
    }

    /// Current executable pinned through the genuine audited proc magic link.
    pub fn proc_executable(&self) -> Result<RawFd, HostPolicyRefusal> {
        self.proc_endpoints
            .as_ref()
            .map(|endpoints| endpoints.executable())
            .ok_or_else(|| proc_unverified("self/exe", "qualified retained endpoint is missing"))
    }

    /// Share retained authority without reopening or duplicating a descriptor.
    pub fn retained_proc_endpoints(&self) -> Option<Arc<QualifiedProcEndpoints>> {
        self.proc_endpoints.clone()
    }

    /// Identify bootstrap handles without opening or duplicating any object.
    ///
    /// These handles are helper state, not part of the original guest's
    /// descriptor view for literal `/proc/self/fd/N` requests.
    pub(crate) fn owns_bootstrap_fd(&self, fd: RawFd) -> bool {
        self.proc_endpoints
            .as_ref()
            .is_some_and(|endpoints| endpoints.owned_fds().contains(&fd))
            || self
                .lookup_roots
                .as_ref()
                .is_some_and(|roots| roots.iter().any(|root| root.directory.as_raw_fd() == fd))
    }

    /// Attach admitted mount roots without opening any supplied pathname.
    ///
    /// # Safety
    ///
    /// Every object's lookup/open provenance must already be qualified by
    /// bootstrap. It is the exact object at the stated mount point in this
    /// complete frozen namespace, rather than another inode in that mount.
    /// Task/root/namespaces and these endpoints must remain unchanged for the
    /// lifetime of all lookups. A declared model is not activation authority.
    pub unsafe fn with_lookup_roots(
        self,
        roots: Vec<RetainedLookupRoot>,
    ) -> Result<Self, HostPolicyRefusal> {
        // SAFETY: the caller's contract covers these exact retained roots.
        unsafe { self.with_retained_lookup_roots(Arc::new(roots)) }
    }

    /// Reuse already retained mount roots without duplicating descriptors.
    ///
    /// # Safety
    ///
    /// The complete [`Self::with_lookup_roots`] provenance and frozen-lifetime
    /// contract applies to every retained object and this host snapshot.
    pub unsafe fn with_retained_lookup_roots(
        mut self,
        roots: Arc<Vec<RetainedLookupRoot>>,
    ) -> Result<Self, HostPolicyRefusal> {
        let mut points = BTreeSet::new();
        let mut modes = BTreeMap::new();
        for root in roots.iter() {
            let fd = root.directory.as_raw_fd();
            if !root.mount_point.starts_with(b"/")
                || root.mount_point.contains(&0)
                || !points.insert(root.mount_point.clone())
            {
                return Err(HostPolicyRefusal::LookupMountUnverified {
                    fd: Some(fd),
                    mount_id: None,
                });
            }
            let metadata =
                retained_metadata(&root.directory, "admitted lookup root").map_err(|_| {
                    HostPolicyRefusal::LookupMountUnverified {
                        fd: Some(fd),
                        mount_id: None,
                    }
                })?;
            let record = self.evidence.mounts.records().iter().find(|record| {
                record.mount_point == root.mount_point && record.mount_id == metadata.mount_id
            });
            // SAFETY: F_GETFL is a read-only query on this retained object.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if metadata.mode & libc::S_IFMT == libc::S_IFLNK
                || flags < 0
                || flags & libc::O_PATH == 0
                || !record.is_some_and(|record| {
                    admitted_lookup_filesystem(&record.filesystem)
                        && filesystem_magic(&record.filesystem) == Some(metadata.filesystem)
                })
            {
                return Err(HostPolicyRefusal::LookupMountUnverified {
                    fd: Some(fd),
                    mount_id: Some(metadata.mount_id),
                });
            }
            modes.insert(root.mount_point.clone(), (metadata.mount_id, metadata.mode));
        }
        if roots.is_empty() {
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: None,
            });
        }
        self.lookup_roots = Some(roots);
        self.lookup_root_modes = modes;
        Ok(self)
    }

    /// Select an absolute path's longest retained component-boundary prefix.
    ///
    /// A nonempty returned name must be resolved with NO_XDEV|NO_MAGICLINKS.
    /// An empty name identifies the already-qualified exact leaf mount root;
    /// do not substitute `.` or traverse through it. A suffix on a leaf gets a
    /// named refusal. Selection grants no permission to cross into another
    /// mount, follow a proc magic link or accept a changed pathname. Relative
    /// requests keep their original base and are not accepted by this method.
    pub fn absolute_lookup_root(&self, path: &CStr) -> Result<(RawFd, CString), HostPolicyRefusal> {
        let bytes = path.to_bytes();
        let root = self
            .lookup_roots
            .as_ref()
            .filter(|_| bytes.starts_with(b"/"))
            .and_then(|roots| {
                roots
                    .iter()
                    .filter(|root| {
                        root.mount_point == b"/"
                            || bytes == root.mount_point
                            || bytes
                                .strip_prefix(root.mount_point.as_slice())
                                .is_some_and(|tail| tail.starts_with(b"/"))
                    })
                    .max_by_key(|root| root.mount_point.len())
            })
            .ok_or(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: None,
            })?;
        let tail = &bytes[root.mount_point.len()..];
        let (mount_id, mode) = self.lookup_root_modes[&root.mount_point];
        if mode & libc::S_IFMT != libc::S_IFDIR {
            if tail.is_empty() {
                return Ok((root.directory.as_raw_fd(), c"".to_owned()));
            }
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: Some(root.directory.as_raw_fd()),
                mount_id: Some(mount_id),
            });
        }
        // Leading slashes are separators. Avoid an absolute returned name,
        // which would bypass the retained root and start from fs->root.
        let relative = tail
            .iter()
            .position(|byte| *byte != b'/')
            .map_or(b".".as_slice(), |start| &tail[start..]);
        Ok((
            root.directory.as_raw_fd(),
            CString::new(relative).expect("suffix of a CString"),
        ))
    }

    /// Share retained lookup roots without reopening or duplicating any FD.
    pub fn retained_lookup_roots(&self) -> Option<Arc<Vec<RetainedLookupRoot>>> {
        self.lookup_roots.clone()
    }

    /// Refuse unqualified discovery before opening the first proc endpoint.
    ///
    /// Use [`Self::collect_current_from`] with retained bootstrap authority.
    /// Collection also cannot prove privileged policy lifetimes, complete
    /// binfmt authority, or absence of executable pre-content watches.
    pub fn collect_current() -> Result<Self, HostPolicyRefusal> {
        let evidence = HostEvidence::collect_current()?;
        qualify_host_evidence(&evidence, &[0; 32])?;
        Err(HostPolicyRefusal::SecurityPolicyUnverified {
            reason: "live snapshots do not establish a frozen run-lifetime contract".to_owned(),
        })
    }

    /// Collect through retained qualified endpoints; unknown policies refuse.
    pub fn collect_current_from(
        endpoints: &QualifiedProcEndpoints,
    ) -> Result<Self, HostPolicyRefusal> {
        let evidence = HostEvidence::collect_current_from(endpoints)?;
        qualify_host_evidence(&evidence, &[0; 32])?;
        Err(HostPolicyRefusal::SecurityPolicyUnverified {
            reason: "live snapshots do not establish a frozen run-lifetime contract".to_owned(),
        })
    }

    pub fn evidence(&self) -> &HostEvidence {
        &self.evidence
    }

    pub fn attestation(&self) -> &FrozenHostAttestation {
        &self.attestation
    }

    pub fn context(&self) -> &ProcessContext {
        match &self.evidence.context {
            ExecContextEvidence::Current(context) => context,
            ExecContextEvidence::Unreadable { .. } => unreachable!("qualification checked context"),
        }
    }

    pub fn check_interpreter_stage(
        &self,
        header: &[u8],
        bprm_interp: &[u8],
    ) -> Result<(), HostPolicyRefusal> {
        self.evidence
            .binfmt
            .check_stage(self.context().user_namespace, header, bprm_interp)
    }

    /// Verify both namespace membership and the filesystem of a pinned inode.
    ///
    /// The caller obtains these facts from the already pinned descriptor.
    /// This method performs no lookup and never admits an absent mount ID.
    pub fn check_pinned_mount(
        &self,
        mount_id: u64,
        filesystem_type: i64,
    ) -> Result<(), HostPolicyRefusal> {
        self.evidence.mounts.verify_mount_id(mount_id)?;
        let record = self
            .evidence
            .mounts
            .records()
            .iter()
            .find(|record| record.mount_id == mount_id)
            .expect("verified namespace member");
        if filesystem_magic(&record.filesystem) != Some(filesystem_type) {
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: Some(mount_id),
            });
        }
        Ok(())
    }

    /// Qualify a caller lookup base using only its existing descriptor.
    ///
    /// `AT_FDCWD` selects the current directory through the kernel's empty-path
    /// rule. An ordinary invalid fd is left for original path-CHECK to report
    /// EBADF. This does not walk a target path or reopen the descriptor's name.
    pub fn check_lookup_fd(&self, fd: RawFd) -> Result<(), HostPolicyRefusal> {
        // SAFETY: statx is an initialized output buffer of the exact ABI size;
        // the C empty string is NUL terminated and has static storage duration.
        let mut metadata: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: flags request read-only metadata on the existing lookup base,
        // and metadata is writable for the full libc statx structure.
        let result = unsafe {
            libc::statx(
                fd,
                c"".as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW | libc::AT_STATX_DONT_SYNC,
                libc::STATX_TYPE | libc::STATX_INO | libc::STATX_MNT_ID,
                &mut metadata,
            )
        };
        if result < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EBADF) {
                return Ok(());
            }
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: Some(fd),
                mount_id: None,
            });
        }
        if metadata.stx_mask & libc::STATX_MNT_ID == 0 {
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: Some(fd),
                mount_id: None,
            });
        }
        match self.evidence.mounts.verify_mount_id(metadata.stx_mnt_id) {
            Ok(()) => Ok(()),
            Err(_) if metadata.stx_mode as u32 & libc::S_IFMT == libc::S_IFREG => {
                // SAFETY: F_GET_SEALS is a read-only query with no third arg.
                let seals = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
                let seal_query_errno = (seals < 0)
                    .then(io::Error::last_os_error)
                    .and_then(|error| error.raw_os_error());
                // O_PATH deliberately rejects F_GET_SEALS. Saved endpoint
                // evidence can still classify that exact inode before any
                // read-open, because its attested F_SEAL_SEAL is immutable.
                let opath = if seal_query_errno == Some(libc::EBADF) {
                    // SAFETY: F_GETFL inspects this existing descriptor only.
                    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                    flags >= 0 && flags & libc::O_PATH != 0
                } else {
                    false
                };
                if seals >= 0 || opath {
                    // SAFETY: statfs is writable for the exact ABI size.
                    let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
                    // SAFETY: fstatfs queries this existing descriptor only.
                    if unsafe { libc::fstatfs(fd, &mut filesystem) } == 0 {
                        if opath {
                            return self.check_pinned_memfd_path(
                                metadata.stx_mnt_id,
                                filesystem.f_type,
                                metadata.stx_ino,
                            );
                        }
                        return self.check_pinned_memfd(
                            metadata.stx_mnt_id,
                            filesystem.f_type,
                            metadata.stx_ino,
                            seals,
                        );
                    }
                }
                Err(HostPolicyRefusal::LookupMountUnverified {
                    fd: Some(fd),
                    mount_id: Some(metadata.stx_mnt_id),
                })
            }
            Err(error) => Err(error),
        }
    }

    /// Classify an O_PATH pin of an individually attested sealed memfd.
    ///
    /// `F_GET_SEALS` cannot query an O_PATH descriptor. The frozen snapshot
    /// instead binds this exact anonymous mount/inode to a valid endpoint
    /// receipt and seals observed on its original readable descriptor. In
    /// particular, `F_SEAL_SEAL` makes that saved complete seal set immutable.
    /// An unlisted tmpfs inode without such endpoint evidence is refused.
    ///
    /// This is a pre-read identity classification only. After reopening the
    /// pinned object for reading, the caller must recheck its actual seals with
    /// [`Self::check_pinned_memfd`] before using its contents.
    pub fn check_pinned_memfd_path(
        &self,
        mount_id: u64,
        filesystem_type: i64,
        inode: u64,
    ) -> Result<(), HostPolicyRefusal> {
        if filesystem_type == 0x0102_1994
            && self.evidence.sealed_memfds.iter().any(|endpoint| {
                endpoint.mount_id == mount_id
                    && endpoint.inode == inode
                    && endpoint.seals & IMMUTABLE_MEMFD_SEALS == IMMUTABLE_MEMFD_SEALS
                    && endpoint.receipt.has_provenance()
            })
        {
            Ok(())
        } else {
            Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: Some(mount_id),
            })
        }
    }

    /// Verify an individually attested sealed memfd endpoint.
    ///
    /// The caller must separately establish that this descriptor is a memfd
    /// (including a successful F_GET_SEALS), not merely an ordinary tmpfs inode.
    pub fn check_pinned_memfd(
        &self,
        mount_id: u64,
        filesystem_type: i64,
        inode: u64,
        seals: i32,
    ) -> Result<(), HostPolicyRefusal> {
        if filesystem_type == 0x0102_1994
            && seals & IMMUTABLE_MEMFD_SEALS == IMMUTABLE_MEMFD_SEALS
            && self.evidence.sealed_memfds.iter().any(|endpoint| {
                endpoint.mount_id == mount_id
                    && endpoint.inode == inode
                    && endpoint.seals == seals
                    && endpoint.receipt.has_provenance()
            })
        {
            Ok(())
        } else {
            Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: Some(mount_id),
            })
        }
    }

    /// Any observed change invalidates the approval, including a digest change.
    pub fn revalidate_evidence(&self, current: &HostEvidence) -> Result<(), HostPolicyRefusal> {
        if current.mounts != self.evidence.mounts {
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: None,
            });
        }
        if current.sealed_memfds != self.evidence.sealed_memfds {
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: None,
            });
        }
        if current.security != self.evidence.security {
            if matches!(self.evidence.security.bpf, BpfEvidence::Attested(_)) {
                return Err(HostPolicyRefusal::BpfLsmUnattested {
                    reason: "approved security evidence changed".to_owned(),
                });
            }
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "approved security evidence changed".to_owned(),
            });
        }
        if current.binfmt != self.evidence.binfmt {
            return Err(HostPolicyRefusal::BinfmtRegistryUnverified {
                reason: "approved registry evidence changed".to_owned(),
            });
        }
        if current.context != self.evidence.context {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: "approved process context changed".to_owned(),
            });
        }
        if current.watches != self.evidence.watches {
            return Err(HostPolicyRefusal::PreContentWatchUnverified);
        }
        qualify_host_evidence(current, &self.attestation.context_digest)
    }
}

fn qualify_host_evidence(
    evidence: &HostEvidence,
    context_digest: &[u8; 32],
) -> Result<(), HostPolicyRefusal> {
    evidence.mounts.qualify()?;
    for endpoint in &evidence.sealed_memfds {
        if endpoint.inode == 0
            || endpoint.seals & IMMUTABLE_MEMFD_SEALS != IMMUTABLE_MEMFD_SEALS
            || !endpoint.receipt.has_provenance()
        {
            return Err(HostPolicyRefusal::LookupMountUnverified {
                fd: None,
                mount_id: Some(endpoint.mount_id),
            });
        }
    }
    evidence.security.qualify(context_digest)?;
    let context = match &evidence.context {
        ExecContextEvidence::Current(context) => context,
        ExecContextEvidence::Unreadable { reason } => {
            return Err(HostPolicyRefusal::SecurityPolicyUnverified {
                reason: reason.clone(),
            });
        }
    };
    context.qualify()?;
    for proc in evidence.mounts.proc_namespaces() {
        if !proc.matches_context(context) {
            return Err(proc_unverified(
                "namespace",
                "proc authority describes a different calling context",
            ));
        }
    }
    evidence.binfmt.registry(context.user_namespace)?;
    evidence.watches.qualify()
}

impl HostEvidence {
    /// Unqualified discovery cannot safely open even its first proc endpoint.
    /// Supply retained, already-qualified bootstrap descriptors explicitly.
    pub fn collect_current() -> Result<Self, HostPolicyRefusal> {
        Err(proc_unverified(
            "self/mountinfo",
            "collection requires retained already-qualified bootstrap endpoints",
        ))
    }

    /// Read live facts through exact retained bootstrap endpoints.
    ///
    /// This does not establish security/binfmt/watch lifetimes. In particular,
    /// no `/sys` security authority is opened or inferred from namespace data.
    pub fn collect_current_from(
        endpoints: &QualifiedProcEndpoints,
    ) -> Result<Self, HostPolicyRefusal> {
        endpoints.evidence.qualify()?;
        let mountinfo = read_retained(&endpoints.descriptors.mountinfo, "self/mountinfo")?;
        let mut mounts = MountEvidence::parse_complete(&mountinfo, Vec::new())?
            .with_proc_namespaces(vec![endpoints.evidence().clone()]);
        // No inherited descriptor or executable is touched until every row of
        // the complete genuine namespace has qualified, including proc's exact
        // endpoint authority. Detached directory bases remain refused.
        mounts.qualify()?;
        mounts.inherited_lookup_fds = collect_inherited_lookup_fds(&mounts, endpoints)?;
        mounts.qualify()?;
        let context = match collect_process_context(endpoints) {
            Ok(context) => ExecContextEvidence::Current(context),
            Err(error) => ExecContextEvidence::Unreadable {
                reason: format!("live process context: {error}"),
            },
        };
        Ok(Self {
            mounts,
            security: SecurityEvidence {
                active_lsms: None,
                bpf: BpfEvidence::Unattested,
                integrity: IntegrityEvidence::Unknown {
                    reason: "no retained qualified security/integrity authority".to_owned(),
                },
            },
            binfmt: BinfmtAuthority::Unverified {
                reason: "retained proc facts cannot prove nearest userns registry authority"
                    .to_owned(),
            },
            context,
            watches: PreContentWatchEvidence::Unverified,
            sealed_memfds: Vec::new(),
        })
    }
}

fn filesystem_magic(filesystem: &str) -> Option<i64> {
    match filesystem {
        "ext4" => Some(0xef53),
        "xfs" => Some(0x5846_5342),
        "btrfs" => Some(0x9123_683e),
        "tmpfs" => Some(0x0102_1994),
        "squashfs" => Some(0x7371_7368),
        "erofs" => Some(0xe0f5_e1e2),
        _ => None,
    }
}

fn collect_inherited_lookup_fds(
    mounts: &MountEvidence,
    endpoints: &QualifiedProcEndpoints,
) -> Result<Vec<InheritedLookupFd>, HostPolicyRefusal> {
    let unknown = |fd, mount_id| HostPolicyRefusal::LookupMountUnverified { fd, mount_id };
    let directory = endpoints.descriptors.fdinfo_directory.as_raw_fd();
    // This retained genuine proc directory has already qualified. Enumeration
    // does not allocate a descriptor or walk a name, and the caller serializes
    // its cursor as well as its frozen descriptor table.
    // SAFETY: lseek changes only the retained endpoint's enumeration cursor.
    if unsafe { libc::lseek(directory, 0, libc::SEEK_SET) } < 0 {
        return Err(proc_unverified(
            "self/fdinfo",
            "retained directory cannot enumerate",
        ));
    }
    let mut buffer = [0_u8; 8192];
    let mut descriptors = Vec::new();
    loop {
        // SAFETY: writable buffer has the supplied exact byte extent.
        let count = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory,
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        if count < 0 {
            return Err(proc_unverified(
                "self/fdinfo",
                "retained directory enumeration failed",
            ));
        }
        if count == 0 {
            break;
        }
        let mut entries = &buffer[..count as usize];
        while !entries.is_empty() {
            // Linux dirent64: ino:u64, off:i64, reclen:u16, type:u8, name:NUL.
            let header = entries.get(..19).ok_or_else(|| unknown(None, None))?;
            let length =
                u16::from_ne_bytes(header[16..18].try_into().expect("dirent size")) as usize;
            let entry = entries.get(..length).ok_or_else(|| unknown(None, None))?;
            let name = entry.get(19..).ok_or_else(|| unknown(None, None))?;
            let nul = name
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| unknown(None, None))?;
            let name = &name[..nul];
            if name != b"." && name != b".." {
                let fd = std::str::from_utf8(name)
                    .ok()
                    .and_then(|name| name.parse::<RawFd>().ok())
                    .ok_or_else(|| unknown(None, None))?;
                descriptors.push(fd);
            }
            entries = &entries[length..];
        }
    }
    let owned = endpoints.owned_fds();
    let mut result = Vec::new();
    for fd in descriptors.into_iter().filter(|fd| !owned.contains(fd)) {
        // F_GETFL reads the file's saved flags without invoking target getattr
        // or filesystem operations. fdinfo of the original OFD can invoke its
        // ->show_fdinfo callback, so do not read that unqualified endpoint.
        // SAFETY: read-only query on a live frozen caller descriptor.
        let original_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if original_flags < 0 {
            return Err(unknown(Some(fd), None));
        }
        let original_name = std::ffi::CString::new(fd.to_string()).expect("numeric fd");
        // O_PATH selects empty_fops, including no ->show_fdinfo. The genuine
        // proc magic link obtains the existing file's f_path without traversing
        // its filesystem or running the target's ordinary open method.
        // SAFETY: retained qualified directory and a generated numeric name.
        let pinned = unsafe {
            libc::openat(
                endpoints.fd_directory(),
                original_name.as_ptr(),
                libc::O_PATH | libc::O_CLOEXEC,
            )
        };
        if pinned < 0 {
            return Err(unknown(Some(fd), None));
        }
        // SAFETY: successful syscall returned a newly owned O_PATH descriptor.
        let pinned = unsafe { File::from_raw_fd(pinned) };
        let name = std::ffi::CString::new(pinned.as_raw_fd().to_string()).expect("numeric fd");
        // The only new proc open is an audited generated numeric child below
        // the already-qualified retained genuine fdinfo directory. No absolute
        // path, symlink alias or guest-provided spelling participates.
        // SAFETY: retained directory and numeric CString live through syscall.
        let opened = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if opened < 0 {
            return Err(unknown(Some(fd), None));
        }
        // SAFETY: the successful syscall returned one newly owned descriptor.
        let file = unsafe { File::from_raw_fd(opened) };
        let metadata = retained_metadata(&file, "self/fdinfo/N")?;
        if metadata.filesystem != PROC_SUPER_MAGIC
            || metadata.mount_id != endpoints.evidence.mount_id
            || metadata.mode & libc::S_IFMT != libc::S_IFREG
        {
            return Err(proc_unverified(
                "self/fdinfo/N",
                "numeric child differs from audited proc authority",
            ));
        }
        let bytes = read_retained(&file, "self/fdinfo/N")?;
        drop(file);
        drop(pinned);
        let text = std::str::from_utf8(&bytes).map_err(|_| unknown(Some(fd), None))?;
        let mount_id = text
            .lines()
            .find_map(|line| line.strip_prefix("mnt_id:"))
            .and_then(|value| value.trim().parse::<u64>().ok())
            .ok_or_else(|| unknown(Some(fd), None))?;
        let flags = text
            .lines()
            .find_map(|line| line.strip_prefix("flags:"))
            .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())
            .ok_or_else(|| unknown(Some(fd), Some(mount_id)))?;
        if flags & libc::O_PATH as u32 == 0 {
            return Err(proc_unverified(
                "self/fdinfo/N",
                "numeric child is not the newly retained O_PATH descriptor",
            ));
        }
        if original_flags & libc::O_PATH != 0 {
            result.push(InheritedLookupFd { fd, mount_id });
            continue;
        }
        if mounts.verify_mount_id(mount_id).is_err() {
            // Only kernel non-path FD names can be excluded without getattr on
            // an unverified detached inode. Readlink is itself an audited
            // numeric operation below the retained genuine proc FD directory.
            let mut link = [0_u8; 4096];
            // SAFETY: retained directory, numeric name, writable bounded buffer.
            let count = unsafe {
                libc::readlinkat(
                    endpoints.fd_directory(),
                    original_name.as_ptr(),
                    link.as_mut_ptr().cast(),
                    link.len(),
                )
            };
            if count < 0 || count as usize >= link.len() {
                return Err(unknown(Some(fd), Some(mount_id)));
            }
            let target = &link[..count as usize];
            if target.starts_with(b"pipe:[")
                || target.starts_with(b"socket:[")
                || target.starts_with(b"anon_inode:")
            {
                continue;
            }
            return Err(unknown(Some(fd), Some(mount_id)));
        }
        // SAFETY: exact ABI output buffer; its namespace mount qualified first.
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: inspect only the existing qualified caller descriptor.
        if unsafe { libc::fstat(fd, &mut metadata) } != 0 {
            return Err(unknown(Some(fd), Some(mount_id)));
        }
        if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
            result.push(InheritedLookupFd { fd, mount_id });
        }
    }
    Ok(result)
}

fn collect_process_context(endpoints: &QualifiedProcEndpoints) -> io::Result<ProcessContext> {
    let status =
        read_retained(&endpoints.descriptors.status, "self/status").map_err(io::Error::other)?;
    let status = std::str::from_utf8(&status).map_err(io::Error::other)?;
    let fields: BTreeMap<_, _> = status
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key, value.trim()))
        .collect();
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process context");
    let ids = |name| -> io::Result<Vec<u32>> {
        let values: Vec<_> = fields
            .get(name)
            .ok_or_else(invalid)?
            .split_ascii_whitespace()
            .map(str::parse::<u32>)
            .collect::<Result<_, _>>()
            .map_err(|_| invalid())?;
        if values.len() != 4 {
            return Err(invalid());
        }
        Ok(values)
    };
    let uids = ids("Uid")?;
    let gids = ids("Gid")?;
    let capabilities = |name| {
        u64::from_str_radix(fields.get(name).ok_or_else(invalid)?, 16).map_err(|_| invalid())
    };
    let namespace_pid = fields
        .get("NSpid")
        .and_then(|value| value.split_ascii_whitespace().last())
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(invalid)?;
    // SAFETY: read-only process query, no pointer arguments.
    let securebits = unsafe { libc::prctl(libc::PR_GET_SECUREBITS, 0, 0, 0, 0) };
    if securebits < 0 {
        return Err(io::Error::last_os_error());
    }
    let no_new_privs = match fields.get("NoNewPrivs").copied() {
        Some("0") => false,
        Some("1") => true,
        _ => return Err(invalid()),
    };
    Ok(ProcessContext {
        namespace_pid,
        mount_namespace: endpoints.evidence.mount_namespace,
        user_namespace: endpoints.evidence.user_namespace,
        pid_namespace: endpoints.evidence.pid_namespace,
        real_uid: uids[0],
        effective_uid: uids[1],
        real_gid: gids[0],
        effective_gid: gids[1],
        securebits: securebits as u32,
        inheritable_capabilities: capabilities("CapInh")?,
        bounding_capabilities: capabilities("CapBnd")?,
        permitted_capabilities: capabilities("CapPrm")?,
        no_new_privs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_receipt() -> PolicyReceipt {
        PolicyReceipt {
            approval_id: "MODELED-LB-host-classifier-fixture".to_owned(),
            scope: "MODEL ONLY: complete isolated namespace and target/launcher policy".to_owned(),
            lifetime: "MODEL ONLY: no namespace, policy, attachment, map or watch changes"
                .to_owned(),
            policy_digest: [3; 32],
            generation: 7,
        }
    }

    fn model_context() -> ProcessContext {
        ProcessContext {
            namespace_pid: 2,
            mount_namespace: 11,
            user_namespace: 12,
            pid_namespace: 13,
            real_uid: 1000,
            effective_uid: 1000,
            real_gid: 1000,
            effective_gid: 1000,
            securebits: 0,
            inheritable_capabilities: 0,
            bounding_capabilities: 0,
            permitted_capabilities: 0,
            no_new_privs: false,
        }
    }

    fn model_registry() -> BinfmtRegistry {
        BinfmtRegistry {
            identity: BinfmtRegistryIdentity {
                owner_user_namespace: 12,
                mount_id: 99,
                policy_digest: [4; 32],
                generation: 8,
            },
            enabled: true,
            entries: Vec::new(),
        }
    }

    fn model_authority(registry: BinfmtRegistry) -> BinfmtAuthority {
        BinfmtAuthority::NearestAncestor {
            visible_registry: registry.identity.clone(),
            ancestry: vec![
                NamespaceBinfmtEvidence {
                    user_namespace: 12,
                    parent: Some(1),
                    registry: Some(registry),
                },
                NamespaceBinfmtEvidence {
                    user_namespace: 1,
                    parent: None,
                    registry: None,
                },
            ],
        }
    }

    fn model_evidence() -> HostEvidence {
        HostEvidence {
            mounts: MountEvidence::parse_complete(b"1 0 0:1 / / rw - tmpfs tmpfs rw\n", Vec::new())
                .unwrap(),
            security: SecurityEvidence {
                active_lsms: Some(vec!["capability".to_owned()]),
                bpf: BpfEvidence::Inactive,
                integrity: IntegrityEvidence::Inactive,
            },
            binfmt: model_authority(model_registry()),
            context: ExecContextEvidence::Current(model_context()),
            watches: PreContentWatchEvidence::AbsentForLifetime(model_receipt()),
            sealed_memfds: Vec::new(),
        }
    }

    fn model_attestation() -> FrozenHostAttestation {
        FrozenHostAttestation {
            origin: EvidenceOrigin::Modeled {
                fixture: "LB5/LB6 pure classifier model; no live qualification".to_owned(),
            },
            receipt: model_receipt(),
            context_digest: [2; 32],
        }
    }

    #[test]
    fn lb5_modeled_complete_mount_allowlist_and_detached_fds() {
        for filesystem in ["ext4", "xfs", "btrfs", "tmpfs", "squashfs", "erofs"] {
            let text = format!("1 0 0:1 / / rw - {filesystem} fixture rw\n");
            let evidence = MountEvidence::parse_complete(
                text.as_bytes(),
                vec![InheritedLookupFd {
                    fd: 1050,
                    mount_id: 1,
                }],
            )
            .unwrap();
            evidence.qualify().unwrap();
        }
        for filesystem in [
            "coda",
            "afs",
            "overlay",
            "fuse",
            "fuse.squashfuse_ll",
            "nfs",
            "cifs",
            "autofs",
            "unknown",
            "proc",
            "sysfs",
            "devtmpfs",
            "cgroup2",
        ] {
            // The unsafe mount is elsewhere, so a textual target-prefix check
            // would erroneously admit this same complete snapshot.
            let text = format!(
                "1 0 0:1 / / rw - tmpfs fixture rw\n2 1 0:2 / /elsewhere rw - {filesystem} fixture rw\n"
            );
            assert!(matches!(
                MountEvidence::parse_complete(text.as_bytes(), Vec::new())
                    .unwrap()
                    .qualify(),
                Err(HostPolicyRefusal::UnsafeLookupMount { mount_id: 2, .. })
            ));
        }
        let detached = MountEvidence::parse_complete(
            b"1 0 0:1 / / rw - ext4 fixture rw\n",
            vec![InheritedLookupFd {
                fd: 100,
                mount_id: 77,
            }],
        )
        .unwrap();
        assert_eq!(
            detached.qualify(),
            Err(HostPolicyRefusal::LookupMountUnverified {
                fd: Some(100),
                mount_id: Some(77),
            })
        );
    }

    #[test]
    fn lb5_mountinfo_rejects_incomplete_and_decodes_kernel_escapes() {
        for text in [
            b"".as_slice(),
            b"1 0 0:1 / / rw - ext4 dev rw".as_slice(),
            b"1 0 0:1 / / rw - ext4 dev rw\n1 0 0:1 / / rw - ext4 dev rw\n".as_slice(),
            b"1 0 0:1 / /bad\\000 rw - ext4 dev rw\n".as_slice(),
        ] {
            assert!(matches!(
                MountEvidence::parse_complete(text, Vec::new()),
                Err(HostPolicyRefusal::LookupMountUnverified { .. })
            ));
        }
        let evidence = MountEvidence::parse_complete(
            b"1 0 0:1 / /space\\040tab\\011slash\\134 rw - ext4 dev rw\n",
            Vec::new(),
        )
        .unwrap();
        assert_eq!(evidence.records()[0].mount_point, b"/space tab\tslash\\");
    }

    #[test]
    fn lb2_lb5_modeled_pinned_mount_and_sealed_memfd_binding() {
        let mut evidence = model_evidence();
        evidence.sealed_memfds.push(SealedMemfdEvidence {
            mount_id: 77,
            inode: 1234,
            seals: IMMUTABLE_MEMFD_SEALS,
            receipt: model_receipt(),
        });
        // SAFETY: explicitly modeled immutable endpoint; never activation.
        let qualification =
            unsafe { HostQualification::from_frozen_evidence(evidence, model_attestation()) }
                .unwrap();
        qualification.check_pinned_mount(1, 0x0102_1994).unwrap();
        assert!(matches!(
            qualification.check_pinned_mount(1, 0xef53),
            Err(HostPolicyRefusal::LookupMountUnverified { .. })
        ));
        // Knowing tmpfs magic alone must not admit a detached mount.
        assert!(matches!(
            qualification.check_pinned_mount(77, 0x0102_1994),
            Err(HostPolicyRefusal::LookupMountUnverified { .. })
        ));
        qualification
            .check_pinned_memfd(77, 0x0102_1994, 1234, IMMUTABLE_MEMFD_SEALS)
            .unwrap();
        qualification
            .check_pinned_memfd_path(77, 0x0102_1994, 1234)
            .unwrap();
        for (mount_id, magic, inode) in [
            (78, 0x0102_1994, 1234),
            (77, 0xef53, 1234),
            (77, 0x0102_1994, 1235),
        ] {
            assert!(matches!(
                qualification.check_pinned_memfd_path(mount_id, magic, inode),
                Err(HostPolicyRefusal::LookupMountUnverified { .. })
            ));
        }
        for (mount_id, magic, inode, seals) in [
            (78, 0x0102_1994, 1234, IMMUTABLE_MEMFD_SEALS),
            (77, 0xef53, 1234, IMMUTABLE_MEMFD_SEALS),
            (77, 0x0102_1994, 1235, IMMUTABLE_MEMFD_SEALS),
            (77, 0x0102_1994, 1234, libc::F_SEAL_FUTURE_WRITE),
        ] {
            assert!(matches!(
                qualification.check_pinned_memfd(mount_id, magic, inode, seals),
                Err(HostPolicyRefusal::LookupMountUnverified { .. })
            ));
        }
    }

    #[test]
    fn lb2_lb5_real_opath_memfd_requires_exact_modeled_endpoint() {
        use std::fs::File;
        use std::fs::OpenOptions;
        use std::os::fd::AsRawFd;
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::OpenOptionsExt;

        fn sealed_memfd() -> File {
            // SAFETY: the static C name and flags are valid; the fresh returned
            // descriptor belongs to this fixture and is immediately owned.
            let fd = unsafe {
                libc::memfd_create(
                    c"LB-actual-O_PATH-sealed-endpoint".as_ptr(),
                    libc::MFD_ALLOW_SEALING | libc::MFD_CLOEXEC,
                )
            };
            assert!(fd >= 0, "create genuine kernel memfd");
            // SAFETY: fd is the fresh owned descriptor from memfd_create.
            let file = unsafe { File::from_raw_fd(fd) };
            // SAFETY: this private unmapped inode admits the complete immutable
            // seal set; the call mutates only this test-owned memfd.
            assert_eq!(
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, IMMUTABLE_MEMFD_SEALS) },
                0
            );
            file
        }

        fn pin(file: &File) -> File {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(format!("/proc/self/fd/{}", file.as_raw_fd()))
                .unwrap()
        }

        let original = sealed_memfd();
        let pinned = pin(&original);
        // This is a genuine syscall control, not a simulated descriptor.
        // It proves why directly querying the O_PATH pin was incorrect.
        // SAFETY: the query only inspects the owned pinned descriptor.
        assert_eq!(
            unsafe { libc::fcntl(pinned.as_raw_fd(), libc::F_GET_SEALS) },
            -1
        );
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        // SAFETY: zeroed statx and statfs are valid full ABI output buffers.
        let mut metadata: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: metadata query uses only this existing O_PATH descriptor.
        assert_eq!(
            unsafe {
                libc::statx(
                    pinned.as_raw_fd(),
                    c"".as_ptr(),
                    libc::AT_EMPTY_PATH,
                    libc::STATX_TYPE | libc::STATX_INO | libc::STATX_MNT_ID,
                    &mut metadata,
                )
            },
            0
        );
        assert_eq!(metadata.stx_mask & libc::STATX_MNT_ID, libc::STATX_MNT_ID);
        assert_eq!(metadata.stx_mode as u32 & libc::S_IFMT, libc::S_IFREG);
        // SAFETY: zeroed statfs is a valid full ABI output buffer.
        let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: fstatfs only queries this existing owned descriptor.
        assert_eq!(
            unsafe { libc::fstatfs(pinned.as_raw_fd(), &mut filesystem) },
            0
        );
        assert_eq!(filesystem.f_type, 0x0102_1994);

        // The descriptor facts above are live. Host namespace/security policy
        // and coordinator approval below are explicitly MODELED only; this
        // fixture cannot qualify or activate preparation on the real machine.
        let mut evidence = model_evidence();
        let model_mount_id = metadata.stx_mnt_id.checked_add(1).unwrap();
        evidence.mounts = MountEvidence::parse_complete(
            format!("{model_mount_id} 0 0:1 / / rw - tmpfs modeled rw\n").as_bytes(),
            Vec::new(),
        )
        .unwrap();
        let without_endpoint = evidence.clone();
        evidence.sealed_memfds.push(SealedMemfdEvidence {
            mount_id: metadata.stx_mnt_id,
            inode: metadata.stx_ino,
            seals: IMMUTABLE_MEMFD_SEALS,
            receipt: model_receipt(),
        });
        // SAFETY: explicitly inactive modeled approval with genuine endpoint
        // metadata; never an attestation of the live namespace or policy.
        let qualification = unsafe {
            HostQualification::from_frozen_evidence(evidence.clone(), model_attestation())
        }
        .unwrap();
        qualification.check_lookup_fd(pinned.as_raw_fd()).unwrap();
        qualification
            .check_pinned_memfd_path(metadata.stx_mnt_id, filesystem.f_type, metadata.stx_ino)
            .unwrap();
        // SAFETY: same explicitly modeled context, without endpoint approval.
        let unapproved = unsafe {
            HostQualification::from_frozen_evidence(without_endpoint, model_attestation())
        }
        .unwrap();
        assert!(matches!(
            unapproved.check_lookup_fd(pinned.as_raw_fd()),
            Err(HostPolicyRefusal::LookupMountUnverified { .. })
        ));
        let another = sealed_memfd();
        let another_pin = pin(&another);
        assert!(matches!(
            qualification.check_lookup_fd(another_pin.as_raw_fd()),
            Err(HostPolicyRefusal::LookupMountUnverified { .. })
        ));

        // After exact pinned reopen, actual seals must still match the saved
        // complete set. An O_PATH identity classification does not replace it.
        let readable = File::open(format!("/proc/self/fd/{}", pinned.as_raw_fd())).unwrap();
        // SAFETY: read-only seal query on this owned readable descriptor.
        let observed = unsafe { libc::fcntl(readable.as_raw_fd(), libc::F_GET_SEALS) };
        assert_eq!(observed, IMMUTABLE_MEMFD_SEALS);
        qualification
            .check_pinned_memfd(
                metadata.stx_mnt_id,
                filesystem.f_type,
                metadata.stx_ino,
                observed,
            )
            .unwrap();
        assert!(matches!(
            qualification.check_pinned_memfd(
                metadata.stx_mnt_id,
                filesystem.f_type,
                metadata.stx_ino,
                observed ^ libc::F_SEAL_WRITE
            ),
            Err(HostPolicyRefusal::LookupMountUnverified { .. })
        ));

        for invalid_seal in [
            libc::F_SEAL_SEAL,
            libc::F_SEAL_SHRINK,
            libc::F_SEAL_GROW,
            libc::F_SEAL_WRITE,
        ] {
            let mut incomplete = evidence.clone();
            incomplete.sealed_memfds[0].seals &= !invalid_seal;
            // SAFETY: classifier rejection control only, never activation.
            assert!(matches!(
                unsafe { HostQualification::from_frozen_evidence(incomplete, model_attestation()) },
                Err(HostPolicyRefusal::LookupMountUnverified { .. })
            ));
        }
        let mut missing_receipt = evidence;
        missing_receipt.sealed_memfds[0].receipt.policy_digest = [0; 32];
        // SAFETY: classifier rejection control only, never activation.
        assert!(matches!(
            unsafe {
                HostQualification::from_frozen_evidence(missing_receipt, model_attestation())
            },
            Err(HostPolicyRefusal::LookupMountUnverified { .. })
        ));
    }

    #[test]
    fn lb5_lb6_live_collection_never_invents_approval() {
        let refusal = HostQualification::collect_current().unwrap_err();
        println!("LIVE host evidence remains refused before target lookup: {refusal}");
        // No claim is made that privileged appraisal, BPF or watch fixtures
        // ran on this machine. Unknown facts remain an activation gate.
        assert!(matches!(
            refusal,
            HostPolicyRefusal::ProcEndpointUnverified { endpoint, .. }
                if endpoint == "self/mountinfo"
        ));
    }

    #[test]
    fn lb5_modeled_nearest_ancestor_hidden_registry_and_status() {
        let mut authority = model_authority(model_registry());
        authority.registry(12).unwrap();
        if let BinfmtAuthority::NearestAncestor {
            visible_registry, ..
        } = &mut authority
        {
            visible_registry.mount_id += 1;
        }
        assert!(matches!(
            authority.registry(12),
            Err(HostPolicyRefusal::BinfmtRegistryUnverified { .. })
        ));
        let mut parent = model_registry();
        parent.identity.owner_user_namespace = 1;
        parent.entries.push(magic_entry("parent", "COF"));
        let child = model_registry();
        let mut authority = BinfmtAuthority::NearestAncestor {
            visible_registry: child.identity.clone(),
            ancestry: vec![
                NamespaceBinfmtEvidence {
                    user_namespace: 12,
                    parent: Some(1),
                    registry: Some(child),
                },
                NamespaceBinfmtEvidence {
                    user_namespace: 1,
                    parent: None,
                    registry: Some(parent),
                },
            ],
        };
        // An empty child instance wins over enabled matching parent handlers.
        authority.check_stage(12, b"\x7fELF", b"/target").unwrap();
        if let BinfmtAuthority::NearestAncestor {
            ancestry,
            visible_registry,
        } = &mut authority
        {
            ancestry[0].registry = None;
            *visible_registry = ancestry[1].registry.as_ref().unwrap().identity.clone();
        }
        assert!(matches!(
            authority.check_stage(12, b"\x7fELF", b"/target"),
            Err(HostPolicyRefusal::BinfmtHandlerUnsupported { .. })
        ));
        if let BinfmtAuthority::NearestAncestor { ancestry, .. } = &mut authority {
            ancestry[1].registry.as_mut().unwrap().enabled = false;
        }
        authority.check_stage(12, b"\x7fELF", b"/target").unwrap();
        assert!(matches!(
            BinfmtAuthority::Unverified {
                reason: "unreadable".to_owned()
            }
            .registry(12),
            Err(HostPolicyRefusal::BinfmtRegistryUnverified { .. })
        ));
    }

    fn magic_entry(name: &str, flags: &str) -> BinfmtEntry {
        BinfmtEntry {
            name: name.to_owned(),
            enabled: true,
            rule: BinfmtMatch::Magic {
                offset: 0,
                magic: b"\x7fELF".to_vec(),
                mask: None,
            },
            interpreter: b"/model/interpreter".to_vec(),
            flags: BinfmtFlags::parse(flags).unwrap(),
        }
    }

    #[test]
    fn lb5_modeled_magic_mask_offset_extension_and_script_stages() {
        let mut entry = magic_entry("elf", "COF");
        assert!(
            entry.flags.credentials && entry.flags.open_binary && entry.flags.fixed_interpreter
        );
        assert!(entry.matches(b"\x7fELF", b"/target"));
        entry.enabled = false;
        assert!(!entry.matches(b"\x7fELF", b"/target"));
        entry.enabled = true;
        entry.rule = BinfmtMatch::Magic {
            offset: 2,
            magic: vec![0xa0, 0],
            mask: Some(vec![0xf0, 0xff]),
        };
        assert!(entry.matches(&[1, 2, 0xa7], b"/target")); // native zero-fill
        assert!(!entry.matches(&[1, 2, 0xb7], b"/target"));
        entry.rule = BinfmtMatch::Extension(b"handler".to_vec());
        assert!(!entry.matches(b"#!", b"/original.sh"));
        assert!(entry.matches(b"#!", b"/nested.handler"));
        let mut registry = model_registry();
        registry.entries.push(entry);
        let authority = model_authority(registry);
        authority.check_stage(12, b"#!", b"/original.sh").unwrap();
        assert!(matches!(
            authority.check_stage(12, b"#!", b"/nested.handler"),
            Err(HostPolicyRefusal::BinfmtHandlerUnsupported { flags, .. })
                if flags.credentials && flags.open_binary && flags.fixed_interpreter
        ));
        assert!(BinfmtFlags::parse("Z").is_err());
    }

    #[test]
    fn lb6_modeled_unknown_lsm_integrity_and_bpf_coverage() {
        let mut evidence = model_evidence();
        evidence.security.active_lsms = None;
        assert!(matches!(
            evidence.security.qualify(&[2; 32]),
            Err(HostPolicyRefusal::SecurityPolicyUnverified { .. })
        ));
        for module in [
            "selinux",
            "apparmor",
            "smack",
            "tomoyo",
            "ipe",
            "future_lsm",
        ] {
            evidence.security.active_lsms = Some(vec!["capability".to_owned(), module.to_owned()]);
            assert!(matches!(
                evidence.security.qualify(&[2; 32]),
                Err(HostPolicyRefusal::SecurityPolicyUnverified { .. })
            ));
        }
        evidence.security.active_lsms = Some(vec!["capability".to_owned(), "bpf".to_owned()]);
        evidence.security.bpf = BpfEvidence::Unattested;
        assert!(matches!(
            evidence.security.qualify(&[2; 32]),
            Err(HostPolicyRefusal::BpfLsmUnattested { .. })
        ));
        let proof = BpfAttestation {
            receipt: model_receipt(),
            context_digest: [2; 32],
            covered_hooks: REQUIRED_EXEC_HOOKS.to_vec(),
        };
        evidence.security.bpf = BpfEvidence::Attested(proof.clone());
        evidence.security.qualify(&[2; 32]).unwrap();
        assert!(matches!(
            evidence.security.qualify(&[9; 32]),
            Err(HostPolicyRefusal::BpfLsmUnattested { .. })
        ));
        let mut incomplete = proof;
        incomplete.covered_hooks.pop();
        evidence.security.bpf = BpfEvidence::Attested(incomplete);
        assert!(matches!(
            evidence.security.qualify(&[2; 32]),
            Err(HostPolicyRefusal::BpfLsmUnattested { .. })
        ));
        evidence = model_evidence();
        evidence
            .security
            .active_lsms
            .as_mut()
            .unwrap()
            .push("ima".to_owned());
        for integrity in [
            IntegrityEvidence::Unknown {
                reason: "policy unreadable".to_owned(),
            },
            IntegrityEvidence::EnforcingAppraisal {
                policy: "BPRM_CHECK only; checked bytes then changed".to_owned(),
            },
            IntegrityEvidence::Inactive,
        ] {
            evidence.security.integrity = integrity;
            assert!(matches!(
                evidence.security.qualify(&[2; 32]),
                Err(HostPolicyRefusal::IntegrityAppraisalUnsupported { .. })
            ));
        }
        evidence.security.integrity = IntegrityEvidence::MeasurementOnly {
            receipt: model_receipt(),
        };
        evidence.security.qualify(&[2; 32]).unwrap();
    }

    #[test]
    fn lb6_modeled_attestation_change_and_no_boolean_waiver() {
        let mut evidence = model_evidence();
        evidence
            .security
            .active_lsms
            .as_mut()
            .unwrap()
            .push("bpf".to_owned());
        evidence.security.bpf = BpfEvidence::Attested(BpfAttestation {
            receipt: model_receipt(),
            context_digest: [2; 32],
            covered_hooks: REQUIRED_EXEC_HOOKS.to_vec(),
        });
        // SAFETY: this is an explicitly labeled pure model, never activation.
        let qualification = unsafe {
            HostQualification::from_frozen_evidence(evidence.clone(), model_attestation())
        }
        .unwrap();
        qualification.revalidate_evidence(&evidence).unwrap();
        if let BpfEvidence::Attested(proof) = &mut evidence.security.bpf {
            proof.receipt.generation += 1;
            proof.receipt.policy_digest[0] ^= 1;
        }
        assert!(matches!(
            qualification.revalidate_evidence(&evidence),
            Err(HostPolicyRefusal::BpfLsmUnattested { .. })
        ));
        let mut attestation = model_attestation();
        attestation.receipt.lifetime.clear();
        // SAFETY: deliberately invalid pure model; constructor must refuse.
        assert!(matches!(
            unsafe { HostQualification::from_frozen_evidence(model_evidence(), attestation) },
            Err(HostPolicyRefusal::SecurityPolicyUnverified { .. })
        ));
    }

    #[test]
    fn lb1_lb6_modeled_context_privileges_and_personality() {
        let mut context = model_context();
        context.qualify().unwrap();
        context.namespace_pid = 1;
        assert_eq!(
            context.qualify(),
            Err(HostPolicyRefusal::NamespaceInitUnsupported)
        );
        context = model_context();
        context.effective_uid += 1;
        assert_eq!(
            context.qualify(),
            Err(HostPolicyRefusal::SecureExecUnsupported)
        );
        context = model_context();
        context.real_uid = 0;
        context.effective_uid = 0;
        context.bounding_capabilities = 0b101;
        context.permitted_capabilities = 0b001;
        for nnp in [false, true] {
            context.no_new_privs = nnp;
            assert!(capability_personality_clearing(&context));
            assert_eq!(
                context.qualify(),
                Err(HostPolicyRefusal::CapabilityPersonalityClearing)
            );
        }
        context.securebits = 1;
        assert!(!capability_personality_clearing(&context));
        context.qualify().unwrap();
        context.securebits = 0;
        context.permitted_capabilities = context.bounding_capabilities;
        assert!(!capability_personality_clearing(&context));
        context.inheritable_capabilities = 0b1000;
        assert!(capability_personality_clearing(&context));
        qualify_executable_privileges(0o755, &CapabilityXattrEvidence::Absent).unwrap();
        for mode in [0o4755, 0o2755] {
            assert_eq!(
                qualify_executable_privileges(mode, &CapabilityXattrEvidence::Absent),
                Err(HostPolicyRefusal::SecureExecUnsupported)
            );
        }
        for namespaced in [false, true] {
            assert_eq!(
                qualify_executable_privileges(
                    0o755,
                    &CapabilityXattrEvidence::Present {
                        bytes: Vec::new(),
                        namespaced
                    }
                ),
                Err(HostPolicyRefusal::SecureExecUnsupported)
            );
        }
        assert!(matches!(
            qualify_executable_privileges(0o755, &CapabilityXattrEvidence::Unreadable),
            Err(HostPolicyRefusal::SecurityPolicyUnverified { .. })
        ));
    }

    #[test]
    fn lb6_modeled_interpreter_denials_and_watch_lifetime() {
        for source in [
            InterpreterDenialSource::Regularity,
            InterpreterDenialSource::ExecuteDac,
            InterpreterDenialSource::NoexecMount,
        ] {
            assert_eq!(
                classify_interpreter_denial(libc::EACCES, source),
                InterpreterAuthorizationOutcome::NativeErrno(libc::EACCES)
            );
        }
        assert_eq!(
            classify_interpreter_denial(libc::ETXTBSY, InterpreterDenialSource::WriterHeld),
            InterpreterAuthorizationOutcome::NativeErrno(libc::ETXTBSY)
        );
        for errno in [libc::EACCES, libc::ETXTBSY, libc::EPERM] {
            assert_eq!(
                classify_interpreter_denial(errno, InterpreterDenialSource::BprmOrUnknown),
                InterpreterAuthorizationOutcome::Refuse(
                    HostPolicyRefusal::InterpreterCheckStronger { errno }
                )
            );
        }
        assert_eq!(
            PreContentWatchEvidence::Unverified.qualify(),
            Err(HostPolicyRefusal::PreContentWatchUnverified)
        );
        PreContentWatchEvidence::AbsentForLifetime(model_receipt())
            .qualify()
            .unwrap();
        PreContentWatchEvidence::NativeEquivalentForLifetime(model_receipt())
            .qualify()
            .unwrap();
    }
}
