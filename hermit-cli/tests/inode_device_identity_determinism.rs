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
//!
//! Hermit gives the guest's inherited stdio descriptors a placeholder host
//! identity: the tracer's own fd-0 `fstat`. A write to stdout used to bump the
//! virtual mtime of that identity, which minted a deterministic inode on the
//! device holding hermit's stdin. With one counter per device that shifted
//! every later inode on that device, so the guest's inode numbers depended on
//! where the person running hermit redirected stdin from. The third test runs
//! the same guest with byte-identical stdin from two filesystems.

#[path = "common/hermit_binary.rs"]
mod hermit_test;

use std::fs;
use std::os::unix::fs::MetadataExt;
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

/// Per-test scratch directory under the Cargo target tmpdir, removed on drop.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(test: &str) -> Self {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "inode-device-identity-{}-{test}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("failed to create the scratch directory");
        ScratchDir(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn host_directory(scratch: &ScratchDir, name: &str, hard_linked: bool) -> PathBuf {
    let dir = scratch.0.join(name);
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
    run_guest_with_stdin(script, extra_options, args, None)
}

fn run_guest_with_stdin(
    script: &str,
    extra_options: &[&str],
    args: &[&std::ffi::OsStr],
    stdin: Option<&Path>,
) -> String {
    let mut command = Command::new(hermit_test::hermit_binary());
    command.args(["run", "--base-env=minimal"]);
    command.args(extra_options);
    command.args(["--", "/bin/sh", "-c", script, "sh"]);
    command.args(args);
    hermit_test::configure_guest_execution(&mut command);
    // After `configure_guest_execution`, which may rebuild the command.
    if let Some(stdin) = stdin {
        let file = fs::File::open(stdin)
            .unwrap_or_else(|error| panic!("failed to open stdin {}: {error}", stdin.display()));
        command.stdin(file);
    }
    let rendered = format!("{command:?} < {stdin:?}");
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
    let scratch = ScratchDir::new("tmpfs");
    let linked = guest_tmpfs_inodes(&host_directory(&scratch, "linked", true));
    let separate = guest_tmpfs_inodes(&host_directory(&scratch, "separate", false));
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

/// The host's own (maps device column, `stat` device) of the shell's
/// executable, read natively, both as `major:minor` in lowercase hex. When they
/// are equal (ext4, xfs, tmpfs) the test below passes without exercising the
/// cross-device pairing it exists for; btrfs and overlayfs make them differ.
fn host_executable_devices() -> (String, String) {
    const SCRIPT: &str = r#"
set -eu
exe=$(readlink /proc/$$/exe)
awk -v exe="$exe" '$6 == exe { print $4; exit }' /proc/$$/maps
printf '%s\n' "$exe"
"#;
    let output = Command::new("/bin/sh")
        .args(["-c", SCRIPT])
        .output()
        .expect("failed to run /bin/sh natively");
    assert!(
        output.status.success(),
        "native device probe failed: {output:?}"
    );
    let stdout = String::from_utf8(output.stdout).expect("native probe output should be UTF-8");
    let mut lines = stdout.lines();
    let maps = lines.next().unwrap_or_default().to_owned();
    let exe = lines.next().unwrap_or_default();
    assert!(
        !maps.is_empty() && !exe.is_empty(),
        "native device probe printed no maps device or executable:\n{stdout}"
    );
    let dev = fs::metadata(exe)
        .unwrap_or_else(|error| panic!("failed to stat {exe}: {error}"))
        .dev();
    let stat = format!("{:02x}:{:02x}", libc::major(dev), libc::minor(dev));
    (maps, stat)
}

#[test]
fn maps_and_stat_agree_on_the_executable_inode_in_either_order() {
    let (maps_dev, stat_dev) = host_executable_devices();
    let case = if maps_dev == stat_dev {
        format!(
            "host maps device {maps_dev} EQUALS stat device {stat_dev}: cross-device pairing \
             NOT exercised on this host"
        )
    } else {
        format!(
            "host maps device {maps_dev} differs from stat device {stat_dev}: cross-device \
             pairing exercised"
        )
    };
    eprintln!("{case}");
    let (maps_first_maps, maps_first_stat) = executable_inodes(["maps", "stat"]);
    assert_eq!(
        maps_first_maps, maps_first_stat,
        "reading /proc/<pid>/maps before stat gave the shell's executable two inodes ({case})"
    );
    let (stat_first_maps, stat_first_stat) = executable_inodes(["stat", "maps"]);
    assert_eq!(
        stat_first_maps, stat_first_stat,
        "stat before /proc/<pid>/maps gave the shell's executable two inodes ({case})"
    );
}

/// Writes to stdout, then stats root-filesystem files.
const STDIO_THEN_STAT_SCRIPT: &str = r#"
set -eu
echo written-to-stdout
stat -c '%i %n' "$@"
"#;

/// A directory on a filesystem other than `root_dev`, for a copy of stdin.
fn directory_on_another_filesystem(root_dev: u64, scratch: &ScratchDir) -> PathBuf {
    let candidates = [PathBuf::from("/dev/shm"), scratch.0.clone()];
    candidates
        .iter()
        .find(|dir| {
            fs::metadata(dir).is_ok_and(|meta| meta.is_dir() && meta.dev() != root_dev)
                && tempfile_probe(dir)
        })
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "none of {candidates:?} is a writable directory on a filesystem other than \
                 device {root_dev:#x}; this test needs two filesystems"
            )
        })
}

fn tempfile_probe(dir: &Path) -> bool {
    let probe = dir.join(format!(
        ".inode-device-identity-probe-{}",
        std::process::id()
    ));
    let ok = fs::write(&probe, b"").is_ok();
    let _ = fs::remove_file(&probe);
    ok
}

/// Removes a file on drop.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn guest_inodes_do_not_depend_on_where_hermit_stdin_comes_from() {
    // Stdin from a root-filesystem file, and the stat targets on that same
    // device, so a mint caused by stdin would shift them.
    let root_stdin = Path::new("/etc/passwd");
    let targets = [
        Path::new("/etc"),
        Path::new("/etc/group"),
        Path::new("/bin/sh"),
    ];
    let root_dev = fs::metadata(root_stdin)
        .expect("/etc/passwd must exist")
        .dev();
    for target in targets {
        let dev = fs::metadata(target)
            .unwrap_or_else(|error| panic!("{} must exist: {error}", target.display()))
            .dev();
        assert_eq!(
            dev,
            root_dev,
            "{} is not on the device of {}; the test premise does not hold",
            target.display(),
            root_stdin.display()
        );
    }

    let scratch = ScratchDir::new("stdin");
    let other_dir = directory_on_another_filesystem(root_dev, &scratch);
    let other_stdin = RemoveOnDrop(other_dir.join(format!(
        "inode-device-identity-stdin-{}",
        std::process::id()
    )));
    fs::copy(root_stdin, &other_stdin.0).expect("failed to copy stdin to the other filesystem");
    assert_eq!(
        fs::read(root_stdin).unwrap(),
        fs::read(&other_stdin.0).unwrap(),
        "the two stdin files must be byte-identical"
    );

    let args: Vec<&std::ffi::OsStr> = targets.iter().map(|path| path.as_os_str()).collect();
    let from_root = run_guest_with_stdin(STDIO_THEN_STAT_SCRIPT, &[], &args, Some(root_stdin));
    let from_other = run_guest_with_stdin(STDIO_THEN_STAT_SCRIPT, &[], &args, Some(&other_stdin.0));
    assert!(
        from_root.starts_with("written-to-stdout\n") && from_root.contains(" /etc/group\n"),
        "the guest omitted its output:\n{from_root}"
    );
    assert_eq!(
        from_root,
        from_other,
        "the guest's root-filesystem inodes changed with where hermit's stdin came from \
         (root filesystem {} vs {}, byte-identical contents)",
        root_stdin.display(),
        other_stdin.0.display()
    );
}
