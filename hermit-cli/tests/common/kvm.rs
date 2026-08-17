/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum KvmAccessErrorKind {
    Absent,
    Denied,
    Unsupported,
    Other,
}

#[derive(Debug)]
pub(crate) struct KvmAccessError {
    pub(crate) kind: KvmAccessErrorKind,
    pub(crate) message: String,
}

impl fmt::Display for KvmAccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(crate) fn probe_kvm(path: &Path) -> Result<(), KvmAccessError> {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(|_| ())
        .map_err(|error| kvm_open_error(path, error))
}

pub(crate) fn kvm_open_error(path: &Path, error: io::Error) -> KvmAccessError {
    let raw_errno = error.raw_os_error();
    let errno = raw_errno
        .map(|code| format!("{:?} ({code})", nix::errno::Errno::from_raw(code)))
        .unwrap_or_else(|| "unknown".to_owned());
    let operation = format!("open({}, O_RDWR)", path.display());

    let (kind, meaning) = match raw_errno {
        Some(libc::ENOENT) => (
            KvmAccessErrorKind::Absent,
            "KVM is unavailable because the device node does not exist",
        ),
        Some(libc::EACCES) | Some(libc::EPERM) => (
            KvmAccessErrorKind::Denied,
            "KVM may be available, but this process was denied read-write access",
        ),
        Some(libc::ENODEV | libc::ENXIO | libc::ENOSYS | libc::EOPNOTSUPP) => (
            KvmAccessErrorKind::Unsupported,
            "KVM is unavailable because the device operation is unsupported",
        ),
        _ => (
            KvmAccessErrorKind::Other,
            "KVM availability could not be determined from this open failure",
        ),
    };

    KvmAccessError {
        kind,
        message: format!("{operation} failed with errno {errno}: {error}; {meaning}"),
    }
}

/// Returns true when `/dev/kvm` can be opened read-write.
///
/// A missing device skips the hardware-dependent test. Permission denials and
/// other failures are test failures because treating them as absence would hide
/// a present but unusable backend.
pub fn kvm_available() -> bool {
    match probe_kvm(Path::new("/dev/kvm")) {
        Ok(()) => true,
        Err(error) if error.kind == KvmAccessErrorKind::Absent => {
            eprintln!("skipping KVM test: {error}");
            false
        }
        Err(error) => panic!("KVM availability probe failed: {error}"),
    }
}
