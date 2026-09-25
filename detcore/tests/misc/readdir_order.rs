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
//! These tests create directories far larger than one buffer, in different
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
    run_five_times_with(guest, true)
}

fn run_five_times_with(guest: fn(), sequentialize_threads: bool) {
    run_five_times_on(|| (), |()| guest(), sequentialize_threads)
}

/// Runs `guest` five times, each on a new value from `setup`. The value is made
/// outside the guest and dropped after its run.
fn run_five_times_on<S: Sync>(
    setup: impl Fn() -> S,
    guest: impl Fn(&S) + Sync,
    sequentialize_threads: bool,
) {
    let config = Config {
        sequentialize_threads,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let mut expected = None;

    for run in 1..=RUNS {
        let value = setup();
        let (output, _state) = detcore_testutils::test_fn_with_config::<Detcore, _>(
            || guest(&value),
            config.clone(),
            true,
        )
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

/// Two directories holding the same names, created in different orders.
fn creation_order_directories() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let forward = root.path().join("forward");
    let shuffled = root.path().join("shuffled");
    std::fs::create_dir(&forward).unwrap();
    std::fs::create_dir(&shuffled).unwrap();
    populate(&forward, 0..ENTRIES);
    populate(&shuffled, scrambled());
    root
}

fn creation_order_does_not_change_enumeration_guest(root: &tempfile::TempDir) {
    let forward = root.path().join("forward");
    let shuffled = root.path().join("shuffled");
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
    // The host's order depends on the order the files were created in, not
    // on which process created them. Creating 6000 files inside each traced
    // run cost most of the test's CPU limit, so they are created outside it,
    // in new directories for each run, so that the host's inode numbers still
    // differ between the runs being compared.
    run_five_times_on(
        creation_order_directories,
        creation_order_does_not_change_enumeration_guest,
        true,
    );
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

/// Every name in the directory, read with `getdents64` on `fd` until the end.
fn drain_names(fd: i32) -> Vec<String> {
    let mut names = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = getdents64(fd, &mut buf).unwrap_or_else(|errno| {
            panic!(
                "getdents64 failed: {}",
                std::io::Error::from_raw_os_error(errno)
            )
        });
        if n == 0 {
            return names;
        }
        names.extend(record_names(&buf[..n], 19));
    }
}

/// Send `fd` to this process over a Unix socket and return the descriptor
/// that arrives. Detcore does not track descriptors received this way.
fn receive_descriptor(fd: i32) -> i32 {
    /// Room for one `SCM_RIGHTS` message, aligned as a `cmsghdr` must be.
    #[repr(C, align(8))]
    struct Control([u8; 32]);

    fn message(byte: &mut u8, control: &mut Control) -> (libc::iovec, libc::msghdr) {
        let iov = libc::iovec {
            iov_base: (byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
        header.msg_iovlen = 1;
        header.msg_control = control.0.as_mut_ptr().cast();
        header.msg_controllen = unsafe { libc::CMSG_SPACE(4) } as usize;
        (iov, header)
    }

    let (sender, receiver) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut byte = b'x';
    let mut control = Control([0; 32]);
    let (mut iov, mut header) = message(&mut byte, &mut control);
    header.msg_iov = &mut iov;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(4) as usize;
        libc::CMSG_DATA(cmsg).cast::<i32>().write_unaligned(fd);
        assert_eq!(libc::sendmsg(sender.as_raw_fd(), &header, 0), 1);
    }

    let mut control = Control([0; 32]);
    let (mut iov, mut header) = message(&mut byte, &mut control);
    header.msg_iov = &mut iov;
    unsafe {
        assert_eq!(libc::recvmsg(receiver.as_raw_fd(), &mut header, 0), 1);
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        assert!(!cmsg.is_null(), "no descriptor received");
        assert_eq!((*cmsg).cmsg_type, libc::SCM_RIGHTS);
        libc::CMSG_DATA(cmsg).cast::<i32>().read_unaligned()
    }
}

fn received_descriptor_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // A descriptor that arrives over a Unix socket has no open file
    // description in Detcore; it must still list the whole directory.
    let dir = File::open(root.path()).unwrap();
    let received = receive_descriptor(dir.as_raw_fd());
    drop(dir);

    let mut names = drain_names(received);
    assert_eq!(names.len(), ENTRIES + 2, "received descriptor lost entries");
    // Without a stream, each kernel buffer is sorted on its own, so only the
    // set of names is fixed.
    names.sort();
    assert_eq!(names, expected);
    unsafe { libc::close(received) };

    println!("received descriptor ok");
}

#[test]
fn received_descriptor_lists_whole_directory() {
    run_five_times(received_descriptor_guest);
}

fn unsequentialized_threads_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // Two threads read one open file description through two descriptors at
    // the same moment. Linux returns every entry exactly once in total.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let alias = unsafe { libc::dup(fd) };
    assert!(alias >= 0);
    let start = std::sync::Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            start.wait();
            drain_names(fd)
        });
        let second = scope.spawn(|| {
            start.wait();
            drain_names(alias)
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    unsafe { libc::close(alias) };

    let mut names = first;
    names.extend(second);
    assert_eq!(
        names.len(),
        ENTRIES + 2,
        "concurrent readers lost or repeated entries"
    );
    names.sort();
    assert_eq!(names, expected);

    println!("concurrent readers ok");
}

#[test]
fn unsequentialized_threads_share_one_stream() {
    run_five_times_with(unsequentialized_threads_guest, false);
}

/// The name and `d_off` of each record in a raw `getdents64` buffer.
fn records(buf: &[u8]) -> Vec<(String, i64)> {
    let mut records = Vec::new();
    let mut at = 0;
    while at < buf.len() {
        let off = i64::from_ne_bytes(buf[at + 8..at + 16].try_into().unwrap());
        let reclen = u16::from_ne_bytes([buf[at + 16], buf[at + 17]]) as usize;
        let name = CStr::from_bytes_until_nul(&buf[at + 19..at + reclen]).unwrap();
        records.push((name.to_str().unwrap().to_owned(), off));
        at += reclen;
    }
    records
}

fn buffer_tail_guest() {
    // Names of many lengths, so a buffer of sorted records and a buffer of
    // host-ordered records fill to different lengths.
    let root = tempfile::tempdir().unwrap();
    for index in 0..300 {
        let name = format!("n{}{index:03}", "x".repeat(index * 7 % 40));
        File::create(root.path().join(name)).unwrap();
    }

    // Linux writes only the records it returns. Reading the host directory
    // must not leave anything after them, whatever the host order.
    for capacity in (150..=450).step_by(10) {
        let dir = File::open(root.path()).unwrap();
        let mut buf = vec![0xaa_u8; capacity + 64];
        let n = getdents64(dir.as_raw_fd(), &mut buf[..capacity]).unwrap();
        assert!(n > 0);
        assert!(
            buf[n..].iter().all(|&byte| byte == 0xaa),
            "a {capacity}-byte getdents64 returned {n} bytes and changed bytes after them"
        );
    }

    println!("buffer tail ok");
}

#[test]
fn buffer_tail_left_untouched() {
    run_five_times(buffer_tail_guest);
}

fn small_buffer_guest() {
    let root = tempfile::tempdir().unwrap();
    File::create(root.path().join("a-longer-filename")).unwrap();

    // A buffer too small for the first entry is EINVAL.
    let dir = File::open(root.path()).unwrap();
    assert_eq!(getdents64(dir.as_raw_fd(), &mut [0; 16]), Err(libc::EINVAL));

    // A 24-byte buffer holds `.` but not the 40-byte record of the long name;
    // Linux still returns the entries that fit, one call at a time.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut buf = [0u8; 24];
    let n = getdents64(fd, &mut buf).unwrap();
    assert_eq!(record_names(&buf[..n], 19), ["."]);
    let mut names = vec![".".to_owned()];
    names.extend(drain_names(fd));
    names.sort();
    assert_eq!(names, [".", "..", "a-longer-filename"]);

    println!("small buffer ok");
}

#[test]
fn small_buffer_returns_entries_that_fit() {
    run_five_times(small_buffer_guest);
}

fn seek_before_first_read_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..5 {
        File::create(root.path().join(name(index))).unwrap();
    }

    // Learn a real host position: a received descriptor has no stream, so
    // its records carry the kernel's own `d_off` cookies. `.` comes first on
    // Linux filesystems, and its cookie is the position after it.
    let probe = File::open(root.path()).unwrap();
    let received = receive_descriptor(probe.as_raw_fd());
    drop(probe);
    let mut buf = [0u8; 4096];
    let n = getdents64(received, &mut buf).unwrap();
    unsafe { libc::close(received) };
    let cookie = records(&buf[..n])
        .into_iter()
        .find_map(|(name, off)| (name == ".").then_some(off))
        .expect("no `.` entry");
    assert_ne!(cookie, 0);

    // Seeking a fresh descriptor there before its first read resumes after
    // `.`, as on Linux, rather than restarting the directory.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    assert_eq!(unsafe { libc::lseek(fd, cookie, libc::SEEK_SET) }, cookie);
    let mut names = drain_names(fd);
    names.sort();
    let mut expected = vec!["..".to_owned()];
    expected.extend((0..5).map(name));
    assert_eq!(names, expected);

    println!("seek before first read ok");
}

#[test]
fn seek_before_first_read_is_kept() {
    run_five_times(seek_before_first_read_guest);
}

fn rewind_after_host_order_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // A descriptor seeked before its first read is read in host order (see
    // `seek_before_first_read_is_kept`). Seeking it back to 0 starts the
    // directory over, and the sorted stream with it.
    let probe = File::open(root.path()).unwrap();
    let received = receive_descriptor(probe.as_raw_fd());
    drop(probe);
    let mut buf = [0u8; 4096];
    let n = getdents64(received, &mut buf).unwrap();
    unsafe { libc::close(received) };
    let cookie = records(&buf[..n])
        .into_iter()
        .find_map(|(name, off)| (name == ".").then_some(off))
        .expect("no `.` entry");
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    assert_eq!(unsafe { libc::lseek(fd, cookie, libc::SEEK_SET) }, cookie);
    assert!(getdents64(fd, &mut buf).unwrap() > 0);
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(drain_names(fd), expected, "after seeking a host position");

    // A buffer too small for a later entry is also read in host order; a
    // rewind and a larger buffer bring the sorted stream back.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut small = [0u8; 24];
    let n = getdents64(fd, &mut small).unwrap();
    assert_eq!(record_names(&small[..n], 19), ["."]);
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(drain_names(fd), expected, "after a small buffer");

    println!("rewind after host order ok");
}

#[test]
fn rewind_after_host_order_sorts_again() {
    run_five_times(rewind_after_host_order_guest);
}

fn passed_on_descriptor_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // Read part of the directory, then hand the open file to a descriptor
    // Detcore does not track. On Linux it reads the entries not yet returned;
    // it may also repeat some here, because the host order differs, but it
    // must not miss any.
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut buf = [0u8; 512];
    let n = getdents64(fd, &mut buf).unwrap();
    let first = record_names(&buf[..n], 19);
    assert!(
        first.len() < expected.len(),
        "the first read was not partial"
    );
    let received = receive_descriptor(fd);
    let passed_on: BTreeSet<String> = drain_names(received).into_iter().collect();
    let missing: Vec<&String> = expected[first.len()..]
        .iter()
        .filter(|name| !passed_on.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "the passed-on descriptor missed {} entries not yet returned, first {:?}",
        missing.len(),
        missing.first()
    );

    // The stream itself continues where it stopped.
    let mut names = first;
    names.extend(drain_names(fd));
    assert_eq!(names, expected);

    // At the end of the stream, the passed-on descriptor is at the end too.
    let mut buf = [0u8; 4096];
    assert_eq!(getdents64(received, &mut buf), Ok(0));

    // A rewind restarts both.
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(drain_names(received).len(), expected.len());
    unsafe { libc::close(received) };

    println!("passed-on descriptor ok");
}

#[test]
fn passed_on_descriptor_misses_no_entry() {
    run_five_times(passed_on_descriptor_guest);
}

fn partly_mapped_buffer_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // A buffer whose first page is mapped and whose next two are not. Linux
    // returns the records that fit in the mapped page.
    let page = 4096;
    let buf = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            3 * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(buf, libc::MAP_FAILED);
    assert_eq!(
        unsafe { libc::munmap(buf.cast::<u8>().add(page).cast(), 2 * page) },
        0
    );

    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf, 3 * page as libc::c_uint) };
    assert!(
        n > 0,
        "getdents64 into a partly mapped buffer failed: {}",
        std::io::Error::last_os_error()
    );
    let n = n as usize;
    assert!(n <= page, "{n} bytes returned from a {page}-byte mapping");
    let first = record_names(
        unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), n) },
        19,
    );
    unsafe { libc::munmap(buf, page) };

    let mut names = first;
    names.extend(drain_names(fd));
    assert_eq!(names, expected);

    println!("partly mapped buffer ok");
}

#[test]
fn partly_mapped_buffer_returns_entries_that_fit() {
    run_five_times(partly_mapped_buffer_guest);
}

fn long_name_past_writable_end_guest() {
    let root = tempfile::tempdir().unwrap();
    // A 200-byte name takes a 224-byte record.
    let long = "y".repeat(200);
    File::create(root.path().join(&long)).unwrap();

    // A 256-byte buffer of which only the first 64 bytes are mapped: `.` and
    // `..` fit, the long name does not. Linux returns the two that fit, and
    // then fails the call that reaches the long name.
    let page = 4096;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            2 * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    assert_eq!(
        unsafe { libc::munmap(map.cast::<u8>().add(page).cast(), page) },
        0
    );
    let buf = unsafe { std::slice::from_raw_parts_mut(map.cast::<u8>().add(page - 64), 64) };
    buf.fill(0xaa);

    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let getdents = || unsafe {
        libc::syscall(
            libc::SYS_getdents64,
            fd,
            map.cast::<u8>().add(page - 64),
            256,
        )
    };
    let n = getdents();
    assert_eq!(n, 48, "{}", std::io::Error::last_os_error());
    assert_eq!(record_names(&buf[..48], 19), [".", ".."]);
    assert!(
        buf[48..].iter().all(|&byte| byte == 0xaa),
        "bytes after the returned records changed: {:02x?}",
        &buf[48..]
    );
    assert_eq!(getdents(), -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EFAULT)
    );
    unsafe { libc::munmap(map, page) };
    assert_eq!(drain_names(fd), [long]);

    println!("long name past writable end ok");
}

#[test]
fn long_name_past_writable_end_returns_entries_before_it() {
    run_five_times(long_name_past_writable_end_guest);
}

fn seek_during_first_read_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend(sorted_names());

    // One thread makes the first getdents64 call on a fresh descriptor, which
    // reads the whole host directory 512 bytes at a time, while another
    // rewinds the same open file up to 100 times, stopping early when that
    // call returns. A rewind before the call has no effect and a rewind after
    // it restarts the stream; neither may land in the middle of reading the
    // host directory. (Unbounded rewinds landing there would keep the read
    // from ever reaching the end, so a broken lock would hang, not fail.)
    for trial in 0..20 {
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let start = std::sync::Barrier::new(2);
        let done = std::sync::atomic::AtomicBool::new(false);
        let first = std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                start.wait();
                let mut buf = [0u8; 512];
                let n = getdents64(fd, &mut buf).unwrap();
                done.store(true, std::sync::atomic::Ordering::SeqCst);
                record_names(&buf[..n], 19)
            });
            scope.spawn(|| {
                start.wait();
                for _ in 0..100 {
                    if done.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
                }
            });
            reader.join().unwrap()
        });
        assert!(
            first == expected[..first.len()],
            "trial {trial}: the first call returned {first:?}, not the start of the sorted directory"
        );
        let rest = drain_names(fd);
        let mut continued = first;
        continued.extend(rest.iter().cloned());
        assert!(
            rest == expected || continued == expected,
            "trial {trial}: {} entries after the first {} did not continue or restart the directory",
            rest.len(),
            continued.len() - rest.len()
        );
    }

    println!("seek during first read ok");
}

#[test]
fn seek_during_first_read_leaves_stream_whole() {
    run_five_times_with(seek_during_first_read_guest, false);
}

/// A directory holding `count` names from `name`, created in both orders:
/// `forward` in sorted order and `reverse` in reverse sorted order.
fn both_orders(count: usize) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    for (order, indices) in [
        ("forward", (0..count).collect::<Vec<_>>()),
        ("reverse", (0..count).rev().collect()),
    ] {
        let dir = root.path().join(order);
        std::fs::create_dir(&dir).unwrap();
        populate(&dir, indices.into_iter());
    }
    root
}

/// `.`, `..` and then the first `count` names from `name`.
fn listing_of(count: usize) -> Vec<String> {
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..count).map(name));
    expected
}

/// Panic unless `names` contains every name in `expected` that is not in
/// `returned`.
fn assert_contains_rest(names: &[String], expected: &[String], returned: &[String], context: &str) {
    let names: BTreeSet<&String> = names.iter().collect();
    let wanted: Vec<&String> = expected
        .iter()
        .filter(|name| !returned.contains(name))
        .collect();
    let missing: Vec<&&String> = wanted
        .iter()
        .filter(|name| !names.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "{context}: {} of {} entries missing, first {:?}",
        missing.len(),
        wanted.len(),
        missing.first()
    );
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names
}

fn alias_after_rewinds_guest() {
    let root = both_orders(300);
    let expected = listing_of(300);

    for order in ["forward", "reverse"] {
        let path = root.path().join(order);

        // Rewinding the stream twice, with the passed-on descriptor reading
        // the whole directory in between, leaves the open file at the start
        // each time: the passed-on descriptor then reads every entry again.
        let dir = File::open(&path).unwrap();
        let fd = dir.as_raw_fd();
        let mut buf = [0u8; 512];
        let n = getdents64(fd, &mut buf).unwrap();
        assert!(record_names(&buf[..n], 19).len() < expected.len());
        let received = receive_descriptor(fd);
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
        assert_eq!(
            sorted(drain_names(received)),
            sorted(expected.clone()),
            "{order}: after the first rewind"
        );
        assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
        drop(dir);
        assert_eq!(
            sorted(drain_names(received)),
            sorted(expected.clone()),
            "{order}: after the second rewind"
        );
        unsafe { libc::close(received) };

        // Seeking the stream back to where it already was, after the
        // passed-on descriptor read to the end, leaves the open file where
        // every entry not yet returned follows.
        let dir = File::open(&path).unwrap();
        let fd = dir.as_raw_fd();
        let n = getdents64(fd, &mut buf).unwrap();
        let first = record_names(&buf[..n], 19);
        let received = receive_descriptor(fd);
        let position = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
        assert!(position > 0);
        assert_contains_rest(
            &drain_names(received),
            &expected,
            &first,
            &format!("{order}: after seeking to the current position"),
        );
        assert_eq!(
            unsafe { libc::lseek(fd, position, libc::SEEK_SET) },
            position
        );
        drop(dir);
        assert_contains_rest(
            &drain_names(received),
            &expected,
            &first,
            &format!("{order}: after seeking back to {position}"),
        );
        unsafe { libc::close(received) };
    }

    println!("alias after rewinds ok");
}

#[test]
fn passed_on_descriptor_follows_repeated_seeks() {
    run_five_times(alias_after_rewinds_guest);
}

/// A mapping of `pages` pages filled with 0xaa, of which the first `writable`
/// stay readable and writable and the rest get `protection`.
fn guarded_pages(pages: usize, writable: usize, protection: i32) -> *mut u8 {
    let page = 4096;
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            pages * page,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(map, libc::MAP_FAILED);
    let map = map.cast::<u8>();
    unsafe { std::ptr::write_bytes(map, 0xaa, pages * page) };
    assert_eq!(
        unsafe {
            libc::mprotect(
                map.add(writable * page).cast(),
                (pages - writable) * page,
                protection,
            )
        },
        0
    );
    map
}

fn raw_getdents64(fd: i32, buf: *mut u8, count: libc::c_uint) -> Result<usize, i32> {
    let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf, count) };
    if n < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap())
    } else {
        Ok(n as usize)
    }
}

fn first_record_short_of_writable_end_guest() {
    // Two-character names take 24-byte records, of which Linux writes the
    // first 22: it does not write the padding after the name.
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // With 22 bytes writable before a read-only page, `.` still fits
    // (it needs 21), and the call returns its whole 24-byte record without
    // touching the two read-only bytes. With 20, nothing fits and the call
    // fails; Linux writes the first 20 bytes of `.` before failing, but
    // Detcore leaves every byte after its result as the guest had it, as
    // everywhere else. Either way a descriptor the open file is passed on to then finds
    // every entry not yet returned; without one, the stream continues. (After
    // the passed-on descriptor reads to the end, Linux gives the first
    // descriptor nothing more, so each is checked on its own open file.)
    for (writable, returned) in [(22, Ok(24)), (20, Err(libc::EFAULT))] {
        for pass_on in [true, false] {
            let context = format!("{writable} bytes writable, passed on {pass_on}");
            let map = guarded_pages(2, 1, libc::PROT_READ);
            let buf = unsafe { map.add(page - writable) };
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            let result = raw_getdents64(fd, buf, 256);
            assert_eq!(result, returned, "{context}");
            let bytes = unsafe { std::slice::from_raw_parts(buf, 24) };
            let first = match result {
                Ok(n) => record_names(&bytes[..n], 19),
                Err(_) => Vec::new(),
            };
            assert_eq!(first, expected[..first.len()], "{context}");
            assert_eq!(&bytes[writable..], &[0xaa; 24][writable..], "{context}");
            if result.is_err() {
                assert!(bytes.iter().all(|&byte| byte == 0xaa), "{context}");
            }
            if pass_on {
                let received = receive_descriptor(fd);
                assert_contains_rest(&drain_names(received), &expected, &first, &context);
                unsafe { libc::close(received) };
            } else {
                let mut names = first;
                names.extend(drain_names(fd));
                assert_eq!(names, expected, "{context}");
            }
            unsafe { libc::munmap(map.cast(), 2 * page) };
        }
    }

    println!("first record short of writable end ok");
}

#[test]
fn first_record_short_of_writable_end_leaves_stream_whole() {
    run_five_times(first_record_short_of_writable_end_guest);
}

fn record_into_inaccessible_page_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // 88 bytes before a page the guest cannot touch: three 24-byte records
    // fit, and the fourth would end 8 bytes inside that page. Linux returns
    // the three and never writes the inaccessible page. (It does write the
    // fourth record's `d_ino` before failing on its length; Detcore leaves
    // the 16 bytes after the three records as the guest had them.) The first call reads a fresh descriptor, the second
    // continues the stream.
    let map = guarded_pages(2, 1, libc::PROT_NONE);
    let buf = unsafe { map.add(page - 88) };
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut names = Vec::new();
    for call in 0..2 {
        let n = raw_getdents64(fd, buf, 256).unwrap();
        assert_eq!(n, 72, "call {call}");
        let bytes = unsafe { std::slice::from_raw_parts(buf, 88) };
        names.extend(record_names(&bytes[..n], 19));
        assert!(
            bytes[n..].iter().all(|&byte| byte == 0xaa),
            "call {call} changed bytes after its records: {:02x?}",
            &bytes[n..]
        );
        assert_eq!(
            unsafe {
                libc::mprotect(
                    map.add(page).cast(),
                    page,
                    libc::PROT_READ | libc::PROT_WRITE,
                )
            },
            0
        );
        let guard = unsafe { std::slice::from_raw_parts(map.add(page), page) };
        assert!(
            guard.iter().all(|&byte| byte == 0xaa),
            "call {call} wrote the inaccessible page"
        );
        assert_eq!(
            unsafe { libc::mprotect(map.add(page).cast(), page, libc::PROT_NONE) },
            0
        );
        unsafe { std::ptr::write_bytes(buf, 0xaa, 88) };
    }
    names.extend(drain_names(fd));
    assert_eq!(names, expected);
    unsafe { libc::munmap(map.cast(), 2 * page) };

    println!("record into inaccessible page ok");
}

#[test]
fn record_into_inaccessible_page_is_not_returned() {
    run_five_times(record_into_inaccessible_page_guest);
}

fn large_buffer_tail_guest() {
    let root = tempfile::tempdir().unwrap();
    populate(root.path(), scrambled());
    let expected = listing_of(ENTRIES);
    let page = 4096;

    // A count of 100000 whose first 70001 bytes are writable, above the
    // 64KiB Detcore reads the host directory with. The records, `.` and
    // `..` at 24 bytes and the rest at 32, fill exactly 70000 bytes on a
    // fresh descriptor, and fewer after a first read of 4096; either way
    // every writable byte after them is left as the guest had it. (Linux
    // writes the leading fields of the record that does not fit.)
    let writable: usize = 70001;
    let pages = (writable + 100000) / page + 2;
    let usable = writable.div_ceil(page);
    for warm in [false, true] {
        let map = guarded_pages(pages, usable, libc::PROT_NONE);
        let buf = unsafe { map.add(usable * page - writable) };
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let mut names = Vec::new();
        if warm {
            let mut first = [0u8; 4096];
            let n = getdents64(fd, &mut first).unwrap();
            names = record_names(&first[..n], 19);
        }
        let n = raw_getdents64(fd, buf, 100000).unwrap();
        let bytes = unsafe { std::slice::from_raw_parts(buf, writable) };
        if !warm {
            assert_eq!(n, 70000);
        }
        assert!(n > 60000 && n <= writable, "{n} bytes returned");
        let first_changed = bytes[n..].iter().position(|&byte| byte != 0xaa);
        assert_eq!(
            first_changed, None,
            "warm {warm}: {n} bytes returned, and a byte after them changed"
        );
        names.extend(record_names(&bytes[..n], 19));
        names.extend(drain_names(fd));
        assert_eq!(names, expected, "warm {warm}");
        unsafe { libc::munmap(map.cast(), pages * page) };
    }

    println!("large buffer tail ok");
}

#[test]
fn large_buffer_tail_left_untouched() {
    run_five_times(large_buffer_tail_guest);
}

fn count_above_int_max_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);

    // Linux keeps the count in an `int`, where these are negative: no entry
    // fits, so the call fails with EINVAL while entries remain and returns 0
    // at the end, and moves nothing. The buffer is on the heap, far enough
    // below the top of user memory for the whole range to be valid.
    let mut buf = vec![0xaa_u8; 4096];
    for count in [0x8000_0000_u32, 0xffff_ffff] {
        for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
            let getdents = |fd: i32, buf: &mut [u8], count: u32| {
                let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), count) };
                if n < 0 {
                    Err(std::io::Error::last_os_error().raw_os_error().unwrap())
                } else {
                    Ok(n as usize)
                }
            };
            let context = format!("syscall {call}, count {count:#x}");
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            // Fresh.
            assert_eq!(
                getdents(fd, &mut buf, count),
                Err(libc::EINVAL),
                "{context}"
            );
            assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
            // After part of the directory.
            let n = getdents(fd, &mut buf[..200], 200).unwrap();
            let mut names = record_names(&buf[..n], name_offset);
            buf.fill(0xaa);
            assert_eq!(
                getdents(fd, &mut buf, count),
                Err(libc::EINVAL),
                "{context}"
            );
            assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
            loop {
                let n = getdents(fd, &mut buf, 4096).unwrap();
                if n == 0 {
                    break;
                }
                names.extend(record_names(&buf[..n], name_offset));
            }
            assert_eq!(names, expected, "{context}");
            // At the end.
            buf.fill(0xaa);
            assert_eq!(getdents(fd, &mut buf, count), Ok(0), "{context}");
            assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
        }
    }

    println!("count above int max ok");
}

#[test]
fn count_above_int_max_matches_linux() {
    run_five_times(count_above_int_max_guest);
}
