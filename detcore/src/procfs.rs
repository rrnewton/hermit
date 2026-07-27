/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Deterministic snapshots for volatile procfs and sysfs files.

use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ProcfsKind {
    SelfStat,
    ProcessStat,
    Statm,
    SelfStatus,
    ProcessStatus,
    Cpuinfo,
    Loadavg,
    Meminfo,
    SystemStat,
    Uptime,
    Vmstat,
    ScalingCurFreq,
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
        let path = path.to_str()?;
        let kind = match path {
            "/proc/self/stat" => ProcfsKind::SelfStat,
            "/proc/self/statm" => ProcfsKind::Statm,
            "/proc/self/status" => ProcfsKind::SelfStatus,
            "/proc/cpuinfo" => ProcfsKind::Cpuinfo,
            "/proc/loadavg" => ProcfsKind::Loadavg,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // TODO-HUMAN-REVIEW(PR-id): Review deterministic process and system accounting.
            "/proc/meminfo" => ProcfsKind::Meminfo,
            "/proc/stat" => ProcfsKind::SystemStat,
            "/proc/uptime" => ProcfsKind::Uptime,
            "/proc/vmstat" => ProcfsKind::Vmstat,
            // AUTONOMOUS-BOT-IMPLEMENTED
            // A cpufreq `*_cur_freq` file reports the instantaneous core clock,
            // a live hardware reading that differs run-to-run and breaks tools
            // like `lscpu` under `--verify`. These are opened relative to a
            // `/sys/devices/system/cpu` directory fd, so match on the suffix
            // rather than an absolute path.
            other
                if other.ends_with("cpufreq/scaling_cur_freq")
                    || other.ends_with("cpufreq/cpuinfo_cur_freq") =>
            {
                ProcfsKind::ScalingCurFreq
            }
            other if is_process_file_path(other, "stat") => ProcfsKind::ProcessStat,
            other if is_process_file_path(other, "statm") => ProcfsKind::Statm,
            other if is_process_file_path(other, "status") => ProcfsKind::ProcessStatus,
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
    // TODO-HUMAN-REVIEW(PR-723): Review procfs snapshot identity normalization.
    pub(crate) fn initialize(
        &mut self,
        contents: Vec<u8>,
        virtual_uptime_seconds: u64,
        virtual_boot_time_seconds: i64,
        virtual_pid: i32,
        virtual_ppid: i32,
    ) {
        self.contents = Some(match self.kind {
            ProcfsKind::SelfStat => sanitize_stat(&contents, Some((virtual_pid, virtual_ppid))),
            ProcfsKind::ProcessStat => sanitize_stat(&contents, None),
            ProcfsKind::Statm => sanitize_statm(&contents),
            ProcfsKind::SelfStatus => sanitize_status(&contents, Some((virtual_pid, virtual_ppid))),
            ProcfsKind::ProcessStatus => sanitize_status(&contents, None),
            ProcfsKind::Cpuinfo => sanitize_cpuinfo(&contents),
            ProcfsKind::Loadavg => sanitize_loadavg(&contents),
            ProcfsKind::Meminfo => sanitize_meminfo(&contents),
            ProcfsKind::SystemStat => {
                sanitize_system_stat(&contents, virtual_uptime_seconds, virtual_boot_time_seconds)
            }
            ProcfsKind::Uptime => sanitize_uptime(&contents, virtual_uptime_seconds),
            ProcfsKind::Vmstat => sanitize_vmstat(&contents),
            ProcfsKind::ScalingCurFreq => sanitize_scaling_cur_freq(&contents),
        });
        self.offset = 0;
    }

    /// Synchronizes the snapshot offset after a successful kernel seek.
    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(PR-id): Review procfs snapshot seek synchronization.
    pub(crate) fn set_offset(&mut self, offset: usize) {
        self.offset = offset;
    }

    /// Returns the next bytes from the normalized snapshot.
    pub(crate) fn take(&mut self, maximum: usize) -> Option<Vec<u8>> {
        let contents = self.contents.as_ref()?;
        if self.offset >= contents.len() {
            return Some(Vec::new());
        }
        let end = self.offset.saturating_add(maximum).min(contents.len());
        let bytes = contents[self.offset..end].to_vec();
        self.offset = end;
        Some(bytes)
    }
}

fn is_process_file_path(path: &str, file: &str) -> bool {
    path.strip_prefix("/proc/")
        .and_then(|path| path.strip_suffix(&format!("/{file}")))
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit()))
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-id): Review deterministic process accounting fields.
// TODO-HUMAN-REVIEW(PR-723): Review /proc stat identity field normalization.
fn sanitize_stat(contents: &[u8], virtual_identity: Option<(i32, i32)>) -> Vec<u8> {
    const VOLATILE_FIELDS: &[usize] = &[
        10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 23, 24, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36,
        37, 39, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let Some(comm_start) = text.find(" (") else {
        return contents.to_vec();
    };
    let Some(comm_end) = text.rfind(") ") else {
        return contents.to_vec();
    };
    let comm = &text[comm_start..=comm_end];
    let mut fields = text[comm_end + 2..]
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if fields.len() < 50 {
        return contents.to_vec();
    }

    // `fields` starts with proc stat field 3 (state).
    let pid = if let Some((virtual_pid, virtual_ppid)) = virtual_identity {
        fields[4 - 3] = virtual_ppid.to_string();
        fields[5 - 3] = "0".to_owned();
        fields[6 - 3] = "0".to_owned();
        virtual_pid.to_string()
    } else {
        fields[0] = "S".to_owned();
        text[..comm_start].to_owned()
    };
    for field in VOLATILE_FIELDS {
        fields[*field - 3] = "0".to_owned();
    }
    format!("{pid}{comm} {}\n", fields.join(" ")).into_bytes()
}

fn sanitize_statm(contents: &[u8]) -> Vec<u8> {
    let fields = contents
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    if fields.len() != 7
        || fields
            .iter()
            .any(|field| !field.iter().all(u8::is_ascii_digit))
    {
        return contents.to_vec();
    }
    b"0 0 0 0 0 0 0\n".to_vec()
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#553)
// TODO-HUMAN-REVIEW(PR-723): Review /proc status identity field normalization.
fn sanitize_status(contents: &[u8], virtual_identity: Option<(i32, i32)>) -> Vec<u8> {
    const STATE: &[u8] = b"State:";
    const TGID: &[u8] = b"Tgid:";
    const PID: &[u8] = b"Pid:";
    const PPID: &[u8] = b"PPid:";
    const TRACER_PID: &[u8] = b"TracerPid:";
    const NS_TGID: &[u8] = b"NStgid:";
    const NS_PID: &[u8] = b"NSpid:";
    const NS_PGID: &[u8] = b"NSpgid:";
    const NS_SID: &[u8] = b"NSsid:";
    const SIGQ: &[u8] = b"SigQ:";
    const CPUS_ALLOWED: &[u8] = b"Cpus_allowed:";
    const CPUS_ALLOWED_LIST: &[u8] = b"Cpus_allowed_list:";
    const VOLUNTARY: &[u8] = b"voluntary_ctxt_switches:";
    const NONVOLUNTARY: &[u8] = b"nonvoluntary_ctxt_switches:";
    const MEMORY_FIELDS: &[&[u8]] = &[
        b"VmPeak",
        b"VmSize",
        b"VmLck",
        b"VmPin",
        b"VmHWM",
        b"VmRSS",
        b"RssAnon",
        b"RssFile",
        b"RssShmem",
        b"VmData",
        b"VmStk",
        b"VmExe",
        b"VmLib",
        b"VmPTE",
        b"VmSwap",
        b"HugetlbPages",
    ];

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(STATE) {
            normalized.extend_from_slice(b"State:\tS (sleeping)");
        } else if let Some((virtual_pid, _)) = virtual_identity
            && (body.starts_with(TGID)
                || body.starts_with(PID)
                || body.starts_with(NS_TGID)
                || body.starts_with(NS_PID))
        {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(format!(":\t{virtual_pid}").as_bytes());
        } else if let Some((_, virtual_ppid)) = virtual_identity
            && body.starts_with(PPID)
        {
            normalized.extend_from_slice(PPID);
            normalized.extend_from_slice(format!("\t{virtual_ppid}").as_bytes());
        } else if body.starts_with(TRACER_PID) {
            normalized.extend_from_slice(TRACER_PID);
            normalized.extend_from_slice(b"\t1");
        } else if body.starts_with(NS_PGID) || body.starts_with(NS_SID) {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(b":\t0");
        } else if body.starts_with(SIGQ) {
            normalized.extend_from_slice(SIGQ);
            normalized.extend_from_slice(b"\t0/0");
        } else if body.starts_with(CPUS_ALLOWED) {
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
        } else if let Some(name_end) = body.iter().position(|byte| *byte == b':')
            && MEMORY_FIELDS.contains(&&body[..name_end])
        {
            normalized.extend_from_slice(&body[..name_end]);
            normalized.extend_from_slice(b":\t0 kB");
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-id): Review deterministic procfs system accounting values.
fn sanitize_meminfo(contents: &[u8]) -> Vec<u8> {
    const VOLATILE_FIELDS: &[&[u8]] = &[
        b"MemFree",
        b"MemAvailable",
        b"Buffers",
        b"Cached",
        b"SwapCached",
        b"Active",
        b"Inactive",
        b"Active(anon)",
        b"Inactive(anon)",
        b"Active(file)",
        b"Inactive(file)",
        b"Unevictable",
        b"Mlocked",
        b"SwapFree",
        b"Dirty",
        b"Writeback",
        b"AnonPages",
        b"Mapped",
        b"Shmem",
        b"KReclaimable",
        b"Slab",
        b"SReclaimable",
        b"SUnreclaim",
        b"KernelStack",
        b"PageTables",
        b"SecPageTables",
        b"NFS_Unstable",
        b"Bounce",
        b"WritebackTmp",
        b"Committed_AS",
        b"VmallocUsed",
        b"VmallocChunk",
        b"Percpu",
        b"AnonHugePages",
        b"ShmemHugePages",
        b"ShmemPmdMapped",
        b"FileHugePages",
        b"FilePmdMapped",
        b"CmaFree",
        b"HugePages_Free",
        b"HugePages_Rsvd",
        b"HugePages_Surp",
        b"Hugetlb",
    ];

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let name_end = body.iter().position(|byte| *byte == b':');
        let name = name_end.map_or(body, |end| &body[..end]);
        if VOLATILE_FIELDS.contains(&name) {
            normalized.extend_from_slice(name);
            normalized.extend_from_slice(b":\t0");
            if body.ends_with(b" kB") {
                normalized.extend_from_slice(b" kB");
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

fn sanitize_system_stat(
    contents: &[u8],
    virtual_uptime_seconds: u64,
    virtual_boot_time_seconds: i64,
) -> Vec<u8> {
    const VOLATILE_FIELDS: &[&[u8]] = &[
        b"intr",
        b"ctxt",
        b"processes",
        b"procs_running",
        b"procs_blocked",
        b"softirq",
    ];

    let cpu_count = contents
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.split(|byte| byte.is_ascii_whitespace()).next())
        .filter(|name| name.starts_with(b"cpu") && *name != b"cpu")
        .count() as u64;
    let per_cpu_idle_ticks = virtual_uptime_seconds.saturating_mul(100);
    let counters = sanitize_named_counters(
        contents,
        |name| name.starts_with(b"cpu") || VOLATILE_FIELDS.contains(&name),
        |name, index| {
            if index == 0 && name == b"cpu" {
                per_cpu_idle_ticks.saturating_mul(cpu_count)
            } else if index == 0 && name.starts_with(b"cpu") {
                per_cpu_idle_ticks
            } else {
                0
            }
        },
    );

    let mut normalized = Vec::with_capacity(counters.len());
    for line in counters.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if body.starts_with(b"btime ") {
            normalized.extend_from_slice(format!("btime {virtual_boot_time_seconds}").as_bytes());
        } else {
            normalized.extend_from_slice(body);
        }
        if has_newline {
            normalized.push(b'\n');
        }
    }
    normalized
}

fn sanitize_vmstat(contents: &[u8]) -> Vec<u8> {
    sanitize_named_counters(contents, |_| true, |_, _| 0)
}

fn sanitize_named_counters(
    contents: &[u8],
    should_normalize: impl Fn(&[u8]) -> bool,
    counter_value: impl Fn(&[u8], usize) -> u64,
) -> Vec<u8> {
    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        let mut fields = body
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let name = fields.next().unwrap_or_default();
        if should_normalize(name) {
            normalized.extend_from_slice(name);
            for (index, _) in fields.enumerate() {
                normalized.push(b' ');
                normalized.extend_from_slice(counter_value(name, index).to_string().as_bytes());
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

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-764)
/// Normalizes a cpufreq `scaling_cur_freq` / `cpuinfo_cur_freq` snapshot. The
/// instantaneous core frequency is a live hardware reading that varies between
/// otherwise identical runs, so replace it with a fixed value. This mirrors the
/// `cpu MHz` zeroing already done for `/proc/cpuinfo` in [`sanitize_cpuinfo`],
/// and keeps the static `cpuinfo_max_freq`/`scaling_max_freq` files untouched.
fn sanitize_scaling_cur_freq(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0\n".to_vec()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_normalized_procfs_paths() {
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/stat"))
                .unwrap()
                .kind,
            ProcfsKind::SelfStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/statm"))
                .unwrap()
                .kind,
            ProcfsKind::Statm
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/self/status"))
                .unwrap()
                .kind,
            ProcfsKind::SelfStatus
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
            ProcfsFile::from_path(Path::new("/proc/meminfo"))
                .unwrap()
                .kind,
            ProcfsKind::Meminfo
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/stat")).unwrap().kind,
            ProcfsKind::SystemStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/uptime"))
                .unwrap()
                .kind,
            ProcfsKind::Uptime
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/vmstat"))
                .unwrap()
                .kind,
            ProcfsKind::Vmstat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/123/stat"))
                .unwrap()
                .kind,
            ProcfsKind::ProcessStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/123/statm"))
                .unwrap()
                .kind,
            ProcfsKind::Statm
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/123/status"))
                .unwrap()
                .kind,
            ProcfsKind::ProcessStatus
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/not-a-pid/stat")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/proc/not-a-pid/statm")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/proc/not-a-pid/status")).is_none());
        assert!(ProcfsFile::from_path(Path::new("/proc/self/maps")).is_none());
    }

    #[test]
    fn recognizes_cpufreq_current_frequency_by_suffix() {
        // Opened relative to a `/sys/devices/system/cpu` directory fd.
        assert_eq!(
            ProcfsFile::from_path(Path::new("cpu0/cpufreq/scaling_cur_freq"))
                .unwrap()
                .kind,
            ProcfsKind::ScalingCurFreq
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new(
                "/sys/devices/system/cpu/cpu3/cpufreq/cpuinfo_cur_freq"
            ))
            .unwrap()
            .kind,
            ProcfsKind::ScalingCurFreq
        );
        // The static min/max limits are deterministic and must not be rewritten.
        assert!(ProcfsFile::from_path(Path::new("cpu0/cpufreq/cpuinfo_max_freq")).is_none());
        assert!(ProcfsFile::from_path(Path::new("cpu0/cpufreq/scaling_max_freq")).is_none());
    }

    #[test]
    fn scaling_cur_freq_is_fixed() {
        assert_eq!(sanitize_scaling_cur_freq(b"2483951\n"), b"0\n");
        assert!(sanitize_scaling_cur_freq(b"").is_empty());
    }

    #[test]
    fn stat_normalizes_runtime_counters() {
        let input = b"3 (name with spaces) R 1 0 0 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(input, Some((3, 1)))).unwrap();
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        for field in [
            10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 23, 24, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35,
            36, 37, 39, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51,
        ] {
            assert_eq!(fields[field - 3], "0", "field {field} was not normalized");
        }
        assert!(output.starts_with("3 (name with spaces) R 1 0 0 "));
    }

    #[test]
    fn process_stat_preserves_identity_and_normalizes_memory() {
        let input = b"42 (worker) S 1 7 8 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(input, None)).unwrap();
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        assert!(output.starts_with("42 (worker) S 1 7 8 "));
        assert_eq!(fields[23 - 3], "0");
        assert_eq!(fields[24 - 3], "0");
    }

    // AUTONOMOUS-BOT-IMPLEMENTED
    // TODO-HUMAN-REVIEW(#553)
    #[test]
    fn status_normalizes_affinity_and_context_switches() {
        let input = b"Name:\tcat\nTgid:\t1234\nPid:\t1234\nPPid:\t1200\nTracerPid:\t0\nNStgid:\t1234\nNSpid:\t1234\nNSpgid:\t1200\nNSsid:\t1190\nSigQ:\t426/2042342\nCpus_allowed:\tffffffff,ffffffff\nCpus_allowed_list:\t0-63\nvoluntary_ctxt_switches:\t120\nnonvoluntary_ctxt_switches:\t3\n";
        assert_eq!(
            sanitize_status(input, Some((3, 1))),
            b"Name:\tcat\nTgid:\t3\nPid:\t3\nPPid:\t1\nTracerPid:\t1\nNStgid:\t3\nNSpid:\t3\nNSpgid:\t0\nNSsid:\t0\nSigQ:\t0/0\nCpus_allowed:\t00000000,00000000,00000000,00000001\nCpus_allowed_list:\t0\nvoluntary_ctxt_switches:\t0\nnonvoluntary_ctxt_switches:\t0\n"
        );
    }

    #[test]
    fn statm_normalizes_memory_page_counts() {
        assert_eq!(
            sanitize_statm(b"62203 7952 5707 4033 0 3255 0\n"),
            b"0 0 0 0 0 0 0\n"
        );
        assert_eq!(sanitize_statm(b"malformed\n"), b"malformed\n");
    }

    #[test]
    fn process_status_preserves_identity_and_normalizes_memory() {
        let input = b"Name:\thermit\nState:\tR (running)\nPid:\t1\nPPid:\t0\nVmPeak:\t250000 kB\nVmSize:\t249000 kB\nVmHWM:\t31000 kB\nVmRSS:\t30000 kB\n";
        assert_eq!(
            sanitize_status(input, None),
            b"Name:\thermit\nState:\tS (sleeping)\nPid:\t1\nPPid:\t0\nVmPeak:\t0 kB\nVmSize:\t0 kB\nVmHWM:\t0 kB\nVmRSS:\t0 kB\n"
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
    fn system_accounting_normalizes_live_counters() {
        assert_eq!(
            sanitize_meminfo(
                b"MemTotal:       1024 kB\nMemFree:         55 kB\nSwapFree:         7 kB\nHugepagesize:   2048 kB\n"
            ),
            b"MemTotal:       1024 kB\nMemFree:\t0 kB\nSwapFree:\t0 kB\nHugepagesize:   2048 kB\n"
        );
        assert_eq!(
            sanitize_system_stat(
                b"cpu  1 2 3 4 5 6 7 8 9 10\ncpu0 1 2 3 4 5 6 7 8 9 10\nintr 9 8 7\nbtime 1234\nprocesses 55\n",
                120,
                1_640_995_079,
            ),
            b"cpu 12000 0 0 0 0 0 0 0 0 0\ncpu0 12000 0 0 0 0 0 0 0 0 0\nintr 0 0 0\nbtime 1640995079\nprocesses 0\n"
        );
        assert_eq!(
            sanitize_vmstat(b"pgpgin 123\nnr_free_pages 456\n"),
            b"pgpgin 0\nnr_free_pages 0\n"
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
        file.initialize(
            b"voluntary_ctxt_switches:\t12\n".to_vec(),
            120,
            1_640_995_079,
            3,
            1,
        );
        assert_eq!(file.take(5).unwrap(), b"volun");
        assert_eq!(file.take(128).unwrap(), b"tary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());

        file.set_offset(0);
        assert_eq!(file.take(5).unwrap(), b"volun");
        file.set_offset(usize::MAX);
        assert!(file.take(1).unwrap().is_empty());
    }
}
