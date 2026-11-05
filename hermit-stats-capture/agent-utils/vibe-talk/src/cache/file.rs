//! The cache file on disk: where it is, who may read it, and the lock that makes it one
//! process's.
//!
//! - **Created `0600` before SQLite sees it**, with `create_new`, so SQLite's `-wal` and `-shm`
//!   inherit that mode rather than the process umask's.
//! - **The directory must be private already**: owned by this user, with no group or other
//!   permission bit. A directory this code creates is made `0700`; one that exists is checked and
//!   refused, never changed, because loosening or tightening someone else's directory is not this
//!   server's business.
//! - **An exclusive `flock` on `<path>.lock`** is held for the life of the process, so a second
//!   process configured with the same path refuses to open it instead of erasing the first one's
//!   file under it.
//!
//! The file system here is copy-on-write on the deployment this was written for. Deleting a file
//! is a logical erase, not a physical one, and the help text says so.

use std::fs::File;
use std::path::{Path, PathBuf};

/// The default cache path: beside the store, named after it and this host,
/// `<store stem>.message-cache-<host>.sqlite3`.
///
/// The host name is a collision guard: a copy synced to another machine is not that machine's
/// cache, and that machine's "off" never matches its name.
#[must_use]
pub fn default_path(store_path: &Path, host: &str) -> PathBuf {
    let stem = store_path.file_stem().map_or_else(
        || "vibe-talk".to_owned(),
        |s| s.to_string_lossy().into_owned(),
    );
    let host: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let host = if host.is_empty() {
        "localhost".to_owned()
    } else {
        host
    };
    let name = format!("{stem}.message-cache-{host}.sqlite3");
    store_path
        .parent()
        .map_or_else(|| PathBuf::from(&name), |dir| dir.join(&name))
}

/// This machine's host name, for [`default_path`].
#[must_use]
pub fn host_name() -> String {
    #[cfg(unix)]
    {
        rustix::system::uname()
            .nodename()
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".to_owned())
    }
}

/// The files that make up one cache: the database and SQLite's three working files.
#[must_use]
pub fn family(path: &Path) -> [PathBuf; 4] {
    let with = |suffix: &str| {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
    };
    [
        path.to_owned(),
        with("-wal"),
        with("-shm"),
        with("-journal"),
    ]
}

/// The lock file beside a cache path.
#[must_use]
pub fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}

/// The bytes the cache occupies: the database, `-wal` and `-shm`, as `stat` sees them.
#[must_use]
pub fn bytes_on_disk(path: &Path) -> u64 {
    family(path)
        .iter()
        .take(3)
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum()
}

/// `path` with `.` components dropped and each `..` taking away the component before it, without
/// asking the file system: `/data/./s.sqlite3` and `/data/x/../s.sqlite3` both become
/// `/data/s.sqlite3`.
#[must_use]
pub fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let popped = out.pop();
                if !popped && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The path with its directory resolved by the file system (symbolic links followed), when the
/// directory exists.
fn resolved(path: &Path) -> Option<PathBuf> {
    if let Ok(full) = std::fs::canonicalize(path) {
        return Some(full);
    }
    let name = path.file_name()?;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty())?;
    std::fs::canonicalize(dir).ok().map(|dir| dir.join(name))
}

/// Whether two paths name the same file: the same once normalized, the same once their
/// directories are resolved, or, when both exist, the same file on the same device.
#[must_use]
pub fn same_file(a: &Path, b: &Path) -> bool {
    if normalize_lexically(a) == normalize_lexically(b) {
        return true;
    }
    if let (Some(x), Some(y)) = (resolved(a), resolved(b)) {
        if x == y {
            return true;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) {
            return x.dev() == y.dev() && x.ino() == y.ino();
        }
    }
    false
}

/// Make sure the cache's directory exists and is private, creating it `0700` if it does not.
///
/// # Errors
///
/// A sentence for the operator when the directory cannot be made or is not private.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    if !dir.exists() {
        create_private_dir(dir)?;
    }
    check_private_dir(dir)
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|error| format!("cannot create {}: {}", dir.display(), error.kind()))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create {}: {}", dir.display(), error.kind()))
}

#[cfg(unix)]
fn check_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(dir)
        .map_err(|error| format!("cannot inspect {}: {}", dir.display(), error.kind()))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let me = rustix::process::geteuid().as_raw();
    if metadata.uid() != me {
        return Err(format!(
            "{} is owned by another user; the message cache holds other people's messages and \
             is kept only in a directory this account owns",
            dir.display()
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(format!(
            "{} can be read or entered by other accounts (mode {:o}); give it 0700",
            dir.display(),
            metadata.mode() & 0o777
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_private_dir(_dir: &Path) -> Result<(), String> {
    Err("the message cache needs a Unix host to keep its file private".to_owned())
}

/// Pre-create the cache file `0600`, if it does not exist yet. Returns whether it was created.
///
/// # Errors
///
/// When the file cannot be created.
pub fn create_if_absent(path: &Path) -> Result<bool, String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!(
            "cannot create {}: {}",
            path.display(),
            error.kind()
        )),
    }
}

/// Set an existing file to `0600`. A no-op off Unix, and for a file that is not there.
pub fn restrict(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if path.exists() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The exclusive lock on one cache path, held until dropped.
#[derive(Debug)]
pub struct PathLock {
    _file: File,
    path: PathBuf,
}

impl PathLock {
    /// Take the lock on `path`, without waiting.
    ///
    /// # Errors
    ///
    /// When another open of the same path, in this process or another, holds it.
    pub fn acquire(path: &Path) -> Result<Self, String> {
        let lock = lock_path(path);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let file = options
            .open(&lock)
            .map_err(|error| format!("cannot open {}: {}", lock.display(), error.kind()))?;
        #[cfg(unix)]
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |_| {
                format!(
                    "{} is already open in another process or another cache handle; two servers \
                     must never share one cache file",
                    path.display()
                )
            },
        )?;
        Ok(Self {
            _file: file,
            path: path.to_owned(),
        })
    }

    /// The cache path this lock guards.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Delete the database and its working files, then make the deletions durable. Returns the
/// bytes they held.
pub fn unlink_family(path: &Path) -> u64 {
    let mut removed = 0;
    for member in family(path) {
        if let Ok(metadata) = std::fs::metadata(&member) {
            if std::fs::remove_file(&member).is_ok() {
                removed += metadata.len();
            }
        }
    }
    sync_parent(path);
    removed
}

/// fsync the directory holding `path`, so a create, rename or unlink in it survives a crash.
pub fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        if let Ok(handle) = File::open(dir) {
            let _ = handle.sync_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_path_sits_beside_the_store_and_names_the_host() {
        assert_eq!(
            default_path(Path::new("/data/state/slack.sqlite3"), "devbox-7"),
            PathBuf::from("/data/state/slack.message-cache-devbox-7.sqlite3")
        );
        assert_eq!(
            default_path(Path::new("/data/s.sqlite3"), "a/b c"),
            PathBuf::from("/data/s.message-cache-a_b_c.sqlite3")
        );
        assert_eq!(
            default_path(Path::new("/data/s.sqlite3"), ""),
            PathBuf::from("/data/s.message-cache-localhost.sqlite3")
        );
        assert!(!host_name().is_empty());
    }

    #[test]
    fn paths_naming_one_file_are_the_same_however_they_are_spelt() {
        assert_eq!(
            normalize_lexically(Path::new("/data/./x/../s.sqlite3")),
            PathBuf::from("/data/s.sqlite3")
        );
        assert_eq!(
            normalize_lexically(Path::new("../a/./b")),
            PathBuf::from("../a/b")
        );
        assert!(same_file(
            Path::new("/data/./s.sqlite3"),
            Path::new("/data/s.sqlite3")
        ));
        assert!(!same_file(
            Path::new("/data/s.sqlite3"),
            Path::new("/data/c.sqlite3")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_or_a_hard_link_to_the_store_is_the_store() {
        let scratch = crate::testing::TempDir::new("cache-same-file");
        let real = scratch.path().join("real");
        std::fs::create_dir(&real).expect("dir");
        let store = real.join("s.sqlite3");
        std::fs::write(&store, b"store").expect("store");
        let linked_dir = scratch.path().join("linked");
        std::os::unix::fs::symlink(&real, &linked_dir).expect("symlink");
        assert!(same_file(&store, &linked_dir.join("s.sqlite3")));
        let hard = scratch.path().join("hard.sqlite3");
        std::fs::hard_link(&store, &hard).expect("hard link");
        assert!(same_file(&store, &hard));
        assert!(!same_file(&store, &real.join("other.sqlite3")));
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_open_to_others_is_refused_and_a_new_one_is_made_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = crate::testing::TempDir::new("cache-dir");
        let open = scratch.path().join("open");
        std::fs::create_dir(&open).expect("dir");
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let refused = ensure_private_dir(&open).expect_err("0755 must be refused");
        assert!(refused.contains("0700"), "{refused}");
        assert_eq!(
            std::fs::metadata(&open).expect("meta").permissions().mode() & 0o777,
            0o755,
            "an existing directory is never changed"
        );
        let made = scratch.path().join("made").join("deeper");
        ensure_private_dir(&made).expect("created private");
        assert_eq!(
            std::fs::metadata(&made).expect("meta").permissions().mode() & 0o777,
            0o700
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_created_0600_once_and_the_lock_refuses_a_second_holder() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = crate::testing::TempDir::new("cache-file");
        let dir = scratch.path().join("c");
        ensure_private_dir(&dir).expect("dir");
        let path = dir.join("x.sqlite3");
        assert!(create_if_absent(&path).expect("create"));
        assert!(!create_if_absent(&path).expect("exists"));
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o600
        );
        let first = PathLock::acquire(&path).expect("first lock");
        assert_eq!(first.path(), path.as_path());
        let second = PathLock::acquire(&path).expect_err("second lock");
        assert!(second.contains("already open"), "{second}");
        drop(first);
        PathLock::acquire(&path).expect("free again once the first is dropped");
        std::fs::write(family(&path)[1].clone(), b"wal").expect("wal");
        assert_eq!(bytes_on_disk(&path), 3);
        assert_eq!(unlink_family(&path), 3);
        assert!(!path.exists());
    }
}
