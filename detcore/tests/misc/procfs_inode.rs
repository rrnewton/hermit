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
use std::os::fd::FromRawFd;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Duration;

use detcore::Config;
use detcore::Detcore;
use detcore::InheritedSeccomp;
use reverie::ExitStatus;

fn config() -> Config {
    Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        // As Hermit does: the guest inherits this thread's seccomp filters.
        inherited_seccomp: InheritedSeccomp::measure(),
        ..Default::default()
    }
}

/// Run `guest` under Detcore and require that it exits 0.
fn under_detcore(guest: impl FnOnce() + Send) {
    under_detcore_with(config(), guest);
}

/// Run `guest` under Detcore with `config`, require that it exits 0, and
/// return the run's global state.
fn under_detcore_with(config: Config, guest: impl FnOnce() + Send) -> detcore::GlobalState {
    let (output, state) =
        detcore_testutils::test_fn_with_config::<Detcore, _>(guest, config, true).unwrap();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    state
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

/// By the time Detcore could read a stat's path again, a result written
/// over it may name another entry. Here it does: x86_64's `struct stat`
/// holds `st_mode` at offset 24, so a path placed there reads "mA" once the
/// result for a directory with mode 0o40555 is written, and "mA" in the
/// guest's directory leads to `/proc/self/fd`. The stat reports
/// `/proc/self/task`, which Linux looked up before it wrote the result.
/// Detcore must not give it `/proc/self/fd`'s number; it records a loss.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_overwritten_path_does_not_name_the_entry_it_now_leads_to() {
    let links = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/proc/self/task", links.path().join("orig")).unwrap();
    std::os::unix::fs::symlink("/proc/self/fd", links.path().join("mA")).unwrap();
    under_detcore(|| {
        let fds = std::fs::metadata("/proc/self/fd").unwrap().ino();
        let directory = File::open(links.path()).unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let buffer = stat.as_mut_ptr().cast::<u8>();
        let path = b"orig\0";
        let at = unsafe { buffer.add(24) };
        unsafe { std::ptr::copy_nonoverlapping(path.as_ptr(), at, path.len()) };
        let result =
            unsafe { libc::syscall(libc::SYS_newfstatat, directory.as_raw_fd(), at, buffer, 0) };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        let stat = unsafe { stat.assume_init() };
        assert_eq!(
            stat.st_mode,
            libc::S_IFDIR | 0o555,
            "the result wrote no \"mA\""
        );
        assert_ne!(stat.st_ino, fds, "the entry took /proc/self/fd's number");
        let task = std::fs::metadata("/proc/self/task").unwrap().ino();
        assert_eq!(task, stat.st_ino, "named, the entry was renumbered");
    });
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some("procfs inode numbers: the call's result overwrote the guest's path")
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
    // Linux before 6.11 refuses a NULL path, and the guest must see what the
    // host's kernel does.
    let native = cwd_link_count(None)
        .map(drop)
        .map_err(|error| error.raw_os_error());
    under_detcore(|| {
        std::env::set_current_dir("/proc").unwrap();
        let empty = CString::new("").unwrap();
        assert_eq!(cwd_link_count(Some(&empty)).unwrap(), 1, "an empty path");
        match native {
            Ok(()) => assert_eq!(cwd_link_count(None).unwrap(), 1, "a NULL path"),
            Err(errno) => {
                let error = cwd_link_count(None).unwrap_err();
                assert_eq!(error.raw_os_error(), errno, "a NULL path");
            }
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

/// The guest lists a thread's descriptor directory in two calls, the thread
/// exits between them, and its directory loses its name: the second call
/// returns entries from the snapshot the first took, keyed as they were
/// then. A descriptor entry rebuilt before the listing keeps the number
/// `lstat` reported, and no determinism loss is recorded, where keying the
/// second call again found nothing to name the directory by.
#[test]
fn a_listing_keeps_its_keys_after_its_task_exits() {
    under_detcore(|| {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            sender.send(unsafe { libc::gettid() }).unwrap();
            finished.recv().unwrap();
        });
        let tid = receiver.recv().unwrap();
        let task = format!("{}/task/{tid}", proc_dir());
        let fds = format!("{task}/fd");
        let fd = File::open("/dev/null").unwrap().into_raw_fd();
        let entry = format!("{fds}/{fd}");
        let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
        let stat_before = lstat().unwrap();
        assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
        // The lookup of a closed descriptor's entry fails and drops its
        // dentry, which the listing builds again.
        assert!(lstat().is_err(), "{entry} exists after close");
        let again = File::open("/dev/null").unwrap().into_raw_fd();
        assert_eq!(again, fd, "the reopened file must take the closed number");
        let directory = File::open(&fds).unwrap();
        // `.` and `..` take 24 bytes each, so the first call returns only
        // them, and takes the snapshot.
        let mut buffer = vec![0u8; 48];
        let read = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                directory.as_raw_fd(),
                buffer.as_mut_ptr(),
                buffer.len(),
            )
        };
        assert_eq!(read, 48, "getdents64: {}", std::io::Error::last_os_error());
        finish.send(()).unwrap();
        thread.join().unwrap();
        // The join returns when the thread clears its ID, before it is reaped.
        let mut tries = 0;
        while std::fs::symlink_metadata(&task).is_ok() {
            tries += 1;
            assert!(tries < 100_000, "{task} outlived its thread");
            unsafe { libc::sched_yield() };
        }
        let rest = rest_of_listing(&directory);
        assert_eq!(
            rest.get(&fd.to_string()).copied(),
            Some(stat_before),
            "{fds} listed {fd} under another number: {rest:?}"
        );
    });
    assert_eq!(detcore::detlog::determinism_loss(), None);
}

/// The entries `getdents64` returns from `directory`'s position on, by name.
fn rest_of_listing(directory: &File) -> std::collections::HashMap<String, u64> {
    let mut buffer = vec![0u8; 32 * 1024];
    let mut entries = std::collections::HashMap::new();
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
            return entries;
        }
        let mut offset = 0;
        while offset < read as usize {
            let record = &buffer[offset..];
            let ino = u64::from_ne_bytes(record[0..8].try_into().unwrap());
            let length = usize::from(u16::from_ne_bytes(record[16..18].try_into().unwrap()));
            let name = &record[19..length];
            let name = &name[..name.iter().position(|byte| *byte == 0).unwrap()];
            entries.insert(String::from_utf8_lossy(name).into_owned(), ino);
            offset += length;
        }
    }
}

/// The descriptor the `O_PATH` open that names a procfs path gets in the
/// filter tests: the lowest free one once the guest fills those below it.
#[cfg(target_arch = "x86_64")]
const FILTERED_FD: i32 = 100;

/// Linux's `ERESTARTSYS`, which it does not export to user space.
#[cfg(target_arch = "x86_64")]
const ERESTARTSYS: u32 = 512;

/// `AUDIT_ARCH_X86_64`, the `arch` of an x86_64 system call in seccomp data.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;

#[cfg(target_arch = "x86_64")]
fn load(offset: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
        jt: 0,
        jf: 0,
        k: offset,
    }
}

#[cfg(target_arch = "x86_64")]
fn jump_if(value: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k: value,
    }
}

#[cfg(target_arch = "x86_64")]
fn ret(action: u32) -> libc::sock_filter {
    libc::sock_filter {
        code: (libc::BPF_RET | libc::BPF_K) as u16,
        jt: 0,
        jf: 0,
        k: action,
    }
}

/// Install `program` as a seccomp filter of this thread and the processes it
/// forks, as a filter a guest inherits from whatever started it can; Detcore
/// refuses a guest's own `seccomp(2)`. `flags` are `seccomp(2)`'s, and the
/// result is what it returns.
#[cfg(target_arch = "x86_64")]
fn install(program: &mut [libc::sock_filter], flags: libc::c_ulong) -> libc::c_long {
    let fprog = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    let installed = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            flags,
            &fprog as *const libc::sock_fprog,
        )
    };
    assert!(
        installed >= 0,
        "seccomp: {}",
        std::io::Error::last_os_error()
    );
    installed
}

/// Make `close(FILTERED_FD)` return `-errno` without running.
#[cfg(target_arch = "x86_64")]
fn filter_close(errno: u32) {
    // seccomp_data: nr at 0, arch at 4, and the low half of args[0] at 16.
    let mut program = [
        load(4),
        jump_if(AUDIT_ARCH_X86_64, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        load(0),
        jump_if(libc::SYS_close as u32, 0, 3),
        load(16),
        jump_if(FILTERED_FD as u32, 0, 1),
        ret(libc::SECCOMP_RET_ERRNO | errno),
        ret(libc::SECCOMP_RET_ALLOW),
    ];
    assert_eq!(install(&mut program, 0), 0);
}

/// Give `action` to every `openat` with exactly the flags of the open Detcore
/// injects to name a path it must not follow, `O_PATH | O_CLOEXEC |
/// O_NOFOLLOW`, and return what `seccomp(2)` returned for `flags`. The filter
/// applies to the test process too, which opens other paths with `O_PATH`
/// while it starts the guest, so it matches the flags exactly.
#[cfg(target_arch = "x86_64")]
fn filter_naming_opens(action: u32, flags: libc::c_ulong) -> libc::c_long {
    const NAMING: u32 = (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u32;
    // seccomp_data: nr at 0, arch at 4, and the low half of args[2] at 32.
    let mut program = [
        load(4),
        jump_if(AUDIT_ARCH_X86_64, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        load(0),
        jump_if(libc::SYS_openat as u32, 0, 3),
        load(32),
        jump_if(NAMING, 0, 1),
        ret(action),
        ret(libc::SECCOMP_RET_ALLOW),
    ];
    install(&mut program, flags)
}

/// The determinism loss Detcore records, before it injects any call to name a
/// procfs entry, for a guest that inherits the one filter a test installed.
const ONE_INHERITED_FILTER: &str = "procfs inode numbers: a seccomp filter the guest inherited may apply to its calls (mode: filter, with 1 filter)";

/// The determinism loss Detcore records when the one filter a test installed
/// answers an `fstat` that Detcore injects without running it.
const ONE_FILTER_FAKED_AN_FSTAT: &str = "file metadata: a seccomp filter the guest inherited answered an fstat Detcore injected without running it (mode: filter, with 1 filter)";

/// The determinism loss Detcore records when the one filter a test installed
/// answers an `fstatat` that Detcore injects, to look a path up, without
/// running it.
#[cfg(target_arch = "x86_64")]
const ONE_FILTER_FAKED_AN_FSTATAT: &str = "file metadata: a seccomp filter the guest inherited answered an fstatat Detcore injected without running it (mode: filter, with 1 filter)";

/// Fill the guest's descriptors below `FILTERED_FD`, which must be free, so
/// that the next descriptor the guest opens, or Detcore opens in it, is that
/// one.
#[cfg(target_arch = "x86_64")]
fn take_the_descriptors_below_the_filtered_one() {
    let open = |fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0;
    assert!(!open(FILTERED_FD), "{FILTERED_FD} is open from the start");
    let null = File::open("/dev/null").unwrap().into_raw_fd();
    let mut last = null;
    while last < FILTERED_FD - 1 {
        last = unsafe { libc::dup(null) };
        assert!(last >= 0, "dup: {}", std::io::Error::last_os_error());
    }
    assert_eq!(
        last,
        FILTERED_FD - 1,
        "a descriptor below {FILTERED_FD} was free"
    );
}

/// Open `/dev/zero` and read 8 bytes through it, returning its descriptor.
fn read_dev_zero() -> i32 {
    let zero = File::open("/dev/zero").unwrap().into_raw_fd();
    let mut bytes = [0xff_u8; 8];
    let read = unsafe { libc::read(zero, bytes.as_mut_ptr().cast(), bytes.len()) };
    assert_eq!(read, 8, "read: {}", std::io::Error::last_os_error());
    assert_eq!(bytes, [0; 8], "/dev/zero read nonzero bytes");
    zero
}

/// A filter the guest inherits can make the `O_PATH` open that names a path
/// return 0 without running, as if it had opened descriptor 0, so the close
/// that follows it would close the guest's descriptor 0. Detcore refuses to
/// name the entry before it injects the open, and descriptor 0 stays open.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_fakes_an_open_leaves_descriptor_0_open() {
    filter_naming_opens(libc::SECCOMP_RET_ERRNO, 0);
    under_detcore(|| {
        let stat = File::open("/proc/self/stat").unwrap().into_raw_fd();
        assert_eq!(unsafe { libc::dup2(stat, 0) }, 0, "dup2 onto 0");
        std::fs::symlink_metadata("/proc/self/stat").unwrap();
        assert!(
            unsafe { libc::fcntl(0, libc::F_GETFD) } >= 0,
            "the guest's descriptor 0 was closed"
        );
    });
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_INHERITED_FILTER)
    );
}

/// A filter the guest inherits can hand the `O_PATH` open to a supervisor
/// that never answers, here one that holds the listener and reads nothing,
/// which would stop the guest for good. Detcore refuses to name the entry
/// before it injects the open, so the guest's own stat completes. (Were the
/// open injected, the test would hang until the test runner's timeout.)
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_suspends_an_open_does_not_stop_the_guest() {
    let listener = filter_naming_opens(
        libc::SECCOMP_RET_USER_NOTIF,
        libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
    );
    let listener = unsafe { std::os::fd::OwnedFd::from_raw_fd(listener as i32) };
    under_detcore(|| {
        std::fs::symlink_metadata("/proc/self/stat").unwrap();
    });
    drop(listener);
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_INHERITED_FILTER)
    );
}

/// Give `action` to every call of system call `number` whose first argument
/// is `first`, and return what `seccomp(2)` returned for `flags`.
#[cfg(target_arch = "x86_64")]
fn filter_first_argument(
    number: libc::c_long,
    first: i32,
    action: u32,
    flags: libc::c_ulong,
) -> libc::c_long {
    // seccomp_data: nr at 0, arch at 4, and the low half of args[0] at 16.
    let mut program = [
        load(4),
        jump_if(AUDIT_ARCH_X86_64, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        load(0),
        jump_if(number as u32, 0, 3),
        load(16),
        jump_if(first as u32, 0, 1),
        ret(action),
        ret(libc::SECCOMP_RET_ALLOW),
    ];
    install(&mut program, flags)
}

/// Give `action` to every `fstat(fd)`, and return what `seccomp(2)` returned
/// for `flags`.
#[cfg(target_arch = "x86_64")]
fn filter_fstat(fd: i32, action: u32, flags: libc::c_ulong) -> libc::c_long {
    filter_first_argument(libc::SYS_fstat, fd, action, flags)
}

/// Give `action` to every `newfstatat(dirfd, ...)`, the call Detcore injects
/// to look a path up before a removal or a utimensat.
#[cfg(target_arch = "x86_64")]
fn filter_newfstatat(dirfd: i32, action: u32) {
    filter_first_argument(libc::SYS_newfstatat, dirfd, action, 0);
}

/// A filter the guest inherits can make the `fstat` Detcore injects, to learn
/// the metadata of a file the guest opened, return 0 without running, which
/// leaves its buffer as it was. Detcore would then take whatever the buffer
/// held for the file's metadata, without a determinism loss. While a filter
/// may apply, Detcore fills the buffer before the call, so it sees that
/// nothing was written, and records the loss. The guest's open still
/// succeeds, and its descriptor works.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_fakes_an_open_files_stat_records_a_determinism_loss() {
    filter_fstat(FILTERED_FD, libc::SECCOMP_RET_ERRNO, 0);
    under_detcore(|| {
        take_the_descriptors_below_the_filtered_one();
        assert_eq!(read_dev_zero(), FILTERED_FD);
    });
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_FILTER_FAKED_AN_FSTAT)
    );
}

/// While a filter the guest inherited may apply, opening a file that cannot be
/// a procfs one records no determinism loss: the `fstat` Detcore injects runs
/// and writes its buffer, and Detcore injects nothing to name the file.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_records_no_loss_for_a_file_that_is_not_procfs() {
    filter_fstat(FILTERED_FD, libc::SECCOMP_RET_ERRNO, 0);
    assert!(
        matches!(InheritedSeccomp::measure(), InheritedSeccomp::Filter(_)),
        "the guest inherits no filter"
    );
    under_detcore(|| {
        assert_ne!(read_dev_zero(), FILTERED_FD);
    });
    assert_eq!(detcore::detlog::determinism_loss(), None);
}

/// Open the directory `root`, create files `a` and `b` in it and number both,
/// apply `change` to `a` through the directory's descriptor, and check that
/// `b` keeps its number. With `filtered` the guest first fills the
/// descriptors below `FILTERED_FD`, so that the directory's is that one.
#[cfg(target_arch = "x86_64")]
fn change_a_file_through_a_directory_descriptor(
    root: &Path,
    filtered: bool,
    change: fn(i32, &CStr) -> libc::c_int,
) {
    if filtered {
        take_the_descriptors_below_the_filtered_one();
    }
    let directory = File::open(root).unwrap().into_raw_fd();
    assert_eq!(
        directory == FILTERED_FD,
        filtered,
        "the directory's descriptor is {directory}"
    );
    let number = |name: &str| std::fs::symlink_metadata(root.join(name)).unwrap().ino();
    File::create(root.join("a")).unwrap();
    File::create(root.join("b")).unwrap();
    number("a");
    let b = number("b");
    assert_eq!(
        change(directory, c"a"),
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    assert_eq!(number("b"), b, "b's number");
}

/// A filter the guest inherits can make the `fstatat` Detcore injects, to
/// learn which file a removal takes the last name of, return 0 without
/// running, which leaves its buffer as it was. Detcore would take the bytes
/// the buffer held for the file's device and inode, and could retire the
/// number of another file they named, whose next stat would then show a new
/// number, without a determinism loss. While a filter may apply, Detcore
/// fills the buffer before the lookup, so it sees that nothing was written,
/// records the loss, and retires nothing.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_fakes_a_removal_lookup_records_a_loss_and_retires_nothing() {
    let root = tempfile::tempdir().unwrap();
    filter_newfstatat(FILTERED_FD, libc::SECCOMP_RET_ERRNO);
    let state = under_detcore_with(config(), || {
        change_a_file_through_a_directory_descriptor(root.path(), true, |directory, name| unsafe {
            libc::unlinkat(directory, name.as_ptr(), 0)
        });
    });
    assert_eq!(state.retired_inodes(), Vec::new());
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_FILTER_FAKED_AN_FSTATAT)
    );
}

/// A filter the guest inherits can likewise make the `fstatat` Detcore
/// injects, to learn which file a utimensat changes, return 0 without
/// running, which would leave the virtual mtime update to whatever the buffer
/// held. While a filter may apply, Detcore fills the buffer before the
/// lookup, records the loss, and leaves the virtual mtime alone.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_fakes_a_utimensat_lookup_records_a_determinism_loss() {
    let root = tempfile::tempdir().unwrap();
    filter_newfstatat(FILTERED_FD, libc::SECCOMP_RET_ERRNO);
    under_detcore_with(config(), || {
        change_a_file_through_a_directory_descriptor(root.path(), true, |directory, name| unsafe {
            libc::utimensat(directory, name.as_ptr(), std::ptr::null(), 0)
        });
    });
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_FILTER_FAKED_AN_FSTATAT)
    );
}

/// While a filter the guest inherited may apply, the lookups before a
/// utimensat and a removal that it lets run record no determinism loss, and
/// the removal retires the number of the file whose last name it takes.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_records_no_loss_for_lookups_it_lets_run() {
    let root = tempfile::tempdir().unwrap();
    filter_newfstatat(FILTERED_FD, libc::SECCOMP_RET_ERRNO);
    assert!(
        matches!(InheritedSeccomp::measure(), InheritedSeccomp::Filter(_)),
        "the guest inherits no filter"
    );
    let state = under_detcore_with(config(), || {
        change_a_file_through_a_directory_descriptor(
            root.path(),
            false,
            |directory, name| unsafe {
                let touched = libc::utimensat(directory, name.as_ptr(), std::ptr::null(), 0);
                if touched != 0 {
                    return touched;
                }
                libc::unlinkat(directory, name.as_ptr(), 0)
            },
        );
    });
    let retired = state.retired_inodes();
    assert_eq!(retired.len(), 1, "retired: {retired:?}");
    assert_eq!(detcore::detlog::determinism_loss(), None);
}

/// The length of the `struct stat` buffer of a lookup Detcore injects, and
/// so of the transient page it once mapped for one while a filter may apply.
#[cfg(target_arch = "x86_64")]
const STAT_LEN: usize = std::mem::size_of::<libc::stat>();

/// Give `action` to every `mmap` of `len` bytes, and return what
/// `seccomp(2)` returned for `flags`. The test process maps nothing that
/// small, so only an `mmap` Detcore injects matches.
#[cfg(target_arch = "x86_64")]
fn filter_mmap_of(len: usize, action: u32, flags: libc::c_ulong) -> libc::c_long {
    // seccomp_data: nr at 0, arch at 4, and args[1], the length, at 24 (low
    // half) and 28 (high half).
    let mut program = [
        load(4),
        jump_if(AUDIT_ARCH_X86_64, 1, 0),
        ret(libc::SECCOMP_RET_ALLOW),
        load(0),
        jump_if(libc::SYS_mmap as u32, 0, 5),
        load(24),
        jump_if(len as u32, 0, 3),
        load(28),
        jump_if((len >> 32) as u32, 0, 1),
        ret(action),
        ret(libc::SECCOMP_RET_ALLOW),
    ];
    install(&mut program, flags)
}

/// The seconds `touch_and_remove` sets a file's times to.
#[cfg(target_arch = "x86_64")]
const TOUCHED: libc::time_t = 1_000_000_000;

/// Set the times of `name` in `directory` to `TOUCHED` seconds, check that
/// its mtime shows them, and remove it.
#[cfg(target_arch = "x86_64")]
fn touch_and_remove(directory: i32, name: &CStr) -> libc::c_int {
    let times = [libc::timespec {
        tv_sec: TOUCHED,
        tv_nsec: 0,
    }; 2];
    let touched = unsafe { libc::utimensat(directory, name.as_ptr(), times.as_ptr(), 0) };
    if touched != 0 {
        return touched;
    }
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        },
        0,
        "fstatat: {}",
        std::io::Error::last_os_error()
    );
    assert_eq!(
        (stat.st_mtime, stat.st_mtime_nsec),
        (TOUCHED, 0),
        "the mtime the guest sees after utimensat"
    );
    unsafe { libc::unlinkat(directory, name.as_ptr(), 0) }
}

/// Run `touch_and_remove` on a file under Detcore while `mmap_action` is the
/// action of an inherited filter on every `mmap` of a lookup buffer's length,
/// and check that the utimensat moved the mtime the guest sees, that the
/// removal retired the file's number, and that nothing was lost.
#[cfg(target_arch = "x86_64")]
fn touch_and_remove_while_mmap_gets(mmap_action: u32) {
    let root = tempfile::tempdir().unwrap();
    filter_mmap_of(STAT_LEN, mmap_action, 0);
    let state = under_detcore_with(config(), || {
        change_a_file_through_a_directory_descriptor(root.path(), false, touch_and_remove);
    });
    let retired = state.retired_inodes();
    assert_eq!(retired.len(), 1, "retired: {retired:?}");
    assert_eq!(detcore::detlog::determinism_loss(), None);
}

/// A filter the guest inherits can deny the `mmap` of the transient page in
/// which Detcore looked a path up while a filter may apply. The lookups
/// before a utimensat and a removal then failed as if the path did not
/// resolve: the virtual mtime stayed where it was, and the removal retired
/// nothing, without a determinism loss. Detcore injects no `mmap` while a
/// filter may apply, and checks the lookups in the guest stack scratch, so
/// both work as without the filter.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_denies_an_mmap_leaves_the_lookups_working() {
    touch_and_remove_while_mmap_gets(libc::SECCOMP_RET_ERRNO | libc::EPERM as u32);
}

/// The same for a filter that makes that `mmap` return 0 without running,
/// which Detcore recorded as a determinism loss.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_fakes_an_mmap_leaves_the_lookups_working() {
    touch_and_remove_while_mmap_gets(libc::SECCOMP_RET_ERRNO);
}

/// Linux's `struct seccomp_notif`, which `SECCOMP_IOCTL_NOTIF_RECV` fills in.
#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct SeccompNotif {
    id: u64,
    pid: u32,
    flags: u32,
    data: [u8; 64],
}

/// Linux's `struct seccomp_notif_resp`, which `SECCOMP_IOCTL_NOTIF_SEND`
/// reads.
#[cfg(target_arch = "x86_64")]
#[repr(C)]
struct SeccompNotifResp {
    id: u64,
    val: i64,
    error: i32,
    flags: u32,
}

/// `_IOWR('!', 0, struct seccomp_notif)`.
#[cfg(target_arch = "x86_64")]
const SECCOMP_IOCTL_NOTIF_RECV: libc::c_ulong = 0xc050_2100;

/// `_IOWR('!', 1, struct seccomp_notif_resp)`.
#[cfg(target_arch = "x86_64")]
const SECCOMP_IOCTL_NOTIF_SEND: libc::c_ulong = 0xc018_2101;

/// Answer every call `listener` is notified of with success and `address`
/// for a result, as a supervisor can for an `mmap`, until `stop`, and count
/// the calls in `answered`.
#[cfg(target_arch = "x86_64")]
fn answer_with(
    listener: i32,
    address: usize,
    stop: &std::sync::atomic::AtomicBool,
    answered: &std::sync::atomic::AtomicUsize,
) {
    use std::sync::atomic::Ordering;
    while !stop.load(Ordering::Relaxed) {
        let mut ready = libc::pollfd {
            fd: listener,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut ready, 1, 100) } <= 0 {
            continue;
        }
        // Linux requires the buffer to be zeroed.
        let mut notification = SeccompNotif {
            id: 0,
            pid: 0,
            flags: 0,
            data: [0; 64],
        };
        let received = unsafe {
            libc::ioctl(
                listener,
                SECCOMP_IOCTL_NOTIF_RECV,
                &mut notification as *mut SeccompNotif,
            )
        };
        if received != 0 {
            continue;
        }
        let response = SeccompNotifResp {
            id: notification.id,
            val: address as i64,
            error: 0,
            flags: 0,
        };
        unsafe {
            libc::ioctl(
                listener,
                SECCOMP_IOCTL_NOTIF_SEND,
                &response as *const SeccompNotifResp,
            )
        };
        answered.fetch_add(1, Ordering::Relaxed);
    }
}

/// A filter the guest inherits can hand that `mmap` to a supervisor, which
/// can answer it with the address of a page the guest uses. Detcore, taking
/// the page for its own, filled it, wrote the lookup there and unmapped it,
/// so the guest lost the page's contents and then the page. Detcore injects
/// no `mmap` while a filter may apply, so the supervisor is never asked, the
/// guest's page keeps its contents, and the removal retires the file's
/// number without a determinism loss.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_answers_an_mmap_with_a_guest_page_leaves_the_page() {
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    const CANARY: u8 = 0x5c;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    // Mapped before the guest forks, so that the guest has it at the same
    // address.
    let page = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            page_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(
        page,
        libc::MAP_FAILED,
        "{}",
        std::io::Error::last_os_error()
    );
    unsafe { std::ptr::write_bytes(page.cast::<u8>(), CANARY, page_size) };
    let address = page as usize;
    let listener = filter_mmap_of(
        STAT_LEN,
        libc::SECCOMP_RET_USER_NOTIF,
        libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
    );
    let listener = unsafe { std::os::fd::OwnedFd::from_raw_fd(listener as i32) };
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let answered = std::sync::Arc::new(AtomicUsize::new(0));
    let supervisor = {
        let (stop, answered) = (stop.clone(), answered.clone());
        let listener = listener.as_raw_fd();
        std::thread::spawn(move || answer_with(listener, address, &stop, &answered))
    };
    let root = tempfile::tempdir().unwrap();
    let state = under_detcore_with(config(), || {
        change_a_file_through_a_directory_descriptor(
            root.path(),
            false,
            |directory, name| unsafe { libc::unlinkat(directory, name.as_ptr(), 0) },
        );
        let page = unsafe { std::slice::from_raw_parts(address as *const u8, page_size) };
        assert!(
            page.iter().all(|&byte| byte == CANARY),
            "the guest's page lost its contents"
        );
    });
    stop.store(true, Ordering::Relaxed);
    supervisor.join().unwrap();
    drop(listener);
    assert_eq!(
        answered.load(Ordering::Relaxed),
        0,
        "Detcore injected an mmap"
    );
    let retired = state.retired_inodes();
    assert_eq!(retired.len(), 1, "retired: {retired:?}");
    assert_eq!(detcore::detlog::determinism_loss(), None);
}

/// The determinism loss Detcore records when the one filter a test installed
/// makes an `fstatat` Detcore injects, to look a path up, fail with `EFAULT`.
#[cfg(target_arch = "x86_64")]
const ONE_FILTER_FAULTED_AN_FSTATAT: &str = "file metadata: Detcore could not check an fstatat it injected while a seccomp filter the guest inherited may apply to its calls: it faulted on memory Detcore had just read and written (mode: filter, with 1 filter)";

/// A filter the guest inherits can make the `fstatat` Detcore injects, to
/// learn which file a removal takes the last name of, fail with `EFAULT`,
/// which Linux cannot report for a path and a buffer Detcore had just read
/// and written. Detcore took it for the lookup's own error, as for a name
/// that does not resolve, and retired nothing without a determinism loss,
/// and the guard against a blocking open in a process that handles SIGALRM,
/// which shares the lookup, took it the same way. Detcore now records the
/// loss, retires nothing, and the guard refuses such an open.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_faults_a_removal_lookup_records_a_loss_and_retires_nothing() {
    let root = tempfile::tempdir().unwrap();
    filter_newfstatat(FILTERED_FD, libc::SECCOMP_RET_ERRNO | libc::EFAULT as u32);
    let state = under_detcore_with(config(), || {
        change_a_file_through_a_directory_descriptor(root.path(), true, |directory, name| unsafe {
            libc::unlinkat(directory, name.as_ptr(), 0)
        });
    });
    assert_eq!(state.retired_inodes(), Vec::new());
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_FILTER_FAULTED_AN_FSTATAT)
    );
}

/// Where the guest inherits a directory. Reverie closes descriptors 3 to 255
/// when it starts a guest (`init_tracee`), so the descriptor is above them.
#[cfg(target_arch = "x86_64")]
const INHERITED_DIRECTORY_FD: i32 = 300;

/// Leave a descriptor of the procfs directory `/proc/sys/vm` at
/// `INHERITED_DIRECTORY_FD` for the guest to inherit. Detcore does not see it
/// opened, so it has no `stat` of it.
#[cfg(target_arch = "x86_64")]
fn inherit_a_procfs_directory() {
    let directory = File::open("/proc/sys/vm").unwrap();
    assert_eq!(
        unsafe { libc::dup2(directory.as_raw_fd(), INHERITED_DIRECTORY_FD) },
        INHERITED_DIRECTORY_FD
    );
}

/// List the directory the guest inherited at `INHERITED_DIRECTORY_FD`.
#[cfg(target_arch = "x86_64")]
fn list_the_inherited_directory() {
    let directory = unsafe { File::from_raw_fd(INHERITED_DIRECTORY_FD) };
    let entries = rest_of_listing(&directory);
    assert!(
        entries.contains_key("overcommit_memory"),
        "/proc/sys/vm lists no overcommit_memory: {entries:?}"
    );
}

/// A filter the guest inherits can make the `fstat` Detcore injects, to learn
/// the device of a directory it did not see opened, return 0 without
/// running. Detcore would then read a `stat` the call did not write, which
/// describes no procfs file, and number the directory's procfs entries by
/// their host numbers without a determinism loss. Detcore injects no `fstat`
/// while a filter may apply, and records the loss.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_fakes_a_directory_stat_records_a_determinism_loss() {
    inherit_a_procfs_directory();
    filter_fstat(INHERITED_DIRECTORY_FD, libc::SECCOMP_RET_ERRNO, 0);
    under_detcore(list_the_inherited_directory);
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_INHERITED_FILTER)
    );
}

/// A filter the guest inherits can hand that `fstat` to a supervisor that
/// never answers, which would stop the guest for good. Detcore injects no
/// `fstat` while a filter may apply, so the listing completes. (Were it
/// injected, the test would hang until the test runner's timeout.)
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_suspends_a_directory_stat_does_not_stop_the_guest() {
    inherit_a_procfs_directory();
    let listener = filter_fstat(
        INHERITED_DIRECTORY_FD,
        libc::SECCOMP_RET_USER_NOTIF,
        libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
    );
    let listener = unsafe { std::os::fd::OwnedFd::from_raw_fd(listener as i32) };
    under_detcore(list_the_inherited_directory);
    drop(listener);
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_INHERITED_FILTER)
    );
}

/// The same for the `fstat` Detcore injects to number the descriptor that a
/// `/proc/<pid>/fdinfo/<n>` file describes, which it does when it has no
/// `stat` of the descriptor, as when it does not virtualize metadata.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_inherited_filter_that_suspends_a_descriptor_stat_does_not_stop_the_guest() {
    let listener = filter_fstat(
        FILTERED_FD,
        libc::SECCOMP_RET_USER_NOTIF,
        libc::SECCOMP_FILTER_FLAG_NEW_LISTENER,
    );
    let listener = unsafe { std::os::fd::OwnedFd::from_raw_fd(listener as i32) };
    let config = Config {
        virtualize_metadata: false,
        ..config()
    };
    under_detcore_with(config, || {
        let null = File::open("/dev/null").unwrap();
        assert_eq!(
            unsafe { libc::dup2(null.as_raw_fd(), FILTERED_FD) },
            FILTERED_FD
        );
        let fdinfo = std::fs::read_to_string(format!("/proc/self/fdinfo/{FILTERED_FD}")).unwrap();
        assert!(fdinfo.contains("\nino:"), "fdinfo has no inode: {fdinfo}");
    });
    drop(listener);
    assert_eq!(
        detcore::detlog::determinism_loss().as_deref(),
        Some(ONE_INHERITED_FILTER)
    );
}

/// Under SaBRe, Detcore shares libc, and so `errno`, with the guest thread
/// whose call it handles, so naming an entry must leave the guest's `errno`
/// as it was. These tests run Detcore under ptrace, in another process,
/// where it cannot change it, so this pins the guest's view; Detcore's unit
/// tests pin the guard itself.
#[test]
fn naming_an_entry_leaves_the_guests_errno() {
    under_detcore(|| {
        const SENTINEL: i32 = 4242;
        let path = CString::new("/proc/self/stat").unwrap();
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        unsafe { *libc::__errno_location() = SENTINEL };
        let result = unsafe {
            libc::syscall(
                libc::SYS_newfstatat,
                libc::AT_FDCWD,
                path.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        let errno = unsafe { *libc::__errno_location() };
        assert_eq!(result, 0, "newfstatat failed");
        assert_eq!(errno, SENTINEL, "naming the entry changed errno");
    });
    assert_eq!(detcore::detlog::determinism_loss(), None);
}

/// Run a guest that fills the descriptors below `FILTERED_FD`, so that the
/// `O_PATH` descriptor Detcore opens to name a procfs path takes it, and then
/// stats a procfs path. The filter leaves that descriptor open in the guest,
/// so the run must record a determinism loss, and must end. The configuration
/// says the guest inherits no filter, as a measurement that missed this one
/// would, so Detcore injects its calls and its close checks catch the filter.
#[cfg(target_arch = "x86_64")]
fn stat_a_procfs_path_with_close_filtered(errno: u32) -> Option<String> {
    filter_close(errno);
    let config = Config {
        inherited_seccomp: InheritedSeccomp::Disabled,
        ..config()
    };
    under_detcore_with(config, || {
        take_the_descriptors_below_the_filtered_one();
        std::fs::symlink_metadata("/proc/self/stat").unwrap();
        assert!(
            unsafe { libc::fcntl(FILTERED_FD, libc::F_GETFD) } >= 0,
            "Detcore's O_PATH descriptor was closed"
        );
    });
    detcore::detlog::determinism_loss()
}

/// A close that reports a restart every time has not run. Detcore tries it a
/// bounded number of times, where it had tried for ever, and records that
/// the descriptor stays open.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_close_that_never_runs_ends_in_a_determinism_loss() {
    let loss = stat_a_procfs_path_with_close_filtered(ERESTARTSYS);
    assert_eq!(
        loss.as_deref(),
        Some(
            "procfs inode numbers: close of the guest's O_PATH descriptor did not run in 8 attempts"
        )
    );
}

/// A close that reports success without running leaves the descriptor open,
/// which Detcore sees in `/proc/<tid>/fd`, where it had trusted the result.
#[cfg(target_arch = "x86_64")]
#[test]
fn a_close_that_reports_success_without_running_ends_in_a_determinism_loss() {
    let loss = stat_a_procfs_path_with_close_filtered(0);
    assert_eq!(
        loss.as_deref(),
        Some(
            "procfs inode numbers: close left the guest's O_PATH descriptor open, returning Ok(0)"
        )
    );
}
