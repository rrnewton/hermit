/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Building tests/c/fixtures/inode_identity_views.c, starting it, and
//! checking what its modes print, for the backends that run it. Each test
//! binary that includes this module uses a subset of it.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// The soft and hard RLIMIT_NOFILE the proc-fd-links runs start Hermit with.
///
/// Hermit keeps a guest's RLIMIT_NOFILE virtual and does not enforce it on
/// open (<https://github.com/rrnewton/hermit/issues/3388>), so the fixture's
/// descriptor fill ends at the host limit the guest inherited. The default
/// host limit is 2^19 or more, which took six minutes to fill under ptrace;
/// 1024 takes about a second and leaves Hermit ample.
#[allow(dead_code)]
pub const PROC_FD_LINKS_HOST_NOFILE: libc::rlim_t = 1024;

/// Lower the soft and hard RLIMIT_NOFILE of the process `command` starts, and
/// so of a guest Hermit runs in it.
#[allow(dead_code)]
pub fn limit_host_nofile(command: &mut Command, limit: libc::rlim_t) {
    // SAFETY: setrlimit is async-signal-safe and touches no parent state.
    unsafe {
        command.pre_exec(move || {
            let limit = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
}

/// Compile the fixture to a path of its own under the target's temporary
/// directory. `name` must be unique per test: nextest may run a binary's
/// tests concurrently.
#[allow(dead_code)]
pub fn compile_guest(name: &str) -> PathBuf {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hermit-cli should be inside the repository");
    let output =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("inode-identity-views-{name}"));
    let compile = Command::new("cc")
        .args(["-std=c11", "-O2", "-Wall", "-Wextra", "-Werror"])
        .arg(repository.join("tests/c/fixtures/inode_identity_views.c"))
        .arg("-o")
        .arg(&output)
        .output()
        .expect("compile inode identity views guest");
    assert!(
        compile.status.success(),
        "failed to compile inode identity views guest:\n{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    output
}

/// What the scm-getdents mode prints for its three files: `.` and `..` plus
/// the files, through each directory-listing system call the target has.
#[allow(dead_code)]
pub fn scm_getdents_expected_stdout() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "scm getdents64 entries=5 matched=3\nscm getdents entries=5 matched=3\n"
    } else {
        "scm getdents64 entries=5 matched=3\n"
    }
}

/// Whether `path` is on btrfs, where `stat` and `/proc/*/maps` report
/// different devices for the same file.
#[allow(dead_code)]
pub fn is_on_btrfs(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path has no NUL");
    // SAFETY: `statfs` is plain data, for which all-zero bytes are valid.
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated and `stats` is a valid out-pointer.
    let result = unsafe { libc::statfs(path.as_ptr(), &mut stats) };
    assert_eq!(
        result,
        0,
        "statfs failed: {}",
        std::io::Error::last_os_error()
    );
    stats.f_type as u64 == libc::BTRFS_SUPER_MAGIC as u64
}

/// Check the maps-stat mode's summary line, whose `stdin` field must read
/// `stdin`, and refuse a btrfs host where no mapping reported two devices:
/// there the test would pass without having exercised the case it exists for.
///
/// `unlinked_directory` is the DIR argument the guest was given, if any. With
/// it, both files the guest mapped and then unlinked there must have been
/// checked, and on btrfs both must have reported two devices, for the same
/// reason. Without it, none may have been.
#[allow(dead_code)]
pub fn assert_maps_stat_summary(
    summary: &str,
    guest: &Path,
    stdin: &str,
    unlinked_directory: Option<&Path>,
) {
    let field = |name: &str| {
        summary
            .split_whitespace()
            .find_map(|field| field.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("maps-stat output has no {name} field: {summary}"))
    };
    let count = |name: &str| -> usize {
        field(name)
            .parse()
            .unwrap_or_else(|error| panic!("{name} is not decimal ({error}): {summary}"))
    };
    assert!(summary.starts_with("maps-stat "), "{summary}");
    let checked = count("checked");
    let split = count("split");
    let unlinked = count("unlinked");
    let unlinked_split = count("unlinked_split");
    assert!(checked > 0, "no file-backed mapping was checked: {summary}");
    assert_eq!(field("exe"), "agrees", "{summary}");
    assert_eq!(field("stdin"), stdin, "{summary}");
    if is_on_btrfs(guest) {
        assert!(
            split > 0,
            "the guest binary {} is on btrfs, where maps reports the superblock device and stat \
             the subvolume device, yet no mapping reported two devices: {summary}",
            guest.display()
        );
    }
    match unlinked_directory {
        None => assert_eq!(unlinked, 0, "{summary}"),
        Some(directory) => {
            assert_eq!(
                unlinked,
                2,
                "both files mapped and unlinked in {} must be checked: {summary}",
                directory.display()
            );
            if is_on_btrfs(directory) {
                assert_eq!(
                    unlinked_split,
                    unlinked,
                    "{} is on btrfs, where maps reports the superblock device and stat the \
                     subvolume device, yet an unlinked mapping did not report two devices: \
                     {summary}",
                    directory.display()
                );
            }
        }
    }
}
