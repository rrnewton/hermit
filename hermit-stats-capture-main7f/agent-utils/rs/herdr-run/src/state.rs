//! Private account-global lock paths shared by session resolution and pane execution.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::client::current_account_home;
use crate::error::{HerdrRunError, Result};

fn state_root_for_home(home: &Path) -> Result<PathBuf> {
    if !home.is_absolute() {
        return Err(HerdrRunError::unavailable(format!(
            "the current account home is not absolute: {}",
            home.display()
        )));
    }
    Ok(home.join(".local/state/herdr-run"))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|error| {
        HerdrRunError::unavailable(format!(
            "cannot prepare private herdr-run state directory {}: {error}",
            path.display()
        ))
    })?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
        HerdrRunError::unavailable(format!(
            "cannot set private mode on herdr-run state directory {}: {error}",
            path.display()
        ))
    })
}

fn lock_path_at(home: &Path, directories: &[&str], filename: &str) -> Result<PathBuf> {
    let root = state_root_for_home(home)?;
    ensure_private_directory(&root)?;
    let mut directory = root.join("locks");
    ensure_private_directory(&directory)?;
    for component in directories {
        directory.push(component);
        ensure_private_directory(&directory)?;
    }
    Ok(directory.join(filename))
}

pub(crate) fn session_lock_path() -> Result<PathBuf> {
    let home = current_account_home()?;
    lock_path_at(&home, &[], "session-resolve.lock")
}

pub(crate) fn pane_lock_path(pane_id: &str) -> Result<PathBuf> {
    let home = current_account_home()?;
    let digest = Sha256::digest(pane_id.as_bytes());
    lock_path_at(&home, &["panes"], &format!("{digest:x}.lock"))
}

pub(crate) fn open_lock_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            HerdrRunError::unavailable(format!(
                "cannot open private herdr-run lock {}: {error}",
                path.display()
            ))
        })?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|error| {
            HerdrRunError::unavailable(format!(
                "cannot set private mode on herdr-run lock {}: {error}",
                path.display()
            ))
        })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn paths_are_account_global_private_and_pane_ids_are_hashed() {
        let root = std::env::temp_dir().join(format!(
            "herdr-run-state-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("account-home");
        let session = lock_path_at(&home, &[], "session-resolve.lock").unwrap();
        let pane = lock_path_at(
            &home,
            &["panes"],
            &format!("{:x}.lock", Sha256::digest(b"opaque:pane/id")),
        )
        .unwrap();

        assert_eq!(
            session,
            home.join(".local/state/herdr-run/locks/session-resolve.lock")
        );
        assert_eq!(
            pane.parent().unwrap(),
            home.join(".local/state/herdr-run/locks/panes")
        );
        assert!(!pane.to_string_lossy().contains("opaque:pane/id"));
        for directory in [
            home.join(".local/state/herdr-run"),
            home.join(".local/state/herdr-run/locks"),
            home.join(".local/state/herdr-run/locks/panes"),
        ] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let lock = open_lock_file(&pane).unwrap();
        assert_eq!(lock.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(lock);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn relative_account_home_is_rejected() {
        let error = state_root_for_home(Path::new("relative/home")).unwrap_err();
        assert!(error.to_string().contains("not absolute"));
    }
}
