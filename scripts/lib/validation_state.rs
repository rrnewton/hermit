// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Where the validation state directory lives on disk, and how its records
//! name it.
//!
//! The validation front door keeps per-run state under its parent checkout:
//! durable driver logs, admission files, retained artifacts, live-run
//! records and caches. That directory was `ignored/validate/`. It moves to
//! `validate_tmp/` beside `ignored/`, and `ignored/validate` stays behind as a
//! relative symlink whose literal target is `../validate_tmp`.
//!
//! Records shared between hosts keep naming files `ignored/validate/...`: the
//! ledger's workspace locators require that spelling, and rows written before
//! the move use it. That spelling is the *logical* name. The *physical* name
//! is where the bytes are. Every writer that walks without following symlinks
//! (the admission walk, artifact publication, the cache-base check) must walk
//! the physical directory, because it refuses the compatibility symlink.
//!
//! The parent root is *migrated* when `validate_tmp` is a real directory and
//! `ignored/validate` is either absent or exactly the compatibility symlink,
//! with `ignored` itself a real directory or absent. A parent with no
//! `validate_tmp` entry keeps the old behaviour unchanged, including its
//! refusals; a parent that has one but is not migrated is refused by
//! `check_configured`, because its state already lives in two places.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;

/// The front door sets this to the physical state directory it prepared.
pub(crate) const STATE_DIR_ENV: &str = "DEV_HERMIT_VALIDATION_STATE_DIR";
/// Physical state directory, relative to the parent root.
pub(crate) const STATE_DIR: &str = "validate_tmp";
/// Logical state directory, relative to the parent root.
pub(crate) const LEGACY_STATE_DIR: &str = "ignored/validate";
/// Literal target of the compatibility symlink at `LEGACY_STATE_DIR`.
pub(crate) const LEGACY_SYMLINK_TARGET: &str = "../validate_tmp";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Layout {
    /// `ignored/validate` is the physical directory (or nothing exists yet).
    Legacy,
    /// `validate_tmp` is the physical directory.
    Migrated,
}

impl Layout {
    /// Read the layout of `parent` from the filesystem.
    pub(crate) fn detect(parent: &Path) -> Self {
        let real_directory = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
        };
        if !real_directory(&parent.join(STATE_DIR)) {
            return Self::Legacy;
        }
        match std::fs::symlink_metadata(parent.join("ignored")) {
            Err(error) if error.kind() == ErrorKind::NotFound => return Self::Migrated,
            Ok(metadata) if metadata.file_type().is_dir() => {}
            _ => return Self::Legacy,
        }
        let legacy = parent.join(LEGACY_STATE_DIR);
        match std::fs::symlink_metadata(&legacy) {
            Err(error) if error.kind() == ErrorKind::NotFound => Self::Migrated,
            // Compare the target's bytes, as the front door does: `Path`
            // equality ignores a trailing `/` and interior `.` components, so
            // it would accept `../validate_tmp/` or `.././validate_tmp`.
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    && std::fs::read_link(&legacy).is_ok_and(|target| {
                        target.as_os_str() == OsStr::new(LEGACY_SYMLINK_TARGET)
                    }) =>
            {
                Self::Migrated
            }
            _ => Self::Legacy,
        }
    }

    /// Parent-relative physical state directory.
    pub(crate) fn prefix(self) -> &'static str {
        match self {
            Self::Legacy => LEGACY_STATE_DIR,
            Self::Migrated => STATE_DIR,
        }
    }

    /// Absolute physical state directory below `parent`.
    pub(crate) fn state_dir(self, parent: &Path) -> PathBuf {
        parent.join(self.prefix())
    }

    /// Physical parent-relative text for a parent-relative name in either
    /// spelling. Text outside the state directory is returned unchanged, so a
    /// caller's own validation still refuses it.
    pub(crate) fn physical(self, relative: &str) -> String {
        match (self, suffix(relative)) {
            (Self::Migrated, Some((_, rest))) => join(STATE_DIR, rest),
            (Self::Legacy, Some((LEGACY_STATE_DIR, rest))) => join(LEGACY_STATE_DIR, rest),
            _ => relative.to_string(),
        }
    }

    /// Logical (`ignored/validate/...`) text for a parent-relative name.
    /// Physical `validate_tmp/...` is translated only when migrated; under the
    /// old layout that spelling is returned unchanged and later refused.
    pub(crate) fn logical(self, relative: &str) -> String {
        match (self, suffix(relative)) {
            (Self::Migrated, Some((_, rest))) => join(LEGACY_STATE_DIR, rest),
            _ => relative.to_string(),
        }
    }
}

fn suffix(relative: &str) -> Option<(&'static str, &str)> {
    for prefix in [LEGACY_STATE_DIR, STATE_DIR] {
        if relative == prefix {
            return Some((prefix, ""));
        }
        if let Some(rest) = relative
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('/'))
        {
            return Some((prefix, rest));
        }
    }
    None
}

fn join(prefix: &str, rest: &str) -> String {
    if rest.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}/{rest}")
    }
}

/// Check the front door's declared state directory against this driver's
/// parent root, before anything is written there.
///
/// Unset (or empty) leaves the layout to the filesystem, as a front door from
/// before the move expects: an unmigrated parent keeps `ignored/validate`, and
/// a migrated one uses `validate_tmp`. A parent that has a `validate_tmp`
/// entry but is not migrated (a real `ignored/validate` beside it, a symlinked
/// `validate_tmp` or `ignored`, or a foreign link) is refused, because either
/// choice would write beside state that already lives elsewhere. Set, it must
/// name exactly `<parent>/validate_tmp` on a migrated parent: any other value
/// means the front door and the driver disagree about where state goes, and
/// writing anyway would split it between two directories.
pub(crate) fn check_configured(
    parent: Option<&Path>,
    configured: Option<&OsStr>,
) -> Result<Layout, String> {
    let configured = configured.filter(|value| !value.is_empty());
    let Some(parent) = parent else {
        return match configured {
            None => Ok(Layout::Legacy),
            Some(value) => Err(format!(
                "{STATE_DIR_ENV}={} names a validation state directory, but this driver \
                 resolved no parent root to hold it",
                Path::new(value).display()
            )),
        };
    };
    let layout = Layout::detect(parent);
    let Some(value) = configured else {
        let state_entry = parent.join(STATE_DIR);
        return match std::fs::symlink_metadata(&state_entry) {
            Ok(_) if layout == Layout::Legacy => Err(format!(
                "{} exists, but {} is not migrated: {STATE_DIR} must be a real directory, \
                 ignored a real directory or absent, and {LEGACY_STATE_DIR} absent or the \
                 symlink {LEGACY_SYMLINK_TARGET}; writing either directory would split \
                 validation state",
                state_entry.display(),
                parent.display()
            )),
            Ok(_) => Ok(layout),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(layout),
            Err(error) => Err(format!("cannot inspect {}: {error}", state_entry.display())),
        };
    };
    let value = Path::new(value);
    let expected = parent.join(STATE_DIR);
    let same = value == expected
        || matches!(
            (value.canonicalize(), expected.canonicalize()),
            (Ok(left), Ok(right)) if left == right
        );
    if !same {
        return Err(format!(
            "{STATE_DIR_ENV}={} does not name {}; the validation front door and this driver \
             disagree about the parent root",
            value.display(),
            expected.display()
        ));
    }
    if layout != Layout::Migrated {
        return Err(format!(
            "{STATE_DIR_ENV} names {}, but {} is not migrated: {STATE_DIR} must be a real \
             directory and {LEGACY_STATE_DIR} absent or the symlink {LEGACY_SYMLINK_TARGET}; \
             writing here would split validation state",
            expected.display(),
            parent.display()
        ));
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn migrate(parent: &Path) {
        std::fs::create_dir_all(parent.join(STATE_DIR)).unwrap();
        std::fs::create_dir_all(parent.join("ignored")).unwrap();
        symlink(LEGACY_SYMLINK_TARGET, parent.join(LEGACY_STATE_DIR)).unwrap();
    }

    #[test]
    fn detect_accepts_only_the_compatibility_layouts() {
        let fresh = root();
        assert_eq!(Layout::detect(fresh.path()), Layout::Legacy, "nothing yet");

        let legacy = root();
        std::fs::create_dir_all(legacy.path().join(LEGACY_STATE_DIR)).unwrap();
        assert_eq!(Layout::detect(legacy.path()), Layout::Legacy);
        std::fs::create_dir(legacy.path().join(STATE_DIR)).unwrap();
        assert_eq!(
            Layout::detect(legacy.path()),
            Layout::Legacy,
            "a real ignored/validate beside validate_tmp is split state, not migrated"
        );

        let migrated = root();
        migrate(migrated.path());
        assert_eq!(Layout::detect(migrated.path()), Layout::Migrated);

        let absent = root();
        std::fs::create_dir(absent.path().join(STATE_DIR)).unwrap();
        assert_eq!(Layout::detect(absent.path()), Layout::Migrated);
        std::fs::create_dir(absent.path().join("ignored")).unwrap();
        assert_eq!(Layout::detect(absent.path()), Layout::Migrated);

        // Spellings that resolve to validate_tmp but are not the literal
        // target. `Path` equality accepts the first two.
        assert_eq!(
            Path::new(".././validate_tmp"),
            Path::new(LEGACY_SYMLINK_TARGET)
        );
        assert_eq!(
            Path::new("../validate_tmp/"),
            Path::new(LEGACY_SYMLINK_TARGET)
        );
        for target in [".././validate_tmp", "../validate_tmp/", "./../validate_tmp"] {
            let nonliteral = root();
            std::fs::create_dir(nonliteral.path().join(STATE_DIR)).unwrap();
            std::fs::create_dir(nonliteral.path().join("ignored")).unwrap();
            symlink(target, nonliteral.path().join(LEGACY_STATE_DIR)).unwrap();
            assert_eq!(
                Layout::detect(nonliteral.path()),
                Layout::Legacy,
                "{target}"
            );
        }

        let foreign = root();
        let outside = root();
        std::fs::create_dir(foreign.path().join(STATE_DIR)).unwrap();
        std::fs::create_dir(foreign.path().join("ignored")).unwrap();
        symlink(outside.path(), foreign.path().join(LEGACY_STATE_DIR)).unwrap();
        assert_eq!(Layout::detect(foreign.path()), Layout::Legacy);

        let linked_state = root();
        std::fs::create_dir(linked_state.path().join("ignored")).unwrap();
        symlink(outside.path(), linked_state.path().join(STATE_DIR)).unwrap();
        symlink(
            LEGACY_SYMLINK_TARGET,
            linked_state.path().join(LEGACY_STATE_DIR),
        )
        .unwrap();
        assert_eq!(Layout::detect(linked_state.path()), Layout::Legacy);

        let linked_ignored = root();
        let elsewhere = root();
        std::fs::create_dir(linked_ignored.path().join(STATE_DIR)).unwrap();
        symlink(LEGACY_SYMLINK_TARGET, elsewhere.path().join("validate")).unwrap();
        symlink(elsewhere.path(), linked_ignored.path().join("ignored")).unwrap();
        assert_eq!(
            Layout::detect(linked_ignored.path()),
            Layout::Legacy,
            "the compatibility link must sit in this root's own ignored directory"
        );
    }

    #[test]
    fn translation_keeps_logical_records_and_physical_walks_apart() {
        let migrated = Layout::Migrated;
        assert_eq!(
            migrated.physical("ignored/validate/admission/x/context.json"),
            "validate_tmp/admission/x/context.json"
        );
        assert_eq!(
            migrated.physical("validate_tmp/a.log"),
            "validate_tmp/a.log"
        );
        assert_eq!(migrated.physical("ignored/validate"), "validate_tmp");
        assert_eq!(
            migrated.logical("validate_tmp/artifacts/r/x.json"),
            "ignored/validate/artifacts/r/x.json"
        );
        assert_eq!(
            migrated.logical("ignored/validate/a.log"),
            "ignored/validate/a.log"
        );
        for outside in [
            "ignored/validation/a.log",
            "validate_tmpx/a.log",
            "ignored/other",
            "",
        ] {
            assert_eq!(migrated.physical(outside), outside);
            assert_eq!(migrated.logical(outside), outside);
        }

        let legacy = Layout::Legacy;
        assert_eq!(
            legacy.physical("ignored/validate/a.log"),
            "ignored/validate/a.log"
        );
        assert_eq!(legacy.physical("validate_tmp/a.log"), "validate_tmp/a.log");
        assert_eq!(legacy.logical("validate_tmp/a.log"), "validate_tmp/a.log");
        assert_eq!(
            legacy.state_dir(Path::new("/p")),
            Path::new("/p/ignored/validate")
        );
        assert_eq!(
            migrated.state_dir(Path::new("/p")),
            Path::new("/p/validate_tmp")
        );
    }

    #[test]
    fn configured_state_dir_must_match_a_migrated_parent() {
        let migrated = root();
        migrate(migrated.path());
        let parent = migrated.path();
        let expected = parent.join(STATE_DIR);
        assert_eq!(
            check_configured(Some(parent), Some(expected.as_os_str())),
            Ok(Layout::Migrated)
        );
        assert_eq!(check_configured(Some(parent), None), Ok(Layout::Migrated));
        assert_eq!(
            check_configured(Some(parent), Some(OsStr::new(""))),
            Ok(Layout::Migrated)
        );
        // A different spelling of the same directory is the same directory.
        let dotted = parent.join("ignored/../validate_tmp");
        assert_eq!(
            check_configured(Some(parent), Some(dotted.as_os_str())),
            Ok(Layout::Migrated)
        );

        let other = root();
        migrate(other.path());
        let error = check_configured(Some(parent), Some(other.path().join(STATE_DIR).as_os_str()))
            .unwrap_err();
        assert!(error.contains("disagree about the parent root"), "{error}");

        let error = check_configured(None, Some(expected.as_os_str())).unwrap_err();
        assert!(error.contains("no parent root"), "{error}");
        assert_eq!(check_configured(None, None), Ok(Layout::Legacy));

        // Unmigrated parents keep the old behaviour when nothing is declared,
        // and refuse a declaration that would split state.
        let legacy = root();
        std::fs::create_dir_all(legacy.path().join(LEGACY_STATE_DIR)).unwrap();
        assert_eq!(
            check_configured(Some(legacy.path()), None),
            Ok(Layout::Legacy)
        );
        std::fs::create_dir(legacy.path().join(STATE_DIR)).unwrap();
        let error = check_configured(
            Some(legacy.path()),
            Some(legacy.path().join(STATE_DIR).as_os_str()),
        )
        .unwrap_err();
        assert!(error.contains("would split validation state"), "{error}");
        // A front door from before the move declares nothing; the split
        // layout is still refused rather than written as Legacy.
        let error = check_configured(Some(legacy.path()), None).unwrap_err();
        assert!(error.contains("would split validation state"), "{error}");

        // So is any other unmigrated layout that has a validate_tmp entry.
        let outside = root();
        let linked_state = root();
        symlink(outside.path(), linked_state.path().join(STATE_DIR)).unwrap();
        let error = check_configured(Some(linked_state.path()), None).unwrap_err();
        assert!(error.contains("is not migrated"), "{error}");
        let linked_ignored = root();
        std::fs::create_dir(linked_ignored.path().join(STATE_DIR)).unwrap();
        symlink(outside.path(), linked_ignored.path().join("ignored")).unwrap();
        let error = check_configured(Some(linked_ignored.path()), None).unwrap_err();
        assert!(error.contains("is not migrated"), "{error}");

        // A migrated parent without the compatibility link is still migrated.
        let unlinked = root();
        std::fs::create_dir(unlinked.path().join(STATE_DIR)).unwrap();
        assert_eq!(
            check_configured(Some(unlinked.path()), None),
            Ok(Layout::Migrated)
        );

        let fresh = root();
        let error = check_configured(
            Some(fresh.path()),
            Some(fresh.path().join(STATE_DIR).as_os_str()),
        )
        .unwrap_err();
        assert!(error.contains("is not migrated"), "{error}");
    }
}
