//! Maintained Unix-policy artifacts and held deployment directories.
//!
//! Discovery never loads BPF or grants a runtime capability. A missing package,
//! incompatible kernel, or inaccessible private root is a pre-guest refusal.

use std::fs::File;
use std::io;
use std::io::Read;
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::BorrowedFd;
use std::os::fd::FromRawFd;
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
const READBACK: &str = "hermit-unix-readback";
const MAX_UNRESOLVED_GUARD_LAUNCHES: usize = 8;

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
    readback: String,
    readback_sha256: String,
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
    /// Separate metadata-only helper; the loader never receives its privilege.
    pub readback: PathBuf,
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
            || manifest.readback != READBACK
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
        let readback = directory.join(READBACK);
        let readback_bytes = read_regular(&readback, MAX_ARTIFACT_BYTES)?;
        validate_elf(&object, 247)?;
        validate_elf(&helper_bytes, 62)?;
        validate_elf(&readback_bytes, 62)?;
        if *Digest::new(&object) != digest_hex(&manifest.object_sha256)?
            || *Digest::new(&helper_bytes) != digest_hex(&manifest.helper_sha256)?
            || *Digest::new(&readback_bytes) != digest_hex(&manifest.readback_sha256)?
            || *Digest::digest_path("/sys/kernel/btf/vmlinux")? != digest_hex(&contract.btf_sha256)?
        {
            return Err(io::Error::other(
                "Unix guard artifact or running kernel identity differs",
            ));
        }
        if helper.metadata()?.mode() & 0o111 == 0 || readback.metadata()?.mode() & 0o111 == 0 {
            return Err(io::Error::other("Unix policy helper is not executable"));
        }
        Ok(Self {
            helper,
            readback,
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
    /// Open explicit deployment roots through the same held-descriptor checks.
    /// Both directories must already exist. This never creates a mount, changes
    /// permissions, or turns configuration into a runtime capability.
    pub fn open_at(bpffs_path: &Path, recovery_path: &Path) -> io::Result<Self> {
        let uid = unsafe { libc::getuid() };
        if uid != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "guard startup requires matching real/effective uid",
            ));
        }
        let bpffs = open_private_directory(bpffs_path, uid, true)?;
        let recovery = open_private_directory(recovery_path, uid, false)?;
        Ok(Self {
            bpffs,
            recovery,
            writable_paths: [bpffs_path.to_owned(), recovery_path.to_owned()],
        })
    }

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
        let recovery = RecoveryDeploymentRoot::open()?;
        Ok(Self {
            bpffs,
            recovery: recovery.directory,
            writable_paths: [bpffs_path, recovery.writable_path],
        })
    }
    /// Refuse another privileged load at the durable unresolved-attempt bound.
    /// This never removes pins, receipts or units.
    pub fn admit_launch(&self) -> io::Result<()> {
        let uid=unsafe{libc::getuid()};
        let bpffs=open_private_directory(&self.writable_paths[0],uid,true)?;
        let recovery=open_private_directory(&self.writable_paths[1],uid,false)?;
        if directory_identity(bpffs.as_fd())?!=directory_identity(self.bpffs.as_fd())?
            || directory_identity(recovery.as_fd())?!=directory_identity(self.recovery.as_fd())? {
            return Err(io::Error::other("guard deployment root identity changed"));
        }
        let before=guard_admission_census(self.bpffs.as_fd(),self.recovery.as_fd(),uid)?;
        if before.unresolved.len()>=MAX_UNRESOLVED_GUARD_LAUNCHES {return Err(io::Error::other("Unix guard unresolved launch admission bound reached"));}
        let after=guard_admission_census(self.bpffs.as_fd(),self.recovery.as_fd(),uid)?;
        if before!=after || directory_identity(bpffs.as_fd())?!=directory_identity(self.bpffs.as_fd())?
            || directory_identity(recovery.as_fd())?!=directory_identity(self.recovery.as_fd())? {
            return Err(io::Error::other("guard deployment roots changed during admission census"));
        }
        Ok(())
    }
}

fn directory_names(directory:BorrowedFd<'_>,bound:usize)->io::Result<BTreeSet<String>>{
    let fd=unsafe{libc::openat(directory.as_raw_fd(),c".".as_ptr(),libc::O_RDONLY|libc::O_DIRECTORY|libc::O_CLOEXEC|libc::O_NOFOLLOW)};
    if fd<0{return Err(io::Error::last_os_error());}let raw=unsafe{libc::fdopendir(fd)};
    if raw.is_null(){let error=io::Error::last_os_error();unsafe{libc::close(fd);}return Err(error);}
    struct Stream(*mut libc::DIR);impl Drop for Stream{fn drop(&mut self){unsafe{libc::closedir(self.0);}}}let stream=Stream(raw);let mut names=BTreeSet::new();
    loop{unsafe{*libc::__errno_location()=0;}let entry=unsafe{libc::readdir(stream.0)};if entry.is_null(){let error=io::Error::last_os_error();if error.raw_os_error()!=Some(0){return Err(error);}break;}
        let bytes=unsafe{std::ffi::CStr::from_ptr((*entry).d_name.as_ptr())};if matches!(bytes.to_bytes(),b"."|b".."){continue;}let name=bytes.to_str().map_err(|_|io::Error::other("guard recovery basename is not UTF-8"))?;
        if names.len()>=bound||!names.insert(name.to_owned()){return Err(io::Error::other("guard deployment population exceeded fixed bound"));}}
    Ok(names)
}
fn exact_hex(value:&str,bytes:usize)->bool{value.len()==bytes*2&&!value.bytes().all(|b|b==b'0')&&value.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))}
fn guard_receipt_file(directory:BorrowedFd<'_>,name:&str,uid:u32)->io::Result<File>{
    let name=std::ffi::CString::new(name).map_err(io::Error::other)?;let raw=unsafe{libc::openat(directory.as_raw_fd(),name.as_ptr(),libc::O_RDONLY|libc::O_CLOEXEC|libc::O_NOFOLLOW)};if raw<0{return Err(io::Error::last_os_error());}
    let file=unsafe{File::from_raw_fd(raw)};let metadata=file.metadata()?;if !metadata.is_file()||metadata.uid()!=uid||metadata.mode()&0o7777!=0o600||metadata.nlink()!=1||metadata.len()>65_536{return Err(io::Error::other("guard recovery receipt shape differs"));}Ok(file)
}
fn guard_terminal_complete(directory:BorrowedFd<'_>,name:&str,incarnation:u64,uid:u32)->io::Result<bool>{
    let file=guard_receipt_file(directory,name,uid)?;
    let mut text=String::new();file.take(65_537).read_to_string(&mut text)?;if text.len()>65_536||!text.ends_with('\n'){return Ok(false);}
    let mut terminal=0;let mut last=false;for line in text.lines(){let matches=serde_json::from_str::<serde_json::Value>(line).ok().is_some_and(|row|row.get("schema").and_then(|v|v.as_u64())==Some(1)&&row.get("stage").and_then(|v|v.as_str())==Some("terminal")&&row.pointer("/guard/incarnation").and_then(|v|v.as_u64())==Some(incarnation));if matches{terminal+=1;}last=matches;}Ok(terminal==1&&last)
}
#[derive(Eq,PartialEq)]struct GuardAdmissionCensus{pins:BTreeSet<String>,receipts:BTreeSet<String>,unresolved:BTreeSet<String>}
fn guard_admission_census(bpffs:BorrowedFd<'_>,recovery:BorrowedFd<'_>,uid:u32)->io::Result<GuardAdmissionCensus>{
    let pins=directory_names(bpffs,64)?;let receipt_names=directory_names(recovery,384)?;let mut unresolved=BTreeSet::new();for name in &pins{let suffix=name.strip_prefix("ug-").ok_or_else(||io::Error::other("unexpected Unix guard pin-root entry"))?;if !exact_hex(suffix,8){return Err(io::Error::other("Unix guard pin-root identity differs"));}
        let c=std::ffi::CString::new(name.as_str()).map_err(io::Error::other)?;let mut stat=std::mem::MaybeUninit::<libc::stat>::uninit();if unsafe{libc::fstatat(bpffs.as_raw_fd(),c.as_ptr(),stat.as_mut_ptr(),libc::AT_SYMLINK_NOFOLLOW)}!=0{return Err(io::Error::last_os_error());}let stat=unsafe{stat.assume_init()};
        if stat.st_mode&libc::S_IFMT!=libc::S_IFDIR||stat.st_uid!=uid||stat.st_mode&0o7777!=0o700{return Err(io::Error::other("Unix guard pin-root entry shape differs"));}unresolved.insert(suffix.to_owned());}
    let mut receipts=BTreeMap::<String,BTreeSet<String>>::new();for name in &receipt_names{let(identity,role)=name.split_once('.').ok_or_else(||io::Error::other("Unix guard recovery filename lacks role"))?;
        if !exact_hex(identity,16)||!matches!(role,"terminal.jsonl"|"stdout.log"|"stderr.log"){return Err(io::Error::other("Unix guard recovery identity or role differs"));}if !receipts.entry(identity.to_owned()).or_default().insert(role.to_owned()){return Err(io::Error::other("Unix guard recovery role repeated"));}}
    for(identity,roles)in receipts{let prefix=&identity[..16];let incarnation=u64::from_str_radix(prefix,16).map_err(io::Error::other)?;let terminal=format!("{identity}.terminal.jsonl");
        let complete_roles=roles.len()==3&&roles.contains("terminal.jsonl")&&roles.contains("stdout.log")&&roles.contains("stderr.log");
        let complete=if complete_roles{guard_receipt_file(recovery,&format!("{identity}.stdout.log"),uid)?;guard_receipt_file(recovery,&format!("{identity}.stderr.log"),uid)?;guard_terminal_complete(recovery,&terminal,incarnation,uid)?}else{false};if !complete{unresolved.insert(prefix.to_owned());}}
    Ok(GuardAdmissionCensus{pins,receipts:receipt_names,unresolved})
}

/// Private persistent receipt directory for a service that owns no bpffs pins.
/// The descriptor and canonical path remain live through service finalization.
#[derive(Debug)]
pub struct RecoveryDeploymentRoot {
    /// Authenticated private directory retained for the lifetime of the owner.
    pub directory: OwnedFd,
    /// Canonical directory used for the bounded recovery receipts and logs.
    pub writable_path: PathBuf,
}

impl RecoveryDeploymentRoot {
    /// Open an existing private recovery directory without requiring bpffs.
    /// This uses the same identity, canonical-path and no-follow checks as the
    /// recovery half of `GuardDeploymentRoots`.
    pub fn open_at(path: &Path) -> io::Result<Self> {
        let uid = unsafe { libc::getuid() };
        if uid != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "guard startup requires matching real/effective uid",
            ));
        }
        let directory = open_private_directory(path, uid, false)?;
        Ok(Self {
            directory,
            writable_path: path.to_owned(),
        })
    }

    /// Create only the user's normal state directory and authenticate it.
    pub fn open() -> io::Result<Self> {
        let uid = unsafe { libc::getuid() };
        if uid != unsafe { libc::geteuid() } {
            return Err(io::Error::other(
                "guard startup requires matching real/effective uid",
            ));
        }
        let state = match std::env::var_os("XDG_STATE_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .ok_or_else(|| io::Error::other("no user state directory"))?,
            )
            .join(".local/state"),
        };
        Self::open_named_state_directory(&state, "network-guard")
    }

    /// Create the accepted-provider receipt root separately from the guard's
    /// exact recovery census. This grants no native provider capability.
    pub fn open_accepted() -> io::Result<Self> {
        if unsafe { libc::getuid() } != unsafe { libc::geteuid() } {
            return Err(io::Error::other("guard startup requires matching real/effective uid"));
        }
        let state = match std::env::var_os("XDG_STATE_HOME") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(
                std::env::var_os("HOME")
                    .ok_or_else(|| io::Error::other("no user state directory"))?,
            )
            .join(".local/state"),
        };
        Self::open_named_state_directory(&state, "network-accepted")
    }

    /// Revalidate the held directory against its current canonical name. The
    /// identity is receipt evidence; the retained directory supplies authority.
    pub fn identity(&self) -> io::Result<RecoveryDirectoryIdentity> {
        let held = directory_identity(self.directory.as_fd())?;
        let named = open_private_directory(&self.writable_path, unsafe { libc::getuid() }, false)?;
        if held != directory_identity(named.as_fd())? {
            return Err(io::Error::other(
                "retained recovery root name or identity changed",
            ));
        }
        Ok(held)
    }

    /// Prove neither actual directory is an alias or ancestor of the other.
    /// Walk held '..' descriptors to the real root, with a fixed refusal bound;
    /// pathname prefixes alone cannot supply this proof across mounts/renames.
    pub fn require_disjoint(&self, other: BorrowedFd<'_>, other_path: &Path) -> io::Result<()> {
        let this = self.identity()?;
        let that = directory_identity(other)?;
        let named = open_private_directory(other_path, unsafe { libc::getuid() }, false)?;
        if directory_identity(named.as_fd())? != that {
            return Err(io::Error::other("other deployment root name changed"));
        }
        reject_ancestor(self.directory.as_fd(), that)?;
        reject_ancestor(other, this)?;
        if self.identity()? != this
            || directory_identity(other)? != that
            || directory_identity(
                open_private_directory(other_path, unsafe { libc::getuid() }, false)?.as_fd(),
            )? != that
        {
            return Err(io::Error::other(
                "deployment root changed during ancestry proof",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn open_in_state_directory(state: &Path) -> io::Result<Self> {
        Self::open_named_state_directory(state, "network-guard")
    }
    fn open_named_state_directory(state: &Path, purpose: &str) -> io::Result<Self> {
        if !matches!(purpose, "network-guard" | "network-accepted") {
            return Err(io::Error::other("unknown recovery directory purpose"));
        }
        if !state.is_absolute() {
            return Err(io::Error::other(
                "guard recovery state directory must be absolute",
            ));
        }
        let recovery_path = state.join("hermit").join(purpose);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&recovery_path)?;
        let recovery_path = recovery_path.canonicalize()?;
        Self::open_at(&recovery_path)
    }
}

/// Stable identity of an actual authenticated private recovery directory.
#[derive(
    Clone,
    Copy,
    Debug,
    Eq,
    PartialEq,
    serde::Serialize,
    serde::Deserialize
)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDirectoryIdentity {
    /// Kernel filesystem device identifier.
    pub device: u64,
    /// Kernel inode identifier.
    pub inode: u64,
    /// Actual owning user.
    pub uid: u32,
    /// Complete directory mode (including type).
    pub mode: u32,
}
fn directory_identity(fd: BorrowedFd<'_>) -> io::Result<RecoveryDirectoryIdentity> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR || stat.st_nlink == 0 {
        return Err(io::Error::other(
            "held deployment root is not a linked directory",
        ));
    }
    Ok(RecoveryDirectoryIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        uid: stat.st_uid,
        mode: stat.st_mode,
    })
}
fn reject_ancestor(start: BorrowedFd<'_>, target: RecoveryDirectoryIdentity) -> io::Result<()> {
    let mut current = start.try_clone_to_owned()?;
    for _ in 0..1024 {
        let identity = directory_identity(current.as_fd())?;
        if (identity.device, identity.inode) == (target.device, target.inode) {
            return Err(io::Error::other(
                "accepted and guard deployment roots overlap",
            ));
        }
        let raw = unsafe {
            libc::openat(
                current.as_raw_fd(),
                c"..".as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let parent = unsafe { OwnedFd::from_raw_fd(raw) };
        let above = directory_identity(parent.as_fd())?;
        if (identity.device, identity.inode) == (above.device, above.inode) {
            return Ok(());
        }
        current = parent;
    }
    Err(io::Error::other(
        "deployment root ancestry exceeds fixed bound",
    ))
}

#[cfg(test)]
mod recovery_root_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn private_file(path:&Path,bytes:&[u8]){std::fs::write(path,bytes).unwrap();std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600)).unwrap();}

    #[test]
    fn accepted_recovery_does_not_require_unrelated_bpffs() {
        let state = tempfile::tempdir().unwrap();
        let recovery = RecoveryDeploymentRoot::open_in_state_directory(state.path()).unwrap();
        let missing_bpffs = state.path().join("missing-bpffs");
        assert!(!missing_bpffs.exists());
        assert!(GuardDeploymentRoots::open_at(&missing_bpffs, &recovery.writable_path).is_err());
        let held = File::from(recovery.directory).metadata().unwrap();
        assert!(held.is_dir());
        assert_eq!(held.uid(), unsafe { libc::getuid() });
        assert_eq!(held.mode() & 0o7777, 0o700);
        assert_eq!(recovery.writable_path, state.path().join("hermit/network-guard"));
    }

    #[test]
    fn recovery_only_rejects_invalid_directory_without_changing_permissions() {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("hermit/network-guard");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RecoveryDeploymentRoot::open_in_state_directory(state.path()).is_err());
        assert_eq!(path.metadata().unwrap().mode() & 0o7777, 0o755);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alias = state.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(RecoveryDeploymentRoot::open_at(&alias).is_err());
        assert!(RecoveryDeploymentRoot::open_at(Path::new("relative")).is_err());
        let file = state.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(RecoveryDeploymentRoot::open_at(&file).is_err());
        assert!(RecoveryDeploymentRoot::open_at(&state.path().join("absent")).is_err());
    }    #[test]
    fn accepted_and_guard_roots_are_actual_disjoint_directories() {
        let state = tempfile::tempdir().unwrap();
        let guard =
            RecoveryDeploymentRoot::open_named_state_directory(state.path(), "network-guard")
                .unwrap();
        let accepted =
            RecoveryDeploymentRoot::open_named_state_directory(state.path(), "network-accepted")
                .unwrap();
        assert_ne!(guard.identity().unwrap(), accepted.identity().unwrap());
        accepted
            .require_disjoint(guard.directory.as_fd(), &guard.writable_path)
            .unwrap();
        assert!(
            accepted
                .require_disjoint(accepted.directory.as_fd(), &accepted.writable_path)
                .is_err()
        );
        let nested = accepted.writable_path.join("nested");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&nested)
            .unwrap();
        let child = RecoveryDeploymentRoot::open_at(&nested).unwrap();
        assert!(
            accepted
                .require_disjoint(child.directory.as_fd(), &child.writable_path)
                .is_err()
        );
        assert!(
            child
                .require_disjoint(accepted.directory.as_fd(), &accepted.writable_path)
                .is_err()
        );
    }
    #[test]
    fn retained_recovery_name_replacement_refuses() {
        let state = tempfile::tempdir().unwrap();
        let root =
            RecoveryDeploymentRoot::open_named_state_directory(state.path(), "network-accepted")
                .unwrap();
        let moved = state.path().join("retained-original");
        std::fs::rename(&root.writable_path, &moved).unwrap();
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.writable_path)
            .unwrap();
        assert!(root.identity().is_err());
        assert_eq!(
            File::from(root.directory).metadata().unwrap().ino(),
            moved.metadata().unwrap().ino()
        );
    }
    #[test]
    fn accepted_roots_refuse_symlink_alias_wrong_mode_missing_and_wrong_owner_expectation() {
        let temp = tempfile::tempdir().unwrap();
        let root = RecoveryDeploymentRoot::open_named_state_directory(temp.path(), "network-accepted").unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root.writable_path, &alias).unwrap();
        assert!(RecoveryDeploymentRoot::open_at(&alias).is_err());
        assert!(root.require_disjoint(root.directory.as_fd(), &alias).is_err());
        assert!(RecoveryDeploymentRoot::open_at(&temp.path().join("missing")).is_err());
        let other_uid = unsafe { libc::getuid() }.wrapping_add(1);
        assert!(open_private_directory(&root.writable_path, other_uid, false).is_err());
        std::fs::set_permissions(&root.writable_path, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(root.identity().is_err());
        assert!(RecoveryDeploymentRoot::open_at(&root.writable_path).is_err());
    }
    #[test]
    fn guard_census_bounds_unresolved_pin_and_receipt_identities_without_deletion(){
        let temp=tempfile::tempdir().unwrap();let pins=temp.path().join("pins");let receipts=temp.path().join("receipts");std::fs::create_dir(&pins).unwrap();std::fs::create_dir(&receipts).unwrap();
        std::fs::set_permissions(&pins,std::fs::Permissions::from_mode(0o700)).unwrap();std::fs::set_permissions(&receipts,std::fs::Permissions::from_mode(0o700)).unwrap();
        for ordinal in 1u64..=7{let identity=format!("{ordinal:016x}{ordinal:016x}");private_file(&receipts.join(format!("{identity}.terminal.jsonl")),b"retained failure\n");let pin=pins.join(format!("ug-{ordinal:016x}"));std::fs::create_dir(&pin).unwrap();std::fs::set_permissions(pin,std::fs::Permissions::from_mode(0o700)).unwrap();}
        let completed=17u64;let identity=format!("{completed:016x}{completed:016x}");private_file(&receipts.join(format!("{identity}.terminal.jsonl")),format!("{{\"schema\":1,\"stage\":\"terminal\",\"guard\":{{\"incarnation\":{completed}}}}}\n").as_bytes());private_file(&receipts.join(format!("{identity}.stdout.log")),b"");private_file(&receipts.join(format!("{identity}.stderr.log")),b"");
        let pin_fd=File::open(&pins).unwrap();let receipt_fd=File::open(&receipts).unwrap();let uid=unsafe{libc::getuid()};assert_eq!(guard_admission_census(pin_fd.as_fd(),receipt_fd.as_fd(),uid).unwrap().unresolved.len(),7);
        let eighth=pins.join("ug-0000000000000008");std::fs::create_dir(&eighth).unwrap();std::fs::set_permissions(&eighth,std::fs::Permissions::from_mode(0o700)).unwrap();let before=directory_names(pin_fd.as_fd(),64).unwrap();let count=guard_admission_census(pin_fd.as_fd(),receipt_fd.as_fd(),uid).unwrap().unresolved.len();assert_eq!(count,MAX_UNRESOLVED_GUARD_LAUNCHES);assert_eq!(directory_names(pin_fd.as_fd(),64).unwrap(),before);
    }

}
