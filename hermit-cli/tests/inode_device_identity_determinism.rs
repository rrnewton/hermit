/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Regression test for https://github.com/rrnewton/hermit/issues/2897.
//!
//! Hermit used to number deterministic inodes from one counter shared by every
//! filesystem, in first-observation order. A host file replaced while the guest
//! ran therefore shifted every inode minted after it on every filesystem. In the
//! compat workloads `chef` renamed a fresh `/etc/ld.so.cache` into place between
//! the two runs of `--verify`; the second run minted one more host inode, so the
//! guest's tmpfs work directory was inode 22 instead of 21 and `getdents64` of
//! that empty directory differed.
//!
//! This test reproduces that without waiting for `chef`. The same guest program
//! runs twice over a host directory holding `a` and `b`. In one run they are
//! hard links to one host inode; in the other they are two host files, which is
//! what a replacement looks like to the inode pool. The guest stats both names
//! and then reports the inodes of a directory it creates on its own tmpfs, both
//! through `stat` and through `getdents64` (`ls -ai`). Those tmpfs inodes must not
//! depend on how many host inodes a different filesystem holds.
//!
//! Keying on `(st_dev, st_ino)` must not split one file in two where
//! `/proc/<pid>/maps` names a different device than `stat` does (btrfs reports
//! `00:20` in maps and `0x21` from `stat` on the development host). The kernel
//! maps the shell's own executable at execve and nothing `stat`s it, so reading
//! the shell's maps before `stat`ing `/proc/<pid>/exe` resolves the maps
//! identity first; the second test checks that both orders agree.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

const GUEST_SCRIPT: &str = r#"
set -eu
stat -c '%n' "$1/a" "$1/b" > /dev/null
mkdir /test/d
printf 'stat:%s\n' "$(stat -c %i /test/d)"
ls -ai /test/d
"#;

fn host_directory(name: &str, hard_linked: bool) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("inode-device-identity-{}", std::process::id()))
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("failed to create the host directory");
    fs::write(dir.join("a"), b"same contents\n").expect("failed to write a");
    if hard_linked {
        fs::hard_link(dir.join("a"), dir.join("b")).expect("failed to link b to a");
    } else {
        fs::write(dir.join("b"), b"same contents\n").expect("failed to write b");
    }
    dir
}

/// The shell's executable inode as its maps header reports it and as `stat`
/// reports it, in the order the arguments name.
const EXECUTABLE_INODE_SCRIPT: &str = r#"
set -eu
exe=$(readlink /proc/$$/exe)
maps() { awk -v exe="$exe" '$6 == exe { print $5; exit }' /proc/$$/maps; }
exe_stat() { stat -L -c %i /proc/$$/exe; }
for source in "$@"; do
    case "$source" in
        maps) printf 'maps:%s\n' "$(maps)" ;;
        stat) printf 'stat:%s\n' "$(exe_stat)" ;;
    esac
done
"#;

fn run_guest(script: &str, extra_options: &[&str], args: &[&std::ffi::OsStr]) -> String {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args(["run", "--base-env=minimal"]);
    command.args(extra_options);
    command.args(["--", "/bin/sh", "-c", script, "sh"]);
    command.args(args);
    hermit_test::configure_guest_execution(&mut command);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("failed to start {rendered}: {error}"));
    assert!(
        output.status.success(),
        "{rendered} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).expect("guest output should be UTF-8")
}

fn guest_tmpfs_inodes(host: &Path) -> String {
    let stdout = run_guest(
        GUEST_SCRIPT,
        &["--mount=type=tmpfs,target=/test", "--workdir=/test"],
        &[host.as_os_str()],
    );
    assert!(
        stdout.starts_with("stat:") && stdout.contains(" .\n") && stdout.contains(" ..\n"),
        "the guest omitted its stat or getdents64 output:\n{stdout}"
    );
    stdout
}

#[test]
fn tmpfs_inodes_do_not_depend_on_host_inodes_of_another_filesystem() {
    let linked = guest_tmpfs_inodes(&host_directory("linked", true));
    let separate = guest_tmpfs_inodes(&host_directory("separate", false));
    assert_eq!(
        linked, separate,
        "the guest's tmpfs inodes changed with the number of host inodes on a \
         different filesystem (https://github.com/rrnewton/hermit/issues/2897)"
    );
}

/// Returns (maps inode, stat inode) of the shell's executable, read in `order`.
fn executable_inodes(order: [&str; 2]) -> (String, String) {
    let args: Vec<&std::ffi::OsStr> = order.iter().map(std::ffi::OsStr::new).collect();
    let stdout = run_guest(EXECUTABLE_INODE_SCRIPT, &[], &args);
    let field = |source: &str| {
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{source}:")))
            .filter(|inode| !inode.is_empty())
            .unwrap_or_else(|| panic!("the guest printed no {source} inode:\n{stdout}"))
            .to_owned()
    };
    (field("maps"), field("stat"))
}

#[test]
fn maps_and_stat_agree_on_the_executable_inode_in_either_order() {
    let (maps_first_maps, maps_first_stat) = executable_inodes(["maps", "stat"]);
    assert_eq!(
        maps_first_maps, maps_first_stat,
        "reading /proc/<pid>/maps before stat gave the shell's executable two inodes"
    );
    let (stat_first_maps, stat_first_stat) = executable_inodes(["stat", "maps"]);
    assert_eq!(
        stat_first_maps, stat_first_stat,
        "stat before /proc/<pid>/maps gave the shell's executable two inodes"
    );
}
