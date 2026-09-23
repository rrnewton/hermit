/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * SPDX-License-Identifier: BSD-3-Clause
 */

//! Discovery and immutable per-process copies of the maintained provider package.
//! This module never loads BPF, invokes sudo, or infers backend capability.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use detcore::Digest;
use detcore::network_runtime::ProviderArtifact;
use serde::Deserialize;

const OBJECT: &str = "accepted-provider.bpf.o";
const LIBRARY: &str = "libhermit_accepted_provider.so";
const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
const ACCEPTED_CONTRACT: &str = include_str!("../network-provider/accepted-contract.json");

#[derive(Debug, Deserialize)]
struct Contract {
    schema: u32,
    abi_version: String,
    btf_sha256: String,
    maps: usize,
    programs: usize,
    links: usize,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    schema: u32,
    kind: String,
    abi_version: String,
    object: String,
    library: String,
    object_sha256: String,
    library_sha256: String,
    btf_sha256: String,
    maps: usize,
    programs: usize,
    links: usize,
}

fn digest_hex(text: &str) -> io::Result<[u8; 32]> {
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::other(
            "provider digest must be 64 lowercase hexadecimal digits",
        ));
    }
    let mut result = [0; 32];
    for (index, value) in result.iter_mut().enumerate() {
        *value =
            u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(io::Error::other)?;
    }
    Ok(result)
}

fn read_regular(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return Err(io::Error::other(
            "provider input is not a bounded regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(io::Error::other(
            "provider input exceeded its byte bound or was empty",
        ));
    }
    Ok(bytes)
}

/// Product artifact paths and authenticated fingerprints for the parent launch.
/// Each helper will snapshot these bytes into sealed files before loading them.
#[derive(Debug)]
pub struct PackagedAcceptedProvider {
    /// Maintained object pathname selected through package discovery.
    pub object: PathBuf,
    /// Maintained C adapter pathname selected through package discovery.
    pub library: PathBuf,
    /// Exact contents and inventory used in private bootstrap authentication.
    pub expected: ProviderArtifact,
}

impl PackagedAcceptedProvider {
    /// Resolve one explicit package; neither a missing package nor an unsupported
    /// ABI permits live-network fallback.
    pub fn open(directory: &Path) -> io::Result<Self> {
        if !directory.is_absolute() || directory.symlink_metadata()?.file_type().is_symlink() {
            return Err(io::Error::other(
                "provider package must be an absolute real directory",
            ));
        }
        let contract: Contract = serde_json::from_str(ACCEPTED_CONTRACT)?;
        let manifest: Manifest =
            serde_json::from_slice(&read_regular(&directory.join("manifest.json"), 32768)?)?;
        if manifest.schema != 1
            || manifest.kind != "hermit-accepted-provider"
            || manifest.abi_version != contract.abi_version
            || manifest.object != OBJECT
            || manifest.library != LIBRARY
            || (manifest.maps, manifest.programs, manifest.links)
                != (contract.maps, contract.programs, contract.links)
            || manifest.btf_sha256 != contract.btf_sha256
            || contract.schema != 1
            || contract.maps == 0
            || contract.programs == 0
            || contract.links == 0
        {
            return Err(io::Error::other(
                "provider package schema, inventory, or kernel ABI is unsupported",
            ));
        }
        let object = directory.join(OBJECT);
        let library = directory.join(LIBRARY);
        let object_bytes = read_regular(&object, MAX_ARTIFACT_BYTES)?;
        let library_bytes = read_regular(&library, MAX_ARTIFACT_BYTES)?;
        let expected = ProviderArtifact {
            object_sha256: digest_hex(&manifest.object_sha256)?,
            library_sha256: digest_hex(&manifest.library_sha256)?,
            btf_sha256: digest_hex(&manifest.btf_sha256)?,
            maps: contract.maps,
            programs: contract.programs,
            links: contract.links,
        };
        if *Digest::new(&object_bytes) != expected.object_sha256
            || *Digest::new(&library_bytes) != expected.library_sha256
        {
            return Err(io::Error::other(
                "provider package content does not match its manifest",
            ));
        }
        validate_elf(&object_bytes, 247)?;
        validate_elf(&library_bytes, 62)?;
        // The same test runs again inside the privileged service before dlopen.
        // Build headers or matching CO-RE fields are not authority for ctx slots.
        if *Digest::digest_path("/sys/kernel/btf/vmlinux")? != expected.btf_sha256 {
            return Err(io::Error::other(
                "running kernel BTF differs from the qualified provider package",
            ));
        }
        Ok(Self {
            object,
            library,
            expected,
        })
    }

    /// Cargo/Make package output is beside the executable. Installation uses
    /// PREFIX/lib/hermit/network-provider beside PREFIX/bin/hermit. There is no
    /// cwd search, environment override, or silent choice between two packages.
    pub fn discover(executable: &Path) -> io::Result<Self> {
        let directory = executable
            .parent()
            .filter(|_| executable.is_absolute())
            .ok_or_else(|| {
                io::Error::other("provider discovery requires the actual absolute executable path")
            })?;
        let mut candidates = vec![directory.join("network-provider/accepted")];
        if let Some(prefix) = directory.parent() {
            candidates.push(prefix.join("lib/hermit/network-provider/accepted"));
        }
        let mut existing = Vec::new();
        for path in candidates {
            match path.symlink_metadata() {
                Ok(_) => existing.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if existing.len() != 1 {
            return Err(io::Error::other(
                "expected exactly one maintained network-provider package; run the explicit provider packaging step",
            ));
        }
        Self::open(&existing[0])
    }
}

fn validate_elf(bytes: &[u8], machine: u16) -> io::Result<()> {
    if bytes.len() < 64
        || bytes[..6] != *b"\x7fELF\x02\x01"
        || u16::from_le_bytes([bytes[18], bytes[19]]) != machine
        || (machine == 62 && u16::from_le_bytes([bytes[16], bytes[17]]) != 3)
        || (machine == 247 && u16::from_le_bytes([bytes[16], bytes[17]]) != 1)
    {
        return Err(io::Error::other(
            "provider artifact has an unexpected ELF architecture or type",
        ));
    }
    Ok(())
}

fn sealed_file(name: &std::ffi::CStr, bytes: &[u8], executable: bool) -> io::Result<File> {
    let flags = libc::MFD_CLOEXEC
        | libc::MFD_ALLOW_SEALING
        | if executable {
            libc::MFD_EXEC
        } else {
            libc::MFD_NOEXEC_SEAL
        };
    // SAFETY: name is terminated and the exact return becomes one owned file.
    let raw = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(raw) };
    file.write_all(bytes)?;
    let mode = if executable { 0o500 } else { 0o400 };
    if unsafe { libc::fchmod(raw, mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let added = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(raw, libc::F_ADD_SEALS, added) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let expected = added | if executable { 0 } else { libc::F_SEAL_EXEC };
    if unsafe { libc::fcntl(raw, libc::F_GET_SEALS) } != expected {
        return Err(io::Error::other("provider memfd seal readback mismatch"));
    }
    file.seek(SeekFrom::Start(0))?;
    if Digest::digest_reader(&mut file)? != Digest::new(bytes) {
        return Err(io::Error::other("provider memfd content readback mismatch"));
    }
    Ok(file)
}

/// These actual files must stay owned until the nonreturning service exits. C
/// opens only these local immutable objects, never the mutable package paths.
#[derive(Debug)]
pub struct SealedAcceptedArtifacts {
    object: File,
    library: File,
}

impl SealedAcceptedArtifacts {
    /// Read once and seal, without loading code. The service subsequently checks
    /// the hashes against its separately authenticated private bootstrap.
    pub fn snapshot(object: &Path, library: &Path) -> io::Result<Self> {
        if !object.is_absolute() || !library.is_absolute() {
            return Err(io::Error::other("provider artifact paths must be absolute"));
        }
        let object_bytes = read_regular(object, MAX_ARTIFACT_BYTES)?;
        let library_bytes = read_regular(library, MAX_ARTIFACT_BYTES)?;
        validate_elf(&object_bytes, 247)?;
        validate_elf(&library_bytes, 62)?;
        Ok(Self {
            object: sealed_file(c"hermit-accepted-bpf", &object_bytes, false)?,
            library: sealed_file(c"hermit-accepted-adapter", &library_bytes, true)?,
        })
    }

    /// Same-process paths are tied to held owned files; no remote numeric FD or
    /// pathname lookup substitutes for these capabilities.
    pub fn paths(&self) -> (CString, CString) {
        (
            CString::new(format!("/proc/self/fd/{}", self.object.as_raw_fd())).unwrap(),
            CString::new(format!("/proc/self/fd/{}", self.library.as_raw_fd())).unwrap(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn package_digests_require_exact_lowercase_bytes() {
        assert_eq!(digest_hex(&"ab".repeat(32)).unwrap(), [0xab; 32]);
        for text in [
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            "gg".repeat(32),
        ] {
            assert!(digest_hex(&text).is_err());
        }
    }
    #[test]
    fn package_elf_requires_expected_architecture_and_shared_type() {
        let mut bytes = [0u8; 64];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        validate_elf(&bytes, 62).unwrap();
        assert!(validate_elf(&bytes, 247).is_err());
        assert!(validate_elf(&bytes[..63], 62).is_err());
        bytes[16] = 2;
        assert!(validate_elf(&bytes, 62).is_err());
        bytes[16] = 3;
        bytes[5] = 2;
        assert!(validate_elf(&bytes, 62).is_err());
    }
    #[test]
    fn sealed_provider_bytes_reject_write_and_resize() {
        let mut file = sealed_file(c"hermit-package-test", b"immutable", false).unwrap();
        assert_eq!(
            file.write(b"x").unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        assert_eq!(
            file.set_len(0).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"immutable");
    }
}
