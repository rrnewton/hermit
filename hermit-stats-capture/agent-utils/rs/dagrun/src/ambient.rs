//! Ambient host-load capture: pure `/proc` readers plus a quiet/moderate/busy verdict.
//!
//! Everything here reads only `/proc` and process counters; nothing writes, forks, or touches
//! cgroupfs. The [`ambient_bucket`] thresholds form a stable profile-classification contract.

use std::fs;
use std::path::{Path, PathBuf};

/// `/proc/stat`.
pub const PROC_STAT_PATH: &str = "/proc/stat";
/// `/proc/pressure/cpu`.
pub const PROC_CPU_PRESSURE_PATH: &str = "/proc/pressure/cpu";
/// `/proc/pressure/memory`.
pub const PROC_MEMORY_PRESSURE_PATH: &str = "/proc/pressure/memory";
/// `/proc/pressure/io`.
pub const PROC_IO_PRESSURE_PATH: &str = "/proc/pressure/io";

// ambient_bucket cut-offs. PRESERVE THESE EXACTLY (cross-language parity with ambient.py).
const BUSY_EXTERNAL_CORES: f64 = 2.0;
const BUSY_PSI_AVG10: f64 = 20.0;
const BUSY_CO_TENANTS: i64 = 8;
const QUIET_EXTERNAL_CORES: f64 = 0.5;
const QUIET_PSI_AVG10: f64 = 5.0;
const QUIET_CO_TENANTS: i64 = 2;
// Fallback USER_HZ when `sysconf(SC_CLK_TCK)` is unavailable (matches Python's `or 100`).
const DEFAULT_CLK_TCK: i64 = 100;

/// Three-level ambient-load verdict recorded in execution profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbientBucket {
    /// The host has little observable competing work.
    Quiet,
    /// The host has some competing work but is below the busy thresholds.
    Moderate,
    /// The host exceeds at least one contention threshold.
    Busy,
}

impl AmbientBucket {
    /// Return the canonical lowercase value used in profile records.
    pub fn value(self) -> &'static str {
        match self {
            AmbientBucket::Quiet => "quiet",
            AmbientBucket::Moderate => "moderate",
            AmbientBucket::Busy => "busy",
        }
    }
}

/// One Pressure-Stall-Information `some` line (`avg10` / `avg60` fractions).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PsiReading {
    /// Percentage of wall time stalled during the previous ten seconds.
    pub avg10: f64,
    /// Percentage of wall time stalled during the previous sixty seconds.
    pub avg60: f64,
}

/// One instant of host-wide load (the typed record `capture_ambient_snapshot` returns).
#[derive(Debug, Clone)]
pub struct AmbientSnapshot {
    /// System-wide non-idle CPU jiffies (`/proc/stat` `cpu` line), or `None` when unreadable.
    pub busy_jiffies: Option<i64>,
    /// One-minute system load average.
    pub load1: f64,
    /// Five-minute system load average.
    pub load5: f64,
    /// CPU pressure-stall sample, when available.
    pub cpu_psi: Option<PsiReading>,
    /// Memory pressure-stall sample, when available.
    pub memory_psi: Option<PsiReading>,
    /// I/O pressure-stall sample, when available.
    pub io_psi: Option<PsiReading>,
    /// Count of matching build processes running OUTSIDE this run's cgroup scope.
    pub co_tenants: i64,
}

/// USER_HZ (jiffies per second), falling back to 100 when it cannot be read.
fn clk_tck() -> i64 {
    // The `/proc/self/stat`-based tick is not exposed by std; read it via `getconf` when present,
    // otherwise use the conventional 100 Hz (matching Python's `os.sysconf(...) or 100` fallback
    // on hosts where sysconf is unavailable).
    if let Ok(out) = std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
    {
        if out.status.success() {
            if let Ok(n) = String::from_utf8_lossy(&out.stdout).trim().parse::<i64>() {
                if n > 0 {
                    return n;
                }
            }
        }
    }
    DEFAULT_CLK_TCK
}

/// 1/5/15-minute load averages from `/proc/loadavg` (Linux `os.getloadavg` analogue).
pub fn read_loadavg() -> (f64, f64, f64) {
    if let Ok(text) = fs::read_to_string("/proc/loadavg") {
        let mut parts = text.split_whitespace();
        let l1 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let l5 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        let l15 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
        return (l1, l5, l15);
    }
    (0.0, 0.0, 0.0)
}

/// System-wide non-idle CPU jiffies from `/proc/stat`'s first `cpu` line.
///
/// `busy = total - idle - iowait` (idle is field 3, iowait field 4 after `cpu`). `None` on any
/// read or parse error.
pub fn host_busy_jiffies(stat_path: &Path) -> Option<i64> {
    let text = fs::read_to_string(stat_path).ok()?;
    let first = text.lines().next()?;
    let values: Vec<i64> = first
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse::<i64>())
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    let idle = values[3];
    let iowait = if values.len() > 4 { values[4] } else { 0 };
    Some(values.iter().sum::<i64>() - idle - iowait)
}

/// Parse the `some` line of a PSI file, or `None` when absent/malformed.
pub fn read_pressure(path: &Path) -> Option<PsiReading> {
    let text = fs::read_to_string(path).ok()?;
    let some = text.lines().find(|l| l.starts_with("some "))?;
    let mut avg10: Option<f64> = None;
    let mut avg60: Option<f64> = None;
    for item in some.split_whitespace().skip(1) {
        if let Some((k, v)) = item.split_once('=') {
            match k {
                "avg10" => avg10 = v.parse().ok(),
                "avg60" => avg60 = v.parse().ok(),
                _ => {}
            }
        }
    }
    Some(PsiReading {
        avg10: avg10?,
        avg60: avg60?,
    })
}

/// Count build processes whose `comm` is in `build_process_names` and that run OUTSIDE this
/// run's cgroup scope (`scope_marker` absent from `/proc/<pid>/cgroup`).
pub fn count_external_build_processes(
    build_process_names: &[&str],
    scope_marker: Option<&str>,
) -> i64 {
    let names: std::collections::HashSet<&str> = build_process_names.iter().copied().collect();
    if names.is_empty() {
        // No build-process names to match => nothing to count. Return early so a caller that only
        // wants the load/PSI parts of a snapshot (the generic runner, which has no project
        // build-process names) does not pay for a full /proc scan that could only ever return 0.
        return 0;
    }
    let marker = scope_marker.unwrap_or("");
    let entries = match fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return 0,
    };
    let mut count = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) if n.chars().all(|c| c.is_ascii_digit()) => n.to_string(),
            _ => continue,
        };
        let dir = PathBuf::from("/proc").join(&name);
        let comm = match fs::read_to_string(dir.join("comm")) {
            Ok(c) => c.trim().to_string(),
            Err(_) => continue,
        };
        let cgroup = match fs::read_to_string(dir.join("cgroup")) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if names.contains(comm.as_str()) && (marker.is_empty() || !cgroup.contains(marker)) {
            count += 1;
        }
    }
    count
}

/// Capture one typed snapshot of current host load.
pub fn capture_ambient_snapshot(
    build_process_names: &[&str],
    scope_marker: Option<&str>,
) -> AmbientSnapshot {
    let (load1, load5, _) = read_loadavg();
    AmbientSnapshot {
        busy_jiffies: host_busy_jiffies(Path::new(PROC_STAT_PATH)),
        load1,
        load5,
        cpu_psi: read_pressure(Path::new(PROC_CPU_PRESSURE_PATH)),
        memory_psi: read_pressure(Path::new(PROC_MEMORY_PRESSURE_PATH)),
        io_psi: read_pressure(Path::new(PROC_IO_PRESSURE_PATH)),
        co_tenants: count_external_build_processes(build_process_names, scope_marker),
    }
}

/// Estimate how many CPU cores other tenants used during a step window.
///
/// Returns `0.0` for a non-positive `elapsed_s`.
pub fn attribute_external_cores(
    busy_jiffies_start: Option<i64>,
    busy_jiffies_end: Option<i64>,
    own_cpu_usec: i64,
    elapsed_s: f64,
) -> f64 {
    let host_busy_s = match (busy_jiffies_start, busy_jiffies_end) {
        (Some(a), Some(b)) => (b - a) as f64 / clk_tck() as f64,
        _ => 0.0,
    };
    let external_cpu_s = (host_busy_s - own_cpu_usec as f64 / 1_000_000.0).max(0.0);
    if elapsed_s <= 0.0 {
        return 0.0;
    }
    external_cpu_s / elapsed_s
}

/// Classify host load as quiet, moderate, or busy using the stable profile thresholds.
///
/// A missing PSI reading contributes an `avg10` value of zero.
pub fn ambient_bucket(external_cores: f64, snapshot: &AmbientSnapshot) -> AmbientBucket {
    let max_avg10 = [snapshot.cpu_psi, snapshot.memory_psi, snapshot.io_psi]
        .into_iter()
        .flatten()
        .map(|p| p.avg10)
        .fold(0.0_f64, f64::max);
    let co = snapshot.co_tenants;
    if external_cores > BUSY_EXTERNAL_CORES || max_avg10 >= BUSY_PSI_AVG10 || co >= BUSY_CO_TENANTS
    {
        return AmbientBucket::Busy;
    }
    if external_cores < QUIET_EXTERNAL_CORES
        && max_avg10 < QUIET_PSI_AVG10
        && co <= QUIET_CO_TENANTS
    {
        return AmbientBucket::Quiet;
    }
    AmbientBucket::Moderate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(cpu_avg10: f64, co: i64) -> AmbientSnapshot {
        AmbientSnapshot {
            busy_jiffies: Some(0),
            load1: 0.0,
            load5: 0.0,
            cpu_psi: Some(PsiReading {
                avg10: cpu_avg10,
                avg60: 0.0,
            }),
            memory_psi: None,
            io_psi: None,
            co_tenants: co,
        }
    }

    #[test]
    fn bucket_thresholds_match_reference() {
        // busy: external cores > 2.0
        assert_eq!(ambient_bucket(2.5, &snap(0.0, 0)), AmbientBucket::Busy);
        // busy: any PSI avg10 >= 20
        assert_eq!(ambient_bucket(0.0, &snap(20.0, 0)), AmbientBucket::Busy);
        // busy: co-tenants >= 8
        assert_eq!(ambient_bucket(0.0, &snap(0.0, 8)), AmbientBucket::Busy);
        // quiet: all below the quiet cut-offs
        assert_eq!(ambient_bucket(0.4, &snap(4.9, 2)), AmbientBucket::Quiet);
        // moderate: in between
        assert_eq!(ambient_bucket(1.0, &snap(10.0, 3)), AmbientBucket::Moderate);
    }

    #[test]
    fn attribute_external_cores_guards_zero_window() {
        assert_eq!(attribute_external_cores(Some(0), Some(100), 0, 0.0), 0.0);
        // With a positive window: (100 jiffies / clk) - 0 own, over 1s.
        let cores = attribute_external_cores(Some(0), Some(clk_tck()), 0, 1.0);
        assert!((cores - 1.0).abs() < 1e-9);
    }
}
