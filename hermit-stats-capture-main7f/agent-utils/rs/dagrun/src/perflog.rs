//! Always-on perf logging for a DAG run: whole-run window + per-step profile CSVs.
//!
//! The stable on-disk schema contains:
//! * [`CSV_COLUMNS`] — one whole-run summary row per run, in `<output_dir>/<machine_id>.csv`.
//! * [`STEP_PROFILE_COLUMNS`] — per-step measurement rows, in
//!   `<output_dir>/step_profiles_<machine_id>_<container_class>.csv`, widened per run with any
//!   dynamic `cpu.*` keys (existing columns first, new columns appended; no column ever dropped).
//! * [`STEP_TIMESERIES_COLUMNS`] — opt-in interval samples, in
//!   `<output_dir>/traces/<run_id>.csv`, separate from the trial-level estimator input.
//!
//! The contention split (`pct_we` / `pct_other` / `total_busy_pct`) is derived from `/proc/stat`
//! sampled at window start/end vs. this process's accumulated CHILD CPU time (from
//! `/proc/self/stat` `cutime`/`cstime`, the getrusage(RUSAGE_CHILDREN) analogue — read without
//! `unsafe`). The output directory is an explicit argument and is created on demand; a failure to
//! create/write it is a visible warning (No Silent Failure), never a silent skip.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::ambient::host_busy_jiffies;

/// Whole-run summary columns (append-only: never reorder or remove).
pub const CSV_COLUMNS: [&str; 15] = [
    "timestamp",
    "machine_id",
    "git_sha",
    "nproc",
    "wall_s",
    "user_s",
    "sys_s",
    "result",
    "n_steps",
    "pct_we",
    "pct_other",
    "total_busy_pct",
    "ipc",
    "cache_miss_pct",
    "jobs",
];

/// Standard per-step profile columns, in stable append-only order.
///
/// Dynamic `cpu.*` keys follow these columns. Parallel-speedup fields remain empty when the
/// corresponding cgroup measurement is unavailable.
///
/// The final block is the provenance a later reader needs to tell a TRUE peak from a CENSORED
/// one, and to know which rows ran at the same time. `peak_bytes` on its own does not say what it
/// measured: a step whose peak equals the `memory.max` applied to it used everything it was
/// allowed and may have wanted more, and treating that as an observed maximum under-estimates the
/// workload permanently.
///
/// * `run_id` groups the rows of exactly ONE DAG execution. Two concurrent runs on one machine
///   share `machine_id`, `container_class`, `git_sha` and often `outer_jobs`, and `timestamp` is
///   stamped once per BATCH, so no other key separates them.
/// * `started_offset_s` / `finished_offset_s` are seconds from that run's own monotonic origin, so
///   the overlap of two steps is a comparison of two intervals.
/// * `memory_max_bytes` is the TIGHTEST cap the KERNEL held over the step across every level the
///   runner can see (its own cgroup and the delegated scope): a decimal byte count, the literal
///   `max` when no such level bounds it, or BLANK when unknown. Blank and `max` are different
///   answers — unknown cannot rule out censoring, while `max` rules it out for the runner's
///   recorded caps. Nested schedulers additionally snapshot the validated parent delegation and
///   its visible ancestors; limits above a top-level run scope or outside the cgroup namespace
///   remain unknowable.
/// * `memory_events_*` are the step cgroup's owned counters: `memory.events` for an ordinary leaf
///   and `memory.events.local` for a delegated scheduler root. They are already per-step deltas
///   because the cgroup lives exactly as long as the step. `memory_events_max > 0` with every
///   OOM counter zero is reclaim-at-cap: a PASSING step pinned to its ceiling. The
///   reclaim forced by an ancestor's limit is not counted here, so `memory_events_max == 0` is
///   not by itself proof of a comfortable fit. The local delegated view prevents an inner step's
///   independently owned event from being attributed again to its outer scheduler.
///   `memory_events_oom_group_kill` preserves kernels' group-kill counter; at a delegated
///   boundary it can be the only durable kill signal.
pub const STEP_PROFILE_COLUMNS: [&str; 59] = [
    "timestamp",
    "machine_id",
    "container_class",
    "git_sha",
    "outer_jobs",
    "profile_base_sha",
    "enforcement_kind",
    "runner_name",
    "step",
    "classification",
    "inner_jobs",
    "elapsed_s",
    "returncode",
    "ok",
    "timed_out",
    "cpu_timed_out",
    "oom_kills",
    "peak_bytes",
    "thread_peak",
    // --- rich parallel-speedup enrichment (blank when unavailable) ---
    "effective_cores",
    "user_s",
    "sys_s",
    "throttled_s",
    "quota_utilization_pct",
    "external_cpu_s",
    "external_cores",
    "co_tenants_start",
    "co_tenants_end",
    "ambient_bucket",
    "load1_start",
    "load1_end",
    "load5_start",
    "load5_end",
    "host_cpu_psi_avg10_start",
    "host_cpu_psi_avg10_end",
    "host_cpu_psi_avg60_start",
    "host_cpu_psi_avg60_end",
    "host_memory_psi_avg10_start",
    "host_memory_psi_avg10_end",
    "host_memory_psi_avg60_start",
    "host_memory_psi_avg60_end",
    "host_io_psi_avg10_start",
    "host_io_psi_avg10_end",
    "host_io_psi_avg60_start",
    "host_io_psi_avg60_end",
    "step_cpu_psi_avg10_start",
    "step_cpu_psi_avg10_end",
    "step_cpu_psi_avg60_start",
    "step_cpu_psi_avg60_end",
    // --- run-overlap + applied-cap provenance (blank when unavailable) ---
    "run_id",
    "started_offset_s",
    "finished_offset_s",
    "memory_max_bytes",
    "memory_events_low",
    "memory_events_high",
    "memory_events_max",
    "memory_events_oom",
    "memory_events_oom_kill",
    "memory_events_oom_group_kill",
];

/// Stable columns for an opt-in per-step CPU/thread time series.
///
/// The provenance prefix identifies the machine, execution, and enforcement lane. Sweep-only
/// metadata is appended as a sorted dynamic tail so ordinary runs keep this compact schema while
/// sweep traces remain attributable to their pass, width, and workload cohort.
pub const STEP_TIMESERIES_COLUMNS: [&str; 24] = [
    "timestamp",
    "machine_id",
    "container_class",
    "git_sha",
    "outer_jobs",
    "profile_base_sha",
    "enforcement_kind",
    "runner_name",
    "run_id",
    "step",
    "inner_jobs",
    "sample_index",
    "sample_kind",
    "elapsed_s",
    "interval_s",
    "cpu_usage_s",
    "user_s",
    "sys_s",
    "effective_cores",
    "user_cores",
    "system_cores",
    "throttled_s",
    "interval_throttled_s",
    "thread_count",
];

/// A per-step measurement row (heterogeneous column -> value, matching the CSV).
pub type ProfileRow = BTreeMap<String, String>;

/// Per-machine identifier from `/proc/cpuinfo` "model name" (spaces -> `_`, then non
/// `[A-Za-z0-9_-]` stripped). Falls back to the hostname, then `"unknown"`.
pub fn machine_id() -> String {
    let sanitize = |name: &str| -> String {
        name.chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
            .collect()
    };
    if let Ok(text) = fs::read_to_string("/proc/cpuinfo") {
        for line in text.lines() {
            if line.starts_with("model name") {
                if let Some((_, v)) = line.split_once(':') {
                    let underscored = v.trim().replace(' ', "_");
                    let cleaned = sanitize(&underscored);
                    if !cleaned.is_empty() {
                        return cleaned;
                    }
                }
            }
        }
    }
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let cleaned = sanitize(host.trim());
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// A fresh identifier for ONE DAG execution: nanoseconds since the epoch and the runner PID, both
/// hex, concatenated.
///
/// Uniqueness comes from the pair, not either half. Two runs on one host cannot share a start
/// nanosecond, and a PID reused after a clock step cannot collide with the earlier run that held
/// it. The value is opaque: a reader groups rows by equality and never parses it.
pub fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}{:08x}", nanos, std::process::id())
}

/// Return the process CPU-affinity width.
///
/// The value is deliberately not clamped to a cgroup quota; quota information is represented
/// separately by [`container_class`]. If the affinity mask is unavailable, this falls back to the
/// runtime's available or logical processor count.
pub fn nproc() -> i64 {
    if let Some(n) = affinity_width() {
        return n;
    }
    std::thread::available_parallelism()
        .map(|n| n.get() as i64)
        .unwrap_or_else(|_| crate::sizing::cpu_count())
}

/// CPU-affinity width from `/proc/self/status` `Cpus_allowed_list` (a CPU-range list such as
/// `0-7,16-23`), matching `len(os.sched_getaffinity(0))`. `None` when the field is absent or
/// unparsable.
fn affinity_width() -> Option<i64> {
    let status = fs::read("/proc/self/status").ok()?;
    affinity_width_from_status(&status)
}

fn affinity_width_from_status(status: &[u8]) -> Option<i64> {
    let list = crate::procstat::status_field(status, b"Cpus_allowed_list")
        .ok()
        .flatten()?;
    crate::sizing::count_cpu_ranges(std::str::from_utf8(list).ok()?)
}

/// Stable CPU-container key: affinity width plus the cgroup-v2 CPU quota (`/sys/fs/cgroup/cpu.max`).
/// This process's cgroup-v2 path relative to the mount root, e.g. `/user.slice/foo.scope`.
fn own_cgroup_relpath() -> Option<String> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("0::") {
            let rest = rest.trim();
            return Some(if rest.is_empty() {
                "/".to_string()
            } else {
                rest.to_string()
            });
        }
    }
    None
}

/// The CPU quota actually BINDING this process, as a normalized `<quota>_<period>` string.
///
/// Walks the selected cgroup and every ancestor up to the mount root and returns the most
/// restrictive `cpu.max` (smallest quota/period ratio), because a cgroup-v2 quota is enforced by
/// the whole ancestor chain, not just the leaf. Returns `max` when nothing in the chain constrains
/// CPU, and `unknown` when the hierarchy cannot be read at all.
///
/// The mount-root `/sys/fs/cgroup/cpu.max` must NOT be used for this: on an ordinary host that file
/// does not exist, so the quota half of [`container_class`] degenerates to the constant `unknown`
/// and stops distinguishing anything. A slice several levels up is where a CPU bandwidth cap
/// typically lives, so a step can run with a full-width affinity mask and still be held to a
/// fraction of a core by an ancestor's quota.
pub fn effective_cpu_quota_at(mount_root: &Path, relpath: &str) -> String {
    let mut best_ratio: Option<f64> = None;
    let mut best_text = "max".to_string();
    let mut seen_any = false;
    let mut node = mount_root.join(relpath.trim_start_matches('/'));
    loop {
        if let Ok(raw) = fs::read_to_string(node.join("cpu.max")) {
            seen_any = true;
            let parts: Vec<&str> = raw.split_whitespace().collect();
            if parts.len() == 2 && parts[0] != "max" {
                if let (Ok(quota), Ok(period)) = (parts[0].parse::<i64>(), parts[1].parse::<i64>())
                {
                    if period > 0 {
                        let ratio = quota as f64 / period as f64;
                        if best_ratio.is_none_or(|b| ratio < b) {
                            best_ratio = Some(ratio);
                            best_text = format!("{quota}_{period}");
                        }
                    }
                }
            }
        }
        if node == mount_root {
            break;
        }
        match node.parent() {
            Some(parent) if parent.starts_with(mount_root) => node = parent.to_path_buf(),
            _ => break,
        }
    }
    if seen_any {
        best_text
    } else {
        "unknown".to_string()
    }
}

/// The CPU quota binding this process, resolved from its own cgroup hierarchy.
pub fn effective_cpu_quota() -> String {
    match own_cgroup_relpath() {
        Some(rel) => effective_cpu_quota_at(Path::new("/sys/fs/cgroup"), &rel),
        None => "unknown".to_string(),
    }
}

/// Stable CPU-container key: affinity width plus the CPU quota binding this process.
///
/// Both halves matter and they constrain independently: a cpuset narrows WHICH cpus (reflected in
/// [`nproc`] via the affinity mask) while `cpu.max` caps total CPU BANDWIDTH. Samples taken under
/// different containers must not share a key, or the speedup model fits one curve across
/// incompatible populations.
pub fn container_class() -> String {
    format!("affinity{}_cpu-max-{}", nproc(), effective_cpu_quota())
}

/// The whole-run summary CSV file name for a machine: `<machine_id>.csv`.
fn whole_run_csv_name(mid: &str) -> String {
    format!("{mid}.csv")
}

/// The per-step profile CSV file name for a machine/container:
/// `step_profiles_<machine_id>_<container_class>.csv`.
fn step_profiles_csv_name(mid: &str, cc: &str) -> String {
    format!("step_profiles_{mid}_{cc}.csv")
}

/// The profile-store file paths a single `run`/`sweep` writes into `output_dir`: the whole-run
/// summary (`<machine_id>.csv`) and the per-step profile CSV
/// (`step_profiles_<machine_id>_<container_class>.csv`). The CLI reports EXACTLY these (filtered to
/// files that exist) rather than globbing the whole store — a persistent store also holds prior
/// runs' other-`container_class` variants on the same machine, and globbing would over-report files
/// this run never wrote.
pub fn store_paths(output_dir: &Path) -> Vec<PathBuf> {
    let mid = machine_id();
    let cc = container_class();
    vec![
        output_dir.join(whole_run_csv_name(&mid)),
        output_dir.join(step_profiles_csv_name(&mid, &cc)),
    ]
}

/// This process's accumulated CHILD (user, sys) CPU seconds, from `/proc/self/stat`
/// `cutime`/`cstime` (fields 16/17, in clock ticks). The getrusage(RUSAGE_CHILDREN) analogue.
/// Public so the sweep (`cli.rs`) can bracket a single-step run and derive its user/sys CPU.
pub fn child_cpu_seconds() -> (f64, f64) {
    let stat = match crate::procstat::read(std::process::id()) {
        Ok(Some(stat)) => stat,
        Ok(None) | Err(_) => return (0.0, 0.0),
    };
    let cutime = stat.child_utime_ticks as f64;
    let cstime = stat.child_stime_ticks as f64;
    let tck = clk_tck() as f64;
    (cutime / tck, cstime / tck)
}

fn clk_tck() -> i64 {
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
    100
}

// Canonical RFC-3339 UTC timestamp `YYYY-MM-DDTHH:MM:SSZ`, matching the Python writer.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Howard Hinnant's days-from-civil inverse (days since 1970-01-01 -> (year, month, day)).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn ensure_dir(output_dir: &Path) -> Option<PathBuf> {
    match fs::create_dir_all(output_dir) {
        Ok(()) => Some(output_dir.to_path_buf()),
        Err(e) => {
            eprintln!(
                "[perflog] skipped: cannot create output dir {} ({e})",
                output_dir.display()
            );
            None
        }
    }
}

// --- minimal CSV field escaping (quote when the value has a comma/quote/newline) ---

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

pub(crate) fn parse_csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else {
            match c {
                ',' => record.push(std::mem::take(&mut field)),
                '\n' => {
                    record.push(std::mem::take(&mut field));
                    if record.len() != 1 || !record[0].is_empty() {
                        records.push(std::mem::take(&mut record));
                    } else {
                        record.clear();
                    }
                }
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    record.push(std::mem::take(&mut field));
                    if record.len() != 1 || !record[0].is_empty() {
                        records.push(std::mem::take(&mut record));
                    } else {
                        record.clear();
                    }
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

#[cfg(test)]
pub(crate) fn parse_csv_line(line: &str) -> Vec<String> {
    parse_csv_records(line)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn write_row(out: &mut String, header: &[String], row: &BTreeMap<String, String>) {
    let cells: Vec<String> = header
        .iter()
        .map(|col| csv_field(row.get(col).map(String::as_str).unwrap_or("")))
        .collect();
    out.push_str(&cells.join(","));
    out.push('\n');
}

/// Append `rows` under a header that is the union of any pre-existing header and `fieldnames`
/// (existing columns first, then new columns appended). Widens an existing narrower file in place.
fn append_rows_merging_header(
    csv_path: &Path,
    rows: &[BTreeMap<String, String>],
    fieldnames: &[String],
) -> std::io::Result<()> {
    let existing = match fs::read_to_string(csv_path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let is_new = existing.as_deref().map(str::is_empty).unwrap_or(true);
    if is_new {
        let mut out = String::new();
        let header: Vec<String> = fieldnames.to_vec();
        out.push_str(
            &header
                .iter()
                .map(|h| csv_field(h))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        for row in rows {
            write_row(&mut out, &header, row);
        }
        return fs::write(csv_path, out);
    }
    let text = existing.unwrap();
    let records = parse_csv_records(&text);
    let old_header: Vec<String> = records.first().cloned().unwrap_or_default();
    let mut widened = old_header.clone();
    for col in fieldnames {
        if !widened.contains(col) {
            widened.push(col.clone());
        }
    }
    if widened != old_header {
        let mut out = String::new();
        out.push_str(
            &widened
                .iter()
                .map(|h| csv_field(h))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        // Re-project old rows onto the widened header so older rows keep their columns.
        for cells in records.into_iter().skip(1) {
            let old_row: BTreeMap<String, String> = old_header.iter().cloned().zip(cells).collect();
            write_row(&mut out, &widened, &old_row);
        }
        for row in rows {
            write_row(&mut out, &widened, row);
        }
        let temporary = csv_path.with_extension(format!("csv.{}.tmp", std::process::id()));
        fs::write(&temporary, out)?;
        fs::rename(temporary, csv_path)
    } else {
        // Ordinary appends never rewrite the historical dataset. Besides being cheaper, this
        // avoids turning an interrupted metrics write into loss of every earlier sample.
        let mut out = String::new();
        for row in rows {
            write_row(&mut out, &widened, row);
        }
        let mut file = OpenOptions::new().append(true).open(csv_path)?;
        file.write_all(out.as_bytes())
    }
}

/// One persistent advisory lock shared by every compatible profile-store writer.
pub(crate) struct ProfileFileLock {
    file: fs::File,
}

impl ProfileFileLock {
    /// Lock the stable sidecar inode under a hidden `.locks` directory.
    ///
    /// Unlinking a held lock would let a racing process create and lock a different inode while
    /// this critical section is still live, so the zero-byte coordination file is retained.
    pub(crate) fn acquire(data_path: &Path) -> std::io::Result<Self> {
        let parent = data_path.parent().unwrap_or_else(|| Path::new("."));
        let lock_dir = parent.join(".locks");
        fs::create_dir_all(&lock_dir)?;
        let name = data_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("profile.data");
        let lock_path = lock_dir.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        // SAFETY: `flock` operates on this live descriptor and retains no pointer.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { file })
    }
}

impl Drop for ProfileFileLock {
    fn drop(&mut self) {
        // SAFETY: `flock` operates on this live descriptor and retains no pointer.
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn with_file_lock<F>(csv_path: &Path, f: F) -> std::io::Result<()>
where
    F: FnOnce() -> std::io::Result<()>,
{
    let _lock = ProfileFileLock::acquire(csv_path)?;
    f()
}

/// Append per-step measurement `rows` to a machine/container-specific CSV. Returns the path, or
/// `None` when the output dir could not be created (a visible warning is emitted).
///
/// `run_id` identifies the one DAG execution these rows came from; pass the run's own id so every
/// batch of a single execution carries the same value, because that column is the only thing that
/// separates two concurrent runs sharing this machine, container class and commit. `None` mints a
/// fresh one via [`new_run_id`].
#[allow(clippy::too_many_arguments)]
pub fn append_step_profiles(
    output_dir: &Path,
    rows: &[ProfileRow],
    git_sha: &str,
    outer_jobs: i64,
    profile_base_sha: Option<&str>,
    enforcement_kind: &str,
    runner_name: &str,
    run_id: Option<&str>,
) -> Option<PathBuf> {
    let dir = ensure_dir(output_dir)?;
    let mid = machine_id();
    let cc = container_class();
    let path = dir.join(step_profiles_csv_name(&mid, &cc));
    let mut common: BTreeMap<String, String> = BTreeMap::new();
    common.insert("timestamp".into(), timestamp());
    common.insert("machine_id".into(), mid);
    common.insert("container_class".into(), cc);
    common.insert("git_sha".into(), git_sha.into());
    common.insert("outer_jobs".into(), outer_jobs.to_string());
    common.insert(
        "profile_base_sha".into(),
        profile_base_sha.unwrap_or(git_sha).into(),
    );
    common.insert("enforcement_kind".into(), enforcement_kind.into());
    common.insert("runner_name".into(), runner_name.into());
    common.insert(
        "run_id".into(),
        run_id.map_or_else(new_run_id, str::to_string),
    );

    let full_rows: Vec<BTreeMap<String, String>> = rows
        .iter()
        .map(|row| {
            let mut r = common.clone();
            for (k, v) in row {
                r.insert(k.clone(), v.clone());
            }
            r
        })
        .collect();

    // Field order: the standard columns first, then any extra per-row keys (dynamic cpu.* counters)
    // SORTED ALPHABETICALLY. Both builds sort this dynamic tail identically (the Python build sorts
    // the same set in _step_profile_fieldnames), so the CSV header is byte-identical across
    // languages — asserted by the cross/ differential. A BTreeSet keeps the tail sorted regardless
    // of row-key iteration order.
    let standard: std::collections::HashSet<&str> = STEP_PROFILE_COLUMNS.iter().copied().collect();
    let mut extra: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for row in &full_rows {
        for key in row.keys() {
            if !standard.contains(key.as_str()) {
                extra.insert(key.clone());
            }
        }
    }
    let mut fieldnames: Vec<String> = STEP_PROFILE_COLUMNS.iter().map(|s| s.to_string()).collect();
    fieldnames.extend(extra);

    match with_file_lock(&path, || {
        append_rows_merging_header(&path, &full_rows, &fieldnames)
    }) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("[perflog] step-profile write failed ({error})");
            None
        }
    }
}

/// Write one execution's opt-in per-step time series to
/// `<output_dir>/traces/<run_id>.csv` and return that exact path.
///
/// Unlike the machine-wide step profile, this file is execution-scoped. Callers use the same
/// `run_id` for this trace and the corresponding aggregate rows. Missing measurements remain
/// empty cells.
#[allow(clippy::too_many_arguments)]
pub fn append_step_timeseries(
    output_dir: &Path,
    rows: &[ProfileRow],
    git_sha: &str,
    outer_jobs: i64,
    profile_base_sha: Option<&str>,
    enforcement_kind: &str,
    runner_name: &str,
    run_id: &str,
) -> Option<PathBuf> {
    if rows.is_empty() {
        return None;
    }
    let traces = ensure_dir(&output_dir.join("traces"))?;
    let safe_run_id: String = run_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let safe_run_id = if safe_run_id.is_empty() {
        "unknown"
    } else {
        &safe_run_id
    };
    let path = traces.join(format!("{safe_run_id}.csv"));
    let mut common: BTreeMap<String, String> = BTreeMap::new();
    common.insert("timestamp".into(), timestamp());
    common.insert("machine_id".into(), machine_id());
    common.insert("container_class".into(), container_class());
    common.insert("git_sha".into(), git_sha.into());
    common.insert("outer_jobs".into(), outer_jobs.to_string());
    common.insert(
        "profile_base_sha".into(),
        profile_base_sha.unwrap_or(git_sha).into(),
    );
    common.insert("enforcement_kind".into(), enforcement_kind.into());
    common.insert("runner_name".into(), runner_name.into());
    common.insert("run_id".into(), run_id.into());

    let full_rows: Vec<BTreeMap<String, String>> = rows
        .iter()
        .map(|row| {
            let mut full = common.clone();
            full.extend(row.clone());
            full
        })
        .collect();
    let standard: std::collections::HashSet<&str> =
        STEP_TIMESERIES_COLUMNS.iter().copied().collect();
    let mut extra = std::collections::BTreeSet::new();
    for row in &full_rows {
        for key in row.keys() {
            if !standard.contains(key.as_str()) {
                extra.insert(key.clone());
            }
        }
    }
    let mut columns: Vec<String> = STEP_TIMESERIES_COLUMNS
        .iter()
        .map(|value| value.to_string())
        .collect();
    columns.extend(extra);

    match with_file_lock(&path, || {
        append_rows_merging_header(&path, &full_rows, &columns)
    }) {
        Ok(()) => Some(path),
        Err(error) => {
            eprintln!("[perflog] time-series write failed ({error})");
            None
        }
    }
}

/// A started whole-run measurement bracket; [`PerfWindow::finish`] appends one summary row.
pub struct PerfWindow {
    output_dir: PathBuf,
    git_sha: String,
    machine_id: String,
    wall_start: Instant,
    user_start: f64,
    sys_start: f64,
    busy_start: Option<i64>,
}

impl PerfWindow {
    /// Open the window, capturing baseline counters at the current instant.
    pub fn start(output_dir: &Path, git_sha: &str) -> Self {
        let (u, s) = child_cpu_seconds();
        PerfWindow {
            output_dir: output_dir.to_path_buf(),
            git_sha: git_sha.to_string(),
            machine_id: machine_id(),
            wall_start: Instant::now(),
            user_start: u,
            sys_start: s,
            busy_start: host_busy_jiffies(Path::new("/proc/stat")),
        }
    }

    /// Compute window metrics and append the summary row (recorded for pass AND fail).
    pub fn finish(&self, result: &str, n_steps: usize, jobs: i64) -> Option<PathBuf> {
        let wall = self.wall_start.elapsed().as_secs_f64().max(0.0);
        let (u_end, s_end) = child_cpu_seconds();
        let user_s = (u_end - self.user_start).max(0.0);
        let sys_s = (s_end - self.sys_start).max(0.0);
        let ncpu = nproc();
        let capacity = if wall > 0.0 { ncpu as f64 * wall } else { 0.0 };
        let our_cpu = user_s + sys_s;

        let pct = |x: f64| -> String {
            if capacity > 0.0 {
                format!("{:.2}", 100.0 * x / capacity)
            } else {
                String::new()
            }
        };
        let pct_we = pct(our_cpu);
        let (pct_other, total_busy_pct) =
            match (self.busy_start, host_busy_jiffies(Path::new("/proc/stat"))) {
                (Some(a), Some(b)) => {
                    let total_busy_s = ((b - a).max(0)) as f64 / clk_tck() as f64;
                    (pct((total_busy_s - our_cpu).max(0.0)), pct(total_busy_s))
                }
                _ => (String::new(), String::new()),
            };

        let mut row: BTreeMap<String, String> = BTreeMap::new();
        row.insert("timestamp".into(), timestamp());
        row.insert("machine_id".into(), self.machine_id.clone());
        row.insert("git_sha".into(), self.git_sha.clone());
        row.insert("nproc".into(), ncpu.to_string());
        row.insert("wall_s".into(), format!("{:.1}", wall));
        row.insert("user_s".into(), format!("{:.1}", user_s));
        row.insert("sys_s".into(), format!("{:.1}", sys_s));
        row.insert("result".into(), result.to_string());
        row.insert("n_steps".into(), n_steps.to_string());
        row.insert("pct_we".into(), pct_we);
        row.insert("pct_other".into(), pct_other);
        row.insert("total_busy_pct".into(), total_busy_pct);
        row.insert("ipc".into(), String::new());
        row.insert("cache_miss_pct".into(), String::new());
        row.insert("jobs".into(), jobs.to_string());

        let dir = ensure_dir(&self.output_dir)?;
        let path = dir.join(whole_run_csv_name(&self.machine_id));
        let fieldnames: Vec<String> = CSV_COLUMNS.iter().map(|s| s.to_string()).collect();
        match with_file_lock(&path, || {
            append_rows_merging_header(&path, std::slice::from_ref(&row), &fieldnames)
        }) {
            Ok(()) => Some(path),
            Err(error) => {
                eprintln!("[perflog] whole-run row skipped ({error})");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(31), (1970, 2, 1));
    }

    #[test]
    fn timestamp_shape() {
        let ts = timestamp();
        assert_eq!(ts.len(), 20);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
        assert!(ts.ends_with('Z'));
    }

    #[test]
    fn affinity_width_rejects_a_field_forged_inside_the_process_name() {
        assert_eq!(
            affinity_width_from_status(
                b"Name:\topaque\xff\rCpus_allowed_list:\t99\nCpus_allowed_list:\t0-3,8\n"
            ),
            Some(5)
        );
        assert_eq!(
            affinity_width_from_status(
                b"Name:\topaque\xff\nCpus_allowed_list:\t99\nCpus_allowed_list:\t0-3,8\n"
            ),
            None
        );
    }

    #[test]
    fn csv_parser_and_appender_preserve_quoted_multiline_fields() {
        let parsed = parse_csv_records("a,b\nx,\"line 1\nline 2\"\n");
        assert_eq!(
            parsed,
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["x".to_string(), "line 1\nline 2".to_string()],
            ]
        );

        let dir = std::env::temp_dir().join(format!(
            "dagrun_csv_multiline_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rows.csv");
        fs::write(&path, "a,b\nx,\"line 1\nline 2\"\n").unwrap();
        let row = BTreeMap::from([
            ("a".to_string(), "y".to_string()),
            ("b".to_string(), "last".to_string()),
        ]);
        append_rows_merging_header(&path, &[row], &["a".into(), "b".into()]).unwrap();
        assert_eq!(
            parse_csv_records(&fs::read_to_string(&path).unwrap()),
            vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["x".to_string(), "line 1\nline 2".to_string()],
                vec!["y".to_string(), "last".to_string()],
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_step_profiles_writes_schema_header() {
        let dir = std::env::temp_dir().join(format!("dagrun_perf_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut row: ProfileRow = BTreeMap::new();
        row.insert("step".into(), "g.j".into());
        row.insert("classification".into(), "light".into());
        row.insert("elapsed_s".into(), "0.1".into());
        row.insert("ok".into(), "true".into());
        row.insert("cpu.usage_usec".into(), "1234".into()); // dynamic key appended after standard
        let path =
            append_step_profiles(&dir, &[row], "abc123", 4, None, "unverified", "local", None)
                .expect("path");
        let text = fs::read_to_string(&path).unwrap();
        let header = text.lines().next().unwrap();
        assert!(header.starts_with("timestamp,machine_id,container_class,git_sha,outer_jobs"));
        assert!(header.contains("step,classification,inner_jobs"));
        assert!(header.contains("ok,timed_out,cpu_timed_out,oom_kills"));
        assert!(header.trim_end().ends_with("cpu.usage_usec"));
        assert!(text.lines().nth(1).unwrap().contains("g.j"));
        assert!(
            dir.join(".locks")
                .join(format!(
                    "{}.lock",
                    path.file_name().unwrap().to_string_lossy()
                ))
                .is_file(),
            "the stable cross-language flock inode must be retained"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_step_profiles_sorts_dynamic_cpu_columns() {
        // Dynamic cpu.* columns must land in ALPHABETICAL order (not row-insertion order), so the
        // header is byte-identical to the Python build's (which sorts the same set). Insert the keys
        // in a deliberately scrambled order and assert the header tail comes back sorted.
        let dir = std::env::temp_dir().join(format!("dagrun_perford_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut row: ProfileRow = BTreeMap::new();
        row.insert("step".into(), "g.j".into());
        // BTreeMap sorts on insert anyway, but that is exactly the property under test: the WRITER
        // must project to a sorted tail regardless of collection type.
        for key in [
            "cpu.usage_usec",
            "cpu.user_usec",
            "cpu.system_usec",
            "cpu.nice_usec",
        ] {
            row.insert(key.into(), "1".into());
        }
        let path =
            append_step_profiles(&dir, &[row], "abc123", 4, None, "unverified", "local", None)
                .expect("path");
        let text = fs::read_to_string(&path).unwrap();
        let header = text.lines().next().unwrap();
        let tail: Vec<&str> = header
            .split(',')
            .filter(|c| c.starts_with("cpu."))
            .collect();
        let mut sorted = tail.clone();
        sorted.sort_unstable();
        assert_eq!(
            tail, sorted,
            "dynamic cpu.* columns must be alphabetically sorted"
        );
        assert_eq!(
            tail,
            vec![
                "cpu.nice_usec",
                "cpu.system_usec",
                "cpu.usage_usec",
                "cpu.user_usec"
            ]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn time_series_uses_run_scoped_path_and_stamps_provenance() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_timeseries_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        let row = ProfileRow::from([
            ("step".into(), "build.app".into()),
            ("inner_jobs".into(), "4".into()),
            ("sample_index".into(), "0".into()),
            ("sample_kind".into(), "start".into()),
            ("elapsed_s".into(), "0.000001".into()),
            ("interval_s".into(), String::new()),
            ("cpu_usage_s".into(), "0.000000".into()),
            ("user_s".into(), "0.000000".into()),
            ("sys_s".into(), "0.000000".into()),
            ("effective_cores".into(), String::new()),
            ("user_cores".into(), String::new()),
            ("system_cores".into(), String::new()),
            ("throttled_s".into(), "0.000000".into()),
            ("interval_throttled_s".into(), String::new()),
            ("thread_count".into(), "1".into()),
            ("sweep_id".into(), "sweep-1".into()),
        ]);
        let path = append_step_timeseries(
            &dir,
            &[row],
            "abc123",
            8,
            Some("base456"),
            "cgroup-v2",
            "sweep",
            "run789",
        )
        .expect("trace path");
        assert_eq!(path, dir.join("traces/run789.csv"));
        let text = fs::read_to_string(path).unwrap();
        let mut lines = text.lines();
        let header = lines.next().unwrap();
        assert_eq!(
            header,
            STEP_TIMESERIES_COLUMNS
                .iter()
                .copied()
                .chain(std::iter::once("sweep_id"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let values = parse_csv_line(lines.next().unwrap());
        let columns = parse_csv_line(header);
        let written: BTreeMap<String, String> = columns.into_iter().zip(values).collect();
        assert_eq!(written["git_sha"], "abc123");
        assert_eq!(written["outer_jobs"], "8");
        assert_eq!(written["profile_base_sha"], "base456");
        assert_eq!(written["enforcement_kind"], "cgroup-v2");
        assert_eq!(written["runner_name"], "sweep");
        assert_eq!(written["run_id"], "run789");
        assert_eq!(written["sample_kind"], "start");
        assert_eq!(written["sweep_id"], "sweep-1");
        assert!(dir.join("traces/.locks/run789.csv.lock").is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_time_series_creates_no_trace_directory() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_empty_timeseries_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&dir);
        assert!(
            append_step_timeseries(&dir, &[], "abc123", 1, None, "cgroup-v2", "run", "empty",)
                .is_none()
        );
        assert!(!dir.join("traces").exists());
    }

    #[test]
    fn perf_window_writes_whole_run_row() {
        let dir = std::env::temp_dir().join(format!("dagrun_perfw_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let w = PerfWindow::start(&dir, "deadbeef");
        let path = w.finish("pass", 3, 4).expect("path");
        let text = fs::read_to_string(&path).unwrap();
        assert!(text
            .lines()
            .next()
            .unwrap()
            .starts_with("timestamp,machine_id,git_sha,nproc"));
        assert!(text.lines().nth(1).unwrap().contains("deadbeef"));
        let _ = fs::remove_dir_all(&dir);
    }
}
