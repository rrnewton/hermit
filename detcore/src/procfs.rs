/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Minimal deterministic procfs exposed to guest programs.

use std::path::Component;
use std::path::Path;

const MAPS: &[u8] = b"00400000-00401000 r-xp 00000000 00:00 0 [hermit]\n";
const STAT: &[u8] = b"1 (hermit) R 0 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
const STATUS: &[u8] = b"Name:\thermit\nState:\tR (running)\nPid:\t1\nPPid:\t0\nThreads:\t1\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n";
const CMDLINE: &[u8] = b"hermit-guest\0";
const CPUINFO: &[u8] = b"processor\t: 0\nvendor_id\t: Hermit\nmodel name\t: Hermit Virtual CPU\ncpu MHz\t\t: 0.000\ncpu cores\t: 1\nsiblings\t: 1\nflags\t\t:\n";
const ENTROPY_AVAILABLE: &[u8] = b"256\n";

/// A file in Hermit's deliberately small virtual procfs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcfsFile {
    Maps,
    Stat,
    Status,
    Cmdline,
    Cpuinfo,
    EntropyAvailable,
}

impl ProcfsFile {
    /// Fixed bytes returned for this virtual file.
    pub(crate) fn contents(self) -> &'static [u8] {
        match self {
            Self::Maps => MAPS,
            Self::Stat => STAT,
            Self::Status => STATUS,
            Self::Cmdline => CMDLINE,
            Self::Cpuinfo => CPUINFO,
            Self::EntropyAvailable => ENTROPY_AVAILABLE,
        }
    }

    /// Stable inode reserved for this virtual file.
    pub(crate) fn inode(self) -> u64 {
        match self {
            Self::Maps => 10_001,
            Self::Stat => 10_002,
            Self::Status => 10_003,
            Self::Cmdline => 10_004,
            Self::Cpuinfo => 10_005,
            Self::EntropyAvailable => 10_006,
        }
    }
}

/// Result of applying the minimal procfs pathname policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcfsLookup {
    /// The path is outside procfs and should keep its normal behavior.
    NotProcfs,
    /// The path is in procfs but is intentionally not exposed.
    Missing,
    /// The process executable symlink, simulated by Detcore.
    SelfExe,
    /// The path names one of the fixed virtual files.
    File(ProcfsFile),
}

impl ProcfsLookup {
    /// Classifies absolute `/proc/...` and relative `proc/...` spellings.
    pub(crate) fn from_path(path: &Path) -> Self {
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                Component::RootDir => parts.clear(),
                Component::CurDir => {}
                Component::ParentDir if parts.pop().is_none() => return Self::NotProcfs,
                Component::ParentDir => {}
                Component::Normal(part) => match part.to_str() {
                    Some(part) => parts.push(part),
                    None => return Self::Missing,
                },
                Component::Prefix(_) => return Self::Missing,
            }
        }

        match parts.as_slice() {
            ["proc", "self", "maps"] => Self::File(ProcfsFile::Maps),
            ["proc", "self", "stat"] => Self::File(ProcfsFile::Stat),
            ["proc", "self", "status"] => Self::File(ProcfsFile::Status),
            ["proc", "self", "cmdline"] => Self::File(ProcfsFile::Cmdline),
            ["proc", "self", "exe"] => Self::SelfExe,
            ["proc", "cpuinfo"] => Self::File(ProcfsFile::Cpuinfo),
            ["proc", "sys", "kernel", "random", "entropy_avail"] => {
                Self::File(ProcfsFile::EntropyAvailable)
            }
            ["proc", ..] => Self::Missing,
            _ => Self::NotProcfs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_only_the_minimal_file_set() {
        let cases = [
            ("/proc/self/maps", ProcfsFile::Maps),
            ("/proc/self/stat", ProcfsFile::Stat),
            ("/proc/self/status", ProcfsFile::Status),
            ("/proc/self/cmdline", ProcfsFile::Cmdline),
            ("/proc/cpuinfo", ProcfsFile::Cpuinfo),
            (
                "/proc/sys/kernel/random/entropy_avail",
                ProcfsFile::EntropyAvailable,
            ),
            ("proc/self/stat", ProcfsFile::Stat),
            ("/proc//self/./status", ProcfsFile::Status),
            ("/tmp/../proc/cpuinfo", ProcfsFile::Cpuinfo),
        ];
        for (path, expected) in cases {
            assert_eq!(
                ProcfsLookup::from_path(Path::new(path)),
                ProcfsLookup::File(expected)
            );
        }
    }

    #[test]
    fn hides_other_procfs_paths_and_does_not_capture_similar_paths() {
        assert_eq!(
            ProcfsLookup::from_path(Path::new("/proc/self/exe")),
            ProcfsLookup::SelfExe
        );
        for path in [
            "/proc",
            "/proc/meminfo",
            "/proc/self/environ",
            "proc/sys/kernel/hostname",
            "/tmp/../proc/meminfo",
        ] {
            assert_eq!(
                ProcfsLookup::from_path(Path::new(path)),
                ProcfsLookup::Missing,
                "{path} was not hidden"
            );
        }
        for path in [
            "/proc/../etc/passwd",
            "/tmp/proc/self/stat",
            "proc-info",
            "/process/self/stat",
        ] {
            assert_eq!(
                ProcfsLookup::from_path(Path::new(path)),
                ProcfsLookup::NotProcfs,
                "{path} was incorrectly classified as procfs"
            );
        }
    }

    #[test]
    fn fixed_stat_has_the_linux_field_count() {
        let text = std::str::from_utf8(ProcfsFile::Stat.contents()).unwrap();
        let comm_end = text.rfind(") ").unwrap();
        assert_eq!(2 + text[comm_end + 2..].split_whitespace().count(), 52);
    }

    #[test]
    fn fixed_contents_are_small_and_host_independent() {
        for file in [
            ProcfsFile::Maps,
            ProcfsFile::Stat,
            ProcfsFile::Status,
            ProcfsFile::Cmdline,
            ProcfsFile::Cpuinfo,
            ProcfsFile::EntropyAvailable,
        ] {
            assert!(!file.contents().is_empty());
            assert!(file.contents().len() <= 256);
        }
        assert_eq!(ProcfsFile::Cmdline.contents(), b"hermit-guest\0");
        assert!(
            ProcfsFile::Cpuinfo
                .contents()
                .windows(6)
                .any(|w| w == b"Hermit")
        );
    }
}
