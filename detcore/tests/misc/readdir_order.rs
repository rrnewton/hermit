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
    run_five_times_under(setup, guest, config)
}

/// Like [`run_five_times`], also hashing the bytes each syscall returns into
/// the log, as `hermit run` does by default.
fn run_five_times_hashing_buffers(guest: fn()) {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        detlog_io_buffers: true,
        ..Default::default()
    };
    run_five_times_under(|| (), |()| guest(), config)
}

/// Like [`run_five_times`], without hashing the bytes each syscall returns
/// into the log.
fn run_five_times_without_hashing_buffers(guest: fn()) {
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        detlog_io_buffers: false,
        ..Default::default()
    };
    run_five_times_under(|| (), |()| guest(), config)
}

fn run_five_times_under<S: Sync>(setup: impl Fn() -> S, guest: impl Fn(&S) + Sync, config: Config) {
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

/// The `room` bytes Linux leaves where it began to store `record`, a
/// `getdents64` record that it could not finish, into a buffer that held
/// `0xaa`, for a `call` of `SYS_getdents64` or `SYS_getdents`. The two
/// formats' records of a name have the same length. `filldir64`
/// (fs/readdir.c) stores the record's `d_ino`, `d_reclen` and `d_type`, each
/// at once, so only if it fits whole, then the NUL after the name, then the
/// name. `filldir` stores `d_type` in the record's last byte, so if it fits,
/// so does the whole record. Neither stores the record's own `d_off` before
/// the next record or the end of the call.
fn left_by_linux(record: &[u8], room: usize, call: libc::c_long) -> Vec<u8> {
    let name_len = record[19..].iter().position(|&byte| byte == 0).unwrap();
    let reclen = usize::from(u16::from_ne_bytes([record[16], record[17]]));
    let type_at = if call == libc::SYS_getdents {
        assert!(room < reclen, "the record fits in {room} bytes");
        reclen - 1
    } else {
        assert!(
            19 + name_len >= room,
            "the record's NUL fits in {room} bytes"
        );
        18
    };
    let mut left = vec![0xaa; room];
    for (at, field) in [(0, 0..8), (16, 16..18), (type_at, 18..19)] {
        if at + field.len() <= room {
            left[at..at + field.len()].copy_from_slice(&record[field]);
        }
    }
    left
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
    // touching its padding or the two read-only bytes. With 20, nothing fits
    // and the call fails. Linux first stores the entry's position, 0, into
    // `.`'s `d_off`, then its `d_ino`, `d_reclen` and `d_type`, and fails at
    // the NUL after the name, byte 20, leaving the first 19 bytes written.
    // Either way a descriptor the open file is passed on to then finds every
    // entry not yet returned; without one, the stream continues. (After the
    // passed-on descriptor reads to the end, Linux gives the first descriptor
    // nothing more, so each is checked on its own open file.)
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
            assert_eq!(&bytes[21..], &[0xaa; 24][21..], "{context}");
            if result.is_err() {
                assert_eq!(&bytes[8..16], &0i64.to_ne_bytes(), "{context}");
                assert_eq!(&bytes[16..18], &24u16.to_ne_bytes(), "{context}");
                assert_eq!(bytes[18], libc::DT_DIR, "{context}");
                assert_eq!(bytes[19..21], [0xaa; 2], "{context}");
            }
            if pass_on {
                let received = receive_descriptor(fd);
                assert_contains_rest(&drain_names(received), &expected, &first, &context);
                unsafe { libc::close(received) };
            } else {
                let mut next = [0u8; 4096];
                let len = getdents64(fd, &mut next).unwrap();
                if result.is_err() {
                    // `.` comes first again, with the `d_ino` stored before.
                    assert_eq!(bytes[..8], next[..8], "{context}");
                }
                let mut names = first;
                names.extend(record_names(&next[..len], 19));
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
    // the three and never writes the inaccessible page. It does store the
    // fourth record's `d_ino` before failing on its length, so the 16 bytes
    // after the three records hold the `d_ino` of the entry the next call
    // returns first. The first call reads a fresh descriptor, the second
    // continues the stream.
    let map = guarded_pages(2, 1, libc::PROT_NONE);
    let buf = unsafe { map.add(page - 88) };
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut names = Vec::new();
    let mut left: Option<Vec<u8>> = None;
    for call in 0..2 {
        let n = raw_getdents64(fd, buf, 256).unwrap();
        assert_eq!(n, 72, "call {call}");
        let bytes = unsafe { std::slice::from_raw_parts(buf, 88) };
        names.extend(record_names(&bytes[..n], 19));
        if let Some(left) = left {
            assert_eq!(
                left,
                left_by_linux(&bytes[..24], 16, libc::SYS_getdents64),
                "call {call}"
            );
        }
        left = Some(bytes[n..].to_vec());
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
    let mut next = [0u8; 4096];
    let len = getdents64(fd, &mut next).unwrap();
    assert_eq!(
        left.unwrap(),
        left_by_linux(&next[..24], 16, libc::SYS_getdents64),
        "after call 1"
    );
    names.extend(record_names(&next[..len], 19));
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
    // fresh descriptor, and fewer after a first read of 4096. After them,
    // Linux leaves what it stored of the record that does not fit (see
    // [`left_by_linux`]): on a fresh descriptor nothing, because its `d_ino`
    // crosses into the inaccessible page and is stored at once.
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
        names.extend(record_names(&bytes[..n], 19));
        let mut next = [0u8; 4096];
        let len = getdents64(fd, &mut next).unwrap();
        assert_eq!(
            bytes[n..],
            left_by_linux(&next[..32], writable - n, libc::SYS_getdents64)[..],
            "warm {warm}: {n} bytes returned, and the bytes after them"
        );
        if !warm {
            assert!(bytes[n..].iter().all(|&byte| byte == 0xaa));
        }
        names.extend(record_names(&next[..len], 19));
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

fn padding_in_inaccessible_page_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // Four 24-byte records take 96 bytes, but Linux writes only the first 22
    // of the fourth: it does not write the padding after the name. With 94 or
    // 95 bytes before a page the guest cannot touch, all four fit and the
    // call returns 96 without touching that page, and hashing the returned
    // bytes into the log must not fail on the two it cannot read. The page is
    // either inaccessible or not mapped at all; ptrace can read the first
    // kind. On a fresh descriptor the records start at `.`; after reading `.`
    // alone they start at `..`. A received descriptor, which Detcore does not
    // track, gets each buffer sorted on its own instead.
    let states = [(false, false), (false, true), (true, false), (true, true)];
    for unmapped in [false, true] {
        for writable in [94, 95] {
            for (warm, received) in states {
                let context = format!(
                    "{writable} bytes writable, unmapped {unmapped}, warm {warm}, received {received}"
                );
                let map = guarded_pages(2, 1, libc::PROT_NONE);
                let guard = unsafe { map.add(page) };
                if unmapped {
                    assert_eq!(unsafe { libc::munmap(guard.cast(), page) }, 0);
                }
                let buf = unsafe { guard.sub(writable) };
                let dir = File::open(root.path()).unwrap();
                let fd = if received {
                    receive_descriptor(dir.as_raw_fd())
                } else {
                    dir.as_raw_fd()
                };
                let mut names = Vec::new();
                if warm {
                    let mut first = [0u8; 24];
                    assert_eq!(getdents64(fd, &mut first), Ok(24), "{context}");
                    names = record_names(&first, 19);
                }
                let returned = raw_getdents64(fd, buf, 256);
                assert_eq!(returned, Ok(96), "{context}");
                let mut bytes = [0u8; 96];
                bytes[..writable]
                    .copy_from_slice(unsafe { std::slice::from_raw_parts(buf, writable) });
                let batch = record_names(&bytes, 19);
                if received {
                    assert!(batch.is_sorted(), "{context}: {batch:?}");
                } else {
                    assert_eq!(batch, expected[names.len()..names.len() + 4], "{context}");
                }
                names.extend(batch);
                if unmapped {
                    // Still nothing there to write.
                    assert_eq!(
                        unsafe { libc::msync(guard.cast(), page, libc::MS_ASYNC) },
                        -1,
                        "{context}"
                    );
                    assert_eq!(
                        std::io::Error::last_os_error().raw_os_error(),
                        Some(libc::ENOMEM),
                        "{context}"
                    );
                } else {
                    assert_eq!(
                        unsafe {
                            libc::mprotect(guard.cast(), page, libc::PROT_READ | libc::PROT_WRITE)
                        },
                        0
                    );
                    let guard = unsafe { std::slice::from_raw_parts(guard, page) };
                    assert!(
                        guard.iter().all(|&byte| byte == 0xaa),
                        "{context}: the inaccessible page was written"
                    );
                }
                names.extend(drain_names(fd));
                if received {
                    assert_eq!(sorted(names), expected, "{context}");
                    unsafe { libc::close(fd) };
                } else {
                    assert_eq!(names, expected, "{context}");
                }
                unsafe { libc::munmap(map.cast(), 2 * page) };
            }
        }
    }

    println!("padding in inaccessible page ok");
}

#[test]
fn padding_in_inaccessible_page_is_returned() {
    run_five_times_hashing_buffers(padding_in_inaccessible_page_guest);
}

fn write_only_prefix_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(format!("{index:02}"))).unwrap();
    }
    let mut expected = vec![".".to_owned(), "..".to_owned()];
    expected.extend((0..20).map(|index| format!("{index:02}")));
    let page = 4096;

    // A page the guest can write but not read, then one it cannot touch.
    // After `.` and `..`, each record is 24 bytes. With 8 bytes before the
    // inaccessible page no record fits and the call fails, writing nothing:
    // Linux's first write to the first record of a call is its `d_off`, 8
    // bytes in. With 32, `00` fits and `01` does not. Linux stores `01`'s
    // `d_ino` and fails at its `d_reclen`, so the 8 bytes after `00` hold
    // the `d_ino` of `01` (see [`left_by_linux`]). With 40, `01`'s own
    // `d_off` is still not written. With 45, `getdents64` also stores `01`'s
    // `d_reclen` and `d_type` and fails at the NUL after its name, while
    // `getdents` fails at `d_type`, in the record's last byte, after
    // `d_reclen`. The stream continues after the returned records.
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        for (writable, returned) in [
            (8, Err(libc::EFAULT)),
            (32, Ok(24)),
            (40, Ok(24)),
            (45, Ok(24)),
        ] {
            let context = format!("syscall {call}, {writable} bytes writable");
            let map = guarded_pages(2, 1, libc::PROT_NONE);
            assert_eq!(
                unsafe { libc::mprotect(map.cast(), page, libc::PROT_WRITE) },
                0
            );
            let buf = unsafe { map.add(page - writable) };
            let dir = File::open(root.path()).unwrap();
            let fd = dir.as_raw_fd();
            let mut first = [0u8; 48];
            assert_eq!(getdents64(fd, &mut first), Ok(48), "{context}");
            let mut names = record_names(&first, 19);
            let result = unsafe { libc::syscall(call, fd, buf, 256) };
            let result = if result < 0 {
                Err(std::io::Error::last_os_error().raw_os_error().unwrap())
            } else {
                Ok(result as usize)
            };
            assert_eq!(result, returned, "{context}");
            assert_eq!(
                unsafe { libc::mprotect(map.cast(), 2 * page, libc::PROT_READ | libc::PROT_WRITE) },
                0
            );
            let bytes = unsafe { std::slice::from_raw_parts(buf, writable) };
            let n = result.unwrap_or(0);
            names.extend(record_names(&bytes[..n], name_offset));
            let mut next = [0u8; 4096];
            let len = getdents64(fd, &mut next).unwrap();
            let left = if n > 0 {
                left_by_linux(&next[..24], writable - n, call)
            } else {
                vec![0xaa; writable]
            };
            assert_eq!(
                bytes[n..],
                left[..],
                "{context}: {n} bytes returned, and the bytes after them"
            );
            let guard = unsafe { std::slice::from_raw_parts(map.add(page), page) };
            assert!(
                guard.iter().all(|&byte| byte == 0xaa),
                "{context}: the inaccessible page was written"
            );
            names.extend(record_names(&next[..len], 19));
            names.extend(drain_names(fd));
            assert_eq!(names, expected, "{context}");
            unsafe { libc::munmap(map.cast(), 2 * page) };
        }
    }

    println!("write-only prefix ok");
}

/// Detcore cannot read back a buffer the guest can write but not read, so
/// with the bytes each syscall returns hashed into the log, as `hermit run`
/// does by default, it cannot log what the call returned; it fails the call
/// instead (see `write_only_buffer_is_refused_while_hashing_buffers`). So
/// the write-only tests run without that hashing.
#[test]
fn write_only_prefix_matches_linux() {
    run_five_times_without_hashing_buffers(write_only_prefix_guest);
}

/// Rewind, seek, then call `getdents` and `getdents64` with each of `counts`,
/// none of which any entry fits.
fn count_after_rewind_and_seek(counts: &[u64]) {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);

    // A rewind drops what Detcore knows of the stream, and a seek after it
    // sets a position the host directory has not been read to. A count too
    // small for any entry must still fail with EINVAL while entries remain
    // after that position and return 0 at or past the end, moving nothing
    // and writing nothing; the next call continues from the position. (On a
    // descriptor never read, a position is a host cookie, as on Linux, so
    // each descriptor first lists the whole directory.)
    let mut buf = vec![0xaa_u8; 4096];
    for &count in counts {
        for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
            let getdents = |fd: i32, buf: &mut [u8], count: u64| {
                let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), count) };
                if n < 0 {
                    Err(std::io::Error::last_os_error().raw_os_error().unwrap())
                } else {
                    Ok(n as usize)
                }
            };
            for position in [5_i64, 22, 1_000_000] {
                let context = format!("syscall {call}, count {count:#x}, position {position}");
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                while getdents(fd, &mut buf, 4096).unwrap() > 0 {}
                assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
                assert_eq!(
                    unsafe { libc::lseek(fd, position, libc::SEEK_SET) },
                    position,
                    "{context}"
                );
                buf.fill(0xaa);
                let rest = &expected[(position as usize).min(expected.len())..];
                let answer = if rest.is_empty() {
                    Ok(0)
                } else {
                    Err(libc::EINVAL)
                };
                assert_eq!(getdents(fd, &mut buf, count), answer, "{context}");
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                assert_eq!(
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) },
                    position,
                    "{context}"
                );
                let mut names = Vec::new();
                loop {
                    let n = getdents(fd, &mut buf, 4096).unwrap();
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n], name_offset));
                }
                assert_eq!(names, rest, "{context}");
            }
        }
    }

    println!("count after rewind and seek ok");
}

fn count_above_int_max_after_rewind_guest() {
    // Negative as an `int`.
    count_after_rewind_and_seek(&[0x8000_0000, 0xffff_ffff]);
}

#[test]
fn count_above_int_max_after_rewind_matches_linux() {
    run_five_times(count_above_int_max_after_rewind_guest);
}

fn count_too_small_after_rewind_guest() {
    // Smaller than the 24 bytes of the shortest record. The count is an
    // `unsigned int`, so the last is 0.
    count_after_rewind_and_seek(&[0, 1, 10, 23, 1 << 32]);
}

#[test]
fn count_too_small_after_rewind_matches_linux() {
    run_five_times(count_too_small_after_rewind_guest);
}

/// Make `call` with `rsp` at `stack` instead of on this thread's stack.
fn syscall_on_stack(
    call: libc::c_long,
    fd: i32,
    buf: *mut u8,
    count: u64,
    stack: *mut u8,
) -> Result<usize, i32> {
    let result: i64;
    unsafe {
        std::arch::asm!(
            "mov {saved}, rsp",
            "mov rsp, {stack}",
            "syscall",
            "mov rsp, {saved}",
            stack = in(reg) stack,
            saved = out(reg) _,
            inlateout("rax") call => result,
            in("rdi") fd as i64,
            in("rsi") buf,
            in("rdx") count,
            lateout("rcx") _,
            lateout("r11") _,
        );
    }
    if result < 0 {
        Err(-result as i32)
    } else {
        Ok(result as usize)
    }
}

fn negative_count_stack_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let page = 4096;

    // After a rewind and a seek, Detcore must read the host directory to
    // answer a count that is negative as an `int`. Linux writes nothing to
    // answer it, so neither may Detcore: not to the buffer, and not below
    // the stack pointer, where the guest could later read what the host
    // returned. The stack is 256 bytes above a page the guest cannot touch,
    // or far from it.
    let pages = 16;
    let map = guarded_pages(pages, pages, libc::PROT_NONE);
    assert_eq!(
        unsafe { libc::mprotect(map.cast(), page, libc::PROT_NONE) },
        0
    );
    let stack = unsafe { std::slice::from_raw_parts(map.add(page), (pages - 1) * page) };
    let mut buf = vec![0xaa_u8; 4096];
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        for above in [256, 12 * page] {
            for position in [5_i64, 22] {
                let context = format!(
                    "syscall {call}, stack {above} bytes above the guard, position {position}"
                );
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                while getdents64(fd, &mut buf).unwrap() > 0 {}
                assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
                assert_eq!(
                    unsafe { libc::lseek(fd, position, libc::SEEK_SET) },
                    position
                );
                buf.fill(0xaa);
                let answer = if position < 22 {
                    Err(libc::EINVAL)
                } else {
                    Ok(0)
                };
                let result = syscall_on_stack(call, fd, buf.as_mut_ptr(), 0x8000_0000, unsafe {
                    map.add(page + above)
                });
                assert_eq!(result, answer, "{context}");
                assert!(
                    buf.iter().all(|&byte| byte == 0xaa),
                    "{context}: the buffer was written"
                );
                let changed = stack.iter().filter(|&&byte| byte != 0xaa).count();
                assert_eq!(
                    changed, 0,
                    "{context}: {changed} bytes of the stack were written"
                );
                assert_eq!(
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) },
                    position,
                    "{context}"
                );
                let mut names = Vec::new();
                loop {
                    let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
                    assert!(n >= 0, "{context}");
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n as usize], name_offset));
                }
                assert_eq!(names, listing_of(20)[position as usize..], "{context}");
            }
        }
    }
    unsafe { libc::munmap(map.cast(), pages * page) };

    println!("negative count stack ok");
}

#[test]
fn negative_count_writes_nothing_below_the_stack_pointer() {
    run_five_times(negative_count_stack_guest);
}

fn record_before_writable_page_guest() {
    let page = 4096;

    // The buffer starts 16 bytes before a writable page, in a page the guest
    // cannot write. The record for `..` crosses into the writable page, but
    // Linux's first write to it, its `d_off`, fails, so the call fails and
    // writes nothing; the stream stays at `..`. Both with entries after `..`
    // and with `..` the last entry, in an empty directory.
    for count in [20, 0] {
        let root = tempfile::tempdir().unwrap();
        for index in 0..count {
            File::create(root.path().join(name(index))).unwrap();
        }
        let expected = listing_of(count);
        for protection in [libc::PROT_NONE, libc::PROT_READ] {
            for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
                let context =
                    format!("{count} files, syscall {call}, first page protection {protection}");
                let map = guarded_pages(2, 2, libc::PROT_NONE);
                assert_eq!(unsafe { libc::mprotect(map.cast(), page, protection) }, 0);
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                let mut first = [0u8; 24];
                assert_eq!(getdents64(fd, &mut first), Ok(24), "{context}");
                let mut names = record_names(&first, 19);
                let result = unsafe { libc::syscall(call, fd, map.add(page - 16), 256) };
                assert_eq!(result, -1, "{context}");
                assert_eq!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EFAULT),
                    "{context}"
                );
                assert_eq!(
                    unsafe { libc::mprotect(map.cast(), page, libc::PROT_READ | libc::PROT_WRITE) },
                    0
                );
                let bytes = unsafe { std::slice::from_raw_parts(map, 2 * page) };
                let changed = bytes.iter().filter(|&&byte| byte != 0xaa).count();
                assert_eq!(changed, 0, "{context}: {changed} bytes were written");
                let mut buf = [0u8; 4096];
                loop {
                    let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
                    assert!(n >= 0, "{context}");
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n as usize], name_offset));
                }
                assert_eq!(names, expected, "{context}");
                unsafe { libc::munmap(map.cast(), 2 * page) };
            }
        }
    }

    println!("record before writable page ok");
}

#[test]
fn record_starting_in_unwritable_page_leaves_next_page_untouched() {
    run_five_times(record_before_writable_page_guest);
}

fn fresh_write_only_buffer_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);
    let page = 4096;

    // The first call on a descriptor, into a buffer the guest can write but
    // not read. Linux fills it; Detcore must not need to read it back.
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        let context = format!("syscall {call}");
        let map = guarded_pages(1, 0, libc::PROT_WRITE);
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        let result = unsafe { libc::syscall(call, fd, map, 256) };
        // `.`, `..` and six 32-byte records.
        assert_eq!(result, 240, "{context}");
        assert_eq!(
            unsafe { libc::mprotect(map.cast(), page, libc::PROT_READ | libc::PROT_WRITE) },
            0
        );
        let bytes = unsafe { std::slice::from_raw_parts(map, page) };
        let mut names = record_names(&bytes[..240], name_offset);
        assert!(bytes[240..].iter().all(|&byte| byte == 0xaa), "{context}");
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), 4096) };
            assert!(n >= 0, "{context}");
            if n == 0 {
                break;
            }
            names.extend(record_names(&buf[..n as usize], name_offset));
        }
        assert_eq!(names, expected, "{context}");
        unsafe { libc::munmap(map.cast(), page) };
    }

    println!("fresh write-only buffer ok");
}

#[test]
fn fresh_write_only_buffer_is_filled() {
    run_five_times_without_hashing_buffers(fresh_write_only_buffer_guest);
}

fn write_only_buffer_refused_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }

    // Hashing the bytes a call returns needs them read back from the
    // guest's buffer, which cannot be read here, so the call fails rather
    // than log bytes other than those the guest holds. Main does the same.
    // (The legacy `getdents` is not among the calls whose bytes are hashed.)
    let map = guarded_pages(1, 0, libc::PROT_WRITE);
    let dir = File::open(root.path()).unwrap();
    let result = unsafe { libc::syscall(libc::SYS_getdents64, dir.as_raw_fd(), map, 256) };
    let errno = std::io::Error::last_os_error().raw_os_error();
    assert_eq!((result, errno), (-1, Some(libc::EFAULT)));
    unsafe { libc::munmap(map.cast(), 4096) };

    println!("write-only buffer refused ok");
}

#[test]
fn write_only_buffer_is_refused_while_hashing_buffers() {
    run_five_times_hashing_buffers(write_only_buffer_refused_guest);
}

/// Map every free range of the address space, largest first, so that Detcore
/// can map nothing in the guest to read a directory into. Nothing may
/// allocate until the ranges are unmapped again.
fn occupy_address_space() -> Vec<(*mut libc::c_void, usize)> {
    let mut maps = Vec::with_capacity(4096);
    let mut len = 1usize << 46;
    while len >= 4096 {
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if map == libc::MAP_FAILED {
            len /= 2;
        } else {
            assert!(maps.len() < maps.capacity());
            maps.push((map, len));
        }
    }
    maps
}

/// Unmap the smallest of `maps`, returning its length. That leaves room for
/// less than the 64KiB Detcore would map, but for the one page it maps
/// instead, which the whole directory is read through.
fn free_smallest(maps: &mut Vec<(*mut libc::c_void, usize)>) -> usize {
    let smallest = (0..maps.len()).min_by_key(|&index| maps[index].1).unwrap();
    let (map, freed) = maps.swap_remove(smallest);
    unsafe { libc::munmap(map, freed) };
    freed
}

fn release(maps: Vec<(*mut libc::c_void, usize)>) {
    for &(map, len) in &maps {
        unsafe { libc::munmap(map, len) };
    }
}

fn address_space_full_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in (0..20).rev() {
        File::create(root.path().join(name(index))).unwrap();
    }
    let expected = listing_of(20);
    let dir = File::open(root.path()).unwrap();
    let fd = dir.as_raw_fd();
    let mut buf = vec![0u8; 4096];

    // With the address space full, a count that is negative as an `int`
    // still gets Linux's answer on a stream never read, which needs no
    // snapshot: EINVAL, as entries remain. The first read then fails with
    // `ENOMEM` and reads nothing.
    let mut maps = occupy_address_space();
    let negative = raw_getdents64(fd, buf.as_mut_ptr(), 0x8000_0000);
    let full = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    let freed = free_smallest(&mut maps);
    let first = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    release(maps);

    assert_eq!(negative, Err(libc::EINVAL));
    assert_eq!(full, Err(libc::ENOMEM));
    assert!(freed < 64 * 1024, "{freed} bytes freed");
    let n = first.unwrap();
    let mut names = record_names(&buf[..n], 19);
    names.extend(drain_names(fd));
    assert_eq!(names, expected);

    // Rewound and then seeked past the start, the stream needs a snapshot
    // again, to know how many entries the directory has even for a negative
    // count. With none possible, both calls fail with `ENOMEM`, writing and
    // moving nothing; with a page free, the stream goes on from where it was
    // seeked to.
    assert_eq!(unsafe { libc::lseek(fd, 0, libc::SEEK_SET) }, 0);
    assert_eq!(unsafe { libc::lseek(fd, 3, libc::SEEK_SET) }, 3);
    buf.fill(0xaa);
    let mut maps = occupy_address_space();
    let negative = raw_getdents64(fd, buf.as_mut_ptr(), 0x8000_0000);
    let full = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    let position = unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) };
    let untouched = buf.iter().all(|&byte| byte == 0xaa);
    free_smallest(&mut maps);
    let first = raw_getdents64(fd, buf.as_mut_ptr(), 4096);
    release(maps);

    assert_eq!(negative, Err(libc::ENOMEM));
    assert_eq!(full, Err(libc::ENOMEM));
    assert_eq!(position, 3);
    assert!(untouched);
    let n = first.unwrap();
    let mut names = record_names(&buf[..n], 19);
    names.extend(drain_names(fd));
    assert_eq!(names, expected[3..]);

    println!("address space full ok");
}

/// Detcore reads the whole directory into a mapping it makes in the guest
/// before returning the first entries. When no mapping can be made, the call
/// fails with `ENOMEM` (Linux needs no memory here) and leaves the stream
/// where it was, rather than return entries in the host's order; a later
/// call with a page free lists the directory in order. So does a count that
/// is negative as an `int` on a stream that needs a snapshot to answer it.
#[test]
fn full_address_space_fails_without_losing_order() {
    run_five_times(address_space_full_guest);
}

fn negative_count_untracked_guest() {
    let root = tempfile::tempdir().unwrap();
    for index in 0..20 {
        File::create(root.path().join(name(index))).unwrap();
    }

    // A descriptor Detcore does not track, and one it reads in host order
    // because it was seeked to a host position before its first read: a
    // count that is negative as an `int` fails with EINVAL while entries
    // remain and returns 0 at the end, writing nothing and moving nothing.
    let mut buf = vec![0xaa_u8; 4096];
    let getdents = |call: libc::c_long, fd: i32, buf: &mut [u8], count: u64| {
        let n = unsafe { libc::syscall(call, fd, buf.as_mut_ptr(), count) };
        if n < 0 {
            Err(std::io::Error::last_os_error().raw_os_error().unwrap())
        } else {
            Ok(n as usize)
        }
    };
    for call in [libc::SYS_getdents64, libc::SYS_getdents] {
        for host_order in [false, true] {
            let context = format!("syscall {call}, host order {host_order}");
            let probe = File::open(root.path()).unwrap();
            let fd = receive_descriptor(probe.as_raw_fd());
            drop(probe);
            if host_order {
                let n = getdents64(fd, &mut buf).unwrap();
                let cookie = records(&buf[..n])
                    .into_iter()
                    .find_map(|(name, off)| (name == ".").then_some(off))
                    .expect("no `.` entry");
                unsafe { libc::close(fd) };
                let dir = File::open(root.path()).unwrap();
                let fd = dir.as_raw_fd();
                assert_eq!(unsafe { libc::lseek(fd, cookie, libc::SEEK_SET) }, cookie);
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0x8000_0000),
                    Err(libc::EINVAL),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                assert_eq!(
                    unsafe { libc::lseek(fd, 0, libc::SEEK_CUR) },
                    cookie,
                    "{context}"
                );
                while getdents(call, fd, &mut buf, 4096).unwrap() > 0 {}
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0xffff_ffff),
                    Ok(0),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
            } else {
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0x8000_0000),
                    Err(libc::EINVAL),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                // Every entry still follows, so the position is still 0.
                // (Asked with `lseek`, Hermit answers EBADF on the ptrace,
                // KVM and LiteInst backends, as for every call on a
                // descriptor received through `SCM_RIGHTS`: Detcore does not
                // track it.)
                let name_offset = if call == libc::SYS_getdents64 { 19 } else { 18 };
                let mut names = Vec::new();
                loop {
                    let n = getdents(call, fd, &mut buf, 4096).unwrap();
                    if n == 0 {
                        break;
                    }
                    names.extend(record_names(&buf[..n], name_offset));
                }
                let mut expected: Vec<String> = (0..20).map(name).collect();
                expected.extend([".".to_owned(), "..".to_owned()]);
                assert_eq!(sorted(names), sorted(expected), "{context}");
                buf.fill(0xaa);
                assert_eq!(
                    getdents(call, fd, &mut buf, 0xffff_ffff),
                    Ok(0),
                    "{context}"
                );
                assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
                unsafe { libc::close(fd) };
            }
        }
    }

    // A descriptor Detcore tracks, rewound to 0 after it was read to the
    // end, while a descriptor it does not track, for the same open file,
    // leaves the kernel's position at the end. Linux keeps one position for
    // both, so there nothing follows and a negative count returns 0.
    // Detcore keeps the tracked stream's own position, from which every
    // entry follows. Either way the answer must agree with what follows:
    // EINVAL while entries remain, 0 at the end.
    let mut all: Vec<String> = (0..20).map(name).collect();
    all.extend([".".to_owned(), "..".to_owned()]);
    for (call, name_offset) in [(libc::SYS_getdents64, 19), (libc::SYS_getdents, 18)] {
        let context = format!("syscall {call}, rewound with an untracked alias at the end");
        let dir = File::open(root.path()).unwrap();
        let fd = dir.as_raw_fd();
        while getdents(call, fd, &mut buf, 4096).unwrap() > 0 {}
        assert_eq!(
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) },
            0,
            "{context}"
        );
        let alias = receive_descriptor(fd);
        while getdents(call, alias, &mut buf, 4096).unwrap() > 0 {}
        unsafe { libc::close(alias) };
        buf.fill(0xaa);
        let answer = getdents(call, fd, &mut buf, 0x8000_0000);
        assert!(buf.iter().all(|&byte| byte == 0xaa), "{context}");
        let mut names = Vec::new();
        loop {
            let n = getdents(call, fd, &mut buf, 4096).unwrap();
            if n == 0 {
                break;
            }
            names.extend(record_names(&buf[..n], name_offset));
        }
        let followed = names.len();
        let expected = if names.is_empty() {
            Ok(0)
        } else {
            assert_eq!(sorted(names), sorted(all.clone()), "{context}");
            Err(libc::EINVAL)
        };
        assert_eq!(answer, expected, "{context}: {followed} entries followed");
    }

    println!("negative count untracked ok");
}

#[test]
fn negative_count_on_untracked_and_host_order_descriptors() {
    run_five_times(negative_count_untracked_guest);
}
