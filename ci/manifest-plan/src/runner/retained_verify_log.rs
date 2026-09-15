//! Verify and copy the exact gzip bytes bound to a retained verification log.
//!
//! Recovered from the S7 retention implementation. These readers validate the
//! cell/attempt/path and both compressed and decoded identities through one
//! held descriptor. They do not manufacture a comparison verdict or activate
//! harness-managed execution; the producer and result-schema integration must
//! provide that authority separately.

use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use flate2::read::MultiGzDecoder;
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
    pub peer_uncompressed_sha256: String,
    pub peer_uncompressed_bytes: u64,
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

fn require_path_identity(
    path: &Path,
    expected: FileIdentity,
    description: &str,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot recheck {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{description} {} is no longer a regular non-symlink file",
            path.display()
        ));
    }
    let actual = FileIdentity::from_metadata(&metadata);
    if actual != expected {
        return Err(format!(
            "{description} {} changed identity before publication",
            path.display()
        ));
    }
    Ok(())
}

pub(super) fn require_plain_directory(path: &Path, description: &str) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "{description} {} is not a non-symlink directory",
            path.display()
        ));
    }
    Ok(())
}

fn require_plain_parent_chain(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<(), String> {
    let relative = checked_relative_path(artifact_dir, path, description)?;
    require_plain_directory(artifact_dir, "cell artifact directory")?;
    let mut current = artifact_dir.to_owned();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let Component::Normal(component) = component else {
                unreachable!("checked_relative_path accepted only normal components")
            };
            current.push(component);
            require_plain_directory(&current, description)?;
        }
    }
    Ok(())
}

fn open_plain_file_below(
    artifact_dir: &Path,
    path: &Path,
    description: &str,
) -> Result<File, String> {
    require_plain_parent_chain(artifact_dir, path, description)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open {description} {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect {description} {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "{description} {} is not a regular file",
            path.display()
        ));
    }
    Ok(file)
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
    let mut file = open_plain_file_below(root, path, description)?;
    let inspection = inspect_open_gzip_file(
        &mut file,
        path,
        maximum_compressed_bytes,
        maximum_uncompressed_bytes,
        description,
    )?;
    Ok(OpenedGzipEvidence { file, inspection })
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
    let uncompressed = {
        let mut decoder = MultiGzDecoder::new(file);
        copy_and_hash_bounded(
            &mut decoder,
            &mut std::io::sink(),
            maximum_uncompressed_bytes,
            description,
        )?
    };
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
    require_path_identity(
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
/// Descriptor validation, gzip decoding, and path-identity checks all use one
/// held `O_NOFOLLOW` file descriptor. This is the scoring/read path; callers
/// retaining the gzip itself should use [`copy_verified_retained_verify_log`].
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
    let digest = {
        let mut decoder = MultiGzDecoder::new(&mut opened.file);
        copy_and_hash_bounded(
            &mut decoder,
            &mut bytes,
            VERIFY_LOG_MAX_UNCOMPRESSED_BYTES,
            "retained uncompressed verify log",
        )?
    };
    if digest != opened.inspection.uncompressed {
        return Err("retained verify log changed while reading its uncompressed bytes".into());
    }
    let path = artifact_dir.join(&retained.relative_path);
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    require_path_identity(
        &path,
        opened.inspection.identity,
        "retained compressed verify log",
    )?;
    Ok(bytes)
}

/// Copy the exact gzip bytes named by a retained-log descriptor from one held
/// source descriptor into a caller-owned destination.
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
    let path = artifact_dir.join(&retained.relative_path);
    require_single_link(&opened.file, &path, "retained compressed verify log")?;
    require_path_identity(
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
