//! The off switch and the registry of cache files: a `0600` JSON sidecar beside the store.
//!
//! `<store path>.message-cache.json` holds whether the owner turned the cache off and when, every
//! cache path this store's server has ever opened, and since when the configuration has had no
//! `[message_cache]` table. It is deliberately NOT in the store: purging the store must not turn
//! the cache back on, restoring an older copy of the store must not lose the switch, and none of
//! this needs a store migration.
//!
//! Turning the cache off writes the marker FIRST and erases second, so a crash between the two
//! still erases, at the next start ([`reconcile_at_start`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{file, schema, CacheError, EraseReport, MessageCache};
use crate::config::MessageCacheConfig;

/// How long the configuration may go without a `[message_cache]` table before a cache file left
/// from an earlier configuration is erased. Not at once: on a slow provider a refill takes days,
/// and an overlay briefly missing is not a decision to throw it away.
pub const ABSENT_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The sidecar's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    /// Whether the owner turned the cache off.
    #[serde(default)]
    pub off: bool,
    /// When, in local milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub off_at_ms: Option<i64>,
    /// Every cache path this store's server has opened.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// Since when the configuration has had no `[message_cache]` table, in local milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_since_ms: Option<i64>,
}

impl Sidecar {
    /// Record a cache path as opened. Returns whether it was new.
    pub fn register(&mut self, path: &Path) -> bool {
        if self.paths.iter().any(|known| known == path) {
            false
        } else {
            self.paths.push(path.to_owned());
            true
        }
    }
}

/// The sidecar's path for a store.
#[must_use]
pub fn sidecar_path(store_path: &Path) -> PathBuf {
    let mut name = store_path.as_os_str().to_owned();
    name.push(".message-cache.json");
    PathBuf::from(name)
}

/// Read the sidecar. A missing one is the default; one that cannot be parsed is read as OFF,
/// because the safe reading of a damaged switch is the one that erases.
///
/// # Errors
///
/// When the file exists but cannot be read.
pub fn load(path: &Path) -> Result<Sidecar, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(&text).unwrap_or(Sidecar {
            off: true,
            ..Sidecar::default()
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Sidecar::default()),
        Err(error) => Err(format!("cannot read {}: {}", path.display(), error.kind())),
    }
}

/// Write the sidecar atomically: a `0600` temporary file, fsync, rename over the old one, fsync
/// of the directory.
///
/// # Errors
///
/// When any of those steps fails.
pub fn store(path: &Path, sidecar: &Sidecar) -> Result<(), String> {
    use std::io::Write as _;
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let _ = std::fs::remove_file(&temporary);
    let text = serde_json::to_vec_pretty(sidecar).map_err(|e| e.to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let fail = |what: &str, error: std::io::Error| {
        format!("cannot {what} {}: {}", path.display(), error.kind())
    };
    let mut handle = options.open(&temporary).map_err(|e| fail("write", e))?;
    handle.write_all(&text).map_err(|e| fail("write", e))?;
    handle.sync_all().map_err(|e| fail("sync", e))?;
    drop(handle);
    std::fs::rename(&temporary, path).map_err(|e| fail("replace", e))?;
    file::sync_parent(path);
    Ok(())
}

/// What the configuration and the sidecar together say to do at startup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupAction {
    /// Open the cache.
    Open,
    /// Erase every cache file this server made, and open none.
    EraseAll,
    /// Open none, erase none: the table is absent and the grace period has not run out. Warn.
    Leave,
}

/// Decide what startup does. `enabled = false` and the owner's off switch both erase; an absent
/// table erases only once it has been absent for [`ABSENT_GRACE_MS`].
#[must_use]
pub fn startup_action(
    config: Option<&MessageCacheConfig>,
    sidecar: &Sidecar,
    now_ms: i64,
) -> StartupAction {
    match config {
        Some(config) if !config.enabled => StartupAction::EraseAll,
        Some(_) if sidecar.off => StartupAction::EraseAll,
        Some(_) => StartupAction::Open,
        None if sidecar.off => StartupAction::EraseAll,
        None => match sidecar.absent_since_ms {
            Some(since) if now_ms.saturating_sub(since) >= ABSENT_GRACE_MS => {
                StartupAction::EraseAll
            }
            _ => StartupAction::Leave,
        },
    }
}

/// Erase every registered cache path, and every file in `default_dir` named like a cache whose
/// header says it is one. Returns the bytes removed.
#[must_use]
pub fn erase_registered(sidecar: &Sidecar, default_dir: Option<&Path>) -> u64 {
    let mut doomed: Vec<PathBuf> = sidecar.paths.clone();
    if let Some(dir) = default_dir {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let named_like_one = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".message-cache-") && n.ends_with(".sqlite3"));
                if named_like_one
                    && schema::read_header(&path).is_ok_and(|h| h == schema::Header::Ours)
                    && !doomed.contains(&path)
                {
                    doomed.push(path);
                }
            }
        }
    }
    doomed.iter().map(|path| erase_one(path)).sum()
}

/// Erase one cache file and its working files, if it is still ours to erase: what is at the path
/// now is a cache file this code made (or the empty file it pre-creates, or nothing, leaving only
/// working files), and no other process has it open. A registered path is only a record of where
/// a cache WAS; something else may have been put there since, and is left alone.
fn erase_one(path: &Path) -> u64 {
    match schema::read_header(path) {
        Ok(schema::Header::Ours | schema::Header::Empty) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => {
            tracing::warn!(
                path = %path.display(),
                "a registered message cache path now holds something else; it is left alone"
            );
            return 0;
        }
        Err(error) => {
            tracing::warn!(
                path = %path.display(),
                kind = ?error.kind(),
                "a registered message cache path cannot be read; it is left alone"
            );
            return 0;
        }
    }
    let Ok(lock) = file::PathLock::acquire(path) else {
        tracing::warn!(
            path = %path.display(),
            "a registered message cache is open in another process; it is left alone"
        );
        return 0;
    };
    let bytes = file::unlink_family(path);
    let _ = std::fs::remove_file(file::lock_path(path));
    drop(lock);
    bytes
}

/// What [`reconcile_at_start`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// What startup should do now.
    pub action: StartupAction,
    /// Bytes erased.
    pub erased_bytes: u64,
}

/// Apply [`startup_action`]: erase when it says so, and keep `absent_since_ms` current.
///
/// # Errors
///
/// When the sidecar cannot be read or written.
pub fn reconcile_at_start(
    config: Option<&MessageCacheConfig>,
    sidecar_file: &Path,
    default_dir: Option<&Path>,
    now_ms: i64,
) -> Result<Reconciled, String> {
    let mut sidecar = load(sidecar_file)?;
    let action = startup_action(config, &sidecar, now_ms);
    let erased_bytes = if action == StartupAction::EraseAll {
        erase_registered(&sidecar, default_dir)
    } else {
        0
    };
    let absent_since = match (config, action) {
        (None, StartupAction::Leave) => Some(sidecar.absent_since_ms.unwrap_or(now_ms)),
        _ => None,
    };
    if absent_since != sidecar.absent_since_ms {
        sidecar.absent_since_ms = absent_since;
        store(sidecar_file, &sidecar)?;
    }
    Ok(Reconciled {
        action,
        erased_bytes,
    })
}

/// The owner's "off": write the marker first, then erase the file and leave it closed.
///
/// # Errors
///
/// When the marker cannot be written; nothing is erased then, and the caller says so.
pub fn switch_off(
    cache: &MessageCache,
    sidecar_file: &Path,
    now_ms: i64,
) -> Result<EraseReport, CacheError> {
    let mut sidecar = load(sidecar_file).map_err(CacheError::Filesystem)?;
    sidecar.off = true;
    sidecar.off_at_ms = Some(now_ms);
    sidecar.register(cache.path());
    store(sidecar_file, &sidecar).map_err(CacheError::Filesystem)?;
    cache.erase_file(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_is_not_off_and_off_wins_over_the_configuration() {
        let on = MessageCacheConfig {
            enabled: true,
            ..MessageCacheConfig::default()
        };
        let off = MessageCacheConfig::default();
        let clear = Sidecar::default();
        let marked = Sidecar {
            off: true,
            ..Sidecar::default()
        };
        assert_eq!(startup_action(Some(&on), &clear, 0), StartupAction::Open);
        assert_eq!(
            startup_action(Some(&on), &marked, 0),
            StartupAction::EraseAll
        );
        assert_eq!(
            startup_action(Some(&off), &clear, 0),
            StartupAction::EraseAll
        );
        assert_eq!(startup_action(None, &clear, 0), StartupAction::Leave);
        assert_eq!(startup_action(None, &marked, 0), StartupAction::EraseAll);
        let absent = Sidecar {
            absent_since_ms: Some(1_000),
            ..Sidecar::default()
        };
        assert_eq!(
            startup_action(None, &absent, 1_000 + ABSENT_GRACE_MS - 1),
            StartupAction::Leave
        );
        assert_eq!(
            startup_action(None, &absent, 1_000 + ABSENT_GRACE_MS),
            StartupAction::EraseAll
        );
    }

    #[test]
    fn the_sidecar_is_written_atomically_0600_and_a_damaged_one_reads_as_off() {
        let scratch = crate::testing::TempDir::new("cache-marker");
        let path = scratch.path().join("store.sqlite3.message-cache.json");
        assert_eq!(load(&path).expect("missing"), Sidecar::default());
        let mut sidecar = Sidecar::default();
        assert!(sidecar.register(Path::new("/x/c.sqlite3")));
        assert!(!sidecar.register(Path::new("/x/c.sqlite3")));
        store(&path, &sidecar).expect("store");
        assert_eq!(load(&path).expect("load"), sidecar);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::write(&path, b"{not json").expect("damage");
        assert!(load(&path).expect("load").off);
        assert_eq!(
            sidecar_path(Path::new("/d/s.sqlite3")),
            PathBuf::from("/d/s.sqlite3.message-cache.json")
        );
    }

    #[test]
    fn erasing_registered_paths_leaves_a_foreign_file_and_an_open_cache_alone() {
        let scratch = crate::testing::TempDir::new("cache-marker-erase");
        let dir = scratch.path().join("c");
        file::ensure_private_dir(&dir).expect("dir");
        let foreign = dir.join("now-someone-elses.sqlite3");
        std::fs::write(&foreign, b"their notes").expect("write");
        let open_one = dir.join("open.message-cache-h.sqlite3");
        let cache = MessageCache::open(crate::cache::CacheOptions::new(&open_one)).expect("open");
        let stale = dir.join("stale.message-cache-h.sqlite3");
        drop(MessageCache::open(crate::cache::CacheOptions::new(&stale)).expect("make one"));
        let mut sidecar = Sidecar::default();
        for path in [&foreign, &open_one, &stale] {
            sidecar.register(path);
        }
        let erased = erase_registered(&sidecar, Some(&dir));
        assert!(erased > 0);
        assert_eq!(std::fs::read(&foreign).expect("kept"), b"their notes");
        assert!(
            open_one.exists(),
            "a cache another handle holds is not erased under it"
        );
        assert!(!stale.exists());
        assert!(!file::lock_path(&stale).exists());
        drop(cache);
    }

    #[test]
    fn an_absent_table_is_remembered_and_erases_only_after_the_grace_period() {
        let scratch = crate::testing::TempDir::new("cache-absent");
        let marker = scratch.path().join("m.json");
        let first = reconcile_at_start(None, &marker, None, 5_000).expect("first");
        assert_eq!(first.action, StartupAction::Leave);
        assert_eq!(load(&marker).expect("load").absent_since_ms, Some(5_000));
        let later = reconcile_at_start(None, &marker, None, 6_000).expect("later");
        assert_eq!(later.action, StartupAction::Leave);
        assert_eq!(load(&marker).expect("load").absent_since_ms, Some(5_000));
        let on = MessageCacheConfig {
            enabled: true,
            ..MessageCacheConfig::default()
        };
        let back = reconcile_at_start(Some(&on), &marker, None, 7_000).expect("back");
        assert_eq!(back.action, StartupAction::Open);
        assert_eq!(load(&marker).expect("load").absent_since_ms, None);
    }
}
