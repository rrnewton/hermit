//! Maintained Unix-policy artifacts and held deployment directories.
//!
//! Discovery never loads BPF or grants a runtime capability. A missing package,
//! incompatible kernel, or inaccessible private root is a pre-guest refusal.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use detcore::Digest;
use serde::Deserialize;

use crate::network_provider_package::MAX_ARTIFACT_BYTES;
use crate::network_provider_package::digest_hex;
use crate::network_provider_package::read_regular;
use crate::network_provider_package::sealed_file;
use crate::network_provider_package::validate_elf;

const CONTRACT: &str = include_str!("../network-provider/unix-guard-contract.json");
const OBJECT: &str = "unix-guard.bpf.o";
const HELPER: &str = "hermit-unix-keeper";

#[derive(Deserialize)]
struct Contract {
    schema: u32,
    abi_version: String,
    btf_sha256: String,
    maps: usize,
    programs: usize,
    links: usize,
}

#[derive(Deserialize)]
struct Manifest {
    schema: u32,
    kind: String,
    abi_version: String,
    object: String,
    helper: String,
    object_sha256: String,
    helper_sha256: String,
    btf_sha256: String,
    maps: usize,
    programs: usize,
    links: usize,
}

/// Checked maintained package. The object is already an immutable owned file.
/// The installed helper and its loader dependencies must be trusted deployment
/// inputs; a package manifest is not authorization to execute untrusted code.
#[derive(Debug)]
pub struct PackagedUnixGuard {
    /// Exact maintained helper executable selected before Container clone.
    pub helper: PathBuf,
    /// Sealed BPF bytes transferred over the private startup channel.
    pub object: OwnedFd,
}

impl PackagedUnixGuard {
    /// Resolve the one maintained build/install location, without cwd or
    /// environment artifact overrides. Ambiguous installations are refused.
    pub fn discover(executable: &Path) -> io::Result<Self> {
        let directory = executable
            .parent()
            .filter(|_| executable.is_absolute())
            .ok_or_else(|| {
                io::Error::other("guard discovery requires the actual absolute executable")
            })?;
        let mut candidates = vec![directory.join("network-provider/unix-guard")];
        if let Some(prefix) = directory.parent() {
            candidates.push(prefix.join("lib/hermit/network-provider/unix-guard"));
        }
        let mut found = Vec::new();
        for path in candidates {
            match path.symlink_metadata() {
                Ok(_) => found.push(path),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if found.len() != 1 {
            return Err(io::Error::other(
                "default Unix network policy requires exactly one maintained unix-guard package; see hermit-cli/network-provider/README.md",
            ));
        }
        Self::open(&found[0])
    }

    /// Validate the exact ABI, inventory, kernel and artifact bytes before any
    /// privileged launch. This is integrity checking, not verifier admission.
    pub fn open(directory: &Path) -> io::Result<Self> {
        if !directory.is_absolute() || directory.symlink_metadata()?.file_type().is_symlink() {
            return Err(io::Error::other(
                "guard package must be an absolute real directory",
            ));
        }
        let contract: Contract = serde_json::from_str(CONTRACT)?;
        let manifest: Manifest =
            serde_json::from_slice(&read_regular(&directory.join("manifest.json"), 32768)?)?;
        if contract.schema != 1
            || manifest.schema != 1
            || manifest.kind != "hermit-unix-guard"
            || manifest.abi_version != contract.abi_version
            || manifest.btf_sha256 != contract.btf_sha256
            || manifest.object != OBJECT
            || manifest.helper != HELPER
            || (manifest.maps, manifest.programs, manifest.links)
                != (contract.maps, contract.programs, contract.links)
            || (contract.maps, contract.programs, contract.links) != (10, 31, 31)
        {
            return Err(io::Error::other(
                "Unix guard package ABI, inventory or kernel contract differs",
            ));
        }
        let object = read_regular(&directory.join(OBJECT), MAX_ARTIFACT_BYTES)?;
        let helper = directory.join(HELPER);
        let helper_bytes = read_regular(&helper, MAX_ARTIFACT_BYTES)?;
        validate_elf(&object, 247)?;
        validate_elf(&helper_bytes, 62)?;
        if *Digest::new(&object) != digest_hex(&manifest.object_sha256)?
            || *Digest::new(&helper_bytes) != digest_hex(&manifest.helper_sha256)?
            || *Digest::digest_path("/sys/kernel/btf/vmlinux")? != digest_hex(&contract.btf_sha256)?
        {
            return Err(io::Error::other(
                "Unix guard artifact or running kernel identity differs",
            ));
        }
        if helper.metadata()?.mode() & 0o111 == 0 {
            return Err(io::Error::other("Unix keeper is not executable"));
        }
        Ok(Self {
            helper,
            object: sealed_file(c"hermit-unix-guard-bpf", &object, false)?.into(),
        })
    }
}

/// Actual private directory references retained through the helper transfer.
#[derive(Debug)]
pub struct GuardDeploymentRoots {
    /// Administrator-provisioned per-user bpffs directory.
    pub bpffs: OwnedFd,
    /// Persistent, private recovery journal directory.
    pub recovery: OwnedFd,
    /// The same two canonical paths used by the helper's mount policy.
    pub writable_paths: [PathBuf; 2],
}

fn open_private_directory(path: &Path, uid: u32, bpffs: bool) -> io::Result<OwnedFd> {
    if !path.is_absolute() || path.canonicalize()? != path {
        return Err(io::Error::other(
            "guard deployment root must be an absolute canonical directory",
        ));
    }
    let file = File::options()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    let stat = file.metadata()?;
    if !stat.is_dir() || stat.uid() != uid || stat.mode() & 0o7777 != 0o700 {
        return Err(io::Error::other(
            "guard deployment root must be owned by this user with mode0700",
        ));
    }
    if bpffs {
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { fs.assume_init() }.f_type != 0xcafe4a11 {
            return Err(io::Error::other("guard pin root is not bpffs"));
        }
    }
    Ok(file.into())
}

impl GuardDeploymentRoots {
    /// Open supported deployment roots. This never mounts bpffs or escalates
    /// privilege; missing administrator provisioning is an explicit refusal.
    /// Only the user's normal persistent state directory may be created here.
    pub fn open() -> io::Result<Self> {
        let uid = unsafe { libc::getuid() };
        if uid != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "guard startup requires matching real/effective uid",
            ));
        }
        let bpffs_path = PathBuf::from(format!("/sys/fs/bpf/hermit-{uid}"));
        let bpffs = open_private_directory(&bpffs_path, uid, true)?;
        let state = match std::env::var_os("XDG_STATE_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .ok_or_else(|| io::Error::other("no user state directory"))?,
            )
            .join(".local/state"),
        };
        if !state.is_absolute() {
            return Err(io::Error::other(
                "guard recovery state directory must be absolute",
            ));
        }
        let recovery_path = state.join("hermit/network-guard");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&recovery_path)?;
        let recovery_path = recovery_path.canonicalize()?;
        let recovery = open_private_directory(&recovery_path, uid, false)?;
        Ok(Self {
            bpffs,
            recovery,
            writable_paths: [bpffs_path, recovery_path],
        })
    }
}
