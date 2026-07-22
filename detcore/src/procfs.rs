/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic snapshots for volatile procfs files.

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ProcfsKind {
    Stat,
    Status,
    Cpuinfo,
}

/// State for a procfs file whose volatile fields require normalization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ProcfsFile {
    kind: ProcfsKind,
    contents: Option<Vec<u8>>,
    offset: usize,
}

impl ProcfsFile {
    /// Recognizes procfs files that contain observed volatile fields.
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let kind = match path.to_str()? {
            "/proc/self/stat" => ProcfsKind::Stat,
            "/proc/self/status" => ProcfsKind::Status,
            "/proc/cpuinfo" => ProcfsKind::Cpuinfo,
            _ => return None,
        };
        Some(Self {
            kind,
            contents: None,
            offset: 0,
        })
    }

    /// Returns true until the underlying procfs content has been captured.
    pub(crate) fn needs_snapshot(&self) -> bool {
        self.contents.is_none()
    }

    /// Normalizes and stores a complete snapshot captured from the kernel.
    pub(crate) fn initialize(&mut self, contents: Vec<u8>) {
        self.contents = Some(match self.kind {
            ProcfsKind::Stat => sanitize_stat(&contents),
            ProcfsKind::Status => sanitize_status(&contents),
            ProcfsKind::Cpuinfo => sanitize_cpuinfo(&contents),
        });
        self.offset = 0;
    }

    /// Returns the next bytes from the normalized snapshot.
    pub(crate) fn take(&mut self, maximum: usize) -> Option<Vec<u8>> {
        let contents = self.contents.as_ref()?;
        let end = self.offset.saturating_add(maximum).min(contents.len());
        let bytes = contents[self.offset..end].to_vec();
        self.offset = end;
        Some(bytes)
    }
}

fn sanitize_stat(contents: &[u8]) -> Vec<u8> {
    const VOLATILE_FIELDS: &[usize] = &[10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let Some(comm_end) = text.rfind(") ") else {
        return contents.to_vec();
    };
    let comm = &text[..=comm_end];
    let mut fields = text[comm_end + 2..].split_whitespace().collect::<Vec<_>>();
    if fields.len() < 50 {
        return contents.to_vec();
    }

    // `fields` starts with proc stat field 3 (state).
    for field in VOLATILE_FIELDS {
        fields[*field - 3] = "0";
    }
    format!("{} {}\n", comm, fields.join(" ")).into_bytes()
}

fn sanitize_status(contents: &[u8]) -> Vec<u8> {
    // `/proc/self/status` lines whose value leaks host state that is not
    // deterministic across runs (scheduler-chosen CPU, host CPU/NUMA topology)
    // or that counts runtime events. Each is rewritten to a fixed value that is
    // consistent with the rest of Hermit's virtualization:
    //
    // * `Cpus_allowed`/`Cpus_allowed_list` and `Mems_allowed`/`Mems_allowed_list`
    //   reflect the single virtual CPU 0 / node 0 that `sched_getaffinity`
    //   already reports, rather than the host's affinity mask which varies run
    //   to run as the guest is scheduled onto different host CPUs.
    // * `*_ctxt_switches` count scheduling events and are zeroed.
    //
    // Matching is on the `Key:` prefix so the tab/value formatting is replaced
    // wholesale.
    const FIXED_FIELDS: &[(&[u8], &[u8])] = &[
        (b"Cpus_allowed:", b"Cpus_allowed:\t00000001"),
        (b"Cpus_allowed_list:", b"Cpus_allowed_list:\t0"),
        (b"Mems_allowed:", b"Mems_allowed:\t00000001"),
        (b"Mems_allowed_list:", b"Mems_allowed_list:\t0"),
        (b"voluntary_ctxt_switches:", b"voluntary_ctxt_switches:\t0"),
        (
            b"nonvoluntary_ctxt_switches:",
            b"nonvoluntary_ctxt_switches:\t0",
        ),
    ];

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let replacement = FIXED_FIELDS
            .iter()
            .find(|(prefix, _)| body.starts_with(prefix))
            .map(|(_, value)| *value);
        normalized.extend_from_slice(replacement.unwrap_or(body));
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_cpuinfo(contents: &[u8]) -> Vec<u8> {
    const CPU_MHZ: &[u8] = b"cpu MHz";

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(CPU_MHZ) {
            normalized.extend_from_slice(b"cpu MHz\t\t: 0.000");
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_normalized_procfs_paths() {
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/stat"))
                .unwrap()
                .kind,
            ProcfsKind::Stat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/status"))
                .unwrap()
                .kind,
            ProcfsKind::Status
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/cpuinfo"))
                .unwrap()
                .kind,
            ProcfsKind::Cpuinfo
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/self/maps")).is_none());
    }

    #[test]
    fn stat_normalizes_runtime_counters() {
        let input = b"3 (name with spaces) R 1 0 0 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(input)).unwrap();
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        for field in [10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44] {
            assert_eq!(fields[field - 3], "0", "field {field} was not normalized");
        }
        assert!(output.starts_with("3 (name with spaces) R "));
    }

    #[test]
    fn status_normalizes_context_switches() {
        let input = b"Name:\tcat\nvoluntary_ctxt_switches:\t120\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(
            sanitize_status(input),
            b"Name:\tcat\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n"
        );
    }

    #[test]
    fn status_normalizes_host_affinity_and_topology() {
        // Host affinity mask and CPU/NUMA lists vary across runs and hosts; they
        // must be rewritten to the fixed single virtual CPU 0 / node 0.
        let input = b"Name:\tcat\n\
Cpus_allowed:\t2000000,00000000,00000000\n\
Cpus_allowed_list:\t313\n\
Mems_allowed:\t00000000,00000003\n\
Mems_allowed_list:\t0-1\n\
voluntary_ctxt_switches:\t7\n";
        assert_eq!(
            sanitize_status(input),
            b"Name:\tcat\n\
Cpus_allowed:\t00000001\n\
Cpus_allowed_list:\t0\n\
Mems_allowed:\t00000001\n\
Mems_allowed_list:\t0\n\
voluntary_ctxt_switches:\t0\n"
                .to_vec()
        );
    }

    #[test]
    fn status_prefixes_do_not_collide() {
        // `Cpus_allowed_list:` must not be matched by the `Cpus_allowed:` rule.
        let input = b"Cpus_allowed_list:\t42\nCpus_allowed:\tdeadbeef\n";
        assert_eq!(
            sanitize_status(input),
            b"Cpus_allowed_list:\t0\nCpus_allowed:\t00000001\n".to_vec()
        );
    }

    #[test]
    fn cpuinfo_normalizes_frequency() {
        let input = b"processor\t: 0\ncpu MHz\t\t: 2994.183\ncache size\t: 1024 KB\n";
        assert_eq!(
            sanitize_cpuinfo(input),
            b"processor\t: 0\ncpu MHz\t\t: 0.000\ncache size\t: 1024 KB\n"
        );
    }

    #[test]
    fn snapshot_supports_partial_reads() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/self/status")).unwrap();
        file.initialize(b"voluntary_ctxt_switches:\t12\n".to_vec());
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
    }
}
