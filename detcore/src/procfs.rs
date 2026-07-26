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
    Loadavg,
    Uptime,
    // AUTONOMOUS-BOT-IMPLEMENTED
    Meminfo,
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
            "/proc/loadavg" => ProcfsKind::Loadavg,
            "/proc/uptime" => ProcfsKind::Uptime,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-722)
            "/proc/meminfo" => ProcfsKind::Meminfo,
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
    pub(crate) fn initialize(&mut self, contents: Vec<u8>, virtual_uptime_seconds: u64) {
        self.contents = Some(match self.kind {
            ProcfsKind::Stat => sanitize_stat(&contents),
            ProcfsKind::Status => sanitize_status(&contents),
            ProcfsKind::Cpuinfo => sanitize_cpuinfo(&contents),
            ProcfsKind::Loadavg => sanitize_loadavg(&contents),
            ProcfsKind::Uptime => sanitize_uptime(&contents, virtual_uptime_seconds),
            // AUTONOMOUS-BOT-IMPLEMENTED
            ProcfsKind::Meminfo => sanitize_meminfo(&contents),
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#553)
fn sanitize_status(contents: &[u8]) -> Vec<u8> {
    const CPUS_ALLOWED: &[u8] = b"Cpus_allowed:";
    const CPUS_ALLOWED_LIST: &[u8] = b"Cpus_allowed_list:";
    const VOLUNTARY: &[u8] = b"voluntary_ctxt_switches:";
    const NONVOLUNTARY: &[u8] = b"nonvoluntary_ctxt_switches:";

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(CPUS_ALLOWED) {
            normalized.extend_from_slice(CPUS_ALLOWED);
            normalized.extend_from_slice(b"\t00000000,00000000,00000000,00000001");
        } else if body.starts_with(CPUS_ALLOWED_LIST) {
            normalized.extend_from_slice(CPUS_ALLOWED_LIST);
            normalized.extend_from_slice(b"\t0");
        } else if body.starts_with(VOLUNTARY) {
            normalized.extend_from_slice(VOLUNTARY);
            normalized.extend_from_slice(b"\t0");
        } else if body.starts_with(NONVOLUNTARY) {
            normalized.extend_from_slice(NONVOLUNTARY);
            normalized.extend_from_slice(b"\t0");
        } else {
            normalized.extend_from_slice(body);
        }
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

fn sanitize_loadavg(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0.00 0.00 0.00 1/1 1\n".to_vec()
    }
}

fn sanitize_uptime(contents: &[u8], virtual_uptime_seconds: u64) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        format!("{virtual_uptime_seconds}.00 0.00\n").into_bytes()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-722)
/// Normalizes `/proc/meminfo`. Every reported quantity except the physically
/// fixed machine totals (`MemTotal`, `SwapTotal`) fluctuates run-to-run as the
/// host allocates and frees memory, so `free`, `vmstat`, and similar tools
/// diverge under `--verify`. Preserve the stable totals verbatim and zero every
/// other value, keeping each line's key, spacing, and unit suffix intact. Using
/// an allowlist of stable keys (rather than a denylist of volatile ones)
/// guarantees a newly introduced volatile field cannot silently reintroduce
/// nondeterminism.
fn sanitize_meminfo(contents: &[u8]) -> Vec<u8> {
    const STABLE_KEYS: &[&[u8]] = &[b"MemTotal:", b"SwapTotal:"];

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);

        if STABLE_KEYS.iter().any(|key| body.starts_with(key)) {
            normalized.extend_from_slice(body);
        } else if let Some(colon) = body.iter().position(|byte| *byte == b':') {
            // Split each "Key:<spaces><digits><rest>" line and rewrite only the
            // numeric value, so alignment and the optional " kB" suffix survive.
            let (key, after) = body.split_at(colon + 1);
            let spaces = after
                .iter()
                .take_while(|byte| byte.is_ascii_whitespace())
                .count();
            let (leading, value_and_rest) = after.split_at(spaces);
            let digits = value_and_rest
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if digits == 0 {
                // No numeric value to normalize; keep the line verbatim.
                normalized.extend_from_slice(body);
            } else {
                normalized.extend_from_slice(key);
                normalized.extend_from_slice(leading);
                normalized.push(b'0');
                normalized.extend_from_slice(&value_and_rest[digits..]);
            }
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
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/loadavg"))
                .unwrap()
                .kind,
            ProcfsKind::Loadavg
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/uptime"))
                .unwrap()
                .kind,
            ProcfsKind::Uptime
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/meminfo"))
                .unwrap()
                .kind,
            ProcfsKind::Meminfo
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/self/maps")).is_none());
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-722)
    #[test]
    fn meminfo_zeroes_volatile_fields_and_keeps_totals() {
        let input = b"MemTotal:       791462432 kB\n\
MemFree:         63838604 kB\n\
MemAvailable:   616884056 kB\n\
Buffers:          1234567 kB\n\
Cached:         559670332 kB\n\
Shmem:             946444 kB\n\
SwapTotal:      136218616 kB\n\
SwapFree:        66940748 kB\n\
HugePages_Total:        0\n\
Hugepagesize:        2048 kB\n";
        let expected = b"MemTotal:       791462432 kB\n\
MemFree:         0 kB\n\
MemAvailable:   0 kB\n\
Buffers:          0 kB\n\
Cached:         0 kB\n\
Shmem:             0 kB\n\
SwapTotal:      136218616 kB\n\
SwapFree:        0 kB\n\
HugePages_Total:        0\n\
Hugepagesize:        0 kB\n";
        assert_eq!(sanitize_meminfo(input), expected);
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-722)
    #[test]
    fn meminfo_is_stable_across_differing_snapshots() {
        let run1 = b"MemTotal:       791462432 kB\nMemFree:         63838604 kB\nCached:         559670332 kB\nSwapTotal:      136218616 kB\n";
        let run2 = b"MemTotal:       791462432 kB\nMemFree:         63836552 kB\nCached:         559679200 kB\nSwapTotal:      136218616 kB\n";
        assert_eq!(sanitize_meminfo(run1), sanitize_meminfo(run2));
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#553)
    #[test]
    fn status_normalizes_affinity_and_context_switches() {
        let input = b"Name:\tcat\nCpus_allowed:\tffffffff,ffffffff\nCpus_allowed_list:\t0-63\nvoluntary_ctxt_switches:\t120\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(
            sanitize_status(input),
            b"Name:\tcat\nCpus_allowed:\t00000000,00000000,00000000,00000001\nCpus_allowed_list:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n"
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
    fn loadavg_and_uptime_use_virtual_values() {
        assert_eq!(
            sanitize_loadavg(b"344.01 369.71 375.04 526/107858 512196\n"),
            b"0.00 0.00 0.00 1/1 1\n"
        );
        assert_eq!(
            sanitize_uptime(b"156980.56 37990755.08\n", 120),
            b"120.00 0.00\n"
        );
    }

    #[test]
    fn snapshot_supports_partial_reads() {
        let mut file = ProcfsFile::from_path(Path::new("/proc/self/status")).unwrap();
        file.initialize(b"voluntary_ctxt_switches:\t12\n".to_vec(), 120);
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
    }
}
