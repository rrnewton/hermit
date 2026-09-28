/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Discovery of resources shipped beside the Hermit executable.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

/// Environment variable selecting a Hermit installation directory.
// TODO-HUMAN-REVIEW(PR-1002): Review the unified installation-directory contract.
pub const INSTALL_DIR_ENV: &str = "HERMIT_INSTALL_DIR";

fn invoked_executable(argv0: Option<&OsStr>, current_dir: &Path) -> Option<PathBuf> {
    let argv0 = Path::new(argv0?);
    if !argv0.is_absolute() && argv0.components().count() <= 1 {
        return None;
    }
    Some(if argv0.is_absolute() {
        argv0.to_path_buf()
    } else {
        current_dir.join(argv0)
    })
}

fn directory_present(path: &Path) -> io::Result<bool> {
    let inspect = || {
        let entry = match fs::symlink_metadata(path) {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let metadata = if entry.file_type().is_symlink() {
            fs::metadata(path)?
        } else {
            entry
        };
        if !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "resource directory is not a directory",
            ));
        }
        Ok(true)
    };
    inspect().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot inspect resource directory {}: {error}",
                path.display()
            ),
        )
    })
}

fn has_resources(directory: &Path) -> io::Result<bool> {
    Ok(directory_present(directory)? && directory_present(&directory.join("rsrcs"))?)
}

fn discover_install_dir_from(
    explicit: Option<&OsStr>,
    argv0: Option<&OsStr>,
    executable: &Path,
    current_dir: &Path,
) -> io::Result<Option<PathBuf>> {
    if let Some(explicit) = explicit {
        if explicit.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{INSTALL_DIR_ENV} is empty"),
            ));
        }
        let directory = PathBuf::from(explicit);
        has_resources(&directory)?;
        return Ok(Some(directory));
    }

    let invoked = invoked_executable(argv0, current_dir);
    let invoked_directory = invoked.as_deref().and_then(Path::parent);
    let executable_directory = executable.parent();
    let built_in_place = executable_directory
        .and_then(Path::parent)
        .map(|target| target.join("install_pkg"));

    for directory in [
        invoked_directory,
        executable_directory,
        built_in_place.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if has_resources(directory)? {
            return Ok(Some(directory.to_path_buf()));
        }
    }
    Ok(None)
}

/// Returns the selected installation directory, if a packaged installation is available.
// TODO-HUMAN-REVIEW(PR-1002): Review executable-relative resource discovery.
pub fn install_dir() -> io::Result<Option<PathBuf>> {
    let executable = env::current_exe()?;
    let current_dir = env::current_dir()?;
    discover_install_dir_from(
        env::var_os(INSTALL_DIR_ENV).as_deref(),
        env::args_os().next().as_deref(),
        &executable,
        &current_dir,
    )
}

/// Returns a path below the selected installation's `rsrcs` directory.
// TODO-HUMAN-REVIEW(PR-1002): Review the shared backend-resource layout.
pub fn resource(relative: impl AsRef<Path>) -> io::Result<Option<PathBuf>> {
    Ok(install_dir()?.map(|directory| directory.join("rsrcs").join(relative)))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = env::temp_dir().join(format!(
                "hermit-resource-root-{name}-{}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            fs::create_dir_all(root.join("target/release")).unwrap();
            Self(root)
        }

        fn discover(&self) -> io::Result<Option<PathBuf>> {
            discover_install_dir_from(
                None,
                Some(self.0.join("invoked/hermit").as_os_str()),
                &self.0.join("target/release/hermit"),
                &self.0,
            )
        }

        fn locations(&self) -> [PathBuf; 3] {
            [
                self.0.join("invoked"),
                self.0.join("target/release"),
                self.0.join("target/install_pkg"),
            ]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    struct RestorePermissions(PathBuf, fs::Permissions);

    impl RestorePermissions {
        fn deny_search(path: &Path) -> Self {
            let permissions = fs::metadata(path).unwrap().permissions();
            fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
            Self(path.to_path_buf(), permissions)
        }
    }

    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            fs::set_permissions(&self.0, self.1.clone()).unwrap();
        }
    }

    #[test]
    fn missing_resource_roots_are_absent() {
        let fixture = Fixture::new("missing");
        assert_eq!(fixture.discover().unwrap(), None);
        fs::create_dir(fixture.0.join("target/install_pkg")).unwrap();
        assert_eq!(fixture.discover().unwrap(), None);
    }

    #[test]
    fn directory_symlinks_keep_the_selected_path() {
        for location in ["root", "rsrcs"] {
            let fixture = Fixture::new(&format!("symlink-{location}"));
            let real = fixture.0.join("real");
            let selected = fixture.0.join("invoked");
            fs::create_dir_all(real.join("rsrcs")).unwrap();
            if location == "root" {
                symlink(&real, &selected).unwrap();
            } else {
                fs::create_dir(&selected).unwrap();
                symlink(real.join("rsrcs"), selected.join("rsrcs")).unwrap();
            }
            assert_eq!(fixture.discover().unwrap(), Some(selected));
        }
    }

    #[test]
    fn malformed_resources_never_fall_through() {
        for index in 0..3 {
            for kind in ["file", "dangling-symlink"] {
                let fixture = Fixture::new(&format!("malformed-{index}-{kind}"));
                let locations = fixture.locations();
                fs::create_dir_all(&locations[index]).unwrap();
                if let Some(fallback) = locations.get(index + 1) {
                    fs::create_dir_all(fallback.join("rsrcs")).unwrap();
                }
                let resources = locations[index].join("rsrcs");
                let expected = if kind == "file" {
                    fs::write(&resources, b"not a directory").unwrap();
                    io::ErrorKind::NotADirectory
                } else {
                    symlink("missing-target", &resources).unwrap();
                    io::ErrorKind::NotFound
                };
                let error = fixture.discover().unwrap_err();
                assert_eq!(error.kind(), expected);
                assert!(error.to_string().contains(resources.to_str().unwrap()));
            }
        }
    }

    #[test]
    fn malformed_install_root_is_not_absence() {
        for kind in ["file", "dangling-symlink"] {
            let fixture = Fixture::new(&format!("malformed-root-{kind}"));
            let root = fixture.0.join("target/install_pkg");
            let expected = if kind == "file" {
                fs::write(&root, b"not a directory").unwrap();
                io::ErrorKind::NotADirectory
            } else {
                symlink("missing-target", &root).unwrap();
                io::ErrorKind::NotFound
            };
            assert_eq!(fixture.discover().unwrap_err().kind(), expected);
        }
    }

    #[test]
    fn real_permission_errors_propagate_at_every_default_root() {
        for index in 0..3 {
            let fixture = Fixture::new(&format!("permission-{index}"));
            let locations = fixture.locations();
            fs::create_dir_all(locations[index].join("rsrcs")).unwrap();
            if let Some(fallback) = locations.get(index + 1) {
                fs::create_dir_all(fallback.join("rsrcs")).unwrap();
            }
            let _restore = RestorePermissions::deny_search(&locations[index]);
            let error = fixture.discover().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(
                error
                    .to_string()
                    .contains(locations[index].to_str().unwrap())
            );
        }
    }

    #[test]
    fn earlier_valid_root_does_not_inspect_later_invalid_root() {
        let fixture = Fixture::new("priority");
        fs::create_dir_all(fixture.0.join("invoked/rsrcs")).unwrap();
        fs::write(fixture.0.join("target/install_pkg"), b"not a directory").unwrap();
        assert_eq!(fixture.discover().unwrap(), Some(fixture.0.join("invoked")));
    }

    #[test]
    fn explicit_absence_keeps_priority_but_malformed_root_refuses() {
        let fixture = Fixture::new("explicit-contract");
        let selected = fixture.0.join("explicit");
        let discover = || {
            discover_install_dir_from(
                Some(selected.as_os_str()),
                None,
                &fixture.0.join("target/release/hermit"),
                &fixture.0,
            )
        };
        fs::create_dir_all(fixture.0.join("target/install_pkg/rsrcs")).unwrap();
        assert_eq!(discover().unwrap(), Some(selected.clone()));
        fs::write(&selected, b"not a directory").unwrap();
        assert_eq!(discover().unwrap_err().kind(), io::ErrorKind::NotADirectory);
    }

    fn test_directory(name: &str) -> PathBuf {
        let directory =
            env::temp_dir().join(format!("hermit-resources-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn explicit_install_directory_has_priority() {
        let root = test_directory("explicit");
        let discovered = discover_install_dir_from(
            Some(root.as_os_str()),
            None,
            Path::new("/tmp/target/release/hermit"),
            Path::new("/tmp"),
        )
        .unwrap();
        assert_eq!(discovered.as_deref(), Some(root.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invoked_symlink_location_finds_colocated_resources() {
        let root = test_directory("argv0");
        std::fs::create_dir(root.join("rsrcs")).unwrap();
        let invoked = root.join("hermit");
        let discovered = discover_install_dir_from(
            None,
            Some(invoked.as_os_str()),
            Path::new("/tmp/target/release/hermit"),
            Path::new("/tmp"),
        )
        .unwrap();
        assert_eq!(discovered.as_deref(), Some(root.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn release_binary_finds_target_install_package() {
        let root = test_directory("build-tree");
        let install = root.join("install_pkg");
        std::fs::create_dir_all(install.join("rsrcs")).unwrap();
        let executable = root.join("release/hermit");
        let discovered = discover_install_dir_from(None, None, &executable, &root).unwrap();
        assert_eq!(discovered.as_deref(), Some(install.as_path()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_explicit_directory_is_rejected() {
        let error = discover_install_dir_from(
            Some(OsStr::new("")),
            None,
            Path::new("/tmp/hermit"),
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
