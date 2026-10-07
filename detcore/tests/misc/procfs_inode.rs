/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! A procfs entry keeps its inode number when the host rebuilds it.
//!
//! Linux numbers a `/proc/<pid>/fd/<n>` entry from a host-wide counter when it
//! builds the entry's dentry, and builds it again under a new number once the
//! dentry is gone. Host memory pressure can evict it at any time, so a second
//! listing of `/proc/<pid>/fd` showed new numbers in one `--verify` run and
//! the old ones in the other (compat/lsof). Here the guest forces the rebuild:
//! it closes a descriptor, looks up the descriptor's entry, which fails and
//! drops the dentry, and opens a file at the same number again.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::fd::IntoRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use detcore::Config;
use detcore::Detcore;
use reverie::ExitStatus;

/// The inode numbers of one `/proc/self/fd/<n>` entry before and after it is
/// rebuilt, each as `getdents64` lists it and as `lstat` reports it.
#[derive(Debug)]
struct Sightings {
    listed_before: u64,
    stat_before: u64,
    listed_after: u64,
    stat_after: u64,
}

/// The inode number `getdents64` lists for `name` in `/proc/self/fd`.
fn listed(name: &str) -> u64 {
    let directory = File::open("/proc/self/fd").unwrap();
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
    found.unwrap_or_else(|| panic!("/proc/self/fd lists no {name}"))
}

fn rebuild_a_descriptor_entry() -> Sightings {
    let fd = File::open("/dev/null").unwrap().into_raw_fd();
    let name = fd.to_string();
    let entry = Path::new("/proc/self/fd").join(&name);
    let lstat = || std::fs::symlink_metadata(&entry).map(|metadata| metadata.ino());
    let listed_before = listed(&name);
    let stat_before = lstat().unwrap();
    assert_eq!(unsafe { libc::close(fd) }, 0, "close({fd})");
    // The lookup of a closed descriptor's entry fails and drops its dentry.
    assert!(lstat().is_err(), "{} exists after close", entry.display());
    let again = File::open("/dev/null").unwrap().into_raw_fd();
    assert_eq!(again, fd, "the reopened file must take the closed number");
    let listed_after = listed(&name);
    let stat_after = lstat().unwrap();
    assert_eq!(unsafe { libc::close(again) }, 0, "close({again})");
    Sightings {
        listed_before,
        stat_before,
        listed_after,
        stat_after,
    }
}

#[test]
fn a_rebuilt_proc_fd_entry_keeps_its_number() {
    // Natively the rebuilt entry gets a new number. Were the host to keep the
    // dentry, the guest below would keep its number at any Detcore revision.
    let native = rebuild_a_descriptor_entry();
    assert_ne!(
        native.listed_before, native.listed_after,
        "the host did not rebuild the entry, so this test cannot tell: {native:?}"
    );
    let config = Config {
        sequentialize_threads: true,
        max_timeslice: None,
        virtualize_metadata: true,
        ..Default::default()
    };
    let (output, _) = detcore_testutils::test_fn_with_config::<Detcore, _>(
        || {
            let seen = rebuild_a_descriptor_entry();
            let number = seen.listed_before;
            assert!(
                seen.stat_before == number
                    && seen.listed_after == number
                    && seen.stat_after == number,
                "the guest saw the rebuilt entry renumbered: {seen:?}"
            );
        },
        config,
        true,
    )
    .unwrap();
    assert_eq!(
        output.status,
        ExitStatus::Exited(0),
        "guest failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
