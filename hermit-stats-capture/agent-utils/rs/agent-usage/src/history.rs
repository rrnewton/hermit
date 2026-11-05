//! The sample history: an append-only JSONL file, plus the advisory lock that serialises writers.

use crate::model::Sample;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Only the newest this-many bytes of the history are read: about a year of 15-minute samples
/// for two providers, and far more than the 24-hour burn window needs.
pub const READ_TAIL: u64 = 32 << 20;

/// An exclusive `flock` held until dropped.
pub struct Lock {
    file: File,
}

impl Lock {
    /// Block until the lock at `path` is held.
    pub fn acquire(path: &Path) -> Result<Lock, String> {
        let file = open_lock(path)?;
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!(
                "flock {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(Lock { file })
    }

    /// Take the lock without waiting; `Ok(None)` when another process holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Lock>, String> {
        let file = open_lock(path)?;
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Ok(None);
            }
            return Err(format!("flock {}: {err}", path.display()));
        }
        Ok(Some(Lock { file }))
    }

    /// The locked file, for writing a pid into it.
    pub fn file(&mut self) -> &mut File {
        &mut self.file
    }
}

fn open_lock(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// Append samples, one JSON line each, in a single write.
pub fn append(path: &Path, samples: &[Sample]) -> Result<(), String> {
    let mut text = String::new();
    for sample in samples {
        text.push_str(&serde_json::to_string(sample).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("append {}: {e}", path.display()))
}

/// Every parseable sample in the history tail, oldest first. Unparseable lines (a torn write, a
/// future format) are skipped.
pub fn read(path: &Path) -> Vec<Sample> {
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(READ_TAIL);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        // Drop the partial first line.
        text = text
            .split_once('\n')
            .map(|(_, r)| r.to_string())
            .unwrap_or_default();
    }
    let mut samples: Vec<Sample> = text
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    samples.sort_by_key(|s| s.ts);
    samples
}

/// The newest sample for `provider`.
pub fn latest<'a>(samples: &'a [Sample], provider: &str) -> Option<&'a Sample> {
    samples.iter().rev().find(|s| s.provider == provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Status;

    #[test]
    fn append_read_and_skip_garbage() {
        let dir = std::env::temp_dir().join(format!("agent-usage-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("history.jsonl");
        let _ = std::fs::remove_file(&path);
        append(&path, &[Sample::new("claude", 20, Status::Ok)]).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{torn\n")
            .unwrap();
        append(
            &path,
            &[
                Sample::new("codex", 10, Status::Unavailable),
                Sample::new("claude", 30, Status::Error),
            ],
        )
        .unwrap();
        let all = read(&path);
        assert_eq!(all.iter().map(|s| s.ts).collect::<Vec<_>>(), [10, 20, 30]);
        assert_eq!(latest(&all, "claude").unwrap().ts, 30);
        assert_eq!(latest(&all, "codex").unwrap().status, Status::Unavailable);
        assert!(latest(&all, "other").is_none());
        let held = Lock::try_acquire(&dir.join("lock")).unwrap();
        assert!(held.is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
