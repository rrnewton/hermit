//! Private copies of the files a command sends, so that what the content guard and the
//! classifier inspect is exactly what gh sends.
//!
//! Without a copy, a body file could change between the moment the guard reads it and the moment
//! gh reads it, which can be many minutes later after a pacing sleep: a small, clean file could
//! be swapped for an archive. The classifier also reads GraphQL documents from files to tell a
//! query from a mutation. So before anything inspects a file, the wrapper copies it to
//! `<state dir>/snap-<pid>-<start>-<nonce>/<n>/<original file name>` (directories mode 0700,
//! files 0600), rewrites the argument to point at the copy, and runs everything after that on
//! the rewritten arguments. The original file name is kept because gh uses it (a gist's file
//! names come from it).
//!
//! # Lifetime
//!
//! The directory holds a `.lock` file on which the wrapper takes an exclusive `flock` before
//! copying anything, and gh inherits that locked descriptor, exactly as it inherits a write lease.
//! The lock is therefore held while the wrapper, gh, or anything gh left running is alive. `<pid>`
//! and `<start>` name the wrapper that created the directory (its process ID and its start time in
//! clock ticks since boot, so a reused PID does not count).
//!
//! When the invocation ends the wrapper closes its copy of the lock and removes the directory,
//! unless the lock is still held: then something gh started still has the descriptor, and the
//! directory is left for a later invocation to remove. Every paced invocation sweeps the state
//! directory first (see [`sweep`]) and removes a snapshot directory only when it is abandoned:
//! its lock is free, its creator is no longer running, and it is at least [`SWEEP_GRACE_SECS`]
//! old. A `snap-*` directory whose name is not in that form is removed once it is
//! [`SWEEP_AGE_SECS`] old.

use crate::guard::BodySource;
use crate::guard::SourceKind;
use crate::state;
use std::fs::{DirBuilder, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// Largest file copied, bytes. A larger file is refused; GitHub text bodies are limited to a few
/// kilobytes by the content guard anyway.
pub const MAX_SNAPSHOT_BYTES: u64 = 64 << 20;

/// Age after which a `snap-*` directory whose name does not identify its creator is removed,
/// seconds.
pub const SWEEP_AGE_SECS: u64 = 24 * 3600;

/// A snapshot directory whose lock is free and whose creator has exited is still kept until it
/// is this old, seconds, as a margin for a creator that has not yet taken the lock.
pub const SWEEP_GRACE_SECS: u64 = 60;

/// Name of the lock file inside a snapshot directory.
pub const LOCK_NAME: &str = ".lock";

/// The copies of one invocation. Dropping it closes this process's lock and removes the copies,
/// unless a process that inherited the lock still holds it (see the module documentation).
#[derive(Debug, Default)]
pub struct Snapshot {
    dir: Option<PathBuf>,
    lock: Option<File>,
    /// Number of files copied.
    pub files: usize,
}

impl Snapshot {
    /// The snapshot directory, when any file was copied.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The locked descriptor for gh to inherit, when any file was copied.
    pub fn lock_fd(&self) -> Option<RawFd> {
        self.lock.as_ref().map(AsRawFd::as_raw_fd)
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let Some(d) = self.dir.take() else {
            return;
        };
        drop(self.lock.take());
        if !state::lease_held(&d, LOCK_NAME) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// `snap-<pid>-<start>-<nonce>` for this process.
fn dir_name(nonce: &str) -> String {
    let pid = std::process::id();
    let start = state::process_start_ticks(pid).unwrap_or(0);
    format!("snap-{pid}-{start}-{nonce}")
}

/// The creator's process ID and start time from a snapshot directory name, or `None` when the
/// name is not `snap-<pid>-<start>-<16 hex digits>`.
fn creator(name: &str) -> Option<(u32, u64)> {
    let mut parts = name.strip_prefix("snap-")?.split('-');
    let pid = parts.next()?.parse().ok()?;
    let start = parts.next()?.parse().ok()?;
    let nonce = parts.next()?;
    let hex = nonce.len() == 16
        && nonce
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (hex && parts.next().is_none()).then_some((pid, start))
}

/// Create `dir` and take the exclusive lock on its lock file.
fn locked_dir(dir: &Path) -> Result<File, String> {
    private_dir(dir)?;
    let path = dir.join(LOCK_NAME);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    // SAFETY: flock on a valid, owned file descriptor. Blocking is safe: the file is new with a
    // random name, so the only other holder can be a sweeper's momentary shared probe.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("cannot lock {}: {err}", path.display()));
    }
    Ok(file)
}

fn private_dir(path: &Path) -> Result<(), String> {
    DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("cannot create snapshot directory {}: {e}", path.display()))
}

/// Copy every file in `sources` (found by `guard::file_sources` on `rest`) and return the
/// argument list with each path replaced by its copy. An error names the file and why it could
/// not be copied: missing, unreadable, larger than [`MAX_SNAPSHOT_BYTES`], or not found where
/// the parser said it was in `rest`.
pub fn take(
    state_dir: &Path,
    nonce: &str,
    sources: &[BodySource],
    rest: &[String],
) -> Result<(Snapshot, Vec<String>), String> {
    let mut snap = Snapshot::default();
    let mut out = rest.to_vec();
    for (i, s) in sources.iter().enumerate() {
        let SourceKind::File(path) = &s.kind else {
            continue;
        };
        let Some(loc) = &s.location else {
            return Err(format!(
                "{} names file {path}, but its place in the command line is unknown, so it \
                 cannot be copied for inspection",
                s.flag
            ));
        };
        let expected = format!("{}{path}", loc.prefix);
        if rest.get(loc.index) != Some(&expected) {
            return Err(format!(
                "{} names file {path}, but argument {} is not `{expected}`",
                s.flag, loc.index
            ));
        }
        let dir = match &snap.dir {
            Some(d) => d.clone(),
            None => {
                let d = state_dir.join(dir_name(nonce));
                let lock = locked_dir(&d);
                // Recorded before checking the lock, so a failure still removes the directory.
                snap.dir = Some(d.clone());
                snap.lock = Some(lock?);
                d
            }
        };
        let sub = dir.join(i.to_string());
        private_dir(&sub)?;
        let name = Path::new(path)
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_else(|| "body".into());
        let copy = sub.join(name);
        copy_capped(Path::new(path), &copy).map_err(|e| format!("{} file {path}: {e}", s.flag))?;
        snap.files += 1;
        out[loc.index] = format!("{}{}", loc.prefix, copy.display());
    }
    Ok((snap, out))
}

fn copy_capped(src: &Path, dst: &Path) -> Result<(), String> {
    let file = std::fs::File::open(src).map_err(|e| format!("cannot read: {e}"))?;
    let mut buf = Vec::new();
    file.take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("cannot read: {e}"))?;
    if buf.len() as u64 > MAX_SNAPSHOT_BYTES {
        return Err(format!(
            "larger than {MAX_SNAPSHOT_BYTES} bytes, too large to copy for inspection"
        ));
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)
        .map_err(|e| format!("cannot write copy {}: {e}", dst.display()))?;
    out.write_all(&buf)
        .map_err(|e| format!("cannot write copy {}: {e}", dst.display()))
}

/// Remove the abandoned snapshot directories in `state_dir` (see the module documentation):
/// those whose lock is free, whose creator is not running, and which are at least
/// [`SWEEP_GRACE_SECS`] old; and other `snap-*` directories at least [`SWEEP_AGE_SECS`] old.
/// Returns how many were removed.
pub fn sweep(state_dir: &Path) -> usize {
    sweep_with(state_dir, &state::process_alive)
}

/// [`sweep`] with the creator liveness test supplied.
pub fn sweep_with(state_dir: &Path, alive: &dyn Fn(u32, u64) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return 0;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("snap-") {
            continue;
        }
        let Some(age) = e
            .metadata()
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .map(|age| age.as_secs())
        else {
            continue;
        };
        let abandoned = match creator(&name) {
            Some((pid, start)) => {
                age >= SWEEP_GRACE_SECS
                    && !state::lease_held(&e.path(), LOCK_NAME)
                    && !alive(pid, start)
            }
            None => age > SWEEP_AGE_SECS,
        };
        if abandoned && std::fs::remove_dir_all(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;
    use crate::config::Config;
    use crate::guard::file_sources;
    use std::os::unix::fs::PermissionsExt;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gh-paced-snap-{tag}-{}-{}",
            std::process::id(),
            crate::state::new_nonce(0.0)
        ));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    fn snap_of(dir: &Path, line: &[String]) -> Result<(Snapshot, Vec<String>), String> {
        let c = classify(line, &Config::default());
        let rest = &line[c.rest_start..];
        take(dir, "00000000000000aa", &file_sources(&c, rest), rest)
    }

    /// The copy keeps the bytes and file name, is private, is what the rewritten argument names,
    /// and does not follow later changes to the original.
    #[test]
    fn copies_replace_the_paths_and_vanish_on_drop() {
        let _children = state::child_guard();
        let dir = tmpdir("copy");
        let body = dir.join("body.md");
        std::fs::write(&body, b"short note").expect("write");
        let g = dir.join("notes.txt");
        std::fs::write(&g, b"gist text").expect("write");
        let line: Vec<String> = [
            "issue",
            "comment",
            "1",
            &format!("--body-file={}", body.display()),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (snap, rest) = snap_of(&dir, &line).expect("snapshot");
        assert_eq!(snap.files, 1);
        let copy = rest[1].strip_prefix("--body-file=").expect("prefix kept");
        assert_ne!(copy, body.display().to_string());
        assert!(copy.ends_with("/0/body.md"), "{copy}");
        std::fs::write(&body, b"CHANGED AFTER THE GUARD").expect("rewrite");
        assert_eq!(std::fs::read(copy).expect("read copy"), b"short note");
        let mode = std::fs::metadata(copy).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let snapdir = snap.dir().expect("directory").to_path_buf();
        let snapname = snapdir
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let me = std::process::id();
        let start = state::process_start_ticks(me).expect("own start time");
        assert_eq!(snapname, format!("snap-{me}-{start}-00000000000000aa"));
        assert_eq!(creator(&snapname), Some((me, start)));
        assert!(state::lease_held(&snapdir, LOCK_NAME), "locked while alive");
        let dmode = std::fs::metadata(&snapdir)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(dmode & 0o777, 0o700);
        drop(snap);
        assert!(!snapdir.exists());

        let line: Vec<String> = ["gist", "create", &g.display().to_string()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (snap, rest) = snap_of(&dir, &line).expect("snapshot");
        assert!(rest[0].ends_with("/0/notes.txt"), "{rest:?}");
        assert_eq!(snap.files, 1);
        drop(snap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_oversized_files_are_errors() {
        let dir = tmpdir("err");
        let line: Vec<String> = ["issue", "comment", "1", "-F", "/nonexistent/gh-paced-body"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = snap_of(&dir, &line).expect_err("missing file");
        assert!(err.contains("cannot read"), "{err}");
        let big = dir.join("big");
        let f = std::fs::File::create(&big).expect("create");
        f.set_len(MAX_SNAPSHOT_BYTES + 1).expect("set_len");
        let line: Vec<String> = ["issue", "comment", "1", "-F", &big.display().to_string()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = snap_of(&dir, &line).expect_err("oversized file");
        assert!(err.contains("too large"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_files_means_no_directory() {
        let dir = tmpdir("none");
        let line: Vec<String> = ["issue", "comment", "1", "-b", "hi"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (snap, rest) = snap_of(&dir, &line).expect("snapshot");
        assert_eq!(snap.files, 0);
        assert_eq!(rest, line[2..].to_vec());
        assert!(snap.dir().is_none() && snap.lock_fd().is_none());
        let entries = std::fs::read_dir(&dir).expect("read_dir").count();
        assert_eq!(entries, 0, "nothing created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_age(path: &Path, secs: u64) {
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(secs);
        std::fs::File::open(path)
            .expect("open")
            .set_modified(t)
            .expect("set mtime");
    }

    /// A directory is removed only when its lock is free, its creator is gone, and it is past
    /// the grace period; a name that does not identify its creator falls back to the 24-hour age.
    #[test]
    fn sweep_removes_only_abandoned_snapshot_directories() {
        let _children = state::child_guard();
        let dir = tmpdir("sweep");
        let day = 86400;
        // Creator 111 is "dead", creator 222 "alive" (start time 7 for both).
        let alive = |pid: u32, start: u64| pid == 222 && start == 7;
        let dead = dir.join("snap-111-7-00000000000000a1");
        let dead_held = dir.join("snap-111-7-00000000000000a2");
        let dead_recent = dir.join("snap-111-7-00000000000000a3");
        let live_unlocked = dir.join("snap-222-7-00000000000000a4");
        let reused_pid = dir.join("snap-222-8-00000000000000a5");
        let legacy_old = dir.join("snap-old");
        let legacy_fresh = dir.join("snap-fresh");
        let bad_nonce = dir.join("snap-111-7-NOTHEX0000000000");
        let other = dir.join("keep-me");
        for d in [
            &dead,
            &dead_recent,
            &live_unlocked,
            &reused_pid,
            &legacy_old,
            &legacy_fresh,
            &bad_nonce,
            &other,
        ] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        // dead_held: something gh started still holds the inherited lock.
        let held = locked_dir(&dead_held).expect("lock");
        for d in [&dead, &dead_held, &live_unlocked, &reused_pid] {
            set_age(d, 2 * SWEEP_GRACE_SECS);
        }
        set_age(&dead_recent, SWEEP_GRACE_SECS / 2);
        for d in [&legacy_old, &bad_nonce, &other] {
            set_age(d, 2 * day);
        }
        set_age(&legacy_fresh, day / 2);

        assert_eq!(sweep_with(&dir, &alive), 4);
        assert!(!dead.exists(), "lock free, creator dead, past the grace");
        assert!(
            !reused_pid.exists(),
            "same PID with another start time is dead"
        );
        assert!(
            !legacy_old.exists() && !bad_nonce.exists(),
            "unknown names: 24 h"
        );
        assert!(dead_held.exists(), "the inherited lock keeps it");
        assert!(dead_recent.exists(), "within the grace period");
        assert!(live_unlocked.exists(), "its creator is running");
        assert!(legacy_fresh.exists());
        assert!(other.exists(), "not a snapshot directory");

        drop(held);
        assert_eq!(sweep_with(&dir, &alive), 1);
        assert!(!dead_held.exists(), "removed once the lock is released");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real sweep keeps a live snapshot of this process however old its directory looks, and
    /// dropping the snapshot leaves the directory while an inherited copy of the lock is open.
    #[test]
    fn inherited_lock_outlives_the_wrapper_copy() {
        let _children = state::child_guard();
        let dir = tmpdir("inherit");
        let body = dir.join("body.md");
        std::fs::write(&body, b"note").expect("write");
        let line: Vec<String> = ["issue", "comment", "1", "-F", &body.display().to_string()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (snap, _) = snap_of(&dir, &line).expect("snapshot");
        let snapdir = snap.dir().expect("directory").to_path_buf();
        set_age(&snapdir, 2 * 86400);
        assert_eq!(sweep(&dir), 0, "creator alive and lock held");
        assert!(snapdir.exists());
        // What gh holds after exec: a duplicate of the locked descriptor.
        // SAFETY: dup of a descriptor owned by `snap`; closed below.
        let inherited = unsafe { libc::dup(snap.lock_fd().expect("lock")) };
        assert!(inherited >= 0);
        drop(snap);
        assert!(snapdir.exists(), "kept while the inherited copy is open");
        assert!(state::lease_held(&snapdir, LOCK_NAME));
        // SAFETY: closing the duplicate made above.
        unsafe { libc::close(inherited) };
        assert!(!state::lease_held(&snapdir, LOCK_NAME));
        // This process created it and is alive, so only a creator-blind sweep removes it.
        assert_eq!(sweep(&dir), 0);
        assert_eq!(sweep_with(&dir, &|_, _| false), 1);
        assert!(!snapdir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
