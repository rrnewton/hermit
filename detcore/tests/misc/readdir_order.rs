/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Directory enumeration order must not depend on the host filesystem.
//!
//! Linux returns entries in filesystem order -- creation order on tmpfs, a
//! per-filesystem-seeded name hash on ext4 -- and glibc reads them 32KiB at a
//! time. Sorting each of those buffers separately still left a directory
//! larger than one buffer as sorted runs whose boundaries depend on the host.
//! These guests create directories far larger than one buffer, in different
//! creation orders, and require one global order, stable offsets, and the
//! POSIX `seekdir`/`rewinddir` behaviour built on them.

use std::collections::BTreeSet;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

const RUNS: usize = 5;

/// Far more than one 32KiB glibc buffer holds: each name below takes a 32-byte
/// `linux_dirent64` record, so 3000 entries are about 94KiB.
const ENTRIES: usize = 3000;

fn run_five_times(guest: fn()) {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let mut expected = None;

    for run in 1..=RUNS {
        let (output, _state) =
            detcore_testutils::test_fn_with_config::<Detcore, _>(guest, config.clone(), true)
                .unwrap_or_else(|error| panic!("readdir guest run {run} failed: {error:#}"));
        assert_eq!(
            output.status,
            ExitStatus::Exited(0),
            "guest run {run} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !output.stdout.is_empty(),
            "guest run {run} printed no listing digest"
        );
        if let Some(expected) = &expected {
            assert_eq!(
                &output.stdout, expected,
                "directory listing diverged on run {run}"
            );
        } else {
            expected = Some(output.stdout);
        }
    }
}

fn name(index: usize) -> String {
    format!("entry-{index:06}")
}

/// Every name, in the order `readdir` must return them after `.` and `..`.
fn sorted_names() -> Vec<String> {
    (0..ENTRIES).map(name).collect()
}

/// Create the directory's entries in `order`.
fn populate(dir: &Path, order: impl Iterator<Item = usize>) {
    for index in order {
        File::create(dir.join(name(index))).unwrap();
    }
}

/// A creation order that is neither sorted nor reverse-sorted: 1297 is prime
/// and does not divide `ENTRIES`, so this visits every index once.
fn scrambled() -> impl Iterator<Item = usize> {
    (0..ENTRIES).map(|step| (step * 1297) % ENTRIES)
}

struct Listing {
    names: Vec<String>,
    offsets: Vec<i64>,
    digest: u64,
}

/// Read a whole directory through glibc's `opendir`/`readdir`.
fn read_listing(dir: *mut libc::DIR) -> Listing {
    let mut listing = Listing {
        names: Vec::new(),
        offsets: Vec::new(),
        digest: 0xcbf2_9ce4_8422_2325,
    };
    loop {
        let entry = unsafe { libc::readdir64(dir) };
        if entry.is_null() {
            break;
        }
        let entry = unsafe { &*entry };
        let name = unsafe { CStr::from_ptr(entry.d_name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned();
        for byte in name
            .bytes()
            .chain(entry.d_ino.to_ne_bytes())
            .chain(entry.d_off.to_ne_bytes())
            .chain([entry.d_type])
        {
            listing.digest = (listing.digest ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
        listing.names.push(name);
        listing.offsets.push(entry.d_off);
    }
    listing
}

fn open_dir(path: &Path) -> *mut libc::DIR {
    let path = CString::new(path.as_os_str().as_bytes()).unwrap();
    let dir = unsafe { libc::opendir(path.as_ptr()) };
    assert!(!dir.is_null(), "opendir failed");
    dir
}

fn assert_whole_directory_sorted(listing: &Listing, expected: &[String]) {
    assert_eq!(listing.names[0], ".");
    assert_eq!(listing.names[1], "..");
    assert!(
        listing.names[2..] == *expected,
        "directory not returned in one global order: {} entries, first mismatch at {:?}",
        listing.names.len() - 2,
        listing.names[2..]
            .iter()
            .zip(expected)
            .position(|(got, want)| got != want)
    );
    // Each d_off names the position after its entry, counting from 1.
    let offsets: Vec<i64> = (1..=listing.names.len() as i64).collect();
    assert_eq!(listing.offsets, offsets);
}

fn creation_order_does_not_change_enumeration_guest() {
    let root = tempfile::tempdir().unwrap();
    let forward = root.path().join("forward");
    let shuffled = root.path().join("shuffled");
    std::fs::create_dir(&forward).unwrap();
    std::fs::create_dir(&shuffled).unwrap();
    populate(&forward, 0..ENTRIES);
    populate(&shuffled, scrambled());
    let expected = sorted_names();

    let dir = open_dir(&forward);
    let forward_listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_whole_directory_sorted(&forward_listing, &expected);

    let dir = open_dir(&shuffled);
    let shuffled_listing = read_listing(dir);
    unsafe { libc::closedir(dir) };
    assert_whole_directory_sorted(&shuffled_listing, &expected);

    // std's iterator is a second, independent reader over the same syscall.
    let std_names: Vec<String> = std::fs::read_dir(&shuffled)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(std_names, expected);

    println!(
        "forward {:016x} shuffled {:016x}",
        forward_listing.digest, shuffled_listing.digest
    );
}

#[test]
fn creation_order_does_not_change_enumeration() {
    run_five_times(creation_order_does_not_change_enumeration_guest);
}

fn seekdir_and_rewinddir_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let dir = open_dir(root.path());

    // Stop part way through, well past the first 32KiB buffer.
    let mut before = Vec::new();
    for _ in 0..2000 {
        let entry = unsafe { libc::readdir64(dir) };
        assert!(!entry.is_null());
        before.push(unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned());
    }
    let mark = unsafe { libc::telldir(dir) };
    let rest = read_listing(dir);
    assert_eq!(before.len() + rest.names.len(), ENTRIES + 2);

    unsafe { libc::seekdir(dir, mark) };
    let again = read_listing(dir);
    assert_eq!(again.names, rest.names, "seekdir did not return to telldir");
    assert_eq!(again.digest, rest.digest);

    // rewinddir refers the stream to the directory's current contents.
    File::create(root.path().join("added-after-opendir")).unwrap();
    std::fs::remove_file(root.path().join(name(0))).unwrap();
    unsafe { libc::rewinddir(dir) };
    let refreshed = read_listing(dir);
    unsafe { libc::closedir(dir) };
    let names: BTreeSet<&str> = refreshed.names.iter().map(String::as_str).collect();
    assert!(names.contains("added-after-opendir"));
    assert!(!names.contains(name(0).as_str()));
    assert_eq!(refreshed.names.len(), ENTRIES + 2);
    assert_eq!(refreshed.names[2], "added-after-opendir");

    println!(
        "rest {:016x} refreshed {:016x}",
        rest.digest, refreshed.digest
    );
}

#[test]
fn seekdir_and_rewinddir() {
    run_five_times(seekdir_and_rewinddir_guest);
}

fn getdents64(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    let n = unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            fd,
            buf.as_mut_ptr(),
            buf.len() as libc::c_uint,
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(n as usize)
    }
}

fn getdents_legacy(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    let n = unsafe {
        libc::syscall(
            libc::SYS_getdents,
            fd,
            buf.as_mut_ptr(),
            buf.len() as libc::c_uint,
        )
    };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(n as usize)
    }
}

/// The names in a raw `getdents` buffer; the legacy layout keeps `d_type` in
/// the last byte of the record, after the name.
fn record_names(buf: &[u8], name_offset: usize) -> Vec<String> {
    let mut names = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let reclen = u16::from_ne_bytes([buf[at + 16], buf[at + 17]]) as usize;
        let name = CStr::from_bytes_until_nul(&buf[at + name_offset..at + reclen]).unwrap();
        names.push(name.to_str().unwrap().to_owned());
        at += reclen;
    }
    names
}

fn raw_getdents_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    for (call, name_offset) in [
        (getdents64 as fn(i32, &mut [u8]) -> Result<usize, i32>, 19),
        (getdents_legacy, 18),
    ] {
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        // A buffer smaller than the first record is EINVAL.
        assert_eq!(call(fd, &mut [0; 16]), Err(libc::EINVAL));

        // Alternate the descriptor and its dup: they share one stream. Small
        // buffers force many calls, each continuing where the last stopped.
        let alias = unsafe { libc::dup(fd) };
        assert!(alias >= 0);
        let mut names = Vec::new();
        let mut buf = [0u8; 200];
        let mut turn = 0;
        loop {
            let n = call(if turn % 2 == 0 { fd } else { alias }, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            names.extend(record_names(&buf[..n], name_offset));
            turn += 1;
        }
        assert_eq!(names, expected);
        assert!(turn > 100, "only {turn} calls: the buffer was not small");

        // A position is an entry index, so seeking to 5 resumes at entry 5.
        assert_eq!(unsafe { libc::lseek(alias, 5, libc::SEEK_SET) }, 5);
        let n = call(fd, &mut buf).unwrap();
        assert_eq!(record_names(&buf[..n], name_offset)[0], expected[5]);
        unsafe { libc::close(alias) };
    }

    // A regular file is not a directory, and asking must not move its offset.
    let file_path = root.path().join(name(7));
    std::fs::write(&file_path, b"0123456789").unwrap();
    let file = File::open(&file_path).unwrap();
    let fd = file.as_raw_fd();
    assert_eq!(getdents64(fd, &mut [0; 4096]), Err(libc::ENOTDIR));
    assert_eq!(unsafe { libc::lseek(fd, 4, libc::SEEK_SET) }, 4);
    assert_eq!(getdents64(fd, &mut [0; 4096]), Err(libc::ENOTDIR));
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) }, 4);

    println!("raw getdents ok");
}

#[test]
fn raw_getdents_share_one_sorted_stream() {
    run_five_times(raw_getdents_guest);
}
