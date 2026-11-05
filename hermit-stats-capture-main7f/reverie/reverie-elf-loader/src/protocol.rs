/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Checked, versioned specification for the still inactive start transport.
//!
//! The legacy LA loader does not consume this format. A future freestanding
//! consumer must validate the same header and descriptor/object bindings before
//! using it. Descriptor numbers are data, with no fixed 100/102 or 1024 ceiling.
//! This format carries an initial root random handoff only. Encoding or decoding
//! an exec continuation refuses it explicitly. The separate LB7 clock carrier
//! does not change this manifest's refusal or establish shared proc-OFD state.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt;

/// Version of the inactive, little-endian transport specification.
pub const START_MANIFEST_VERSION: u16 = 1;
/// Maximum transport size, including the header and all extents.
pub const MAX_START_MANIFEST_BYTES: usize = 8192;
/// Bounded descriptor bookkeeping; descriptor numbers themselves are unrestricted.
pub const MAX_START_MANIFEST_DESCRIPTORS: usize = 64;
const MAGIC: &[u8; 8] = b"REVLBST\0";
const HEADER_BYTES: usize = 144;
const DESCRIPTOR_BYTES: usize = 40;
const ROOT_INITIAL_TAG: u32 = 1;
const EXEC_CONTINUATION_TAG: u32 = 2;
const IDENTITY_PRESENT: u16 = 1;
const MOUNT_PRESENT: u16 = 2;

/// Initial root launch and later exec have different state-restoration contracts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartKind {
    RootInitial,
    /// Unsupported until the later transactional exec design is implemented.
    ExecContinuation,
}

/// A descriptor's exact private role. Script indices describe rewrite order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DescriptorRole {
    Program,
    Interpreter,
    Metadata,
    Failure,
    Control,
    Scratch,
    ProgramPath,
    InterpreterPath,
    Script(u8),
}

impl DescriptorRole {
    fn encode(self) -> Result<u16, ProtocolError> {
        Ok(match self {
            Self::Program => 1,
            Self::Interpreter => 2,
            Self::Metadata => 3,
            Self::Failure => 4,
            Self::Control => 5,
            Self::Scratch => 6,
            Self::ProgramPath => 7,
            Self::InterpreterPath => 8,
            Self::Script(index) if index < 6 => 0x100 + u16::from(index),
            Self::Script(_) => return Err(ProtocolError::InvalidDescriptor("script depth")),
        })
    }

    fn decode(value: u16) -> Result<Self, ProtocolError> {
        Ok(match value {
            1 => Self::Program,
            2 => Self::Interpreter,
            3 => Self::Metadata,
            4 => Self::Failure,
            5 => Self::Control,
            6 => Self::Scratch,
            7 => Self::ProgramPath,
            8 => Self::InterpreterPath,
            0x100..=0x105 => Self::Script((value - 0x100) as u8),
            _ => return Err(ProtocolError::InvalidDescriptor("unknown role")),
        })
    }

    fn has_file_identity(self) -> bool {
        matches!(
            self,
            Self::Program
                | Self::Interpreter
                | Self::ProgramPath
                | Self::InterpreterPath
                | Self::Script(_)
        )
    }
}

/// Pinned inode identity. A mount identity is included when the host verified it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectIdentity {
    pub device: u64,
    pub inode: u64,
    pub mount_id: Option<u64>,
}

impl ObjectIdentity {
    pub fn from_file(file: &File, mount_id: Option<u64>) -> io::Result<Self> {
        let metadata = file.metadata()?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            mount_id,
        })
    }
}

/// One owned descriptor and, for file roles, the pinned object it must identify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ManifestDescriptor {
    pub role: DescriptorRole,
    pub fd: RawFd,
    pub identity: Option<ObjectIdentity>,
}

/// All fields are checked before encoding and independently checked on decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartManifest {
    pub kind: StartKind,
    pub nonce: [u8; 16],
    pub generation: u64,
    pub mm_generation: u64,
    pub configuration_digest: [u8; 32],
    /// The 16 actual initial-root bytes; this format performs no PRNG draws.
    pub at_random: [u8; 16],
    pub descriptors: Vec<ManifestDescriptor>,
    /// Original native F, rather than final script interpreter or final ELF T.
    pub original_filename: Vec<u8>,
    /// Native comm bytes, excluding their terminating NUL (at most 15 bytes).
    pub comm: Vec<u8>,
    pub failure_fd: RawFd,
    pub scratch_fd: RawFd,
}

/// Invalid transport data is a launcher failure, never a native exec errno.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    Truncated,
    InvalidMagic,
    UnsupportedVersion(u16),
    UnsupportedContinuation,
    InvalidKind(u32),
    SizeOverflow,
    ManifestTooLarge,
    InvalidExtent(&'static str),
    InvalidDescriptor(&'static str),
    DuplicateRole(DescriptorRole),
    DuplicateFd(RawFd),
    MissingRole(DescriptorRole),
    InvalidString(&'static str),
    ObjectIdentityChanged(DescriptorRole),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(formatter, "truncated start manifest"),
            Self::InvalidMagic => write!(formatter, "invalid start manifest magic"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported start manifest version {version}")
            }
            Self::UnsupportedContinuation => {
                write!(
                    formatter,
                    "start manifest refuses unsupported exec continuation"
                )
            }
            Self::InvalidKind(kind) => write!(formatter, "unknown start manifest kind {kind}"),
            Self::SizeOverflow => write!(formatter, "start manifest size overflow"),
            Self::ManifestTooLarge => write!(formatter, "start manifest exceeds its size bound"),
            Self::InvalidExtent(field) => {
                write!(formatter, "invalid start manifest {field} extent")
            }
            Self::InvalidDescriptor(reason) => {
                write!(formatter, "invalid private descriptor: {reason}")
            }
            Self::DuplicateRole(role) => {
                write!(formatter, "duplicate private descriptor role {role:?}")
            }
            Self::DuplicateFd(fd) => write!(formatter, "duplicate private descriptor number {fd}"),
            Self::MissingRole(role) => {
                write!(formatter, "missing private descriptor role {role:?}")
            }
            Self::InvalidString(field) => {
                write!(formatter, "invalid start manifest {field} string")
            }
            Self::ObjectIdentityChanged(role) => {
                write!(formatter, "pinned object identity changed for {role:?}")
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

impl StartManifest {
    /// Encode a canonical layout with explicit checked F and comm extents.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let descriptors_size = checked_mul(self.descriptors.len(), DESCRIPTOR_BYTES)?;
        let filename_offset = checked_add(HEADER_BYTES, descriptors_size)?;
        let filename_size = checked_add(self.original_filename.len(), 1)?;
        let comm_offset = checked_add(filename_offset, filename_size)?;
        let comm_size = checked_add(self.comm.len(), 1)?;
        let total = checked_add(comm_offset, comm_size)?;
        if total > MAX_START_MANIFEST_BYTES {
            return Err(ProtocolError::ManifestTooLarge);
        }
        let mut bytes = Vec::with_capacity(total);
        bytes.extend_from_slice(MAGIC);
        put_u16(&mut bytes, START_MANIFEST_VERSION);
        put_u16(&mut bytes, HEADER_BYTES as u16);
        put_u32(&mut bytes, as_u32(total)?);
        put_u32(&mut bytes, ROOT_INITIAL_TAG);
        put_u32(&mut bytes, 0);
        bytes.extend_from_slice(&self.nonce);
        put_u64(&mut bytes, self.generation);
        put_u64(&mut bytes, self.mm_generation);
        bytes.extend_from_slice(&self.configuration_digest);
        bytes.extend_from_slice(&self.at_random);
        put_u32(&mut bytes, as_u32(self.descriptors.len())?);
        put_u32(&mut bytes, HEADER_BYTES as u32);
        put_u32(&mut bytes, as_u32(filename_offset)?);
        put_u32(&mut bytes, as_u32(filename_size)?);
        put_u32(&mut bytes, as_u32(comm_offset)?);
        put_u32(&mut bytes, as_u32(comm_size)?);
        put_i32(&mut bytes, self.failure_fd);
        put_i32(&mut bytes, self.scratch_fd);
        put_u64(&mut bytes, 0);
        debug_assert_eq!(bytes.len(), HEADER_BYTES);
        for descriptor in &self.descriptors {
            put_u16(&mut bytes, descriptor.role.encode()?);
            let flags = descriptor.identity.map_or(0, |identity| {
                IDENTITY_PRESENT
                    | if identity.mount_id.is_some() {
                        MOUNT_PRESENT
                    } else {
                        0
                    }
            });
            put_u16(&mut bytes, flags);
            put_i32(&mut bytes, descriptor.fd);
            let identity = descriptor.identity.unwrap_or(ObjectIdentity {
                device: 0,
                inode: 0,
                mount_id: None,
            });
            put_u64(&mut bytes, identity.device);
            put_u64(&mut bytes, identity.inode);
            put_u64(&mut bytes, identity.mount_id.unwrap_or(0));
            put_u64(&mut bytes, 0);
        }
        bytes.extend_from_slice(&self.original_filename);
        bytes.push(0);
        bytes.extend_from_slice(&self.comm);
        bytes.push(0);
        debug_assert_eq!(bytes.len(), total);
        Ok(bytes)
    }

    /// Decode without trusting lengths, roles, flag bits, or descriptor numbers.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < HEADER_BYTES {
            return Err(ProtocolError::Truncated);
        }
        if bytes.len() > MAX_START_MANIFEST_BYTES {
            return Err(ProtocolError::ManifestTooLarge);
        }
        if &bytes[..8] != MAGIC {
            return Err(ProtocolError::InvalidMagic);
        }
        let version = read_u16(bytes, 8)?;
        if version != START_MANIFEST_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        if usize::from(read_u16(bytes, 10)?) != HEADER_BYTES {
            return Err(ProtocolError::InvalidExtent("header"));
        }
        if as_usize(read_u32(bytes, 12)?)? != bytes.len() {
            return Err(ProtocolError::InvalidExtent("total"));
        }
        let kind = match read_u32(bytes, 16)? {
            ROOT_INITIAL_TAG => StartKind::RootInitial,
            EXEC_CONTINUATION_TAG => return Err(ProtocolError::UnsupportedContinuation),
            other => return Err(ProtocolError::InvalidKind(other)),
        };
        if read_u32(bytes, 20)? != 0 || read_u64(bytes, 136)? != 0 {
            return Err(ProtocolError::InvalidDescriptor("reserved header bits"));
        }
        let count = as_usize(read_u32(bytes, 104)?)?;
        if count > MAX_START_MANIFEST_DESCRIPTORS {
            return Err(ProtocolError::InvalidDescriptor("descriptor count"));
        }
        let descriptor_offset = as_usize(read_u32(bytes, 108)?)?;
        if descriptor_offset != HEADER_BYTES {
            return Err(ProtocolError::InvalidExtent("descriptors"));
        }
        let filename_offset = as_usize(read_u32(bytes, 112)?)?;
        let filename_size = as_usize(read_u32(bytes, 116)?)?;
        let comm_offset = as_usize(read_u32(bytes, 120)?)?;
        let comm_size = as_usize(read_u32(bytes, 124)?)?;
        let descriptor_end = checked_add(descriptor_offset, checked_mul(count, DESCRIPTOR_BYTES)?)?;
        let filename_end = checked_add(filename_offset, filename_size)?;
        let comm_end = checked_add(comm_offset, comm_size)?;
        // Canonical adjacent extents reject overlaps, holes and trailing bytes.
        if descriptor_end != filename_offset
            || filename_end != comm_offset
            || comm_end != bytes.len()
            || descriptor_end > bytes.len()
            || filename_end > bytes.len()
            || filename_size == 0
            || filename_size > crate::PATH_MAX
            || comm_size == 0
            || comm_size > 16
        {
            return Err(ProtocolError::InvalidExtent("payload"));
        }
        let original_filename = decode_string(&bytes[filename_offset..filename_end], "F")?;
        let comm = decode_string(&bytes[comm_offset..comm_end], "comm")?;
        let mut descriptors = Vec::with_capacity(count);
        for index in 0..count {
            let offset = checked_add(descriptor_offset, checked_mul(index, DESCRIPTOR_BYTES)?)?;
            let role = DescriptorRole::decode(read_u16(bytes, offset)?)?;
            let flags = read_u16(bytes, offset + 2)?;
            if flags & !(IDENTITY_PRESENT | MOUNT_PRESENT) != 0
                || flags & MOUNT_PRESENT != 0 && flags & IDENTITY_PRESENT == 0
                || read_u64(bytes, offset + 32)? != 0
            {
                return Err(ProtocolError::InvalidDescriptor("identity/reserved bits"));
            }
            let device = read_u64(bytes, offset + 8)?;
            let inode = read_u64(bytes, offset + 16)?;
            let mount = read_u64(bytes, offset + 24)?;
            if flags & IDENTITY_PRESENT == 0 && (device != 0 || inode != 0 || mount != 0)
                || flags & MOUNT_PRESENT == 0 && mount != 0
            {
                return Err(ProtocolError::InvalidDescriptor("unused identity bytes"));
            }
            let identity = (flags & IDENTITY_PRESENT != 0).then_some(ObjectIdentity {
                device,
                inode,
                mount_id: (flags & MOUNT_PRESENT != 0).then_some(mount),
            });
            descriptors.push(ManifestDescriptor {
                role,
                fd: read_i32(bytes, offset + 4)?,
                identity,
            });
        }
        let manifest = Self {
            kind,
            nonce: read_array(bytes, 24)?,
            generation: read_u64(bytes, 40)?,
            mm_generation: read_u64(bytes, 48)?,
            configuration_digest: read_array(bytes, 56)?,
            at_random: read_array(bytes, 88)?,
            descriptors,
            original_filename,
            comm,
            failure_fd: read_i32(bytes, 128)?,
            scratch_fd: read_i32(bytes, 132)?,
        };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Check the actual pinned object's identity before using the descriptor.
    ///
    /// The caller supplies a freshly verified mount identity if the manifest
    /// included one. Inode identity alone is not security or content protection.
    pub fn verify_object(
        &self,
        role: DescriptorRole,
        actual: ObjectIdentity,
    ) -> Result<(), ProtocolError> {
        let descriptor = self
            .descriptors
            .iter()
            .find(|descriptor| descriptor.role == role)
            .ok_or(ProtocolError::MissingRole(role))?;
        if descriptor.identity != Some(actual) {
            return Err(ProtocolError::ObjectIdentityChanged(role));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), ProtocolError> {
        if self.kind == StartKind::ExecContinuation {
            return Err(ProtocolError::UnsupportedContinuation);
        }
        if self.descriptors.len() > MAX_START_MANIFEST_DESCRIPTORS {
            return Err(ProtocolError::InvalidDescriptor("descriptor count"));
        }
        if self.original_filename.is_empty()
            || self.original_filename.len() >= crate::PATH_MAX
            || self.original_filename.contains(&0)
        {
            return Err(ProtocolError::InvalidString("F"));
        }
        if self.comm.len() > 15 || self.comm.contains(&0) {
            return Err(ProtocolError::InvalidString("comm"));
        }
        let mut roles = BTreeSet::new();
        let mut numbers = BTreeSet::new();
        for descriptor in &self.descriptors {
            descriptor.role.encode()?;
            if descriptor.fd < 0 {
                return Err(ProtocolError::InvalidDescriptor("negative FD"));
            }
            if !roles.insert(descriptor.role) {
                return Err(ProtocolError::DuplicateRole(descriptor.role));
            }
            if !numbers.insert(descriptor.fd) {
                return Err(ProtocolError::DuplicateFd(descriptor.fd));
            }
            if descriptor.role.has_file_identity() != descriptor.identity.is_some() {
                return Err(ProtocolError::InvalidDescriptor("role/object identity"));
            }
        }
        for role in [
            DescriptorRole::Program,
            DescriptorRole::Interpreter,
            DescriptorRole::Metadata,
            DescriptorRole::Failure,
            DescriptorRole::Control,
            DescriptorRole::Scratch,
        ] {
            if !roles.contains(&role) {
                return Err(ProtocolError::MissingRole(role));
            }
        }
        for (role, number) in [
            (DescriptorRole::Failure, self.failure_fd),
            (DescriptorRole::Scratch, self.scratch_fd),
        ] {
            if !self
                .descriptors
                .iter()
                .any(|descriptor| descriptor.role == role && descriptor.fd == number)
            {
                return Err(ProtocolError::InvalidDescriptor("header/role FD mismatch"));
            }
        }
        Ok(())
    }
}

fn checked_add(one: usize, two: usize) -> Result<usize, ProtocolError> {
    one.checked_add(two).ok_or(ProtocolError::SizeOverflow)
}

fn checked_mul(one: usize, two: usize) -> Result<usize, ProtocolError> {
    one.checked_mul(two).ok_or(ProtocolError::SizeOverflow)
}

fn as_u32(value: usize) -> Result<u32, ProtocolError> {
    value.try_into().map_err(|_| ProtocolError::SizeOverflow)
}

fn as_usize(value: u32) -> Result<usize, ProtocolError> {
    value.try_into().map_err(|_| ProtocolError::SizeOverflow)
}

fn put_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_i32(bytes: &mut Vec<u8>, value: i32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], ProtocolError> {
    let end = checked_add(offset, N)?;
    bytes
        .get(offset..end)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ProtocolError> {
    Ok(u16::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ProtocolError> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, ProtocolError> {
    Ok(i32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ProtocolError> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

fn decode_string(bytes: &[u8], field: &'static str) -> Result<Vec<u8>, ProtocolError> {
    let Some((&0, contents)) = bytes.split_last() else {
        return Err(ProtocolError::InvalidString(field));
    };
    if contents.contains(&0) {
        return Err(ProtocolError::InvalidString(field));
    }
    Ok(contents.to_vec())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;

    use super::*;

    fn manifest() -> StartManifest {
        let identity = ObjectIdentity {
            device: 3,
            inode: 7,
            mount_id: Some(11),
        };
        StartManifest {
            kind: StartKind::RootInitial,
            nonce: [0x23; 16],
            generation: 29,
            mm_generation: 31,
            configuration_digest: [0x37; 32],
            at_random: [0x41; 16],
            descriptors: [
                (DescriptorRole::Program, 100, Some(identity)),
                (DescriptorRole::Interpreter, 102, Some(identity)),
                (DescriptorRole::Metadata, 1024, None),
                (DescriptorRole::Failure, 1025, None),
                (DescriptorRole::Control, 1026, None),
                (DescriptorRole::Scratch, 1027, None),
                (DescriptorRole::Script(0), 1028, Some(identity)),
            ]
            .into_iter()
            .map(|(role, fd, identity)| ManifestDescriptor { role, fd, identity })
            .collect(),
            original_filename: b"./original-script".to_vec(),
            comm: b"original-script".to_vec(),
            failure_fd: 1025,
            scratch_fd: 1027,
        }
    }

    #[test]
    fn lb4_manifest_roundtrip_dynamic_fds_and_exact_root_payload() {
        let source = manifest();
        let bytes = source.encode().unwrap();
        assert_eq!(StartManifest::decode(&bytes).unwrap(), source);
        assert_eq!(&bytes[88..104], &[0x41; 16]);
        assert_eq!(source.original_filename, b"./original-script");
        assert_ne!(source.descriptors[0].role, DescriptorRole::Script(0));
    }

    #[test]
    fn lb4_manifest_rejects_version_extents_roles_and_fd_mutations() {
        let original = manifest().encode().unwrap();
        let mutations: Vec<(usize, Vec<u8>)> = vec![
            (8, 2_u16.to_le_bytes().to_vec()),
            (10, 0_u16.to_le_bytes().to_vec()),
            (12, u32::MAX.to_le_bytes().to_vec()),
            (104, u32::MAX.to_le_bytes().to_vec()),
            (108, 0_u32.to_le_bytes().to_vec()),
            (112, 144_u32.to_le_bytes().to_vec()),
            (116, u32::MAX.to_le_bytes().to_vec()),
            (120, 144_u32.to_le_bytes().to_vec()),
            (124, u32::MAX.to_le_bytes().to_vec()),
            (128, 77_i32.to_le_bytes().to_vec()),
            (132, 77_i32.to_le_bytes().to_vec()),
            (136, 1_u64.to_le_bytes().to_vec()),
            (HEADER_BYTES, 0_u16.to_le_bytes().to_vec()),
            (HEADER_BYTES + 2, 0x80_u16.to_le_bytes().to_vec()),
            (HEADER_BYTES + 4, (-1_i32).to_le_bytes().to_vec()),
            (
                HEADER_BYTES + DESCRIPTOR_BYTES,
                1_u16.to_le_bytes().to_vec(),
            ),
            (
                HEADER_BYTES + DESCRIPTOR_BYTES + 4,
                100_i32.to_le_bytes().to_vec(),
            ),
        ];
        for (offset, replacement) in mutations {
            let mut changed = original.clone();
            changed[offset..offset + replacement.len()].copy_from_slice(&replacement);
            assert!(
                StartManifest::decode(&changed).is_err(),
                "mutation at {offset} was accepted"
            );
        }
        for length in [0, 8, HEADER_BYTES - 1, original.len() - 1] {
            assert!(StartManifest::decode(&original[..length]).is_err());
        }
        let mut trailing = original;
        trailing.push(0);
        assert!(StartManifest::decode(&trailing).is_err());
    }

    #[test]
    fn lb4_manifest_checks_string_bounds_and_required_private_roles() {
        let mut source = manifest();
        source.original_filename = vec![b'f'; crate::PATH_MAX - 1];
        source.comm = vec![b'c'; 15];
        assert_eq!(
            StartManifest::decode(&source.encode().unwrap()).unwrap(),
            source
        );
        source.original_filename.push(b'f');
        assert_eq!(
            source.encode().unwrap_err(),
            ProtocolError::InvalidString("F")
        );
        source = manifest();
        source.comm.push(b'c');
        assert_eq!(
            source.encode().unwrap_err(),
            ProtocolError::InvalidString("comm")
        );
        source = manifest();
        source
            .descriptors
            .retain(|descriptor| descriptor.role != DescriptorRole::Scratch);
        assert_eq!(
            source.encode().unwrap_err(),
            ProtocolError::MissingRole(DescriptorRole::Scratch)
        );
        source = manifest();
        source.descriptors[0].identity = None;
        assert!(matches!(
            source.encode(),
            Err(ProtocolError::InvalidDescriptor("role/object identity"))
        ));
    }

    #[test]
    fn lb7_continuation_is_not_initial_root_state() {
        let mut source = manifest();
        source.kind = StartKind::ExecContinuation;
        assert_eq!(
            source.encode().unwrap_err(),
            ProtocolError::UnsupportedContinuation
        );
        source.kind = StartKind::RootInitial;
        let mut bytes = source.encode().unwrap();
        bytes[16..20].copy_from_slice(&EXEC_CONTINUATION_TAG.to_le_bytes());
        assert_eq!(
            StartManifest::decode(&bytes).unwrap_err(),
            ProtocolError::UnsupportedContinuation
        );
    }

    #[test]
    fn lb2_manifest_identity_check_rejects_path_reopen_mutation() {
        // Atomic per-invocation creation: PID namespaces sharing the artifact
        // directory may reuse this process's PID.
        let directory = crate::test_support::fixture_dir_in(
            std::path::Path::new(
                option_env!("ELF_LOADER_ARTIFACT_DIR").unwrap_or("target/lb4-artifacts"),
            ),
            "lb2-manifest-binding",
        );
        let pathname = directory.join("program");
        let replacement_path = directory.join("replacement");
        let original_bytes = b"retained original object";
        std::fs::write(&pathname, original_bytes).unwrap();
        std::fs::write(&replacement_path, b"pathname replacement").unwrap();
        let pinned = File::open(&pathname).unwrap();
        let mut stat: libc::statx = unsafe { std::mem::zeroed() };
        // SAFETY: the descriptor remains owned; empty-path metadata does no
        // pathname traversal and fills the correctly sized output buffer.
        assert_eq!(
            unsafe {
                libc::statx(
                    pinned.as_raw_fd(),
                    c"".as_ptr(),
                    libc::AT_EMPTY_PATH,
                    libc::STATX_MNT_ID,
                    &mut stat,
                )
            },
            0
        );
        assert_ne!(stat.stx_mask & libc::STATX_MNT_ID, 0);
        let original = ObjectIdentity::from_file(&pinned, Some(stat.stx_mnt_id)).unwrap();
        let mut source = manifest();
        source.descriptors[0].fd = pinned.as_raw_fd();
        source.descriptors[0].identity = Some(original);
        let source = StartManifest::decode(&source.encode().unwrap()).unwrap();
        source
            .verify_object(DescriptorRole::Program, original)
            .unwrap();
        std::fs::rename(&replacement_path, &pathname).unwrap();
        // The retained descriptor still reads its original object. Mutation:
        // reopening by pathname now selects an actual different inode.
        let mut retained_bytes = [0; 24];
        std::os::unix::fs::FileExt::read_exact_at(&pinned, &mut retained_bytes, 0).unwrap();
        assert_eq!(&retained_bytes, original_bytes);
        source
            .verify_object(
                DescriptorRole::Program,
                ObjectIdentity::from_file(&pinned, Some(stat.stx_mnt_id)).unwrap(),
            )
            .unwrap();
        let reopened = File::open(&pathname).unwrap();
        // This fixture's rename replaces within one directory/mount. Inode
        // mismatch alone suffices to reject the path-reopen mutation.
        let replacement = ObjectIdentity::from_file(&reopened, Some(stat.stx_mnt_id)).unwrap();
        assert_ne!(original.inode, replacement.inode);
        assert_eq!(
            source
                .verify_object(DescriptorRole::Program, replacement)
                .unwrap_err(),
            ProtocolError::ObjectIdentityChanged(DescriptorRole::Program)
        );
        let detached = ObjectIdentity {
            mount_id: None,
            ..original
        };
        assert!(
            source
                .verify_object(DescriptorRole::Program, detached)
                .is_err()
        );
        std::fs::remove_file(pathname).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
