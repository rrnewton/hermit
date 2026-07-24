/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::ffi::OsStr;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use digest::Digest;
use serde::Deserialize;
use serde::Serialize;

use crate::chroot::TempChroot;
use crate::consts::FILES_DIR_NAME;
use crate::consts::FILES_MANIFEST_NAME;
use crate::consts::REPLAY_FILES_ROOT;

/// Metadata for one regular file captured in a record trace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct RecordedFile {
    pub path: String,
    pub mtime_secs: i64,
    pub mtime_nanos: i64,
    pub size: u64,
    pub mode: u32,
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
}

fn serde_io(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn snapshot_path(data: &Path, sha256: &str) -> PathBuf {
    data.join(FILES_DIR_NAME).join(sha256)
}

fn is_sha256_name(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_safe_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn append_manifest(data: &Path, recorded: RecordedFile) -> io::Result<()> {
    let manifest_path = data.join(FILES_MANIFEST_NAME);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&manifest_path)?;

    // SAFETY: file is a valid descriptor and the lock is released when it is
    // closed, including all error paths below.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == -1 {
        return Err(io::Error::last_os_error());
    }

    let mut contents = Vec::new();
    file.read_to_end(&mut contents)?;
    let mut entries: Vec<RecordedFile> = if contents.is_empty() {
        Vec::new()
    } else {
        serde_json::from_slice(&contents).map_err(serde_io)?
    };
    if !entries.iter().any(|entry| entry == &recorded) {
        entries.push(recorded);
        entries
            .sort_by(|left, right| (&left.path, &left.sha256).cmp(&(&right.path, &right.sha256)));
    }

    file.set_len(0)?;
    file.rewind()?;
    serde_json::to_writer_pretty(&mut file, &entries).map_err(serde_io)?;
    file.write_all(b"\n")?;
    file.sync_data()
}

fn capture_open_file(
    data: &Path,
    mut source: File,
    recorded_path: &Path,
) -> io::Result<RecordedFile> {
    let metadata = source.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", recorded_path.display()),
        ));
    }

    let files = data.join(FILES_DIR_NAME);
    fs::create_dir_all(&files)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&files)?;
    let copied = io::copy(&mut source, &mut temporary)?;
    if copied != metadata.len() {
        return Err(io::Error::other(format!(
            "{} changed size while it was being captured (expected {}, copied {copied})",
            recorded_path.display(),
            metadata.len(),
        )));
    }
    temporary.as_file().sync_data()?;

    let sha256 = Digest::digest_path(temporary.path())?.to_string();
    temporary
        .as_file()
        .set_permissions(metadata.permissions())?;
    let destination = snapshot_path(data, &sha256);
    match temporary.persist_noclobber(&destination) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            drop(error.file);
        }
        Err(error) => return Err(error.error),
    }

    let recorded = RecordedFile {
        path: recorded_path.to_string_lossy().into_owned(),
        mtime_secs: metadata.mtime(),
        mtime_nanos: metadata.mtime_nsec(),
        size: metadata.len(),
        mode: metadata.mode(),
        sha256,
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    append_manifest(data, recorded.clone())?;
    Ok(recorded)
}

/// Captures the regular file currently referenced by a guest descriptor.
pub(crate) fn capture_fd(
    data: &Path,
    pid: libc::pid_t,
    fd: libc::c_int,
) -> io::Result<RecordedFile> {
    let proc_path = PathBuf::from(format!("/proc/{pid}/fd/{fd}"));
    let recorded_path = fs::read_link(&proc_path)?;
    capture_open_file(data, File::open(&proc_path)?, &recorded_path)
}

/// Captures an absolute executable path that the kernel will read during exec.
pub(crate) fn capture_path(data: &Path, path: &Path) -> io::Result<RecordedFile> {
    capture_open_file(data, File::open(path)?, path)
}

/// Returns whether a guest descriptor refers to an inode already captured in
/// this trace.
pub(crate) fn manifest_contains_fd(
    data: &Path,
    pid: libc::pid_t,
    fd: libc::c_int,
) -> io::Result<bool> {
    let metadata = fs::metadata(format!("/proc/{pid}/fd/{fd}"))?;
    Ok(load_manifest(data)?
        .iter()
        .any(|entry| entry.device == metadata.dev() && entry.inode == metadata.ino()))
}

pub(crate) fn load_manifest(data: &Path) -> io::Result<Vec<RecordedFile>> {
    match fs::read(data.join(FILES_MANIFEST_NAME)) {
        Ok(contents) => serde_json::from_slice(&contents).map_err(serde_io),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Returns whether a guest descriptor was opened read-only.
pub(crate) fn fd_is_read_only(pid: libc::pid_t, fd: libc::c_int) -> bool {
    let Ok(fdinfo) = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")) else {
        return false;
    };
    fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .and_then(|value| i32::from_str_radix(value, 8).ok())
        .is_some_and(|flags| flags & libc::O_ACCMODE == libc::O_RDONLY)
}

fn validate_snapshot(data: &Path, recorded: &RecordedFile) -> io::Result<PathBuf> {
    if !is_sha256_name(&recorded.sha256) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid recorded file SHA-256 name {:?}", recorded.sha256),
        ));
    }
    let source = snapshot_path(data, &recorded.sha256);
    let metadata = source.metadata()?;
    if metadata.len() != recorded.size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorded file {} has size {}, expected {}",
                source.display(),
                metadata.len(),
                recorded.size,
            ),
        ));
    }
    let actual = Digest::digest_path(&source)?.to_string();
    if actual != recorded.sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "recorded file {} has SHA-256 {actual}, expected {}",
                source.display(),
                recorded.sha256,
            ),
        ));
    }
    Ok(source)
}

/// Verifies all snapshots and copies them into the ephemeral replay chroot.
pub(crate) fn populate_chroot(data: &Path, chroot: &TempChroot) -> io::Result<()> {
    for recorded in load_manifest(data)? {
        let source = validate_snapshot(data, &recorded)?;
        let replay_path = Path::new(REPLAY_FILES_ROOT).join(&recorded.sha256);
        chroot.copy(&source, &replay_path)?;
        chroot.set_mode(&replay_path, recorded.mode)?;

        let original = Path::new(&recorded.path);
        if is_safe_absolute_path(original) {
            chroot.copy(&source, original)?;
            chroot.set_mode(original, recorded.mode)?;
        } else if original.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsafe recorded file path {:?}", recorded.path),
            ));
        }
    }
    Ok(())
}

/// Identifies a physical descriptor opened from the replay snapshot directory.
pub(crate) fn is_snapshot_fd(pid: libc::pid_t, fd: libc::c_int) -> bool {
    fs::read_link(format!("/proc/{pid}/fd/{fd}")).is_ok_and(|path| {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(is_sha256_name)
            && path.parent().and_then(Path::file_name) == Some(OsStr::new(&REPLAY_FILES_ROOT[1..]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_file_is_content_addressed_and_manifested() {
        let data = tempfile::tempdir().unwrap();
        let input = data.path().join("input");
        fs::write(&input, b"captured contents").unwrap();

        let recorded = capture_path(data.path(), &input).unwrap();

        assert_eq!(recorded.size, 17);
        assert_eq!(
            fs::read(snapshot_path(data.path(), &recorded.sha256)).unwrap(),
            b"captured contents"
        );
        assert_eq!(load_manifest(data.path()).unwrap(), vec![recorded]);
    }

    #[test]
    fn descriptor_access_mode_distinguishes_read_only_and_writable_opens() {
        let data = tempfile::tempdir().unwrap();
        let input = data.path().join("input");
        fs::write(&input, b"contents").unwrap();
        let read_only = File::open(&input).unwrap();
        let read_write = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&input)
            .unwrap();
        let pid = unsafe { libc::getpid() };

        assert!(fd_is_read_only(pid, read_only.as_raw_fd()));
        assert!(!fd_is_read_only(pid, read_write.as_raw_fd()));
    }

    #[test]
    fn manifest_paths_and_hash_names_are_confined() {
        assert!(is_sha256_name(&"ab".repeat(32)));
        assert!(!is_sha256_name(&"AB".repeat(32)));
        assert!(is_safe_absolute_path(Path::new("/usr/lib/input")));
        assert!(!is_safe_absolute_path(Path::new("/../../tmp/escape")));
        assert!(!is_safe_absolute_path(Path::new("relative/input")));
    }
}
