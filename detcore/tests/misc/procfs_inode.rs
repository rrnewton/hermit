/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Procfs metadata the guest sees does not depend on the host's procfs.
//!
//! Linux numbers a `/proc/<pid>/fd/<n>` entry from a host-wide counter when it
//! builds the entry's dentry, and builds it again under a new number once the
//! dentry is gone. Host memory pressure can evict it at any time, so a second
//! listing of `/proc/<pid>/fd` showed new numbers in one `--verify` run and
//! the old ones in the other (compat/lsof). Here the guest forces the rebuild:
//! it closes a descriptor, looks up the descriptor's entry, which fails and
//! drops the dentry, and opens a file at the same number again.
//!
//! Detcore names an entry by its path within procfs, which it reads back from
//! a descriptor the guest holds or opens for it, so the guest may reach the
//! entry as `/proc/<pid>`, `/proc/self` or `/proc/thread-self`. When it
//! cannot name the entry it records a determinism loss, so verification
//! refuses to compare the run rather than trusting host numbering.

use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

fn config() -> Config {
    Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    }
}

/// Run `guest` under Detcore and require that it exits 0.
fn under_detcore(guest: impl FnOnce() + Send) {
    let (output, _) =
        detcore_testutils::test_fn_with_config::<Detcore, _>(guest, config(), true).unwrap();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// This process's procfs directory, by PID rather than through `self`.
fn proc_dir() -> String {
    format!("/proc/{}", std::process::id())
}

/// The inode number `getdents64` lists for `name` in `directory`.
fn listed(directory: &str, name: &str) -> u64 {
    let directory = File::open(directory).unwrap();
    let mut buffer = vec![0u8; 32 * 1024];
    let mut found = None;
    loop {
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        assert!(read >= 0, "getdents64: {}", std::io::Error::last_os_error());
        if read == 0 {
            break;
        }
        let mut offset = 0;
        while offset < read as usize {
            let record = &buffer[offset..];
            let ino = u64::from_ne_bytes(record[0..8].try_into().unwrap());
            let length = usize::from(u16::from_ne_bytes(record[16..18].try_into().unwrap()));
            let entry = &record[19..length];
            let entry = &entry[..entry.iter().position(|byte| *byte == 0).unwrap()];
            if entry == name.as_bytes() {
                found = Some(ino);
            }
            offset += length;
        }
    }
    found.unwrap_or_else(|| panic!("{directory:?} lists no {name}"))
}

/// The inode number and link count `statx` reports for `path` relative to
/// `dirfd`.
fn statx(dirfd: i32, path: &str, flags: i32) -> std::io::Result<(u64, u32)> {
    let path = CString::new(path).unwrap();
    let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let mask = libc::STATX_INO | libc::STATX_NLINK;
    if unsafe { libc::statx(dirfd, path.as_ptr(), flags, mask, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let buf = unsafe { buf.assume_init() };
    Ok((buf.stx_ino, buf.stx_nlink))
}

/// The inode numbers of one entry of the descriptor directory `fds` before
/// and after it is rebuilt: as `getdents64` lists it, and as `lstat`,
/// `fstatat` relative to the directory and `statx` report it, which see the
/// rebuilt entry first.
#[derive(Debug)]
struct Sightings {
    listed_before: u64,
    stat_before: u64,
    stat_after: [u64; 3],
    listed_after: u64,
}

fn rebuild_a_descriptor_entry(fds: &str) -> Sightings {
    let fd = File::open("/dev/null").unwrap().into_raw_fd();
    let name = fd.to_string();
    let entry = format!("{fds}/{name}");
    let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
    let listed_before = listed(fds, &name);
    let stat_before = lstat().unwrap();
    assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
    // The lookup of a closed descriptor's entry fails and drops its dentry.
    assert!(lstat().is_err(), "{entry} exists after close");
    let again = File::open("/dev/null").unwrap().into_raw_fd();
    assert_eq!(again, fd, "the reopened file must take the closed number");
    let directory = File::open(fds).unwrap();
    let nofollow = libc::AT_SYMLINK_NOFOLLOW;
    let stat_after = [
        lstat().unwrap(),
        statx(directory.as_raw_fd(), &name, nofollow).unwrap().0,
        statx(libc::AT_FDCWD, &entry, nofollow).unwrap().0,
    ];
    let listed_after = listed(fds, &name);
    assert_eq!(unsafe { libc::close(again) }, 0, "close({again})");
    Sightings {
        listed_before,
        stat_before,
        stat_after,
        listed_after,
    }
}

/// Rebuilds an entry of each descriptor directory `fds` names, natively and
/// then under Detcore, and requires that the guest saw each keep its number.
fn rebuilt_entries_keep_their_numbers(fds: fn() -> Vec<String>) {
    // Natively the rebuilt entry gets a new number. Were the host to keep the
    // dentry, the guest below would keep its number at any Detcore revision.
    for fds in fds() {
        let native = rebuild_a_descriptor_entry(&fds);
        assert_ne!(
            native.listed_before, native.listed_after,
            "the host did not rebuild the entry in {fds}, so this test cannot tell: {native:?}"
        );
    }
    under_detcore(move || {
        for fds in fds() {
            let seen = rebuild_a_descriptor_entry(&fds);
            let number = seen.listed_before;
            assert!(
                seen.stat_before == number
                    && seen.stat_after.iter().all(|stat| *stat == number)
                    && seen.listed_after == number,
                "the guest saw the rebuilt entry in {fds} renumbered: {seen:?}"
            );
        }
    });
}

#[test]
fn a_rebuilt_proc_fd_entry_keeps_its_number() {
    rebuilt_entries_keep_their_numbers(|| vec![format!("{}/fd", proc_dir())]);
}

/// `/proc/self` and `/proc/thread-self` name the guest, not the tracer, when
/// Detcore names the entry a guest reached through them. The thread's own
/// `fd` directory is a second directory, whose entries are other inodes.
#[test]
fn a_rebuilt_entry_reached_through_self_keeps_its_number() {
    rebuilt_entries_keep_their_numbers(|| {
        vec![
            "/proc/self/fd".to_owned(),
            "/proc/thread-self/fd".to_owned(),
        ]
    });
}

/// Detcore names an entry the guest reached by path by opening the path
/// again after the stat. A stat whose result buffer holds its own path has
/// overwritten the path by then (Linux reads the path before it writes the
/// result), so Detcore cannot name the entry. It records a determinism loss
/// instead of passing host numbering on silently, and the stat still
/// succeeds. Named by a later stat, the entry keeps the number it was given
/// unnamed, and keeps it when the host rebuilds the entry.
#[test]
fn an_entry_detcore_cannot_name_records_a_determinism_loss() {
    under_detcore(|| {
        let fd = File::open("/dev/null").unwrap().into_raw_fd();
        let entry = format!("/proc/self/fd/{fd}");
        let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
        let path = CString::new(entry.clone()).unwrap();
        let path = path.as_bytes_with_nul();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        assert!(path.len() <= std::mem::size_of::<libc::stat>());
        let buffer = stat.as_mut_ptr();
        unsafe { std::ptr::copy_nonoverlapping(path.as_ptr(), buffer.cast(), path.len()) };
        let flags = libc::AT_SYMLINK_NOFOLLOW;
        let result =
            unsafe { libc::syscall(libc::SYS_newfstatat, libc::AT_FDCWD, buffer, buffer, flags) };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        let unnamed = unsafe { stat.assume_init() }.st_ino;
        assert_eq!(lstat().unwrap(), unnamed, "named, the entry was renumbered");
        assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
        assert!(lstat().is_err(), "{entry} exists after close");
        let again = File::open("/dev/null").unwrap().into_raw_fd();
        assert_eq!(again, fd, "the reopened file must take the closed number");
        assert_eq!(
            lstat().unwrap(),
            unnamed,
            "the rebuilt entry was renumbered"
        );
    });
    let loss = detcore::detlog::determinism_loss();
    assert!(
        loss.as_deref()
            .is_some_and(|loss| loss.starts_with("procfs inode numbers: ")),
        "no procfs determinism loss was recorded: {loss:?}"
    );
}

/// `tcp` listed in `/proc/<pid>/net` and reached as
/// `/proc/<pid>/task/<pid>/net/tcp` is one host inode.
fn net_tcp_numbers() -> (u64, u64) {
    let pid = std::process::id();
    let listed = listed(&format!("/proc/{pid}/net"), "tcp");
    let alias = format!("/proc/{pid}/task/{pid}/net/tcp");
    (listed, std::fs::metadata(alias).unwrap().ino())
}

#[test]
fn a_procfs_entry_reached_by_two_paths_keeps_one_number() {
    let (listed, alias) = net_tcp_numbers();
    assert_eq!(listed, alias, "natively one inode");
    under_detcore(|| {
        let (listed, alias) = net_tcp_numbers();
        assert_eq!(listed, alias, "the guest saw two numbers for one entry");
    });
}

/// The link counts of `/proc` as `stat`, `fstat` and `statx` report them.
fn proc_root_link_counts() -> [u64; 3] {
    let root = File::open("/proc").unwrap();
    [
        std::fs::metadata("/proc").unwrap().nlink(),
        root.metadata().unwrap().nlink(),
        u64::from(statx(libc::AT_FDCWD, "/proc", 0).unwrap().1),
    ]
}

#[test]
fn the_proc_root_link_count_does_not_count_host_processes() {
    // Linux counts every process in the procfs's PID namespace.
    let native = proc_root_link_counts();
    assert!(native.iter().all(|count| *count > 2), "{native:?}");
    under_detcore(|| assert_eq!(proc_root_link_counts(), [1, 1, 1]));
}

/// The link count `statx(AT_FDCWD, path, AT_EMPTY_PATH)` reports for the
/// current directory, where `path` is empty or NULL.
fn cwd_link_count(path: Option<&CStr>) -> std::io::Result<u32> {
    let path = path.map_or(std::ptr::null(), CStr::as_ptr);
    let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let (flags, mask) = (libc::AT_EMPTY_PATH, libc::STATX_NLINK);
    if unsafe { libc::statx(libc::AT_FDCWD, path, flags, mask, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { buf.assume_init() }.stx_nlink)
}

/// `statx` of the current directory, by an empty path or (from Linux 6.11)
/// a NULL one, reports `/proc`'s link count as `stat` of `/proc` does.
#[test]
fn the_proc_root_link_count_through_the_cwd_does_not_count_host_processes() {
    under_detcore(|| {
        std::env::set_current_dir("/proc").unwrap();
        let empty = CString::new("").unwrap();
        assert_eq!(cwd_link_count(Some(&empty)).unwrap(), 1, "an empty path");
        match cwd_link_count(None) {
            // Linux before 6.11 refuses a NULL path.
            Err(error) if error.raw_os_error() == Some(libc::EFAULT) => {}
            count => assert_eq!(count.unwrap(), 1, "a NULL path"),
        }
    });
}

#[test]
fn a_write_to_a_procfs_file_moves_its_mtime() {
    under_detcore(|| {
        let mut comm = std::fs::OpenOptions::new()
            .write(true)
            .open(format!("{}/comm", proc_dir()))
            .unwrap();
        let mtime = |file: &File| {
            let metadata = file.metadata().unwrap();
            (metadata.mtime(), metadata.mtime_nsec())
        };
        let before = mtime(&comm);
        std::thread::sleep(Duration::from_millis(10));
        comm.write_all(b"procfs-inode").unwrap();
        assert_ne!(mtime(&comm), before, "the write left the mtime unchanged");
    });
}
