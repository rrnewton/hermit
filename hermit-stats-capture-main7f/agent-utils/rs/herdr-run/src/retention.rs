//! Bounded, write-triggered retention for per-run spool directories.
//!
//! Pruning is deliberately scoped to direct, non-symlink directory entries below the configured
//! `runs` directory. The root itself and sibling evidence such as `audit.jsonl` are never removed.
//! Every failure is reported in [`PruneResult`] or treated as a no-op: housekeeping must not mask
//! the command that triggered it.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Default number of days for which captured run output is retained.
pub const RETENTION_DAYS: u64 = 4;

/// Largest accepted retention window.
pub const MAX_RETENTION_DAYS: u64 = 365_000;

const SECONDS_PER_DAY: u64 = 86_400;

/// Observable result of one best-effort prune pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PruneResult {
    /// Direct child run directories removed by this pass.
    pub removed: Vec<PathBuf>,
    /// Recent direct child run directories retained by this pass.
    pub kept: usize,
    /// Symlinks, non-directories, or entries that could not be inspected or removed.
    pub skipped: Vec<PathBuf>,
}

impl PruneResult {
    /// Return the number of run directories removed by this pass.
    #[must_use]
    pub fn removed_count(&self) -> usize {
        self.removed.len()
    }
}

/// Resolve the directory holding per-run spools for one project configuration.
#[must_use]
pub fn runs_root(spool_dir: &Path, project_root: &Path) -> PathBuf {
    let spool = if spool_dir.is_absolute() {
        spool_dir.to_path_buf()
    } else {
        project_root.join(spool_dir)
    };
    spool.join("runs")
}

/// Remove completed direct-child runs older than `retention_days` without following links.
///
/// A symlink configured as the `runs` root is rejected outright. This function never removes the
/// root itself and never raises an error; an unavailable or unsafe root is a best-effort no-op.
#[must_use]
pub fn prune_runs(root: &Path, retention_days: u64) -> PruneResult {
    prune_runs_at(root, retention_days, SystemTime::now())
}

fn prune_runs_at(root: &Path, retention_days: u64, now: SystemTime) -> PruneResult {
    let mut result = PruneResult::default();
    let Some(lexical_root) = lexical_absolute(root) else {
        return result;
    };
    if retention_days > MAX_RETENTION_DAYS {
        result.skipped.push(lexical_root);
        return result;
    }
    let Ok(root_metadata) = fs::symlink_metadata(&lexical_root) else {
        return result;
    };
    if root_metadata.file_type().is_symlink() {
        result.skipped.push(lexical_root);
        return result;
    }
    if !root_metadata.is_dir() {
        return result;
    }
    // Reject symlinks in every path component, not only a final `runs` link. Canonicalization is
    // used only to prove the lexical path already names itself; its result is never deletion scope.
    let Ok(canonical_root) = fs::canonicalize(&lexical_root) else {
        return result;
    };
    if canonical_root != lexical_root {
        result.skipped.push(lexical_root);
        return result;
    }

    let Ok(read_dir) = fs::read_dir(&lexical_root) else {
        return result;
    };
    let mut entries = read_dir
        .filter_map(std::result::Result::ok)
        .collect::<Vec<_>>();
    // By the raw bytes of each name (`OsString` orders by bytes), which is the order the other
    // edition sorts in too.
    entries.sort_by_key(fs::DirEntry::file_name);

    let retention = retention_days
        .checked_mul(SECONDS_PER_DAY)
        .and_then(|seconds| now.checked_sub(Duration::from_secs(seconds)));

    for entry in entries {
        let path = entry.path();
        // `read_dir` constructs a direct child path, but assert that lexical containment rather than
        // relying on that API detail before reaching the only recursive deletion in this crate.
        if path.parent() != Some(lexical_root.as_path()) {
            result.skipped.push(path);
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            result.skipped.push(path);
            continue;
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            result.skipped.push(path);
            continue;
        }
        // A regular, parseable `exit_code` is the completion marker. Directory mtime reflects
        // allocation, not completion, and pruning by it races active commands on other panes.
        let Some(modified) = completion_modified(&path.join("exit_code")) else {
            result.kept += 1;
            continue;
        };
        // If an exceptionally large retention window cannot be represented by SystemTime, fail
        // closed toward retaining evidence rather than deleting it.
        if retention.is_none_or(|cutoff| modified >= cutoff) {
            result.kept += 1;
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            result.removed.push(path);
        } else {
            result.skipped.push(path);
        }
    }
    result
}

fn completion_modified(path: &Path) -> Option<SystemTime> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut payload = Vec::new();
    file.by_ref().take(129).read_to_end(&mut payload).ok()?;
    if payload.len() > 128 {
        return None;
    }
    crate::textfmt::parse_exit_marker(&payload)?;
    metadata.modified().ok()
}

fn lexical_absolute(path: &Path) -> Option<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temporary_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "herdr-retention-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("temporary root");
        path
    }

    fn plant(root: &Path, name: &str, modified: SystemTime) -> PathBuf {
        let path = root.join(name);
        fs::create_dir_all(&path).expect("run directory");
        for leaf in ["stdout", "stderr", "exit_code", "command"] {
            let contents = if leaf == "exit_code" {
                "0\n".to_owned()
            } else {
                format!("{leaf} of {name}\n")
            };
            fs::write(path.join(leaf), contents).expect("run evidence");
        }
        File::open(path.join("exit_code"))
            .expect("open completion marker")
            .set_times(FileTimes::new().set_modified(modified))
            .expect("backdate completion marker");
        File::open(&path)
            .expect("open run directory")
            .set_times(FileTimes::new().set_modified(modified))
            .expect("backdate run directory");
        path
    }

    #[test]
    fn old_run_is_pruned_and_recent_run_survives() {
        let base = temporary_root("window");
        let root = base.join("runs");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let old = plant(&root, "old", now - Duration::from_secs(5 * SECONDS_PER_DAY));
        let recent = plant(&root, "recent", now - Duration::from_secs(SECONDS_PER_DAY));

        let result = prune_runs_at(&root, RETENTION_DAYS, now);
        assert_eq!(result.removed.as_slice(), std::slice::from_ref(&old));
        assert_eq!(result.removed_count(), 1);
        assert_eq!(result.kept, 1);
        assert!(!old.exists());
        assert!(recent.is_dir());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn boundary_and_all_fresh_controls_keep_expected_runs() {
        let base = temporary_root("boundary");
        let root = base.join("runs");
        fs::create_dir(&root).unwrap();
        let now = SystemTime::now();
        let inside = plant(
            &root,
            "inside",
            now - Duration::from_secs(RETENTION_DAYS * SECONDS_PER_DAY - 1),
        );
        let outside = plant(
            &root,
            "outside",
            now - Duration::from_secs(RETENTION_DAYS * SECONDS_PER_DAY + 1),
        );

        let result = prune_runs_at(&root, RETENTION_DAYS, now);
        assert_eq!(result.removed.as_slice(), std::slice::from_ref(&outside));
        assert_eq!(result.kept, 1);
        assert!(inside.is_dir());
        assert!(!outside.exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn old_incomplete_run_survives_while_old_completed_run_is_removed() {
        let base = temporary_root("active");
        let root = base.join("runs");
        fs::create_dir(&root).unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * SECONDS_PER_DAY);
        let completed = plant(&root, "completed", old);
        let active = plant(&root, "active", old);
        fs::remove_file(active.join("exit_code")).unwrap();

        let result = prune_runs_at(&root, RETENTION_DAYS, SystemTime::now());
        assert_eq!(result.removed, [completed]);
        assert_eq!(result.kept, 1);
        assert!(active.is_dir());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn excessive_window_fails_closed() {
        let base = temporary_root("huge-window");
        let root = base.join("runs");
        fs::create_dir(&root).unwrap();
        let old = plant(
            &root,
            "old",
            SystemTime::now() - Duration::from_secs(30 * SECONDS_PER_DAY),
        );

        let result = prune_runs_at(&root, MAX_RETENTION_DAYS + 1, SystemTime::now());
        assert!(result.removed.is_empty());
        assert_eq!(result.skipped, [root]);
        assert!(old.is_dir());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn fifo_completion_marker_cannot_block_retention() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;

        let base = temporary_root("fifo-marker");
        let root = base.join("runs");
        fs::create_dir(&root).unwrap();
        let run = plant(
            &root,
            "corrupt",
            SystemTime::now() - Duration::from_secs(30 * SECONDS_PER_DAY),
        );
        let marker = run.join("exit_code");
        fs::remove_file(&marker).unwrap();
        let marker_c = CString::new(marker.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(marker_c.as_ptr(), 0o600) }, 0);

        let result = prune_runs_at(&root, RETENTION_DAYS, SystemTime::now());
        assert!(run.is_dir());
        assert!(fs::symlink_metadata(marker).unwrap().file_type().is_fifo());
        assert_eq!(result.kept, 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn symlink_root_and_symlink_entries_are_never_followed_or_removed() {
        let base = temporary_root("links");
        let real_root = base.join("real-runs");
        let outside = base.join("precious");
        fs::create_dir(&real_root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("keep.txt"), "must survive\n").unwrap();

        let root_link = base.join("runs-link");
        symlink(&real_root, &root_link).unwrap();
        let root_result = prune_runs_at(&root_link, RETENTION_DAYS, SystemTime::now());
        assert_eq!(
            root_result.skipped.as_slice(),
            std::slice::from_ref(&root_link)
        );

        let escape = real_root.join("escape");
        symlink(&outside, &escape).unwrap();
        let result = prune_runs_at(
            &real_root,
            RETENTION_DAYS,
            SystemTime::now() + Duration::from_secs(30 * SECONDS_PER_DAY),
        );
        assert_eq!(result.skipped.as_slice(), std::slice::from_ref(&escape));
        assert!(escape.is_symlink());
        assert_eq!(
            fs::read_to_string(outside.join("keep.txt")).unwrap(),
            "must survive\n"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn symlinked_spool_ancestor_is_rejected_as_deletion_scope() {
        let base = temporary_root("ancestor-link");
        let real_spool = base.join("real-spool");
        let real_runs = real_spool.join("runs");
        fs::create_dir_all(&real_runs).unwrap();
        let old = plant(
            &real_runs,
            "old",
            SystemTime::now() - Duration::from_secs(30 * SECONDS_PER_DAY),
        );
        let spool_link = base.join("spool-link");
        symlink(&real_spool, &spool_link).unwrap();
        let linked_runs = spool_link.join("runs");

        let result = prune_runs_at(&linked_runs, RETENTION_DAYS, SystemTime::now());
        assert_eq!(
            result.skipped.as_slice(),
            std::slice::from_ref(&linked_runs)
        );
        assert!(old.is_dir());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn root_siblings_and_loose_files_are_untouched() {
        let base = temporary_root("scope");
        let root = base.join("runs");
        fs::create_dir(&root).unwrap();
        let audit = base.join("audit.jsonl");
        fs::write(&audit, "{\"verdict\":\"RAN\"}\n").unwrap();
        let loose = root.join("stray.txt");
        fs::write(&loose, "x").unwrap();
        let old = plant(
            &root,
            "old",
            SystemTime::now() - Duration::from_secs(9 * SECONDS_PER_DAY),
        );

        let result = prune_runs_at(&root, RETENTION_DAYS, SystemTime::now());
        assert_eq!(result.removed, [old]);
        assert_eq!(result.skipped.as_slice(), std::slice::from_ref(&loose));
        assert!(root.is_dir());
        assert!(loose.is_file());
        assert_eq!(
            fs::read_to_string(audit).unwrap(),
            "{\"verdict\":\"RAN\"}\n"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn missing_root_is_a_noop_and_paths_resolve_under_the_spool() {
        let base = temporary_root("missing");
        assert_eq!(
            prune_runs(&base.join("missing"), RETENTION_DAYS),
            PruneResult::default()
        );
        assert_eq!(
            runs_root(Path::new(".herdr-run"), Path::new("/project")),
            Path::new("/project/.herdr-run/runs")
        );
        assert_eq!(
            runs_root(Path::new("/absolute/spool"), Path::new("/project")),
            Path::new("/absolute/spool/runs")
        );
        fs::remove_dir_all(base).unwrap();
    }
}
