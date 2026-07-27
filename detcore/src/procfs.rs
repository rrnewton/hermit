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
    Stat,
    ProcessStat,
    ProcessStatm,
    Status,
    ProcessStatus,
    Cpuinfo,
    Loadavg,
    Meminfo,
    SystemStat,
    Uptime,
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
        let kind = match path.to_str()? {
            "/proc/self/stat" => ProcfsKind::Stat,
            "/proc/self/status" => ProcfsKind::Status,
            "/proc/cpuinfo" => ProcfsKind::Cpuinfo,
            "/proc/loadavg" => ProcfsKind::Loadavg,
            "/proc/meminfo" => ProcfsKind::Meminfo,
            "/proc/stat" => ProcfsKind::SystemStat,
            "/proc/uptime" => ProcfsKind::Uptime,
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
            // AUTONOMOUS-BOT-IMPLEMENTED
            other if is_numeric_proc_file(other, "stat") => ProcfsKind::ProcessStat,
            other if is_numeric_proc_file(other, "statm") => ProcfsKind::ProcessStatm,
            other if is_numeric_proc_file(other, "status") => ProcfsKind::ProcessStatus,
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
        virtual_memory_bytes: u64,
        virtual_pid: i32,
        virtual_ppid: i32,
    ) {
        self.contents = Some(match self.kind {
            ProcfsKind::Stat => sanitize_stat(&contents, Some((virtual_pid, virtual_ppid))),
            ProcfsKind::ProcessStat => sanitize_stat(&contents, None),
            ProcfsKind::ProcessStatm => sanitize_statm(&contents),
            ProcfsKind::Status => sanitize_status(&contents, Some((virtual_pid, virtual_ppid))),
            ProcfsKind::ProcessStatus => sanitize_status(&contents, None),
            ProcfsKind::Cpuinfo => sanitize_cpuinfo(&contents),
            ProcfsKind::Loadavg => sanitize_loadavg(&contents),
            ProcfsKind::Meminfo => sanitize_meminfo(&contents, virtual_memory_bytes),
            ProcfsKind::SystemStat => sanitize_system_stat(&contents, virtual_uptime_seconds),
            ProcfsKind::Uptime => sanitize_uptime(&contents, virtual_uptime_seconds),
            ProcfsKind::ScalingCurFreq => sanitize_scaling_cur_freq(&contents),
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

    // AUTONOMOUS-BOT-IMPLEMENTED
    pub(crate) fn seek(&mut self, offset: usize) {
        self.offset = self
            .contents
            .as_ref()
            .map_or(offset, |contents| offset.min(contents.len()));
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
fn is_numeric_proc_file(path: &str, filename: &str) -> bool {
    let Some(process) = path
        .strip_prefix("/proc/")
        .and_then(|path| path.strip_suffix(filename))
        .and_then(|path| path.strip_suffix('/'))
    else {
        return false;
    };
    !process.is_empty() && process.bytes().all(|byte| byte.is_ascii_digit())
}

// TODO-HUMAN-REVIEW(PR-723): Review /proc stat identity field normalization.
fn sanitize_stat(contents: &[u8], virtual_identity: Option<(i32, i32)>) -> Vec<u8> {
    const VOLATILE_FIELDS: &[usize] = &[10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44];
    const NUMERIC_PROCESS_VOLATILE_FIELDS: &[usize] =
        &[23, 25, 26, 27, 28, 29, 35, 45, 46, 47, 48, 49, 50, 51];

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

    // `fields` starts with proc stat field 3 (state). Explicit numeric
    // `/proc/<pid>/stat` paths already carry namespace-relative identities;
    // only `/proc/self/stat` needs its host identity rewritten.
    if virtual_identity.is_none() {
        // Numeric process state is sampled from the host scheduler rather than
        // Detcore, so expose one fixed valid state to process-table readers.
        fields[0] = "S".to_owned();
        for field in NUMERIC_PROCESS_VOLATILE_FIELDS {
            fields[*field - 3] = "0".to_owned();
        }
    }
    let pid = if let Some((virtual_pid, virtual_ppid)) = virtual_identity {
        fields[4 - 3] = virtual_ppid.to_string();
        fields[5 - 3] = "0".to_owned();
        fields[6 - 3] = "0".to_owned();
        virtual_pid.to_string()
    } else {
        text[..comm_start].to_owned()
    };
    for field in VOLATILE_FIELDS {
        fields[*field - 3] = "0".to_owned();
    }
    format!("{pid}{comm} {}\n", fields.join(" ")).into_bytes()
}

// AUTONOMOUS-BOT-IMPLEMENTED
fn sanitize_statm(contents: &[u8]) -> Vec<u8> {
    if contents.is_empty() {
        Vec::new()
    } else {
        b"0 0 0 0 0 0 0\n".to_vec()
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(#553)
// TODO-HUMAN-REVIEW(PR-723): Review /proc status identity field normalization.
fn sanitize_status(contents: &[u8], virtual_identity: Option<(i32, i32)>) -> Vec<u8> {
    const TGID: &[u8] = b"Tgid:";
    const PID: &[u8] = b"Pid:";
    const PPID: &[u8] = b"PPid:";
    const STATE: &[u8] = b"State:";
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
    const VOLATILE_MEMORY_FIELDS: &[&[u8]] = &[
        b"VmPeak:",
        b"VmSize:",
        b"VmLck:",
        b"VmPin:",
        b"VmHWM:",
        b"VmRSS:",
        b"RssAnon:",
        b"RssFile:",
        b"RssShmem:",
        b"VmData:",
        b"VmStk:",
        b"VmExe:",
        b"VmLib:",
        b"VmPTE:",
        b"VmSwap:",
        b"HugetlbPages:",
    ];
    let (virtual_pid, virtual_ppid) = virtual_identity.unwrap_or_default();

    let mut normalized = Vec::with_capacity(contents.len());
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        let has_newline = line.last() == Some(&b'\n');
        let body = line.strip_suffix(b"\n").unwrap_or(line);
        if virtual_identity.is_some()
            && (body.starts_with(TGID)
                || body.starts_with(PID)
                || body.starts_with(NS_TGID)
                || body.starts_with(NS_PID))
        {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(format!(":\t{virtual_pid}").as_bytes());
        } else if virtual_identity.is_some() && body.starts_with(PPID) {
            normalized.extend_from_slice(PPID);
            normalized.extend_from_slice(format!("\t{virtual_ppid}").as_bytes());
        } else if virtual_identity.is_none() && body.starts_with(STATE) {
            normalized.extend_from_slice(b"State:\tS (sleeping)");
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
        } else if virtual_identity.is_none()
            && VOLATILE_MEMORY_FIELDS
                .iter()
                .any(|field| body.starts_with(field))
        {
            let label = body.split(|byte| *byte == b':').next().unwrap_or_default();
            normalized.extend_from_slice(label);
            normalized.extend_from_slice(b":\t       0 kB");
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
fn sanitize_meminfo(contents: &[u8], virtual_memory_bytes: u64) -> Vec<u8> {
    const PRESERVED_FIELDS: &[&str] = &[
        "Hugepagesize",
        "VmallocTotal",
        "DirectMap4k",
        "DirectMap2M",
        "DirectMap1G",
    ];

    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let total_kb = virtual_memory_bytes / 1024;
    let mut normalized = String::with_capacity(text.len());
    for line in text.lines() {
        let Some((label, value)) = line.split_once(':') else {
            normalized.push_str(line);
            normalized.push('\n');
            continue;
        };
        if PRESERVED_FIELDS.contains(&label) {
            normalized.push_str(line);
            normalized.push('\n');
            continue;
        }

        let unit = value
            .split_whitespace()
            .nth(1)
            .map(|unit| format!(" {unit}"))
            .unwrap_or_default();
        let deterministic_value = match label {
            "MemTotal" | "MemFree" | "MemAvailable" => total_kb,
            _ => 0,
        };
        normalized.push_str(&format!("{label}: {deterministic_value}{unit}\n"));
    }
    normalized.into_bytes()
}

// AUTONOMOUS-BOT-IMPLEMENTED
fn sanitize_system_stat(contents: &[u8], virtual_uptime_seconds: u64) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(contents) else {
        return contents.to_vec();
    };
    let cpu_count = text
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|label| {
            label
                .strip_prefix("cpu")
                .is_some_and(|cpu| !cpu.is_empty() && cpu.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .count() as u64;
    let per_cpu_idle_ticks = virtual_uptime_seconds.saturating_mul(100);
    let total_idle_ticks = per_cpu_idle_ticks.saturating_mul(cpu_count.max(1));
    let mut normalized = String::with_capacity(text.len());
    for line in text.lines() {
        let label = line.split_whitespace().next().unwrap_or_default();
        if label == "cpu"
            || label
                .strip_prefix("cpu")
                .is_some_and(|cpu| !cpu.is_empty() && cpu.bytes().all(|byte| byte.is_ascii_digit()))
        {
            let idle_ticks = if label == "cpu" {
                total_idle_ticks
            } else {
                per_cpu_idle_ticks
            };
            normalized.push_str(label);
            normalized.push_str(&format!(" 0 0 0 {idle_ticks} 0 0 0 0 0 0\n"));
        } else if matches!(
            label,
            "intr" | "ctxt" | "processes" | "procs_blocked" | "softirq"
        ) {
            normalized.push_str(label);
            normalized.push_str(" 0\n");
        } else if label == "procs_running" {
            normalized.push_str("procs_running 1\n");
        } else {
            normalized.push_str(line);
            normalized.push('\n');
        }
    }
    normalized.into_bytes()
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
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/stat")).unwrap().kind,
            ProcfsKind::SystemStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/42/stat"))
                .unwrap()
                .kind,
            ProcfsKind::ProcessStat
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/42/statm"))
                .unwrap()
                .kind,
            ProcfsKind::ProcessStatm
        );
        assert_eq!(
            ProcfsFile::from_path(Path::new("/proc/42/status"))
                .unwrap()
                .kind,
            ProcfsKind::ProcessStatus
        );
        assert!(ProcfsFile::from_path(Path::new("/proc/not-a-pid/stat")).is_none());
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
        for field in [10, 11, 12, 13, 14, 15, 16, 17, 21, 22, 24, 39, 42, 43, 44] {
            assert_eq!(fields[field - 3], "0", "field {field} was not normalized");
        }
        assert!(output.starts_with("3 (name with spaces) R 1 0 0 "));
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
    fn numeric_process_files_preserve_identity_and_hide_memory_usage() {
        let stat = b"1 (hermit) S 0 1 1 0 -1 0 89 0 1 2 3 4 5 6 20 0 1 7 520343512 2879488 123 18446744073709551615 100 200 300 0 0 0 0 3145728 0 0 0 0 17 114 0 0 9 10 11 400 500 600 700 800 900 1000 0\n";
        let output = String::from_utf8(sanitize_stat(stat, None)).unwrap();
        let comm_end = output.rfind(") ").unwrap();
        let fields = output[comm_end + 2..]
            .split_whitespace()
            .collect::<Vec<_>>();
        assert!(output.starts_with("1 (hermit) S 0 1 1 "));
        assert_eq!(fields[23 - 3], "0");
        assert_eq!(fields[24 - 3], "0");
        assert_eq!(fields[25 - 3], "0");
        assert_eq!(fields[45 - 3], "0");

        assert_eq!(sanitize_statm(b"123 45 6 7 0 89 0\n"), b"0 0 0 0 0 0 0\n");

        let status =
            b"Name:\thermit\nState:\tR (running)\nPid:\t1\nPPid:\t0\nVmSize:\t  248700 kB\nVmRSS:\t   30956 kB\n";
        assert_eq!(
            sanitize_status(status, None),
            b"Name:\thermit\nState:\tS (sleeping)\nPid:\t1\nPPid:\t0\nVmSize:\t       0 kB\nVmRSS:\t       0 kB\n"
        );
    }

    #[test]
    fn meminfo_uses_configured_virtual_memory() {
        let input = b"MemTotal:       791462428 kB\nMemFree:         1234567 kB\nMemAvailable:  700000000 kB\nBuffers:           1234 kB\nCached:        500000000 kB\nSwapTotal:     136218620 kB\nSwapFree:       69649352 kB\nHugepagesize:    1048576 kB\n";
        assert_eq!(
            sanitize_meminfo(input, 1_000_000_000),
            b"MemTotal: 976562 kB\nMemFree: 976562 kB\nMemAvailable: 976562 kB\nBuffers: 0 kB\nCached: 0 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\nHugepagesize:    1048576 kB\n"
        );
    }

    #[test]
    fn system_stat_uses_fixed_cpu_counters() {
        let input = b"cpu  10 20 30 40 50 60 70 80 90 100\ncpu0 1 2 3 4 5 6 7 8 9 10\nintr 999 1 2\nctxt 123\nbtime 456\nprocesses 789\nprocs_running 8\nprocs_blocked 3\nsoftirq 999 1 2\n";
        assert_eq!(
            sanitize_system_stat(input, 120),
            b"cpu 0 0 0 12000 0 0 0 0 0 0\ncpu0 0 0 0 12000 0 0 0 0 0 0\nintr 0\nctxt 0\nbtime 456\nprocesses 0\nprocs_running 1\nprocs_blocked 0\nsoftirq 0\n"
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
        file.initialize(
            b"voluntary_ctxt_switches:\t12\n".to_vec(),
            120,
            1_000_000_000,
            3,
            1,
        );
        assert_eq!(file.take(5).unwrap(), b"volun");
        file.seek(0);
        assert_eq!(file.take(128).unwrap(), b"voluntary_ctxt_switches:\t0\n");
        assert!(file.take(1).unwrap().is_empty());
        file.seek(usize::MAX);
        assert!(file.take(1).unwrap().is_empty());
    }
}
