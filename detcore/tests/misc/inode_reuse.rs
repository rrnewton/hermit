/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A host may give a freed inode to a new file (ext4 and XFS do), and whether
//! it does differs between runs, so Detcore retires an inode's number when its
//! file loses its last name and numbers the next file the host gives that
//! inode afresh (https://github.com/rrnewton/hermit/issues/3840).
//!
//! One test makes the host reuse an inode for real: a fresh tmpfs numbers its
//! first file the same as the last one did, and the kernel gives the new mount
//! the freed device number. The others cover the removal paths that retire a
//! number, the removals that must not, and the Linux behaviour that must
//! survive a retirement: an unlinked file reached through a descriptor, or
//! through `/proc/self/fd/N`, keeps its inode number.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirEntryExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

fn ino_of_fd(fd: i32) -> u64 {
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::fstat(fd, &mut stat) }, 0, "fstat({fd})");
    stat.st_ino
}

fn ino_of_path(path: &Path) -> u64 {
    std::fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("lstat {}: {error}", path.display()))
        .ino()
}

fn renameat2_exchange(a: &Path, b: &Path) {
    let a = CString::new(a.as_os_str().as_bytes()).unwrap();
    let b = CString::new(b.as_os_str().as_bytes()).unwrap();
    let rc = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    assert_eq!(rc, 0, "renameat2(RENAME_EXCHANGE)");
}

/// Each removal kind, printing the inode numbers whose number must be retired
/// and those that must not, after checking that every number the guest can
/// still reach stayed put.
fn removals_guest(root: &tempfile::TempDir) {
    let dir = root.path();

    // Open, unlink, keep using the descriptor.
    let unlinked = dir.join("unlinked");
    let file = File::create(&unlinked).unwrap();
    let fd = file.as_raw_fd();
    let number = ino_of_fd(fd);
    assert_eq!(ino_of_path(&unlinked), number);
    std::fs::remove_file(&unlinked).unwrap();
    assert_eq!(ino_of_fd(fd), number, "fstat after unlink");
    let through_proc = Path::new("/proc/self/fd").join(fd.to_string());
    assert_eq!(
        std::fs::metadata(&through_proc).unwrap().ino(),
        number,
        "stat of /proc/self/fd/N after unlink"
    );
    let reopened = File::open(&through_proc).unwrap();
    assert_eq!(
        ino_of_fd(reopened.as_raw_fd()),
        number,
        "fstat of a reopen through /proc/self/fd/N after unlink"
    );

    // Removing one of two links leaves a name.
    let first = dir.join("first-link");
    let second = dir.join("second-link");
    File::create(&first).unwrap();
    std::fs::hard_link(&first, &second).unwrap();
    let linked = ino_of_path(&first);
    std::fs::remove_file(&first).unwrap();
    assert_eq!(ino_of_path(&second), linked, "the remaining link");

    // A directory has one name.
    let directory = dir.join("directory");
    std::fs::create_dir(&directory).unwrap();
    let removed_directory = ino_of_path(&directory);
    std::fs::remove_dir(&directory).unwrap();

    // A rename replaces its target.
    let source = dir.join("source");
    let target = dir.join("target");
    File::create(&source).unwrap();
    File::create(&target).unwrap();
    let moved = ino_of_path(&source);
    let replaced = ino_of_path(&target);
    std::fs::rename(&source, &target).unwrap();
    assert_eq!(ino_of_path(&target), moved, "the renamed file");

    // A rename onto its own file removes nothing: onto the same name, or onto
    // another link of the file (where Linux leaves both names).
    std::fs::rename(&target, &target).unwrap();
    let third = dir.join("third-link");
    std::fs::hard_link(&second, &third).unwrap();
    std::fs::rename(&third, &second).unwrap();
    assert_eq!(ino_of_path(&second), linked, "a rename between two links");

    // An exchange removes nothing.
    let left = dir.join("left");
    let right = dir.join("right");
    File::create(&left).unwrap();
    File::create(&right).unwrap();
    let (left_number, right_number) = (ino_of_path(&left), ino_of_path(&right));
    renameat2_exchange(&left, &right);
    assert_eq!(ino_of_path(&left), right_number);
    assert_eq!(ino_of_path(&right), left_number);

    // Straight to descriptor 1: `println!` in a test thread goes to the
    // harness's capture buffer, which the forked guest cannot hand back.
    let report = format!(
        "retired {number} {removed_directory} {replaced}\n\
         kept {linked} {moved} {left_number} {right_number}\n"
    );
    std::io::Write::write_all(&mut std::io::stdout(), report.as_bytes()).unwrap();
    drop(reopened);
    drop(file);
}

fn numbers(stdout: &str, label: &str) -> BTreeSet<u64> {
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .unwrap_or_else(|| panic!("no {label:?} line in {stdout:?}"));
    line.split_whitespace()
        .map(|number| number.parse().unwrap())
        .collect()
}

#[test]
fn removing_a_last_name_retires_its_number_and_nothing_else() {
    let root = tempfile::tempdir().unwrap();
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let (output, state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        || removals_guest(&root),
        config,
        true,
    )
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let retired: BTreeSet<u64> = state
        .retired_inodes()
        .into_iter()
        .map(|number| number.as_raw())
        .collect();
    for number in numbers(&stdout, "retired ") {
        assert!(
            retired.contains(&number),
            "{number} lost its last name but is not retired: {retired:?}"
        );
    }
    for number in numbers(&stdout, "kept ") {
        assert!(
            !retired.contains(&number),
            "{number} still has a name but is retired: {retired:?}"
        );
    }
}

/// Set in the re-executed test process, which runs as root in a user and mount
/// namespace of its own and so may mount tmpfs.
const REUSE_INNER: &str = "HERMIT_INODE_REUSE_TEST_INNER";

/// `hermit_test_workdir::REQUEST_ENV`. With it set, each guest runs in a fresh
/// mount namespace of its own (on an empty `/test`), where the remounts this
/// test makes from its own thread would never appear; this test uses no
/// `/test`, so the re-executed process runs without it.
const ISOLATED_WORKDIR_ENV: &str = "HERMIT_E2E_EMPTY_WORKDIR";

/// Reuse cycles per run. The host reuses the device number only when nothing
/// else takes it between the unmount and the mount, so one cycle could miss.
const REUSE_CYCLES: usize = 4;

/// How the guest first reaches the file the host gave the freed inode, which
/// is also what discards the retired number.
#[derive(Clone, Copy, Debug)]
enum FirstReach {
    /// `open(O_CREAT)` discards it, and an `fstat` on that descriptor numbers
    /// the file.
    CreatingDescriptor,
    /// The directory listing that names the new directory.
    DirectoryListing,
    /// A stat of the new directory's path.
    PathStat,
}

/// Mount a fresh tmpfs at `mount_point`, replacing the one there, if any.
fn mount_fresh_tmpfs(mount_point: &Path, replace: bool) {
    let target = CString::new(mount_point.as_os_str().as_bytes()).unwrap();
    if replace {
        let rc = unsafe { libc::umount2(target.as_ptr(), 0) };
        assert_eq!(rc, 0, "umount: {}", std::io::Error::last_os_error());
    }
    let tmpfs = CString::new("tmpfs").unwrap();
    let rc = unsafe {
        libc::mount(
            tmpfs.as_ptr(),
            target.as_ptr(),
            tmpfs.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    assert_eq!(rc, 0, "mount tmpfs: {}", std::io::Error::last_os_error());
}

/// Ask the test process to replace the tmpfs, and wait until it has. The
/// guest opens both FIFOs itself: Detcore refuses I/O on a descriptor it never
/// saw opened.
fn remount(request: i32, done: i32) {
    assert_eq!(unsafe { libc::write(request, b"r".as_ptr().cast(), 1) }, 1);
    let mut byte = 0_u8;
    assert_eq!(
        unsafe { libc::read(done, (&raw mut byte).cast(), 1) },
        1,
        "remount acknowledgement"
    );
}

/// Each cycle numbers the first entry of a fresh tmpfs, removes its last name,
/// has the tmpfs replaced, and reaches the new tmpfs's first entry, which the
/// host gives the same device and inode number.
fn reuse_guest(mount_point: &Path, fifos: &Path, reach: FirstReach) {
    // The test process holds both FIFOs open for reading and writing, so
    // neither open waits for a peer.
    let request = File::options()
        .write(true)
        .open(fifos.join("request"))
        .unwrap();
    let done = File::open(fifos.join("done")).unwrap();
    let (request, done) = (request.as_raw_fd(), done.as_raw_fd());
    for cycle in 0..REUSE_CYCLES {
        remount(request, done);
        let old = mount_point.join(format!("old-{cycle}"));
        let new = mount_point.join(format!("new-{cycle}"));
        let old_number = match reach {
            FirstReach::CreatingDescriptor => {
                drop(File::create(&old).unwrap());
                let number = ino_of_path(&old);
                std::fs::remove_file(&old).unwrap();
                number
            }
            FirstReach::DirectoryListing | FirstReach::PathStat => {
                std::fs::create_dir(&old).unwrap();
                let number = ino_of_path(&old);
                std::fs::remove_dir(&old).unwrap();
                number
            }
        };
        remount(request, done);
        let new_number = match reach {
            FirstReach::CreatingDescriptor => {
                let file = File::create(&new).unwrap();
                ino_of_fd(file.as_raw_fd())
            }
            FirstReach::DirectoryListing => {
                std::fs::create_dir(&new).unwrap();
                std::fs::read_dir(mount_point)
                    .unwrap()
                    .map(Result::unwrap)
                    .find(|entry| entry.file_name() == new.file_name().unwrap())
                    .expect("the listing names the new directory")
                    .ino()
            }
            FirstReach::PathStat => {
                std::fs::create_dir(&new).unwrap();
                ino_of_path(&new)
            }
        };
        assert_ne!(
            new_number, old_number,
            "{reach:?}, cycle {cycle}: the new file took the removed file's number"
        );
    }
}

/// Run [`reuse_guest`] under Detcore while this thread replaces the tmpfs on
/// request, and return how many retired numbers the run saw reused.
fn run_reuse_guest(reach: FirstReach) -> u64 {
    let root = tempfile::tempdir().unwrap();
    let mount_point = root.path().join("tmpfs");
    std::fs::create_dir(&mount_point).unwrap();
    mount_fresh_tmpfs(&mount_point, false);
    let fifos = root.path();
    let mut held = Vec::new();
    for name in ["request", "done"] {
        let path = CString::new(fifos.join(name).as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::mkfifo(path.as_ptr(), 0o600) },
            0,
            "mkfifo {name}"
        );
        held.push(
            File::options()
                .read(true)
                .write(true)
                .open(fifos.join(name))
                .unwrap(),
        );
    }
    let (request, done) = (held[0].as_raw_fd(), held[1].as_raw_fd());

    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let (output, state) = std::thread::scope(|scope| {
        let mount_point = &mount_point;
        scope.spawn(move || {
            let mut byte = 0_u8;
            while unsafe { libc::read(request, (&raw mut byte).cast(), 1) } == 1 && byte == b'r' {
                mount_fresh_tmpfs(mount_point, true);
                assert_eq!(unsafe { libc::write(done, b"d".as_ptr().cast(), 1) }, 1);
            }
        });
        let run = detcore_testutils::test_fn_with_config::<Detcore, _>(
            || reuse_guest(mount_point, fifos, reach),
            config,
            true,
        );
        // Ends the remount thread.
        assert_eq!(unsafe { libc::write(request, b"q".as_ptr().cast(), 1) }, 1);
        run.unwrap()
    });
    drop(held);
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "{reach:?}: guest failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let target = CString::new(mount_point.as_os_str().as_bytes()).unwrap();
    unsafe { libc::umount2(target.as_ptr(), 0) };
    state.reused_inode_count()
}

/// The host reuses an inode for real, and the new file must not take the
/// removed file's number, whichever way it is first reached. The test runs
/// itself again as root in a user and mount namespace of its own, which may
/// mount tmpfs on a host that grants no privilege.
#[test]
fn a_reused_inode_gets_a_fresh_number_however_it_is_first_reached() {
    const NAME: &str =
        "inode_reuse::a_reused_inode_gets_a_fresh_number_however_it_is_first_reached";
    if std::env::var_os(REUSE_INNER).is_none() {
        let output = Command::new("unshare")
            .args(["--user", "--map-root-user", "--mount", "--"])
            .arg(std::env::current_exe().unwrap())
            .args([NAME, "--exact", "--nocapture", "--test-threads=1"])
            .env(REUSE_INNER, "1")
            .env_remove(ISOLATED_WORKDIR_ENV)
            .output()
            .expect("run unshare");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "the re-executed test failed: {stdout}{stderr}"
        );
        assert!(
            stdout.contains("inode reuse: checked 3 ways"),
            "the re-executed test did not run: {stdout}{stderr}"
        );
        return;
    }
    assert!(
        std::env::var_os(ISOLATED_WORKDIR_ENV).is_none(),
        "the guest must share this process's mount namespace"
    );
    for reach in [
        FirstReach::CreatingDescriptor,
        FirstReach::DirectoryListing,
        FirstReach::PathStat,
    ] {
        let reused = run_reuse_guest(reach);
        assert!(
            reused > 0,
            "{reach:?}: the host reused no inode in {REUSE_CYCLES} cycles, so nothing was checked"
        );
    }
    writeln!(std::io::stdout(), "inode reuse: checked 3 ways").unwrap();
}
