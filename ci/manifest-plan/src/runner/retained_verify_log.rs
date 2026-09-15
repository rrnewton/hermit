//! Verify and copy the exact gzip bytes bound to a retained verification log.
//!
//! Recovered from the S7 retention implementation. These readers validate the
//! cell/attempt/path and both compressed and decoded identities through one
//! held descriptor. They do not manufacture a comparison verdict or activate
//! harness-managed execution; the producer and result-schema integration must
//! provide that authority separately.

use std::ffi::CString;
use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use flate2::bufread::GzDecoder;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

use super::CellId;

// Preserve the S7 per-log bounds and the current ordinary-run evidence limit.
const VERIFY_LOG_MAX_UNCOMPRESSED_BYTES: u64 = 1024 * 1024 * 1024;
const VERIFY_LOG_MAX_COMPRESSED_BYTES: u64 = VERIFY_LOG_MAX_UNCOMPRESSED_BYTES + 16 * 1024 * 1024;

/// The single member of a verify pair retained after comparison.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum RetainedVerifyLogRole {
    #[serde(rename = "run-1")]
    Run1,
}

/// Durable evidence for the one compressed verify log retained by an attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedVerifyLog {
    /// Path relative to the cell's `artifact_dir`.
    pub relative_path: String,
    pub role: RetainedVerifyLogRole,
    pub cell_id: CellId,
    pub attempt: u64,
    pub uncompressed_sha256: String,
    pub uncompressed_bytes: u64,
    pub compressed_sha256: String,
    pub compressed_bytes: u64,
    /// Producer claim about the discarded peer; the retained-file readers do
    /// not authenticate it. Bind it to the authoritative comparison separately.
    pub peer_uncompressed_sha256: String,
    /// Producer claim about the discarded peer, unchecked by these readers.
    pub peer_uncompressed_bytes: u64,
    /// Producer claim about the comparison, unchecked by these readers.
    pub compared_info_messages: u64,
}

/// Digests and sizes checked while copying one retained gzip from a single
/// held source descriptor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedRetainedLogCopy {
    pub compressed_sha256: String,
    pub compressed_bytes: u64,
    pub uncompressed_sha256: String,
    pub uncompressed_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ContentDigest {
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InspectedGzip {
    identity: FileIdentity,
    compressed: ContentDigest,
    uncompressed: ContentDigest,
}

struct OpenedGzipEvidence {
    file: File,
    inspection: InspectedGzip,
    parents: HeldParentChain,
}

struct HeldDirectory {
    file: File,
    identity: FileIdentity,
    path: PathBuf,
}

/// Hold every directory from the caller's artifact root through the leaf's
/// parent. Opening and checking each child relative to its held parent avoids
/// following a substituted ancestor even when the leaf inode stays unchanged.
struct HeldParentChain {
    directories: Vec<HeldDirectory>,
}

fn checked_relative_path<'a>(
    artifact_dir: &'a Path,
    path: &'a Path,
    description: &str,
) -> Result<&'a Path, String> {
    let relative = path.strip_prefix(artifact_dir).map_err(|_| {
        format!(
            "{description} {} is outside cell artifact directory {}",
            path.display(),
            artifact_dir.display()
        )
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!(
            "{description} {} is not a normal relative path below {}",
            path.display(),
            artifact_dir.display()
        ));
    }
    Ok(relative)
}

fn open_at(
    parent_fd: RawFd,
    name: &OsStr,
    flags: libc::c_int,
    display_path: &Path,
    description: &str,
) -> Result<File, String> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| format!("{description} {} contains NUL", display_path.display()))?;
    // SAFETY: name is terminated and flags never include O_CREAT/O_TMPFILE, so
    // openat takes no mode argument. Callers retain the parent descriptor.
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(format!(
            "cannot open {description} {}: {}",
            display_path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: a successful openat returned a new descriptor owned by this File.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn require_entry_identity(
    parent_fd: RawFd,
    name: &OsStr,
    expected: FileIdentity,
    expected_type: libc::mode_t,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| format!("{description} {} contains NUL", path.display()))?;
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: name is terminated, metadata points to writable storage, and the
    // parent descriptor remains held. Do not follow even the last component.
    if unsafe {
        libc::fstatat(
            parent_fd,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(format!(
            "cannot recheck {description} {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: successful fstatat initialized metadata.
    let metadata = unsafe { metadata.assume_init() };
    let actual = FileIdentity {
        device: metadata.st_dev,
        inode: metadata.st_ino,
    };
    if metadata.st_mode & libc::S_IFMT != expected_type || actual != expected {
        return Err(format!(
            "{description} {} changed identity or type before publication",
            path.display()
        ));
    }
    Ok(())
}

impl HeldParentChain {
    fn open(artifact_dir: &Path, path: &Path, description: &str) -> Result<Self, String> {
        let relative = checked_relative_path(artifact_dir, path, description)?;
        let mut directories = Vec::<HeldDirectory>::new();
        let mut current = artifact_dir.to_owned();
        // The artifact directory is the caller-supplied boundary. All names
        // below it are single normal components checked above.
        let names = std::iter::once(artifact_dir.as_os_str()).chain(
            relative
                .parent()
                .expect("checked relative path has a parent")
                .components()
                .map(|component| component.as_os_str()),
        );
        for name in names {
            let parent_fd = match directories.last() {
                Some(parent) => {
                    current.push(name);
                    parent.file.as_raw_fd()
                }
                None => libc::AT_FDCWD,
            };
            // O_PATH needs search permission, preserving support for directory
            // trees that can be traversed but cannot be listed.
            let file = open_at(
                parent_fd,
                name,
                libc::O_PATH | libc::O_DIRECTORY,
                &current,
                "non-symlink directory",
            )?;
            let identity = FileIdentity::from_metadata(&file.metadata().map_err(|error| {
                format!("cannot inspect directory {}: {error}", current.display())
            })?);
            directories.push(HeldDirectory {
                file,
                identity,
                path: current.clone(),
            });
        }
        Ok(Self { directories })
    }

    fn leaf_parent_fd(&self) -> RawFd {
        self.directories
            .last()
            .expect("the artifact directory is always held")
            .file
            .as_raw_fd()
    }

    fn require_path_identity(
        &self,
        path: &Path,
        expected: FileIdentity,
        description: &str,
    ) -> Result<(), String> {
        for (index, directory) in self.directories.iter().enumerate() {
            let (parent_fd, name) = if index == 0 {
                (libc::AT_FDCWD, directory.path.as_os_str())
            } else {
                (
                    self.directories[index - 1].file.as_raw_fd(),
                    directory.path.file_name().expect("normal directory name"),
                )
            };
            require_entry_identity(
                parent_fd,
                name,
                directory.identity,
                libc::S_IFDIR,
                &directory.path,
                "retained verify-log directory",
            )?;
        }
        require_entry_identity(
            self.leaf_parent_fd(),
            path.file_name().expect("normal retained file name"),
            expected,
            libc::S_IFREG,
            path,
            description,
        )
    }
}

fn open_plain_file_below(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<(File, HeldParentChain), String> {
    let parents = HeldParentChain::open(artifact_dir, path, description)?;
    let file = open_at(
        parents.leaf_parent_fd(),
        path.file_name().expect("checked relative file name"),
        libc::O_RDONLY | libc::O_NONBLOCK,
        path,
        description,
    )?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} {} is not a regular file",
            path.display()
        ));
    }
    Ok((file, parents))
}

fn copy_and_hash_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    maximum_bytes: u64,
    description: &str,
) -> Result<ContentDigest, String> {
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("cannot read {description}: {error}"))?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(read).expect("read buffer length fits u64"))
            .ok_or_else(|| format!("{description} size overflowed u64"))?;
        if bytes > maximum_bytes {
            return Err(format!(
                "{description} exceeds the {maximum_bytes}-byte limit"
            ));
        }
        digest.update(&buffer[..read]);
        writer
            .write_all(&buffer[..read])
            .map_err(|error| format!("cannot write {description}: {error}"))?;
    }
    Ok(ContentDigest {
        sha256: format!("{:x}", digest.finalize()),
        bytes,
    })
}

fn decode_single_gzip_bounded(
    file: &mut File,
    destination: &mut impl Write,
    maximum_bytes: u64,
    description: &str,
) -> Result<ContentDigest, String> {
    // The bufread decoder leaves bytes after the first member in this buffer.
    // Requiring EOF therefore rejects both concatenation and trailing garbage,
    // rather than validating only one header in a multi-member stream.
    let mut input = BufReader::new(file);
    let digest = {
        let mut decoder = GzDecoder::new(&mut input);
        copy_and_hash_bounded(&mut decoder, destination, maximum_bytes, description)?
    };
    if !input
        .fill_buf()
        .map_err(|error| format!("cannot finish reading {description}: {error}"))?
        .is_empty()
    {
        return Err(format!(
            "{description} must contain exactly one gzip member with no trailing bytes"
        ));
    }
    Ok(digest)
}

fn require_single_link(file: &File, path: &Path, description: &str) -> Result<(), String> {
    let links = file
        .metadata()
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?
        .nlink();
    if links != 1 {
        return Err(format!(
            "{description} {} has {links} hard links; expected exactly one",
            path.display()
        ));
    }
    Ok(())
}

fn open_and_inspect_gzip_file(
    root: &Path,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<OpenedGzipEvidence, String> {
    let (mut file, parents) = open_plain_file_below(root, path, description)?;
    let inspection = inspect_open_gzip_file(
        &mut file,
        path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        description,
    )?;
    Ok(OpenedGzipEvidence {
        file,
        inspection,
        parents,
    })
}

fn inspect_open_gzip_file(
    file: &mut File,
    path: &Path,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
    description: &str,
) -> Result<InspectedGzip, String> {
    let identity =
        FileIdentity::from_metadata(&file.metadata().map_err(|error| {
            format!("cannot inspect {description} {}: {error}", path.display())
        })?);
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let compressed = copy_and_hash_bounded(
        file,
        &mut std::io::sink(),
        maximum_compressed_bytes,
        description,
    )?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let mut header = [0u8; 10];
    file.read_exact(&mut header).map_err(|error| {
        format!(
            "cannot read deterministic gzip header from {description} {}: {error}",
            path.display()
        )
    })?;
    if header[..3] != [0x1f, 0x8b, 8] || header[3] != 0 || header[4..8] != [0, 0, 0, 0] {
        return Err(format!(
            "{description} {} does not have the canonical deterministic gzip header",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek {description} {}: {error}", path.display()))?;
    let uncompressed = decode_single_gzip_bounded(
        file,
        &mut std::io::sink(),
        maximum_uncompressed_bytes,
        description,
    )?;
    Ok(InspectedGzip {
        identity,
        compressed,
        uncompressed,
    })
}

pub(super) fn retained_verify_log_relative_path(attempt: u64) -> Result<PathBuf, String> {
    if attempt == 0 {
        return Err("retained verify-log attempt must be positive".into());
    }
    Ok(PathBuf::from("retained")
        .join("verify")
        .join(attempt.to_string())
        .join("run-1.log.gz"))
}

fn open_verified_retained_verify_log_with_limits(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    maximum_compressed_bytes: u64,
    maximum_uncompressed_bytes: u64,
) -> Result<OpenedGzipEvidence, String> {
    if retained.role != RetainedVerifyLogRole::Run1 {
        return Err("retained verify log has an unsupported role".into());
    }
    if &retained.cell_id != expected_cell_id || retained.attempt != expected_attempt {
        return Err("retained verify log does not match its cell id and attempt".into());
    }
    let expected_relative = retained_verify_log_relative_path(expected_attempt)?;
    if Path::new(&retained.relative_path) != expected_relative {
        return Err(format!(
            "retained verify log path must be exactly {}, got {}",
            expected_relative.display(),
            retained.relative_path
        ));
    }
    let path = artifact_dir.join(&expected_relative);
    let opened = open_and_inspect_gzip_file(
        artifact_dir,
        &path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        "retained compressed verify log",
    )?;
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    validate_retained_verify_log_inspection(retained, &opened.inspection)?;
    opened.parents.require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(opened)
}

fn verify_retained_verify_log_with_limit(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    maximum_uncompressed_bytes: u64,
) -> Result<(), String> {
    open_verified_retained_verify_log_with_limits(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        maximum_uncompressed_bytes,
    )?;
    Ok(())
}

fn validate_retained_verify_log_inspection(
    retained: &RetainedVerifyLog,
    inspected: &InspectedGzip,
) -> Result<(), String> {
    if inspected.compressed.sha256 != retained.compressed_sha256
        || inspected.compressed.bytes != retained.compressed_bytes
    {
        return Err(format!(
            "retained compressed verify log digest/size mismatch: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            retained.compressed_bytes,
            retained.compressed_sha256,
            inspected.compressed.bytes,
            inspected.compressed.sha256
        ));
    }
    if inspected.uncompressed.sha256 != retained.uncompressed_sha256
        || inspected.uncompressed.bytes != retained.uncompressed_bytes
    {
        return Err(format!(
            "retained uncompressed verify log digest/size mismatch: expected {} bytes sha256 {}, got {} bytes sha256 {}",
            retained.uncompressed_bytes,
            retained.uncompressed_sha256,
            inspected.uncompressed.bytes,
            inspected.uncompressed.sha256
        ));
    }
    Ok(())
}

/// Re-read and authenticate one retained verify log against its typed binding.
pub fn verify_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
) -> Result<(), String> {
    verify_retained_verify_log_with_limit(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )
}

/// Read the exact uncompressed bytes named by a retained-log descriptor.
///
/// Descriptor validation and gzip decoding use one held `O_NOFOLLOW` file
/// descriptor. Held directory descriptors authenticate the path from the
/// artifact root through the leaf before success. This is the scoring/read
/// path; callers retaining the gzip itself should use
/// [`copy_verified_retained_verify_log`].
pub fn read_verified_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
) -> Result<Vec<u8>, String> {
    let mut opened = open_verified_retained_verify_log_with_limits(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        VERIFY_LOG_MAX_COMPRESSED_BYTES,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )?;
    opened
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek retained compressed verify log: {error}"))?;
    let mut bytes = Vec::new();
    let digest = decode_single_gzip_bounded(
        &mut opened.file,
        &mut bytes,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
        "retained uncompressed verify log",
    )?;
    if digest != opened.inspection.uncompressed {
        return Err("retained verify log changed while reading its uncompressed bytes".into());
    }
    let path = artifact_dir.join(retained_verify_log_relative_path(expected_attempt)?);
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    opened.parents.require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(bytes)
}

/// Copy the exact gzip bytes named by a retained-log descriptor from one held
/// source descriptor into a caller-owned destination.
///
/// On error, the destination may already contain partial or complete gzip
/// bytes. Publish it only after this function succeeds; the caller owns
/// destination rollback and durable publication.
pub fn copy_verified_retained_verify_log(
    artifact_dir: &Path,
    retained: &RetainedVerifyLog,
    expected_cell_id: &CellId,
    expected_attempt: u64,
    destination: &mut impl Write,
    maximum_compressed_bytes: u64,
) -> Result<VerifiedRetainedLogCopy, String> {
    let maximum_compressed_bytes = maximum_compressed_bytes.min(VERIFY_LOG_MAX_COMPRESSED_BYTES);
    let mut opened = open_verified_retained_verify_log_with_limits(
        artifact_dir,
        retained,
        expected_cell_id,
        expected_attempt,
        maximum_compressed_bytes,
        VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
    )?;
    opened
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot seek retained compressed verify log: {error}"))?;
    let copied = copy_and_hash_bounded(
        &mut opened.file,
        destination,
        maximum_compressed_bytes,
        "retained compressed verify log copy",
    )?;
    if copied != opened.inspection.compressed {
        return Err("retained verify log changed while copying its compressed bytes".into());
    }
    let path = artifact_dir.join(retained_verify_log_relative_path(expected_attempt)?);
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    opened.parents.require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(VerifiedRetainedLogCopy {
        compressed_sha256: copied.sha256,
        compressed_bytes: copied.bytes,
        uncompressed_sha256: opened.inspection.uncompressed.sha256,
        uncompressed_bytes: opened.inspection.uncompressed.bytes,
    })
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::unix::fs::symlink;

    use flate2::Compression;
    use flate2::GzBuilder;
    use tempfile::TempDir;

    use super::*;

    const LOG: &[u8] = b"INFO DETLOG syscall read fd=3 bytes=4\nINFO DETLOG exit code=0\n";

    struct Fixture {
        directory: TempDir,
        descriptor: RetainedVerifyLog,
        compressed: Vec<u8>,
    }

    impl Fixture {
        fn new() -> Self {
            // Construct the fixture independently of the readers under test.
            let directory = tempfile::tempdir().unwrap();
            let mut encoder = GzBuilder::new()
                .mtime(0)
                .write(Vec::new(), Compression::default());
            encoder.write_all(LOG).unwrap();
            let compressed = encoder.finish().unwrap();
            let descriptor = RetainedVerifyLog {
                relative_path: "retained/verify/2/run-1.log.gz".into(),
                role: RetainedVerifyLogRole::Run1,
                cell_id: CellId {
                    test: "reader-fixture".into(),
                    mode: "verify".into(),
                    backend: Some("ptrace".into()),
                },
                attempt: 2,
                uncompressed_sha256: format!("{:x}", Sha256::digest(LOG)),
                uncompressed_bytes: LOG.len() as u64,
                compressed_sha256: format!("{:x}", Sha256::digest(&compressed)),
                compressed_bytes: compressed.len() as u64,
                peer_uncompressed_sha256: format!("{:x}", Sha256::digest(LOG)),
                peer_uncompressed_bytes: LOG.len() as u64,
                compared_info_messages: 2,
            };
            let path = directory.path().join(&descriptor.relative_path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, &compressed).unwrap();
            Self {
                directory,
                descriptor,
                compressed,
            }
        }

        fn path(&self) -> PathBuf {
            self.directory.path().join(&self.descriptor.relative_path)
        }

        fn verify(&self) -> Result<(), String> {
            verify_retained_verify_log(
                self.directory.path(),
                &self.descriptor,
                &self.descriptor.cell_id,
                2,
            )
        }
    }

    #[test]
    fn descriptor_bound_read_and_copy_return_exact_bytes() {
        let fixture = Fixture::new();
        fixture.verify().unwrap();
        assert_eq!(
            read_verified_retained_verify_log(
                fixture.directory.path(),
                &fixture.descriptor,
                &fixture.descriptor.cell_id,
                2,
            )
            .unwrap(),
            LOG,
        );
        let mut copied = Vec::new();
        let identity = copy_verified_retained_verify_log(
            fixture.directory.path(),
            &fixture.descriptor,
            &fixture.descriptor.cell_id,
            2,
            &mut copied,
            fixture.descriptor.compressed_bytes,
        )
        .unwrap();
        assert_eq!(copied, fixture.compressed);
        assert_eq!(identity.compressed_bytes, copied.len() as u64);
        assert_eq!(
            identity.compressed_sha256,
            fixture.descriptor.compressed_sha256
        );
        assert_eq!(identity.uncompressed_bytes, LOG.len() as u64);
        assert_eq!(
            identity.uncompressed_sha256,
            fixture.descriptor.uncompressed_sha256
        );
    }

    #[test]
    fn refuses_wrong_cell_attempt_and_retained_path() {
        let fixture = Fixture::new();
        let mut wrong_cell = fixture.descriptor.cell_id.clone();
        wrong_cell.backend = Some("kvm".into());
        for (cell, attempt) in [(&wrong_cell, 2), (&fixture.descriptor.cell_id, 1)] {
            let error = verify_retained_verify_log(
                fixture.directory.path(),
                &fixture.descriptor,
                cell,
                attempt,
            )
            .unwrap_err();
            assert!(error.contains("cell id and attempt"), "{error}");
        }
        for path in [
            "../run-1.log.gz",
            "/run-1.log.gz",
            "retained/verify/2/run-2.log.gz",
        ] {
            let mut descriptor = fixture.descriptor.clone();
            descriptor.relative_path = path.into();
            let error = verify_retained_verify_log(
                fixture.directory.path(),
                &descriptor,
                &descriptor.cell_id,
                2,
            )
            .unwrap_err();
            assert!(error.contains("path must be exactly"), "{error}");
        }
        let mut descriptor = fixture.descriptor.clone();
        descriptor.attempt = 0;
        let error = verify_retained_verify_log(
            fixture.directory.path(),
            &descriptor,
            &descriptor.cell_id,
            0,
        )
        .unwrap_err();
        assert!(error.contains("attempt must be positive"), "{error}");
    }

    #[test]
    fn refuses_each_incorrect_digest_and_length() {
        let fixture = Fixture::new();
        for field in 0..4 {
            let mut descriptor = fixture.descriptor.clone();
            match field {
                0 => descriptor.compressed_sha256 = "0".repeat(64),
                1 => descriptor.compressed_bytes += 1,
                2 => descriptor.uncompressed_sha256 = "0".repeat(64),
                3 => descriptor.uncompressed_bytes += 1,
                _ => unreachable!(),
            }
            let error = verify_retained_verify_log(
                fixture.directory.path(),
                &descriptor,
                &descriptor.cell_id,
                2,
            )
            .unwrap_err();
            assert!(
                error.contains("digest/size mismatch"),
                "field={field}: {error}"
            );
        }
    }

    #[test]
    fn refuses_crc_corruption_truncation_and_nondeterministic_header() {
        for mutation in 0..3 {
            let mut fixture = Fixture::new();
            let mut bytes = fixture.compressed.clone();
            match mutation {
                0 => {
                    let crc_offset = bytes.len() - 8;
                    bytes[crc_offset] ^= 1;
                }
                1 => bytes.truncate(bytes.len() - 4),
                2 => bytes[4] = 1,
                _ => unreachable!(),
            }
            // Even a descriptor naming the mutated gzip cannot excuse invalid
            // framing, a broken CRC, or a nonzero wall-clock timestamp.
            fixture.descriptor.compressed_sha256 = format!("{:x}", Sha256::digest(&bytes));
            fixture.descriptor.compressed_bytes = bytes.len() as u64;
            fs::write(fixture.path(), bytes).unwrap();
            let error = fixture.verify().unwrap_err();
            if mutation == 2 {
                assert!(
                    error.contains("canonical deterministic gzip header"),
                    "{error}"
                );
            } else {
                assert!(error.contains("cannot read"), "{error}");
            }
        }
    }

    #[test]
    fn refuses_concatenated_members_even_when_all_recorded_digests_match() {
        for timestamp in [0, 1] {
            let mut fixture = Fixture::new();
            let mut encoder = GzBuilder::new()
                .mtime(timestamp)
                .write(Vec::new(), Compression::default());
            encoder.write_all(LOG).unwrap();
            fixture.compressed.extend(encoder.finish().unwrap());
            let decoded = [LOG, LOG].concat();
            fixture.descriptor.compressed_sha256 =
                format!("{:x}", Sha256::digest(&fixture.compressed));
            fixture.descriptor.compressed_bytes = fixture.compressed.len() as u64;
            fixture.descriptor.uncompressed_sha256 = format!("{:x}", Sha256::digest(&decoded));
            fixture.descriptor.uncompressed_bytes = decoded.len() as u64;
            fs::write(fixture.path(), &fixture.compressed).unwrap();
            let error = fixture.verify().unwrap_err();
            assert!(
                error.contains("exactly one gzip member"),
                "timestamp={timestamp}: {error}"
            );
        }
    }

    #[test]
    fn refuses_trailing_bytes_even_when_the_compressed_digest_matches() {
        let mut fixture = Fixture::new();
        fixture.compressed.push(b'\n');
        fixture.descriptor.compressed_sha256 = format!("{:x}", Sha256::digest(&fixture.compressed));
        fixture.descriptor.compressed_bytes = fixture.compressed.len() as u64;
        fs::write(fixture.path(), &fixture.compressed).unwrap();
        let error = fixture.verify().unwrap_err();
        assert!(error.contains("no trailing bytes"), "{error}");
    }

    #[test]
    fn refuses_compressed_and_decoded_size_caps() {
        let fixture = Fixture::new();
        let mut copied = Vec::new();
        let error = copy_verified_retained_verify_log(
            fixture.directory.path(),
            &fixture.descriptor,
            &fixture.descriptor.cell_id,
            2,
            &mut copied,
            fixture.descriptor.compressed_bytes - 1,
        )
        .unwrap_err();
        assert!(error.contains("byte limit"), "{error}");
        assert!(
            copied.is_empty(),
            "a refused source must not reach the destination"
        );
        let error = verify_retained_verify_log_with_limit(
            fixture.directory.path(),
            &fixture.descriptor,
            &fixture.descriptor.cell_id,
            2,
            LOG.len() as u64 - 1,
        )
        .unwrap_err();
        assert!(error.contains("byte limit"), "{error}");
    }

    #[test]
    fn refuses_symlinked_ancestors_leaf_and_hardlink_alias() {
        for mutation in 0..3 {
            let fixture = Fixture::new();
            let source = fixture.path();
            let alias = fixture.directory.path().join("alias");
            match mutation {
                0 => {
                    fs::rename(&source, &alias).unwrap();
                    symlink(&alias, &source).unwrap();
                }
                1 => {
                    let ancestor = fixture.directory.path().join("retained");
                    fs::rename(&ancestor, &alias).unwrap();
                    symlink(&alias, &ancestor).unwrap();
                }
                2 => fs::hard_link(&source, &alias).unwrap(),
                _ => unreachable!(),
            }
            let error = fixture.verify().unwrap_err();
            match mutation {
                0 => assert!(error.contains("cannot open"), "{error}"),
                1 => assert!(error.contains("non-symlink directory"), "{error}"),
                2 => assert!(error.contains("hard links"), "{error}"),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn copying_from_held_descriptor_refuses_path_replacement() {
        struct ReplacingDestination {
            source: PathBuf,
            saved: PathBuf,
            copied: Vec<u8>,
            replaced: bool,
        }
        impl Write for ReplacingDestination {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.replaced {
                    fs::rename(&self.source, &self.saved)?;
                    fs::write(&self.source, b"unrelated replacement")?;
                    self.replaced = true;
                }
                self.copied.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let mut destination = ReplacingDestination {
            source: fixture.path(),
            saved: fixture.directory.path().join("original.gz"),
            copied: Vec::new(),
            replaced: false,
        };
        let error = copy_verified_retained_verify_log(
            fixture.directory.path(),
            &fixture.descriptor,
            &fixture.descriptor.cell_id,
            2,
            &mut destination,
            fixture.descriptor.compressed_bytes,
        )
        .unwrap_err();
        assert!(error.contains("changed identity"), "{error}");
        // The held descriptor returns the authenticated bytes, but the changed
        // path still prevents the caller from treating this copy as published.
        assert_eq!(destination.copied, fixture.compressed);
    }

    #[test]
    fn copying_from_held_descriptor_refuses_ancestor_substitution() {
        struct ReplacingAncestor {
            ancestor: PathBuf,
            alias: PathBuf,
            copied: Vec<u8>,
            replaced: bool,
        }
        impl Write for ReplacingAncestor {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.replaced {
                    fs::rename(&self.ancestor, &self.alias)?;
                    symlink(&self.alias, &self.ancestor)?;
                    self.replaced = true;
                }
                self.copied.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let identity = FileIdentity::from_metadata(&fs::metadata(fixture.path()).unwrap());
        let mut destination = ReplacingAncestor {
            ancestor: fixture.directory.path().join("retained"),
            alias: fixture.directory.path().join("renamed-retained"),
            copied: Vec::new(),
            replaced: false,
        };
        let copied = copy_verified_retained_verify_log(
            fixture.directory.path(),
            &fixture.descriptor,
            &fixture.descriptor.cell_id,
            2,
            &mut destination,
            fixture.descriptor.compressed_bytes,
        );
        assert!(destination.replaced);
        assert_eq!(destination.copied, fixture.compressed);
        let after = fs::metadata(fixture.path()).unwrap();
        assert_eq!(FileIdentity::from_metadata(&after), identity);
        assert_eq!(
            after.nlink(),
            1,
            "the leaf identity and link count cannot catch this substitution"
        );
        let error =
            copied.expect_err("a substituted symlink ancestor must prevent successful copying");
        assert!(
            error.contains("directory") || error.contains("ancestor"),
            "{error}"
        );
    }

    #[test]
    fn copying_refuses_plain_directory_replacement_even_with_the_same_leaf() {
        struct ReplacingDirectory {
            directory: PathBuf,
            saved: PathBuf,
            copied: Vec<u8>,
            replaced: bool,
        }
        impl Write for ReplacingDirectory {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.replaced {
                    fs::rename(&self.directory, &self.saved)?;
                    fs::create_dir(&self.directory)?;
                    fs::rename(self.saved.join("verify"), self.directory.join("verify"))?;
                    self.replaced = true;
                }
                self.copied.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let fixture = Fixture::new();
        let identity = FileIdentity::from_metadata(&fs::metadata(fixture.path()).unwrap());
        let mut destination = ReplacingDirectory {
            directory: fixture.directory.path().join("retained"),
            saved: fixture.directory.path().join("renamed-retained"),
            copied: Vec::new(),
            replaced: false,
        };
        let copied = copy_verified_retained_verify_log(
            fixture.directory.path(),
            &fixture.descriptor,
            &fixture.descriptor.cell_id,
            2,
            &mut destination,
            fixture.descriptor.compressed_bytes,
        );
        assert!(destination.replaced);
        assert_eq!(destination.copied, fixture.compressed);
        let after = fs::metadata(fixture.path()).unwrap();
        assert_eq!(FileIdentity::from_metadata(&after), identity);
        assert_eq!(after.nlink(), 1);
        assert!(
            fs::symlink_metadata(&destination.directory)
                .unwrap()
                .is_dir()
        );
        let error = copied.expect_err("a different plain ancestor directory must also be refused");
        assert!(
            error.contains("directory") && error.contains("changed identity"),
            "{error}"
        );
    }

    #[test]
    fn descriptor_wire_format_refuses_unknown_fields_and_peer_role() {
        let fixture = Fixture::new();
        let wire = serde_json::to_value(&fixture.descriptor).unwrap();
        assert_eq!(
            serde_json::from_value::<RetainedVerifyLog>(wire.clone()).unwrap(),
            fixture.descriptor,
        );
        for mutation in 0..3 {
            let mut malformed = wire.clone();
            match mutation {
                0 => malformed["authority"] = "asserted".into(),
                1 => malformed["cell_id"]["authority"] = "asserted".into(),
                2 => malformed["role"] = "run-2".into(),
                _ => unreachable!(),
            }
            assert!(serde_json::from_value::<RetainedVerifyLog>(malformed).is_err());
        }
    }
}
