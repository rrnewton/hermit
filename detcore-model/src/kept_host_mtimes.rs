/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Experimental (provrepro, `--keep-host-mtimes-under`): the guest paths
//! whose pre-existing files keep their real host mtime instead of the epoch.
//!
//! Detcore gives every file it first sees with a real host mtime the epoch
//! as its virtual mtime. A package build whose build root comes from a
//! pinned image can record its dependencies' mtimes (GNAT writes each
//! dependency's timestamp into its `.ali` files), and a native build
//! records the image files' real mtimes. With this flag, a stat of a file
//! below one of the given guest paths keeps the host's mtime, which the
//! pinned image fixes. The match is on the guest's spelling of the path,
//! made absolute (see [`covers`]); Detcore's stat handling decides which
//! spelling a call names.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Parse one `--keep-host-mtimes-under` value: an absolute guest path
/// without a `..` component, which could not name a fixed subtree once
/// symbolic links are in play. `.` components and repeated or trailing
/// slashes are allowed and ignored.
pub fn parse_guest_path_prefix(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!(
            "expected an absolute guest path such as /usr, not {value:?}"
        ));
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(format!(
            "expected a guest path without `..` components, not {value:?}"
        ));
    }
    Ok(lexically_normalized(path.as_path()))
}

/// `path` with `.` dropped and each `..` removing the component before it,
/// as text: a `..` at the root stays at the root, as Linux's lookup does.
/// Symbolic links are not consulted.
pub fn lexically_normalized(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::CurDir => {}
            Component::RootDir => normalized.push(Component::RootDir),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

/// Whether one of `prefixes` covers the guest path `path`: `path` is
/// absolute and, normalized lexically ([`lexically_normalized`]), is the
/// prefix or lies below it, component by component. `/usr` covers `/usr`
/// and `/usr/lib/x`, not `/usrx`. A relative path (one Detcore could not
/// make absolute) is covered by nothing.
pub fn covers(prefixes: &[PathBuf], path: &Path) -> bool {
    if prefixes.is_empty() || !path.is_absolute() {
        return false;
    }
    let path = lexically_normalized(path);
    prefixes
        .iter()
        .any(|prefix| path.starts_with(lexically_normalized(prefix)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes(paths: &[&str]) -> Vec<PathBuf> {
        paths
            .iter()
            .map(|path| parse_guest_path_prefix(path).unwrap())
            .collect()
    }

    #[test]
    fn kept_host_mtimes_cover_paths_below_a_prefix() {
        let under = prefixes(&["/usr", "/etc", "/var/lib"]);
        for path in [
            "/usr",
            "/usr/",
            "/usr/bin/true",
            "/usr/lib/gcc/x86_64-linux-gnu/13/adainclude/a-textio.ads",
            "/etc/passwd",
            "/var/lib/dpkg/status",
            // `.` and `..` are resolved as text.
            "/usr/./bin/true",
            "/usr/bin/../lib/gcc/x86_64-linux-gnu/13/adalib/a-textio.ali",
            "/build/../usr/bin/true",
            "//usr//bin/true",
        ] {
            assert!(covers(&under, Path::new(path)), "{path} is under a prefix");
        }
    }

    #[test]
    fn kept_host_mtimes_do_not_cover_siblings_or_paths_outside() {
        let under = prefixes(&["/usr", "/etc", "/var/lib"]);
        for path in [
            "/usrx",
            "/usrx/bin/true",
            "/etcetera/passwd",
            "/var/libexec/x",
            "/var",
            "/",
            "/inputs/liblog4ada.dsc",
            "/build/liblog4ada/src/log4ada.ads",
            "/usr/../build/x",
            "/usr/lib/../../inputs/x",
            // A spelling Detcore could not make absolute.
            "usr/bin/true",
            "bin/true",
            "",
        ] {
            assert!(!covers(&under, Path::new(path)), "{path} is not covered");
        }
    }

    #[test]
    fn kept_host_mtimes_without_the_flag_cover_nothing() {
        for path in ["/usr/bin/true", "/etc/passwd", "/", "/inputs/x"] {
            assert!(!covers(&[], Path::new(path)), "{path}");
        }
    }

    #[test]
    fn kept_host_mtimes_refuse_relative_and_climbing_prefixes() {
        assert!(parse_guest_path_prefix("usr").is_err());
        assert!(parse_guest_path_prefix("./usr").is_err());
        assert!(parse_guest_path_prefix("").is_err());
        assert!(parse_guest_path_prefix("/usr/../build").is_err());
        assert_eq!(
            parse_guest_path_prefix("/usr/./lib/").unwrap(),
            PathBuf::from("/usr/lib")
        );
        // The root covers every absolute path.
        let root = prefixes(&["/"]);
        assert!(covers(&root, Path::new("/inputs/x")));
    }
}
