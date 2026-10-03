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
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;

use detcore::Digest;
use detcore::network_runtime::ProviderArtifact;
use detcore::network_runtime::ProviderTopology;
use detcore::network_runtime::ProviderWireFormat;
use serde::Deserialize;

const OBJECT: &str = "accepted-provider.bpf.o";
const LIBRARY: &str = "libhermit_accepted_provider.so";
const NAMESPACE_SETUP: &str = "hermit-grouped-namespace-setup";
pub(crate) const MAX_ARTIFACT_BYTES: usize = 1024 * 1024;
const ACCEPTED_CONTRACT: &str = include_str!("../network-provider/accepted-contract.json");
// Immutable reviewed grouped-v1 schema/site constants, independent of the
// selected compiled package. Ftrace-v1 instead matches this source's selected
// contract, whose compatibility table names every retired physical role.
const GROUPED_V1_CONTRACT: &str = include_str!("network_provider_package/grouped-v1-contract.json");

#[derive(Debug, Default, PartialEq, Eq)]
enum GroupedDeclaration {
    #[default]
    Absent,
    Present(serde_json::Value),
}
fn present_grouped<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<GroupedDeclaration, D::Error> {
    serde_json::Value::deserialize(deserializer).map(GroupedDeclaration::Present)
}

#[derive(Debug, Deserialize)]
struct Contract {
    #[serde(default, deserialize_with = "present_grouped")]
    grouped_event: GroupedDeclaration,
    #[serde(default)]
    ftrace_only: bool,
    schema: u32,
    abi_version: String,
    #[serde(default)]
    copy_version: Option<u64>,
    btf_sha256: String,
    maps: usize,
    programs: usize,
    links: usize,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default, deserialize_with = "present_grouped")]
    grouped_event: GroupedDeclaration,
    #[serde(default)]
    ftrace_only: bool,
    schema: u32,
    kind: String,
    abi_version: String,
    #[serde(default)]
    copy_version: Option<u64>,
    object: String,
    library: String,
    object_sha256: String,
    library_sha256: String,
    #[serde(default, deserialize_with = "present_string")]
    namespace_setup: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    namespace_setup_sha256: Option<String>,
    btf_sha256: String,
    maps: usize,
    programs: usize,
    links: usize,
}

// Absence preserves classic packages; an explicit null is not a declaration.
fn present_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

// Explicitly sort every object regardless of serde_json feature unification.
// Array order, integer values, absent fields and every complete contract field
// remain significant; this is not a digest of just the site count or inventory.
fn canonical_value(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(fields) => {
            let sorted: std::collections::BTreeMap<_, _> = fields
                .iter()
                .map(|(key, value)| (key.clone(), canonical_value(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_value).collect()),
        other => other.clone(),
    }
}

fn package_metadata(
    compiled: &str,
    manifest_bytes: &[u8],
) -> io::Result<(Contract, Manifest, ProviderWireFormat, ProviderTopology)> {
    let contract: Contract = serde_json::from_str(compiled)?;
    let manifest: Manifest = serde_json::from_slice(manifest_bytes)?;
    let wire_format =
        ProviderWireFormat::from_package(&contract.abi_version, contract.copy_version)?;
    let actual_wire =
        ProviderWireFormat::from_package(&manifest.abi_version, manifest.copy_version)?;
    if actual_wire != wire_format
        || manifest.schema != 1
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
    digest_hex(&manifest.object_sha256)?;
    digest_hex(&manifest.library_sha256)?;
    digest_hex(&manifest.btf_sha256)?;
    if contract.grouped_event != manifest.grouped_event
        || contract.ftrace_only != manifest.ftrace_only
    {
        return Err(io::Error::other(
            "provider nonclassic topology differs from compiled contract",
        ));
    }
    let topology = match (&contract.grouped_event, contract.ftrace_only) {
        (GroupedDeclaration::Absent, false) => {
            if (contract.maps, contract.programs, contract.links)
                != (
                    23 + usize::from(contract.abi_version == "415052555354000b"),
                    46,
                    58,
                )
            {
                return Err(io::Error::other("unsupported classic provider topology"));
            }
            ProviderTopology::ClassicV40
        }
        (GroupedDeclaration::Present(group), ftrace) => {
            let known: serde_json::Value = serde_json::from_str(if ftrace {
                ACCEPTED_CONTRACT
            } else {
                GROUPED_V1_CONTRACT
            })?;
            if Some(group) != known.get("grouped_event")
                || known.get("btf_sha256").and_then(serde_json::Value::as_str)
                    != Some(contract.btf_sha256.as_str())
                || (contract.maps, contract.programs, contract.links)
                    != if ftrace {
                        (
                            24 + usize::from(contract.abi_version == "415052555354000b"),
                            49,
                            49,
                        )
                    } else {
                        (
                            23 + usize::from(contract.abi_version == "415052555354000b"),
                            44,
                            44,
                        )
                    }
            {
                return Err(io::Error::other(
                    "unsupported grouped provider schema or physical sites",
                ));
            }
            let complete: serde_json::Value = serde_json::from_str(compiled)?;
            let canonical = serde_json::to_vec(&canonical_value(&complete))?;
            if ftrace {
                ProviderTopology::FtraceV1 {
                    contract_sha256: *Digest::new(&canonical),
                }
            } else {
                ProviderTopology::GroupedV1 {
                    contract_sha256: *Digest::new(&canonical),
                }
            }
        }
        (GroupedDeclaration::Absent, true) => {
            return Err(io::Error::other(
                "ftrace provider lacks its compatibility coverage table",
            ));
        }
    };
    topology.validate()?;
    match (
        &topology,
        &manifest.namespace_setup,
        &manifest.namespace_setup_sha256,
    ) {
        (ProviderTopology::ClassicV40, None, None) => {}
        (ProviderTopology::FtraceV1 { .. }, None, None) => {}
        (ProviderTopology::GroupedV1 { .. }, Some(name), Some(hash)) if name == NAMESPACE_SETUP => {
            digest_hex(hash)?;
        }
        _ => {
            return Err(io::Error::other(
                "provider namespace setup member is missing or unsupported",
            ));
        }
    }
    Ok((contract, manifest, wire_format, topology))
}

pub(crate) fn digest_hex(text: &str) -> io::Result<[u8; 32]> {
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

pub(crate) fn read_regular(path: &Path, limit: usize) -> io::Result<Vec<u8>> {
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
    /// Trusted installed setup executable, required only by grouped packages.
    pub namespace_setup: Option<PathBuf>,
    /// Full setup-file fingerprint; this does not grant launcher privilege.
    pub namespace_setup_sha256: Option<[u8; 32]>,
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
        let manifest_bytes = read_regular(&directory.join("manifest.json"), 32768)?;
        let (contract, manifest, wire_format, topology) =
            package_metadata(ACCEPTED_CONTRACT, &manifest_bytes)?;
        let object = directory.join(OBJECT);
        let library = directory.join(LIBRARY);
        let namespace_setup = manifest
            .namespace_setup
            .as_ref()
            .map(|name| directory.join(name));
        let namespace_setup_sha256 = manifest
            .namespace_setup_sha256
            .as_deref()
            .map(digest_hex)
            .transpose()?;
        let object_bytes = read_regular(&object, MAX_ARTIFACT_BYTES)?;
        let library_bytes = read_regular(&library, MAX_ARTIFACT_BYTES)?;
        let expected = ProviderArtifact {
            topology,
            wire_format,
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
        let package = Self {
            object,
            library,
            namespace_setup,
            namespace_setup_sha256,
            expected,
        };
        // Validate the actual file now. Startup obtains its own continuously
        // held, revalidated file through this same method before any spawn.
        drop(package.open_namespace_setup()?);
        Ok(package)
    }

    /// Open and authenticate one bounded native executable without running it.
    /// The caller must retain this file through launch and use the existing
    /// trusted immutable installation and launcher policy. A caller-supplied
    /// pathname or matching hash never grants privileged execution authority.
    pub fn open_namespace_setup(&self) -> io::Result<Option<File>> {
        match (&self.namespace_setup, self.namespace_setup_sha256) {
            (None, None) => Ok(None),
            (Some(path), Some(expected)) => open_namespace_setup(path, expected).map(Some),
            _ => Err(io::Error::other(
                "provider namespace setup path/hash pair is incomplete",
            )),
        }
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

fn open_namespace_setup(path: &Path, expected: [u8; 32]) -> io::Result<File> {
    if !path.is_absolute() || path.file_name() != Some(std::ffi::OsStr::new(NAMESPACE_SETUP)) {
        return Err(io::Error::other(
            "provider namespace setup path is unsupported",
        ));
    }
    let mut file = File::options()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_ARTIFACT_BYTES as u64
        || metadata.permissions().mode() & 0o111 == 0
    {
        return Err(io::Error::other(
            "provider namespace setup is not a bounded executable file",
        ));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() != metadata.len() as usize || *Digest::new(&bytes) != expected {
        return Err(io::Error::other(
            "provider namespace setup content does not match its manifest",
        ));
    }
    validate_namespace_setup_elf(&bytes)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

fn validate_namespace_setup_elf(bytes: &[u8]) -> io::Result<()> {
    validate_elf(bytes, 62)?;
    // The approved native PIE has an entry point; a shared adapter alone is
    // not the setup executable. The complete expected hash binds all bytes.
    if u64::from_le_bytes(bytes[24..32].try_into().unwrap()) == 0 {
        return Err(io::Error::other(
            "provider namespace setup ELF has no entry point",
        ));
    }
    Ok(())
}

pub(crate) fn validate_elf(bytes: &[u8], machine: u16) -> io::Result<()> {
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

pub(crate) fn sealed_file(
    name: &std::ffi::CStr,
    bytes: &[u8],
    executable: bool,
) -> io::Result<File> {
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

    /// Borrow the actual sealed library for grouped broker custody. The caller
    /// must still authenticate its digest against the private bootstrap before
    /// loading it; this borrow itself grants no provider authority.
    pub fn library_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.library)
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
    fn manifest_for(contract: &serde_json::Value) -> serde_json::Value {
        let mut value = serde_json::json!({
            "schema": 1, "kind": "hermit-accepted-provider",
            "abi_version": contract["abi_version"], "btf_sha256": contract["btf_sha256"],
            "maps": contract["maps"], "programs": contract["programs"], "links": contract["links"],
            "object": OBJECT, "library": LIBRARY,
            "object_sha256": "11".repeat(32), "library_sha256": "22".repeat(32),
        });
        for key in ["copy_version", "grouped_event", "ftrace_only"] {
            if let Some(field) = contract.get(key) {
                value[key] = field.clone();
            }
        }
        if contract.get("grouped_event").is_some()
            && contract
                .get("ftrace_only")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            value["namespace_setup"] = serde_json::json!(NAMESPACE_SETUP);
            value["namespace_setup_sha256"] = serde_json::json!("33".repeat(32));
        }
        value
    }
    fn metadata(
        contract: &serde_json::Value,
        manifest: &serde_json::Value,
    ) -> io::Result<ProviderTopology> {
        package_metadata(
            &serde_json::to_string(contract).unwrap(),
            &serde_json::to_vec(manifest).unwrap(),
        )
        .map(|v| v.3)
    }
    fn grouped_fixture() -> serde_json::Value {
        assert_eq!(
            *Digest::new(GROUPED_V1_CONTRACT.as_bytes()),
            digest_hex("a3ef4954a0045105be33d82d58d70dafe9d70df33e948f427aedb898987a80f1").unwrap()
        );
        serde_json::from_str(GROUPED_V1_CONTRACT).unwrap()
    }
    #[test]
    fn current_ftrace_manifest_requires_exact_123_shape() {
        let contract: serde_json::Value = serde_json::from_str(ACCEPTED_CONTRACT).unwrap();
        let manifest = manifest_for(&contract);
        assert_eq!(
            (
                manifest["maps"].as_u64(),
                manifest["programs"].as_u64(),
                manifest["links"].as_u64()
            ),
            (Some(25), Some(49), Some(49))
        );
        assert!(metadata(&contract, &manifest).is_ok());
        eprintln!("current Ftrace123 qualifying neighbor accepted");
        for counts in [
            [23, 47, 47],
            [23, 49, 49],
            [24, 49, 49],
            [26, 49, 49],
            [24, 48, 49],
            [25, 48, 49],
            [25, 50, 49],
            [25, 49, 48],
            [25, 49, 50],
            [24, 50, 49],
            [24, 49, 48],
            [24, 49, 50],
        ] {
            let mut wrong = manifest.clone();
            for (key, count) in ["maps","programs","links"].into_iter().zip(counts) {
                wrong[key] = serde_json::json!(count);
            }
            assert!(metadata(&contract, &wrong).is_err(), "{counts:?}");
            let mut paired_contract=contract.clone();
            for key in ["maps","programs","links"] {
                paired_contract[key]=wrong[key].clone();
            }
            // Internal compiled-contract shape control, not a runtime API for
            // replacing the trusted compiled contract or its hook declaration.
            assert!(metadata(&paired_contract, &wrong).is_err(),
                "hard Ftrace topology predicate accepted paired {counts:?}");
        }
    }
    #[test]
    fn package_topology_preserves_absence_and_rejects_crossed_or_unknown_contracts() {
        let grouped = grouped_fixture();
        let group_manifest = manifest_for(&grouped);
        let historical = include_str!("network_provider_package/classic-v40-contract.json");
        assert_eq!(
            *Digest::new(historical.as_bytes()),
            digest_hex("1ee918ec0f9cdf04040d7b2c53fbda7089de3118f0d0497fcd97d5838df2e751").unwrap()
        );
        let classic: serde_json::Value = serde_json::from_str(historical).unwrap();
        let selected: serde_json::Value = serde_json::from_str(ACCEPTED_CONTRACT).unwrap();
        let selected_manifest = manifest_for(&selected);
        assert!(matches!(
            metadata(&selected, &selected_manifest).unwrap(),
            ProviderTopology::FtraceV1 { .. }
        ));
        assert!(selected_manifest.get("namespace_setup").is_none());
        let mut missing_ftrace = selected_manifest.clone();
        missing_ftrace
            .as_object_mut()
            .unwrap()
            .remove("ftrace_only");
        assert!(metadata(&selected, &missing_ftrace).is_err());
        for role in 0..17 {
            let mut changed = selected.clone();
            let cookie = changed["grouped_event"]["sites"][role]["cookie"]
                .as_u64()
                .unwrap();
            changed["grouped_event"]["sites"][role]["cookie"] = serde_json::json!(cookie ^ 1);
            assert!(
                metadata(&changed, &manifest_for(&changed)).is_err(),
                "accepted changed role {role}"
            );
        }
        let classic_manifest = manifest_for(&classic);
        assert_eq!(
            metadata(&classic, &classic_manifest).unwrap(),
            ProviderTopology::ClassicV40
        );
        assert!(matches!(
            metadata(&grouped, &group_manifest).unwrap(),
            ProviderTopology::GroupedV1 { .. }
        ));
        assert!(metadata(&classic, &group_manifest).is_err());
        assert!(metadata(&grouped, &classic_manifest).is_err());
        for contract in [&classic, &grouped] {
            let manifest = manifest_for(contract);
            let mut missing = manifest.clone();
            missing.as_object_mut().unwrap().remove("grouped_event");
            assert_eq!(
                metadata(contract, &missing).is_ok(),
                contract.get("grouped_event").is_none()
            );
            let mut null = manifest.clone();
            null["grouped_event"] = serde_json::Value::Null;
            assert!(metadata(contract, &null).is_err());
            let mut both_null = contract.clone();
            both_null["grouped_event"] = serde_json::Value::Null;
            assert!(metadata(&both_null, &null).is_err());
        }
        for version in [0, 2, u64::MAX] {
            let mut unknown = grouped.clone();
            unknown["grouped_event"]["version"] = serde_json::json!(version);
            assert!(metadata(&unknown, &manifest_for(&unknown)).is_err());
        }
        let mut extra = grouped.clone();
        extra["grouped_event"]["unknown"] = serde_json::json!(true);
        assert!(metadata(&extra, &manifest_for(&extra)).is_err());
        // Duplicate declarations are refused by the production typed parser.
        let raw = serde_json::to_string(&group_manifest).unwrap();
        let duplicate = format!("{{\"grouped_event\":null,{}", &raw[1..]);
        assert!(package_metadata(GROUPED_V1_CONTRACT, duplicate.as_bytes()).is_err());
    }

    fn group_leaf_paths(value: &serde_json::Value, prefix: String, paths: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, child) in fields {
                    group_leaf_paths(child, format!("{prefix}/{key}"), paths);
                }
            }
            serde_json::Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    group_leaf_paths(child, format!("{prefix}/{index}"), paths);
                }
            }
            _ => paths.push(prefix),
        }
    }
    #[test]
    fn package_topology_requires_every_grouped_field_and_original_manifest_field() {
        let contract = grouped_fixture();
        let manifest = manifest_for(&contract);
        let mut paths = Vec::new();
        group_leaf_paths(
            &contract["grouped_event"],
            "/grouped_event".into(),
            &mut paths,
        );
        assert_eq!(paths.len(), 162); // 5 anchor fields + 17*5 sites + (20+4)*3 receive fields.
        for path in paths {
            let mut altered = manifest.clone();
            *altered.pointer_mut(&path).unwrap() = serde_json::Value::Null;
            assert!(
                metadata(&contract, &altered).is_err(),
                "accepted altered {path}"
            );
            let mut unknown_contract = contract.clone();
            *unknown_contract.pointer_mut(&path).unwrap() = serde_json::Value::Null;
            assert!(
                metadata(&unknown_contract, &manifest_for(&unknown_contract)).is_err(),
                "accepted unknown schema {path}"
            );
        }
        for key in [
            "version",
            "program",
            "cookie",
            "anchor_symbol",
            "anchor_address",
            "sites",
            "receive_entry",
            "receive_return",
        ] {
            let mut altered = manifest.clone();
            altered["grouped_event"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(metadata(&contract, &altered).is_err());
        }
        for key in [
            "schema",
            "kind",
            "abi_version",
            "btf_sha256",
            "maps",
            "programs",
            "links",
            "object",
            "library",
            "object_sha256",
            "library_sha256",
        ] {
            let mut absent = manifest.clone();
            absent.as_object_mut().unwrap().remove(key);
            assert!(
                metadata(&contract, &absent).is_err(),
                "accepted missing {key}"
            );
        }
        for key in ["schema", "maps", "programs", "links"] {
            let mut altered = manifest.clone();
            altered[key] = serde_json::json!(0);
            assert!(metadata(&contract, &altered).is_err());
        }
        for key in [
            "kind",
            "abi_version",
            "btf_sha256",
            "object",
            "library",
            "object_sha256",
            "library_sha256",
        ] {
            let mut altered = manifest.clone();
            altered[key] = serde_json::json!("wrong");
            assert!(metadata(&contract, &altered).is_err());
        }
    }

    #[test]
    fn package_topology_digest_binds_complete_contract_with_canonical_object_order() {
        let contract = grouped_fixture();
        let manifest = manifest_for(&contract);
        let baseline = metadata(&contract, &manifest).unwrap();
        let pretty = serde_json::to_string_pretty(&contract).unwrap();
        assert_eq!(
            package_metadata(&pretty, &serde_json::to_vec(&manifest).unwrap())
                .unwrap()
                .3,
            baseline
        );
        let fields = contract
            .as_object()
            .unwrap()
            .iter()
            .rev()
            .map(|(key, value)| {
                format!(
                    "{}:{}",
                    serde_json::to_string(key).unwrap(),
                    serde_json::to_string(value).unwrap()
                )
            })
            .collect::<Vec<_>>();
        let reordered = format!("{{{}}}", fields.join(","));
        assert_eq!(
            package_metadata(&reordered, &serde_json::to_vec(&manifest).unwrap())
                .unwrap()
                .3,
            baseline
        );
        let mut changed_complete_contract = contract.clone();
        changed_complete_contract["source_files"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("a-new-reviewed-source.h"));
        // This is a different hypothetical compiled contract, not permission to
        // change the deployed manifest. The digest must cover this field too.
        let changed = metadata(&changed_complete_contract, &manifest).unwrap();
        assert_ne!(changed, baseline);
        let ProviderTopology::GroupedV1 { contract_sha256 } = baseline else {
            panic!("grouped expected")
        };
        assert_eq!(
            contract_sha256,
            *Digest::new(&serde_json::to_vec(&canonical_value(&contract)).unwrap())
        );
    }

    #[test]
    fn package_namespace_setup_requires_exact_grouped_pair_and_preserves_classic_absence() {
        let grouped = grouped_fixture();
        let manifest = manifest_for(&grouped);
        metadata(&grouped, &manifest).unwrap();
        let classic: serde_json::Value = serde_json::from_str(include_str!(
            "network_provider_package/classic-v40-contract.json"
        ))
        .unwrap();
        let classic_manifest = manifest_for(&classic);
        assert!(classic_manifest.get("namespace_setup").is_none());
        assert!(classic_manifest.get("namespace_setup_sha256").is_none());
        assert_eq!(
            metadata(&classic, &classic_manifest).unwrap(),
            ProviderTopology::ClassicV40
        );
        let mut old_grouped = manifest.clone();
        for key in ["namespace_setup", "namespace_setup_sha256"] {
            old_grouped.as_object_mut().unwrap().remove(key);
            let mut missing = manifest.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(
                metadata(&grouped, &missing).is_err(),
                "accepted missing {key}"
            );
            for invalid in [
                serde_json::Value::Null,
                serde_json::json!(1),
                serde_json::json!(false),
            ] {
                let mut wrong = manifest.clone();
                wrong[key] = invalid;
                assert!(
                    metadata(&grouped, &wrong).is_err(),
                    "accepted malformed {key}"
                );
            }
            let mut classic_extra = classic_manifest.clone();
            classic_extra[key] = manifest[key].clone();
            assert!(metadata(&classic, &classic_extra).is_err());
            let raw = serde_json::to_string(&manifest).unwrap();
            let duplicate = format!(
                "{{{}:{},{}",
                serde_json::to_string(key).unwrap(),
                manifest[key],
                &raw[1..]
            );
            assert!(package_metadata(GROUPED_V1_CONTRACT, duplicate.as_bytes()).is_err());
        }
        assert!(metadata(&grouped, &old_grouped).is_err());
        for name in [
            "../hermit-grouped-namespace-setup",
            "/usr/bin/true",
            LIBRARY,
            "hermit-grouped-namespace-setup.old",
            "",
        ] {
            let mut wrong = manifest.clone();
            wrong["namespace_setup"] = serde_json::json!(name);
            assert!(metadata(&grouped, &wrong).is_err());
        }
        for hash in [
            "AB".repeat(32),
            "00".repeat(31),
            "00".repeat(33),
            "zz".repeat(32),
        ] {
            let mut wrong = manifest.clone();
            wrong["namespace_setup_sha256"] = serde_json::json!(hash);
            assert!(metadata(&grouped, &wrong).is_err());
        }
    }

    #[test]
    fn package_namespace_setup_holds_verified_file_and_refuses_changed_or_unbounded_inputs() {
        // These bytes qualify only the header/hash/file reader, never execution
        // of a synthetic ELF or the setup helper's security behavior.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(NAMESPACE_SETUP);
        let mut bytes = vec![0u8; 64];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
        bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
        bytes[24..32].copy_from_slice(&4096u64.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let hash = *Digest::new(&bytes);
        let mut held = open_namespace_setup(&path, hash).unwrap();
        let original = directory.path().join("retained-original");
        std::fs::rename(&path, &original).unwrap();
        let mut altered = bytes.clone();
        altered[63] = 1;
        std::fs::write(&path, &altered).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(open_namespace_setup(&path, hash).is_err());
        let mut held_bytes = Vec::new();
        held.read_to_end(&mut held_bytes).unwrap();
        assert_eq!(held_bytes, bytes);
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&original, &path).unwrap();
        assert_eq!(
            open_namespace_setup(&path, hash)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(open_namespace_setup(&path, hash).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len((MAX_ARTIFACT_BYTES + 1) as u64)
            .unwrap();
        assert!(open_namespace_setup(&path, hash).is_err());
        for (offset, value) in [(5, 2), (16, 2), (18, 247), (25, 0)] {
            let mut invalid = bytes.clone();
            invalid[offset] = value;
            std::fs::write(&path, &invalid).unwrap();
            assert!(open_namespace_setup(&path, *Digest::new(&invalid)).is_err());
        }
    }
}
