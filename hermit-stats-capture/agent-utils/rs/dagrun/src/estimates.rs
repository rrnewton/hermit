//! Profile-derived runtime estimates and execution planners.

// Profile-store FEEDBACK: turn recorded per-step samples into planning estimates.
//
// Direct port of `py/dagrun/estimates.py`; the derived numbers and the `plan --format json` /
// `plan` text output are BYTE-IDENTICAL to the Python build for a given store + DAG
// (cross-tested in `cross/differential.py`). This is the READING half of the learned-duration
// profile store (ds-7pzdgm / ds-afzsqf); the writing half already ships in [`crate::perflog`].
//
// For the current `(machine_id, container_class)` it derives, per step:
// * `est_duration_s` — a contention-discounted, MAD-trimmed MEDIAN of the recorded `elapsed_s`
//   (the step's INTRINSIC / uncontended duration). Median, not mean, so one slow sample cannot
//   drag it; discounted by whatever contention column the store carries.
// * `rss_estimate_bytes` — a robust HIGH-WATER (90th-percentile, nearest-rank via INTEGER rank
//   arithmetic so it is cross-identical) of the recorded `peak_bytes` for the memory model.
//
// Sparse/missing data degrades to `None` (the caller falls back to the DAG hint); a malformed
// cell is skipped, never coerced.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::io::json_str;
use crate::model::{effective_cpu_count, step_width_is_resizable, DagConfig, ResourceHint, Step};
use crate::perflog::{container_class, machine_id, parse_csv_records};
use crate::sizing::{
    memory_footprint_fits, outer_mem_footprint_bytes, schedulable_peak_mem_bytes_widths,
};

/// Environment variable that overrides the machine component of the feedback identity.
pub const MACHINE_ID_ENV: &str = "DAGRUN_MACHINE_ID";
/// Environment variable that overrides the container-class component of the feedback identity.
pub const CONTAINER_CLASS_ENV: &str = "DAGRUN_CONTAINER_CLASS";

/// Minimum recorded samples before the store may refine a step. Duration feedback may replace its
/// hint; ordinary RSS feedback may raise an authored baseline but never lower it. Deliberate
/// memory-cap lowering is the separate censor-aware `--profile-memory-feedback` contract.
pub const DEFAULT_MIN_SAMPLES: i64 = 1;

/// MAD-trim: drop duration samples more than this many MADs from the median before re-medianing.
const MAD_TRIM_K: f64 = 3.5;
/// RSS high-water percentile as an exact integer fraction (num/den) — no floating `ceil`.
const RSS_PCTL_NUM: i64 = 9;
const RSS_PCTL_DEN: i64 = 10;
/// Clamp a contention fraction so a bogus signal cannot discount a duration to zero.
const MAX_CONTENTION: f64 = 0.95;

// Contention percentage columns understood by the reader, in priority order (see Python's
// `_CONTENTION_PCT_COLUMNS`).
const CONTENTION_PCT_COLUMNS: [&str; 3] =
    ["pct_other", "psi_cpu_some_avg10", "cpu_pressure_some_avg10"];

/// Which scheduling planner to use for dispatch ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planner {
    /// Dispatch longest estimated steps first, breaking ties by tag.
    GreedyLpt,
    /// Dispatch by decreasing dependency-graph bottom level.
    CriticalPath,
    // CPA (Radulescu & van Gemund 2001): a two-phase moldable allocator that first picks each
    // step's inner-jobs width by balancing the critical path against the per-core area over the
    // MEASURED speedup curves, then list-schedules by critical-path order at the allocated widths.
    // See `common/docs/dagrun/PLANNER_DESIGN.md`.
    /// Allocate measured parallel widths before critical-path list scheduling.
    Cpa,
}

impl Planner {
    /// Return the canonical command-line and serialized value.
    pub fn value(self) -> &'static str {
        match self {
            Planner::GreedyLpt => "greedy-lpt",
            Planner::CriticalPath => "critical-path",
            Planner::Cpa => "cpa",
        }
    }

    /// Parse the canonical string form, or `None` for an unknown value.
    pub fn from_value(text: &str) -> Option<Planner> {
        match text {
            "greedy-lpt" => Some(Planner::GreedyLpt),
            "critical-path" => Some(Planner::CriticalPath),
            "cpa" => Some(Planner::Cpa),
            _ => None,
        }
    }
}

/// Aggregated store estimates for ONE step, from its recorded samples.
#[derive(Debug, Clone)]
pub struct StepSamples {
    /// Fully qualified step tag.
    pub step: String,
    /// Number of profile rows aggregated for the step.
    pub samples: i64,
    /// Robust contention-adjusted wall-time estimate in seconds.
    pub est_duration_s: Option<f64>,
    /// Robust high-water resident-memory estimate in bytes.
    pub rss_estimate_bytes: Option<i64>,
}

/// One recorded step measurement reduced to the fields consumed by the estimators.
///
/// Raw wall time and contention remain separate so [`Sample::intrinsic_s`] can calculate the
/// uncontended duration used by robust aggregation.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Observed wall time in seconds, when recorded.
    pub elapsed_s: Option<f64>,
    /// Clamped fraction of the sample attributed to ambient contention.
    pub contention: f64,
    /// Total user plus system CPU time in seconds, when recorded.
    pub cpu_s: Option<f64>,
    /// Observed average CPU concurrency, when recorded.
    pub effective_cores: Option<f64>,
    /// Cgroup CPU throttling time in seconds, when recorded.
    pub throttled_s: Option<f64>,
    /// Peak resident memory in bytes, when recorded.
    pub peak_bytes: Option<i64>,
    /// Peak proven usable as an exact-width estimate. Rows without cap/event provenance retain
    /// the compatibility treatment; censored or unknown rows do not.
    pub uncensored_peak_bytes: Option<i64>,
    /// Censored peak usable only as a lower bound on demand.
    pub peak_floor_bytes: Option<i64>,
    /// Stable identity used only by bounded summary sampling/merge.
    pub observation_id: String,
    /// Stable command/workload identity recorded by scaling sweeps; empty for older rows.
    pub workload_digest: String,
}

impl Sample {
    /// The contention-discounted (intrinsic) wall the estimator medians, or `None` when this sample
    /// carried no `elapsed_s`.
    pub fn intrinsic_s(&self) -> Option<f64> {
        self.elapsed_s.map(|e| e * (1.0 - self.contention))
    }
}

/// Aggregation key containing a step tag and its inner-job width.
///
/// Width zero represents a missing, invalid, or non-positive recorded width. Such samples still
/// contribute to step estimates but are excluded from speedup-curve fitting.
pub type BucketKey = (String, i64);

/// Filter normalized samples to the workload cohort expected by the current DAG.
///
/// Matching digest samples win as soon as any exist for a step. Before then, blank compatibility samples
/// remain usable; samples carrying another non-empty digest are never blended into either cohort.
pub(crate) fn buckets_for_workloads(
    buckets: &HashMap<BucketKey, Vec<Sample>>,
    expected: &HashMap<String, String>,
) -> HashMap<BucketKey, Vec<Sample>> {
    if expected.is_empty() {
        return buckets.clone();
    }
    let matched_steps: HashSet<String> = buckets
        .iter()
        .filter_map(|((step, _), samples)| {
            let wanted = expected.get(step)?;
            samples
                .iter()
                .any(|sample| sample.workload_digest == *wanted)
                .then(|| step.clone())
        })
        .collect();
    buckets
        .iter()
        .filter_map(|(key, samples)| {
            let Some(wanted) = expected.get(&key.0) else {
                return Some((key.clone(), samples.clone()));
            };
            let selected: Vec<Sample> = samples
                .iter()
                .filter(|sample| {
                    if matched_steps.contains(&key.0) {
                        sample.workload_digest == *wanted
                    } else {
                        sample.workload_digest.is_empty()
                    }
                })
                .cloned()
                .collect();
            (!selected.is_empty()).then(|| (key.clone(), selected))
        })
        .collect()
}

/// Reduce one raw profile row to the normalized sample consumed by all estimators.
pub fn sample_from_row(row: &HashMap<String, String>, affinity: Option<i64>) -> Sample {
    let elapsed_s = parse_float(row.get("elapsed_s")).filter(|e| *e >= 0.0);
    let cpu_s = match (
        parse_float(row.get("user_s")),
        parse_float(row.get("sys_s")),
    ) {
        (Some(u), Some(s)) if u >= 0.0 && s >= 0.0 => Some(u + s),
        _ => None,
    };
    let peak_bytes = parse_int(row.get("peak_bytes")).filter(|p| *p >= 0);
    let mut uncensored_peak_bytes = peak_bytes;
    let mut peak_floor_bytes = None;
    if peak_bytes.is_some()
        && [
            "memory_max_bytes",
            "memory_events_high",
            "memory_events_max",
            "memory_events_oom",
            "memory_events_oom_kill",
            "memory_events_oom_group_kill",
        ]
        .iter()
        .any(|column| row.contains_key(*column))
    {
        match crate::memory_feedback::peak_observation_from_row(row).verdict {
            crate::memory_feedback::Censoring::Uncensored => {}
            crate::memory_feedback::Censoring::Censored => {
                peak_floor_bytes = peak_bytes;
                uncensored_peak_bytes = None;
            }
            crate::memory_feedback::Censoring::Unknown => {
                uncensored_peak_bytes = None;
            }
        }
    }
    Sample {
        elapsed_s,
        contention: contention_fraction(row, affinity),
        cpu_s,
        effective_cores: parse_float(row.get("effective_cores")).filter(|e| *e >= 0.0),
        throttled_s: parse_float(row.get("throttled_s")).filter(|t| *t >= 0.0),
        peak_bytes,
        uncensored_peak_bytes,
        peak_floor_bytes,
        observation_id: row
            .get("observation_id")
            .filter(|value| !value.is_empty())
            .or_else(|| row.get("run_id").filter(|value| !value.is_empty()))
            .cloned()
            .unwrap_or_default(),
        workload_digest: row
            .get("workload_digest")
            .map(|value| value.trim().to_string())
            .unwrap_or_default(),
    }
}

/// The `inner_jobs` bucket component for a row: the parsed positive width, else `0`.
fn row_inner_jobs(row: &HashMap<String, String>) -> i64 {
    match parse_int(row.get("inner_jobs")) {
        Some(j) if j > 0 => j,
        _ => 0,
    }
}

/// Cells that, when explicitly affirmative, mean the step did NOT complete its work.
const FAILURE_FLAG_COLUMNS: [&str; 2] = ["timed_out", "cpu_timed_out"];

/// True only for an explicit affirmative cell; a blank or absent cell is NOT a failure.
fn is_truthy_flag(value: Option<&String>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("true") | Some("1") | Some("yes")
    )
}

/// True when a profile row is a TIMING MEASUREMENT rather than a record of a failed step.
///
/// A timed-out run's duration is the moment the guard fired, and an OOM-killed run's duration is
/// the moment the kernel intervened; neither is how long the work takes. Admitting them fits the
/// speedup curve partly to failures, and a step that dies fast at every width then looks exactly
/// like a step that is flat and very quick.
///
/// FAIL-OPEN ON SILENCE, BY DESIGN. Only an explicit failure signal rejects a row: `ok` explicitly
/// falsy, a non-zero parseable `returncode`, an affirmative `timed_out` / `cpu_timed_out`, or a
/// positive `oom_kills`. A row whose verdict cells are absent or blank is ACCEPTED, because a store
/// may carry no verdict columns at all and a gate that rejected silence would leave the model with
/// nothing -- trading a wrong answer for no answer.
pub fn row_is_measurement(row: &HashMap<String, String>) -> bool {
    if let Some(ok) = row.get("ok") {
        let ok = ok.trim().to_ascii_lowercase();
        if !ok.is_empty() && !matches!(ok.as_str(), "true" | "1" | "yes") {
            return false;
        }
    }
    if let Some(rc) = row
        .get("returncode")
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        if rc != 0 {
            return false;
        }
    }
    if FAILURE_FLAG_COLUMNS
        .iter()
        .any(|col| is_truthy_flag(row.get(*col)))
    {
        return false;
    }
    if let Some(oom) = row
        .get("oom_kills")
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        if oom > 0 {
            return false;
        }
    }
    true
}

/// Group raw profile rows into uncapped per-step, per-width sample lists.
///
/// Rows without a non-empty `step` cell are ignored, and so are rows recording a FAILED step
/// rather than a measurement (see [`row_is_measurement`]): a timed-out or OOM-killed duration is
/// not a timing.
pub fn bucketize_rows(
    rows: &[HashMap<String, String>],
    affinity: Option<i64>,
) -> HashMap<BucketKey, Vec<Sample>> {
    let mut buckets: HashMap<BucketKey, Vec<Sample>> = HashMap::new();
    for row in rows {
        let step = match row.get("step") {
            Some(s) if !s.is_empty() => s.clone(),
            _ => continue,
        };
        if !row_is_measurement(row) {
            continue;
        }
        let key: BucketKey = (step, row_inner_jobs(row));
        buckets
            .entry(key)
            .or_default()
            .push(sample_from_row(row, affinity));
    }
    buckets
}

/// Aggregate sample buckets into robust per-step estimates across every recorded width.
pub fn step_samples_from_buckets(
    buckets: &HashMap<BucketKey, Vec<Sample>>,
) -> HashMap<String, StepSamples> {
    step_samples_from_buckets_for_workloads(buckets, &HashMap::new())
}

/// Aggregate scalar estimates from only the expected workload cohorts.
pub(crate) fn step_samples_from_buckets_for_workloads(
    buckets: &HashMap<BucketKey, Vec<Sample>>,
    expected: &HashMap<String, String>,
) -> HashMap<String, StepSamples> {
    let selected = buckets_for_workloads(buckets, expected);
    let mut durations: HashMap<String, Vec<f64>> = HashMap::new();
    let mut peaks: HashMap<String, Vec<i64>> = HashMap::new();
    let mut counts: HashMap<String, i64> = HashMap::new();
    for ((step, _inner), samples) in &selected {
        *counts.entry(step.clone()).or_insert(0) += samples.len() as i64;
        for sample in samples {
            if let Some(intrinsic) = sample.intrinsic_s() {
                durations.entry(step.clone()).or_default().push(intrinsic);
            }
            if let Some(peak) = sample.peak_bytes {
                peaks.entry(step.clone()).or_default().push(peak);
            }
        }
    }
    let mut result: HashMap<String, StepSamples> = HashMap::new();
    for (step, n) in counts {
        let est = durations.get(&step).map(|d| robust_median(d));
        let rss = peaks.get(&step).map(|p| high_percentile(p));
        result.insert(
            step.clone(),
            StepSamples {
                step,
                samples: n,
                est_duration_s: est,
                rss_estimate_bytes: rss,
            },
        );
    }
    result
}

/// The `(machine_id, container_class)` the feedback reader selects the store file by: the current
/// host's identity unless the corresponding environment overrides are set.
pub fn feedback_identity() -> (String, String) {
    let mid = std::env::var(MACHINE_ID_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(machine_id);
    let cc = std::env::var(CONTAINER_CLASS_ENV)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(container_class);
    (mid, cc)
}

fn median(sorted_values: &[f64]) -> f64 {
    let n = sorted_values.len();
    let mid = n / 2;
    if n % 2 == 1 {
        sorted_values[mid]
    } else {
        (sorted_values[mid - 1] + sorted_values[mid]) / 2.0
    }
}

fn sort_f64(values: &mut [f64]) {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
}

// A robust central estimate of contention-/noise-inflated samples (mirrors Python's
// `_robust_median`). At THREE or more samples: MAD-trimmed median (drop samples more than
// `MAD_TRIM_K` MADs from the median, falling back to the plain median when the MAD is zero). At
// FEWER than three samples MAD-trim provably cannot reject an outlier — the median of two points
// sits midway between them, so any symmetric cutoff keeps BOTH and the estimate collapses to their
// MEAN, which a single slow sample drags by half its excess (`[5, 100] -> 52.5`, inverting a real
// speedup). Since these quantities can only be INFLATED by contention/noise, the smaller
// observation is the better intrinsic estimate, so at `n < 3` we return the MINIMUM: robust to one
// upward (slow/contended) outlier at `n == 2` and self-healing to the MAD-trimmed median as samples
// accumulate. Caller guarantees a non-empty slice.
fn robust_median(values: &[f64]) -> f64 {
    let mut xs = values.to_vec();
    sort_f64(&mut xs);
    if xs.len() < 3 {
        return xs[0]; // sorted ascending -> min (robust to a slow outlier; see doc comment)
    }
    let m = median(&xs);
    let mut deviations: Vec<f64> = xs.iter().map(|x| (x - m).abs()).collect();
    sort_f64(&mut deviations);
    let mad = median(&deviations);
    if mad > 0.0 {
        let cutoff = MAD_TRIM_K * mad;
        let mut kept: Vec<f64> = xs
            .iter()
            .copied()
            .filter(|x| (x - m).abs() <= cutoff)
            .collect();
        if !kept.is_empty() {
            sort_f64(&mut kept);
            return median(&kept);
        }
    }
    m
}

// The 90th percentile of a non-empty int slice by NEAREST-RANK with integer arithmetic (mirrors
// Python's `_high_percentile`): `rank = ceil(num*n/den) == (num*n + den - 1) / den`, clamped to
// `1..=n`, returning the rank-th smallest.
pub(crate) fn high_percentile(values: &[i64]) -> i64 {
    let mut xs = values.to_vec();
    xs.sort_unstable();
    let n = xs.len() as i64;
    let mut rank = (RSS_PCTL_NUM * n + RSS_PCTL_DEN - 1) / RSS_PCTL_DEN;
    if rank < 1 {
        rank = 1;
    }
    if rank > n {
        rank = n;
    }
    xs[(rank - 1) as usize]
}

/// Parse the affinity (core) width out of a `container_class` like `affinity316_cpu-max-...` ->
/// `316`. `None` if the shape is unexpected. Shared with the summary (its speedup core budget).
pub(crate) fn affinity_width(container_class: &str) -> Option<i64> {
    let rest = container_class.strip_prefix("affinity")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i64>().ok()
    }
}

// Trim the surrounding ASCII whitespace that Python's `str.strip` removes over the same
// five-character set (tab, newline, form-feed, carriage-return, space) and return the cleaned
// token, or `None` when it is empty. Rust's `str::parse` already rejects the non-ASCII characters,
// `_` separators, and out-of-`i64` magnitudes that Python's permissive `float()` / `int()` would
// otherwise accept, so trimming is all that is needed for the two builds to accept EXACTLY the same
// numeric tokens from a store cell (mirrors `_clean_numeric_cell` in the Python port).
fn clean_numeric_cell(cell: Option<&String>) -> Option<&str> {
    let token = cell?.trim_matches(|c: char| c.is_ascii_whitespace());
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn parse_float(cell: Option<&String>) -> Option<f64> {
    // Reject non-finite values (`inf`/`-inf`/`nan`, and overflowing literals like `1e400` -> inf):
    // Python's `float()` accepts the same set, so both builds must drop them to stay byte-identical
    // AND to keep one bogus cell from poisoning a median or a contention fraction.
    clean_numeric_cell(cell)
        .and_then(|t| t.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

pub(crate) fn parse_int(cell: Option<&String>) -> Option<i64> {
    clean_numeric_cell(cell).and_then(|t| t.parse::<i64>().ok())
}

// The fraction of machine capacity taken by OTHER work during a sample, from whichever
// contention column is present (see [`CONTENTION_PCT_COLUMNS`]). `0.0` when no usable signal;
// clamped to [`MAX_CONTENTION`]. Mirrors Python's `_contention_fraction`.
fn contention_fraction(row: &HashMap<String, String>, affinity: Option<i64>) -> f64 {
    let mut fraction: Option<f64> = None;
    for col in CONTENTION_PCT_COLUMNS {
        if let Some(pct) = parse_float(row.get(col)) {
            fraction = Some(pct / 100.0);
            break;
        }
    }
    if fraction.is_none() {
        if let Some(external) = parse_float(row.get("external_cores")) {
            if let Some(width) = affinity {
                if width > 0 {
                    fraction = Some(external / width as f64);
                }
            }
        }
    }
    if fraction.is_none() {
        if let Some(co) = parse_float(row.get("co_tenants")) {
            if co > 0.0 {
                fraction = Some(co / (co + 1.0));
            }
        }
    }
    match fraction {
        None => 0.0,
        Some(f) if f < 0.0 => 0.0,
        Some(f) if f > MAX_CONTENTION => MAX_CONTENTION,
        Some(f) => f,
    }
}

/// A loaded store: the column->value row maps plus the parsed affinity (core) width.
pub(crate) type LoadedStore = (Vec<HashMap<String, String>>, Option<i64>);

// Read `<profile_dir>/step_profiles_<machine_id>_<container_class>.csv` into a list of
// column->value row maps plus the parsed affinity (core) width, or `None` when the file is absent.
// The single CSV-reading path shared by [`load_step_samples`] and [`load_step_speedups`] (DRY);
// mirrors Python's `_load_store`. Also used by the summary builder to read a store into rows.
pub(crate) fn load_store(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
) -> Option<LoadedStore> {
    let path = profile_dir.join(format!("step_profiles_{machine_id}_{container_class}.csv"));
    let text = std::fs::read_to_string(&path).ok()?;
    let affinity = affinity_width(container_class);
    let records = parse_csv_records(&text);
    let header: Vec<String> = match records.first() {
        Some(header) => header.clone(),
        None => return Some((Vec::new(), affinity)),
    };
    let mut rows: Vec<HashMap<String, String>> = Vec::new();
    for cells in records.into_iter().skip(1) {
        let row: HashMap<String, String> = header
            .iter()
            .enumerate()
            .map(|(i, name)| (name.clone(), cells.get(i).cloned().unwrap_or_default()))
            .collect();
        rows.push(row);
    }
    Some((rows, affinity))
}

/// Select rows for the command shape currently present in the DAG.
///
/// Once a step has any rows matching its expected workload digest, only that cohort is eligible.
/// Before the first matching sweep, blank pre-digest rows remain a compatibility fallback; rows
/// carrying a different non-empty digest are never mixed into the model.
fn select_workload_rows(
    rows: &[HashMap<String, String>],
    expected: &HashMap<String, String>,
) -> Vec<HashMap<String, String>> {
    if expected.is_empty() {
        return rows.to_vec();
    }
    let matched_steps: HashSet<String> = rows
        .iter()
        .filter_map(|row| {
            let step = row.get("step")?;
            let wanted = expected.get(step)?;
            (row.get("workload_digest").is_some_and(|got| got == wanted)).then(|| step.clone())
        })
        .collect();
    rows.iter()
        .filter(|row| {
            let Some(step) = row.get("step") else {
                return true;
            };
            let Some(wanted) = expected.get(step) else {
                return true;
            };
            let got = row.get("workload_digest").map(String::as_str).unwrap_or("");
            if matched_steps.contains(step) {
                got == wanted
            } else {
                got.is_empty()
            }
        })
        .cloned()
        .collect()
}

/// Load and aggregate profile samples for one machine and container class.
///
/// Returns an empty map when the corresponding profile file is absent.
pub fn load_step_samples(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
) -> HashMap<String, StepSamples> {
    load_step_samples_for_workloads(profile_dir, machine_id, container_class, &HashMap::new())
}

/// Load scalar estimates while excluding rows for a different command/workload digest.
pub(crate) fn load_step_samples_for_workloads(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
    expected: &HashMap<String, String>,
) -> HashMap<String, StepSamples> {
    let (rows, affinity) = match load_store(profile_dir, machine_id, container_class) {
        Some(x) => x,
        None => return HashMap::new(),
    };
    step_samples_from_buckets(&bucketize_rows(
        &select_workload_rows(&rows, expected),
        affinity,
    ))
}

// --------------------------------------------------------------------------- speedup model

/// The economic plateau is the NARROWEST measured width whose wall is within this fraction of the
/// best measured wall. A global definition is grid-invariant: inserting a midpoint such as 48
/// between 32 and 64 cannot move the recommendation merely because adjacent ratios changed.
const PLATEAU_WALL_TOLERANCE: f64 = 0.10;
/// If total CPU-seconds grow by more than this factor relative to the BASELINE width, the point is
/// retained diagnostically but is not an economic plateau candidate.
const SPEEDUP_MAX_WORK_GROWTH: f64 = 1.5;
/// A step needs at least this many DISTINCT inner_jobs levels (with wall data) to model a curve.
const SPEEDUP_MIN_LEVELS: usize = 2;
/// A width-specific memory response needs this many peak observations before it replaces the
/// conservative authored/pooled fallback in allocation.
const MEMORY_MIN_SAMPLES_PER_LEVEL: i64 = 3;

/// One measured point on a step's parallel speedup curve.
#[derive(Debug, Clone)]
pub struct SpeedupLevel {
    /// Inner worker width used by the samples.
    pub inner_jobs: i64,
    /// Number of wall-time samples contributing to this level.
    pub samples: i64,
    /// Robust contention-adjusted wall time in seconds. MODELLED: the curve is fitted to this, so
    /// the speedup and the recommendation both derive from it. A discount of ~25% has been observed
    /// on a busy host, so a consumer printing only this is showing a model as a measurement.
    pub wall_s: f64,
    /// Robust median of the RAW recorded wall times at this width, undiscounted -- the measurement
    /// `wall_s` was derived from.
    pub raw_wall_s: Option<f64>,
    /// Smallest observed contention-adjusted wall at this width.
    pub wall_min_s: Option<f64>,
    /// Largest observed contention-adjusted wall at this width.
    pub wall_max_s: Option<f64>,
    /// Robust total CPU time in seconds, when available.
    pub cpu_s: Option<f64>,
    /// Robust observed CPU concurrency, when available.
    pub effective_cores: Option<f64>,
    /// Robust throttling time in seconds, when available.
    pub throttled_s: Option<f64>,
    /// High-percentile cgroup `memory.peak` at this exact width.
    pub peak_bytes: Option<i64>,
    /// Number of peak observations contributing to `peak_bytes`.
    pub peak_samples: i64,
    /// Largest censored peak observed at this width. This is a proven floor, not an estimate of the
    /// maximum, so planning may not choose a memory requirement below it.
    pub peak_floor_bytes: Option<i64>,
    /// Number of censored observations contributing to `peak_floor_bytes`.
    pub peak_floor_samples: i64,
    /// Wall-time speedup relative to the narrowest measured level.
    pub speedup: f64,
}

/// A fitted step speedup curve and its recommended worker width.
#[derive(Debug, Clone)]
pub struct StepSpeedup {
    /// Fully qualified step tag.
    pub step: String,
    /// Narrowest measured inner-job width.
    pub baseline_inner_jobs: i64,
    /// Narrowest measured width within 10% of the best eligible wall time, subject to the
    /// CPU-work-growth guard and configured core limit.
    pub recommended_inner_jobs: i64,
    /// Effective CPU concurrency measured at the recommended width.
    pub measured_effective_cores: Option<f64>,
    /// Narrowest width ABOVE the fastest measured one where going wider is measurably SLOWER, or
    /// `None` when nothing in the measured range regresses. A plateau and a cliff stop the fit at
    /// the same width and yield the same recommendation, so this is the only field that tells them
    /// apart. See [`regression_inner_jobs`] for the dispersion test a width must pass.
    pub regression_inner_jobs: Option<i64>,
    /// Curve levels ordered by increasing inner-job width.
    pub levels: Vec<SpeedupLevel>,
}

/// The per-width aggregate one [`SpeedupLevel`] is fitted from. A named carrier rather than a
/// positional tuple: the fit needs the discounted median, the raw median it came from, and the
/// observed spread, and a nine-slot tuple makes those trivially easy to transpose at a call site.
#[derive(Debug, Clone, Copy)]
pub struct LevelAggregate {
    /// Inner worker width these samples ran at.
    pub inner_jobs: i64,
    /// Number of wall-time samples at this width.
    pub samples: i64,
    /// Contention-adjusted robust median wall (what the curve is fitted to).
    pub wall_s: f64,
    /// Robust median of the raw recorded walls, undiscounted.
    pub raw_wall_s: Option<f64>,
    /// Smallest observed adjusted wall at this width.
    pub wall_min_s: Option<f64>,
    /// Largest observed adjusted wall at this width.
    pub wall_max_s: Option<f64>,
    /// Robust median total CPU seconds, when available.
    pub cpu_s: Option<f64>,
    /// Robust median observed CPU concurrency, when available.
    pub effective_cores: Option<f64>,
    /// Robust median throttling seconds, when available.
    pub throttled_s: Option<f64>,
    /// High-percentile cgroup `memory.peak`, when available.
    pub peak_bytes: Option<i64>,
    /// Number of peak observations.
    pub peak_samples: i64,
    /// Largest censored peak at this width, usable only as a floor.
    pub peak_floor_bytes: Option<i64>,
    /// Number of censored peak observations.
    pub peak_floor_samples: i64,
}

/// A wider level must be at least this much SLOWER than the fastest measured level before it can be
/// called a regression. Paired with the dispersion test in [`regression_inner_jobs`]; neither check
/// is sufficient alone.
const REGRESSION_MIN_SLOWDOWN: f64 = 1.05;

/// The narrowest width above the fastest one where going wider is measurably SLOWER.
///
/// Two conditions must BOTH hold: the level's median wall exceeds the fastest level's by
/// [`REGRESSION_MIN_SLOWDOWN`], AND its observed `[min, max]` range is DISJOINT from the fastest
/// level's. The second is what keeps this honest on a shared machine -- a percentage test alone
/// reports ordinary sample noise as a cliff, and a width whose range still overlaps the best one's
/// is not distinguishable from it. A level missing either bound is skipped rather than guessed at.
pub fn regression_inner_jobs(levels: &[SpeedupLevel]) -> Option<i64> {
    let ranked: Vec<&SpeedupLevel> = levels.iter().filter(|l| l.wall_s > 0.0).collect();
    let best = ranked.iter().min_by(|a, b| a.wall_s.total_cmp(&b.wall_s))?;
    let (best_min, best_max) = (best.wall_min_s?, best.wall_max_s?);
    let mut candidates: Vec<&&SpeedupLevel> = ranked
        .iter()
        .filter(|l| l.inner_jobs > best.inner_jobs)
        .collect();
    candidates.sort_by_key(|l| l.inner_jobs);
    for level in candidates {
        let (Some(lo), Some(hi)) = (level.wall_min_s, level.wall_max_s) else {
            continue;
        };
        if level.wall_s <= best.wall_s * REGRESSION_MIN_SLOWDOWN {
            continue;
        }
        if lo > best_max || hi < best_min {
            return Some(level.inner_jobs);
        }
    }
    None
}

// Assemble a [`StepSpeedup`] from per-level tuples SORTED ascending by inner_jobs. Deterministic
// across builds (only compares robust medians of identical inputs). Mirrors Python's
// `_build_step_speedup`.
fn build_step_speedup(
    step: String,
    raw_levels: &[LevelAggregate],
    core_budget: Option<i64>,
) -> StepSpeedup {
    let baseline_j = raw_levels[0].inner_jobs;
    let baseline_wall = raw_levels[0].wall_s;
    let mut levels: Vec<SpeedupLevel> = Vec::with_capacity(raw_levels.len());
    let baseline_cpu = raw_levels[0].cpu_s;
    let mut eff_by_j: HashMap<i64, Option<f64>> = HashMap::new();
    for aggregate in raw_levels {
        let (j, wall, cpu, eff) = (
            aggregate.inner_jobs,
            aggregate.wall_s,
            aggregate.cpu_s,
            aggregate.effective_cores,
        );
        let speedup = if wall > 0.0 {
            baseline_wall / wall
        } else {
            1.0
        };
        levels.push(SpeedupLevel {
            inner_jobs: j,
            samples: aggregate.samples,
            wall_s: wall,
            raw_wall_s: aggregate.raw_wall_s,
            wall_min_s: aggregate.wall_min_s,
            wall_max_s: aggregate.wall_max_s,
            cpu_s: cpu,
            effective_cores: eff,
            throttled_s: aggregate.throttled_s,
            peak_bytes: aggregate.peak_bytes,
            peak_samples: aggregate.peak_samples,
            peak_floor_bytes: aggregate.peak_floor_bytes,
            peak_floor_samples: aggregate.peak_floor_samples,
            speedup,
        });
        eff_by_j.insert(j, eff);
    }
    let within_budget: Vec<&SpeedupLevel> = levels
        .iter()
        .filter(|level| core_budget.is_none_or(|budget| level.inner_jobs <= budget))
        .collect();
    let economic: Vec<&SpeedupLevel> = within_budget
        .iter()
        .copied()
        .filter(|level| match (level.cpu_s, baseline_cpu) {
            (Some(cpu), Some(base)) if base > 0.0 => cpu / base <= SPEEDUP_MAX_WORK_GROWTH,
            _ => true,
        })
        .collect();
    let candidates: Vec<&SpeedupLevel> = if !economic.is_empty() {
        economic
    } else if !within_budget.is_empty() {
        within_budget
    } else {
        levels.iter().collect()
    };
    let best_wall = candidates
        .iter()
        .map(|level| level.wall_s)
        .fold(f64::INFINITY, f64::min);
    let plateau_limit = best_wall * (1.0 + PLATEAU_WALL_TOLERANCE);
    let recommended = candidates
        .iter()
        .filter(|level| level.wall_s <= plateau_limit)
        .map(|level| level.inner_jobs)
        .min()
        .unwrap_or(baseline_j);
    let regression = regression_inner_jobs(&levels);
    StepSpeedup {
        step,
        baseline_inner_jobs: baseline_j,
        recommended_inner_jobs: recommended,
        measured_effective_cores: eff_by_j.get(&recommended).copied().flatten(),
        regression_inner_jobs: regression,
        levels,
    }
}

/// Load and fit parallel speedup curves from a profile store.
///
/// Only steps with enough distinct positive widths receive a model. The recommended width is the
/// narrowest point within 10% of the best eligible wall time, subject to the CPU-work-growth guard
/// and the machine core budget encoded by the container class.
pub fn load_step_speedups(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
) -> HashMap<String, StepSpeedup> {
    load_step_speedups_for_workloads(profile_dir, machine_id, container_class, &HashMap::new())
}

/// Load speedup curves while excluding rows for a different command/workload digest.
pub(crate) fn load_step_speedups_for_workloads(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
    expected: &HashMap<String, String>,
) -> HashMap<String, StepSpeedup> {
    let (rows, affinity) = match load_store(profile_dir, machine_id, container_class) {
        Some(x) => x,
        None => return HashMap::new(),
    };
    step_speedups_from_buckets(
        &bucketize_rows(&select_workload_rows(&rows, expected), affinity),
        affinity,
    )
}

/// Fit parallel speedup curves from normalized per-step, per-width buckets.
pub fn step_speedups_from_buckets(
    buckets: &HashMap<BucketKey, Vec<Sample>>,
    core_budget: Option<i64>,
) -> HashMap<String, StepSpeedup> {
    step_speedups_from_buckets_for_workloads(buckets, core_budget, &HashMap::new())
}

/// Fit speedup curves from only the expected workload cohorts.
pub(crate) fn step_speedups_from_buckets_for_workloads(
    buckets: &HashMap<BucketKey, Vec<Sample>>,
    core_budget: Option<i64>,
    expected: &HashMap<String, String>,
) -> HashMap<String, StepSpeedup> {
    let selected = buckets_for_workloads(buckets, expected);
    let mut walls: HashMap<BucketKey, Vec<f64>> = HashMap::new();
    let mut raws: HashMap<BucketKey, Vec<f64>> = HashMap::new();
    let mut cpus: HashMap<BucketKey, Vec<f64>> = HashMap::new();
    let mut effs: HashMap<BucketKey, Vec<f64>> = HashMap::new();
    let mut thrs: HashMap<BucketKey, Vec<f64>> = HashMap::new();
    let mut peaks: HashMap<BucketKey, Vec<i64>> = HashMap::new();
    let mut peak_floors: HashMap<BucketKey, Vec<i64>> = HashMap::new();
    for ((step, inner), samples) in &selected {
        if *inner <= 0 {
            continue;
        }
        let key: BucketKey = (step.clone(), *inner);
        for sample in samples {
            if let Some(intrinsic) = sample.intrinsic_s() {
                walls.entry(key.clone()).or_default().push(intrinsic);
            }
            if let Some(raw) = sample.elapsed_s {
                raws.entry(key.clone()).or_default().push(raw);
            }
            if let Some(c) = sample.cpu_s {
                cpus.entry(key.clone()).or_default().push(c);
            }
            if let Some(e) = sample.effective_cores {
                effs.entry(key.clone()).or_default().push(e);
            }
            if let Some(t) = sample.throttled_s {
                thrs.entry(key.clone()).or_default().push(t);
            }
            if let Some(peak) = sample.uncensored_peak_bytes {
                peaks.entry(key.clone()).or_default().push(peak);
            }
            if let Some(peak) = sample.peak_floor_bytes {
                peak_floors.entry(key.clone()).or_default().push(peak);
            }
        }
    }
    let mut by_step: HashMap<String, Vec<i64>> = HashMap::new();
    for (step, inner) in walls.keys() {
        by_step.entry(step.clone()).or_default().push(*inner);
    }
    let mut result: HashMap<String, StepSpeedup> = HashMap::new();
    for (step, widths) in by_step {
        let mut levels_j = widths;
        levels_j.sort_unstable();
        levels_j.dedup();
        if levels_j.len() < SPEEDUP_MIN_LEVELS {
            continue;
        }
        let mut raw_levels: Vec<LevelAggregate> = Vec::with_capacity(levels_j.len());
        for inner in &levels_j {
            let key = (step.clone(), *inner);
            let wall_samples = &walls[&key];
            let median_of = |m: &HashMap<BucketKey, Vec<f64>>| -> Option<f64> {
                m.get(&key)
                    .filter(|v| !v.is_empty())
                    .map(|v| robust_median(v))
            };
            raw_levels.push(LevelAggregate {
                inner_jobs: *inner,
                samples: wall_samples.len() as i64,
                wall_s: robust_median(wall_samples),
                raw_wall_s: median_of(&raws),
                wall_min_s: wall_samples.iter().copied().reduce(f64::min),
                wall_max_s: wall_samples.iter().copied().reduce(f64::max),
                cpu_s: median_of(&cpus),
                effective_cores: median_of(&effs),
                throttled_s: median_of(&thrs),
                peak_bytes: peaks
                    .get(&key)
                    .filter(|values| !values.is_empty())
                    .map(|values| high_percentile(values)),
                peak_samples: peaks.get(&key).map_or(0, |values| values.len() as i64),
                peak_floor_bytes: peak_floors
                    .get(&key)
                    .and_then(|values| values.iter().copied().max()),
                peak_floor_samples: peak_floors
                    .get(&key)
                    .map_or(0, |values| values.len() as i64),
            });
        }
        let model = build_step_speedup(step.clone(), &raw_levels, core_budget);
        result.insert(step, model);
    }
    result
}

// --------------------------------------------------------------------------- planner

/// The resolved estimate + planner metadata for one step (what the plan display shows).
#[derive(Debug, Clone)]
pub struct PlanEntry {
    /// Fully qualified step tag.
    pub tag: String,
    /// Resolved step duration in seconds.
    pub est_duration_s: f64,
    /// Source label for the duration estimate.
    pub est_source: String,
    /// Resolved resident-memory estimate in bytes.
    pub rss_estimate_bytes: Option<i64>,
    /// Source label for the memory estimate.
    pub rss_source: String,
    /// Longest estimated path from this step to a sink, in seconds.
    pub bottom_level_s: f64,
    /// Number of stored profile samples for this step.
    pub samples: i64,
    /// The learned parallel-speedup curve for this step, or `None` when the store has fewer than
    /// two inner_jobs widths for it.
    pub speedup: Option<StepSpeedup>,
    /// The executable inner-jobs width CPA assigned to a runner-controlled step, or `None` for
    /// ordering-only planners and self-managed commands whose empty jobs flag prevents rewriting.
    /// Run-level `-j` is the outer bandwidth/per-step ceiling, not an admission reservation.
    pub alloc_inner_jobs: Option<i64>,
    /// Width at which `rss_estimate_bytes` is an empirical M(p). This transient provenance stops
    /// runtime sizing from applying its fallback width heuristic a second time.
    pub rss_estimate_inner_jobs: Option<i64>,
}

/// Whole-DAG metrics produced by the parallel-width allocator.
#[derive(Debug, Clone)]
pub struct Allocation {
    /// Core budget used by allocation and the no-overcommit reference simulation.
    pub core_budget: i64,
    /// Total modeled CPU service. Measured CPU seconds win; width times wall is the fallback.
    pub area_s: f64,
    /// Work-area lower bound, `area_s / core_budget`.
    pub area_bound_s: f64,
    /// Modeled duration of the allocated critical path.
    pub critical_path_s: f64,
    /// Maximum of the work-area and critical-path lower bounds.
    pub lower_bound_s: f64,
    /// Makespan from the deterministic no-overcommit reference list schedule; not a prediction of
    /// the live scheduler, which may overlap widths beyond this capacity under the outer quota.
    pub modeled_makespan_s: f64,
    /// Stable reason the allocation loop stopped widening steps.
    pub stop_reason: String,
    /// Active-step overlap the memory model can sustain, bounded by the requested max-steps.
    pub modeled_max_steps: i64,
}

/// A complete plan: per-step resolved estimates, the dispatch order, and the critical path.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Planner used to produce this result.
    pub planner: Planner,
    /// Deterministic dispatch order of fully qualified step tags.
    pub order: Vec<String>,
    /// Fully qualified tags on the estimated critical path.
    pub critical_path: Vec<String>,
    /// Estimated critical-path duration in seconds.
    pub critical_path_length_s: f64,
    /// Per-step estimates and allocation metadata.
    pub entries: Vec<PlanEntry>,
    /// The CPA allocator summary (`--planner cpa` only), else `None`.
    pub allocation: Option<Allocation>,
}

impl Plan {
    fn by_tag(&self) -> HashMap<String, &PlanEntry> {
        self.entries.iter().map(|e| (e.tag.clone(), e)).collect()
    }
}

struct Resolved {
    est: f64,
    est_source: &'static str,
    rss: Option<i64>,
    rss_source: &'static str,
    samples: i64,
}

fn resolved_estimate(step: &Step, samples: Option<&StepSamples>, min_samples: i64) -> Resolved {
    if step.skip_reason.is_some() {
        return Resolved {
            est: 0.0,
            est_source: "skip",
            rss: None,
            rss_source: "none",
            samples: 0,
        };
    }
    let n = samples.map(|s| s.samples).unwrap_or(0);
    let store_ok = samples.is_some() && n >= min_samples;

    let (est, est_source) = match samples {
        Some(s) if store_ok && s.est_duration_s.is_some() => (s.est_duration_s.unwrap(), "store"),
        _ if step.hint.est_duration_s != 0.0 => (step.hint.est_duration_s, "hint"),
        _ => (step.hint.est_duration_s, "default"),
    };

    // Ordinary feedback starts after one sample. That is useful for scheduling, but a quiet
    // subprocess run is not evidence that the next run's enforced memory.max may safely shrink:
    // fan-out and allocator high-water marks vary discontinuously. Treat the authored RSS as a
    // floor here. The explicit censor-aware --profile-memory-feedback path retains authority to
    // lower it after its five-sample policy and margin.
    let (rss, rss_source) = match samples {
        Some(s)
            if store_ok
                && s.rss_estimate_bytes.is_some()
                && (step.hint.rss_baseline_bytes.is_none()
                    || s.rss_estimate_bytes > step.hint.rss_baseline_bytes) =>
        {
            (s.rss_estimate_bytes, "store")
        }
        _ if step.hint.rss_baseline_bytes.is_some() => (step.hint.rss_baseline_bytes, "hint"),
        _ => (None, "none"),
    };

    Resolved {
        est,
        est_source,
        rss,
        rss_source,
        samples: n,
    }
}

/// Map each step tag to the tags that DEPEND on it (its downstream successors), in cfg order.
fn successors(cfg: &DagConfig) -> HashMap<String, Vec<String>> {
    let mut succ: HashMap<String, Vec<String>> =
        cfg.steps.iter().map(|s| (s.tag(), Vec::new())).collect();
    for step in &cfg.steps {
        let tag = step.tag();
        for dep in &step.deps {
            if let Some(v) = succ.get_mut(dep) {
                v.push(tag.clone());
            }
        }
    }
    succ
}

/// Bottom-level (longest remaining est-weighted path to a sink) for every step; memoized DFS.
fn bottom_levels(
    cfg: &DagConfig,
    est: &HashMap<String, f64>,
    succ: &HashMap<String, Vec<String>>,
) -> HashMap<String, f64> {
    let mut bottom: HashMap<String, f64> = HashMap::new();

    fn visit(
        tag: &str,
        est: &HashMap<String, f64>,
        succ: &HashMap<String, Vec<String>>,
        bottom: &mut HashMap<String, f64>,
    ) -> f64 {
        if let Some(v) = bottom.get(tag) {
            return *v;
        }
        let mut value = est.get(tag).copied().unwrap_or(0.0);
        if let Some(children) = succ.get(tag) {
            if !children.is_empty() {
                let best = children
                    .iter()
                    .map(|w| visit(w, est, succ, bottom))
                    .fold(f64::NEG_INFINITY, f64::max);
                value += best;
            }
        }
        bottom.insert(tag.to_string(), value);
        value
    }

    for step in &cfg.steps {
        visit(&step.tag(), est, succ, &mut bottom);
    }
    bottom
}

/// `true` if `a` should sort BEFORE `b` under "greatest bottom-level, tie-break smallest tag".
fn better_bottom(a: &str, b: &str, bottom: &HashMap<String, f64>) -> bool {
    let ba = bottom.get(a).copied().unwrap_or(0.0);
    let bb = bottom.get(b).copied().unwrap_or(0.0);
    if ba != bb {
        ba > bb
    } else {
        a < b
    }
}

// The longest est-weighted path through the DAG (mirrors Python's `_critical_path`).
fn critical_path(
    cfg: &DagConfig,
    bottom: &HashMap<String, f64>,
    succ: &HashMap<String, Vec<String>>,
) -> (Vec<String>, f64) {
    if cfg.steps.is_empty() {
        return (Vec::new(), 0.0);
    }
    let tags: Vec<String> = cfg.steps.iter().map(|s| s.tag()).collect();
    let mut start = tags[0].clone();
    for t in &tags[1..] {
        if better_bottom(t, &start, bottom) {
            start = t.clone();
        }
    }
    let length = bottom.get(&start).copied().unwrap_or(0.0);
    let mut path = vec![start.clone()];
    let mut current = start;
    loop {
        let children = match succ.get(&current) {
            Some(c) if !c.is_empty() => c,
            _ => break,
        };
        let mut next = children[0].clone();
        for c in &children[1..] {
            if better_bottom(c, &next, bottom) {
                next = c.clone();
            }
        }
        path.push(next.clone());
        current = next;
    }
    (path, length)
}

// The deterministic dispatch order (mirrors Python's `_plan_order`).
fn plan_order(
    cfg: &DagConfig,
    planner: Planner,
    est: &HashMap<String, f64>,
    bottom: &HashMap<String, f64>,
) -> Vec<String> {
    let mut tags: Vec<String> = cfg.steps.iter().map(|s| s.tag()).collect();
    match planner {
        // CPA list-schedules by critical-path order at the allocated weights, so it uses the same
        // bottom-level ordering as CriticalPath (PLANNER_DESIGN.md §5.7).
        Planner::CriticalPath | Planner::Cpa => {
            // by bottom_level DESC, ties by tag ASC.
            tags.sort_by(|a, b| {
                let ba = bottom.get(a).copied().unwrap_or(0.0);
                let bb = bottom.get(b).copied().unwrap_or(0.0);
                bb.partial_cmp(&ba)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            });
        }
        Planner::GreedyLpt => {
            // STABLE reverse sort by est (ties keep registration/cfg order), matching Python's
            // sorted(..., reverse=True). Vec::sort_by is stable.
            tags.sort_by(|a, b| {
                let ea = est.get(a).copied().unwrap_or(0.0);
                let eb = est.get(b).copied().unwrap_or(0.0);
                eb.partial_cmp(&ea).unwrap_or(std::cmp::Ordering::Equal)
            });
        }
    }
    tags
}

/// Restrict a learned speedup model's recommendation to the run budget.
///
/// Measurements above `P` remain visible as curve points, but neither the recommended
/// width nor a regression marker can claim a width this run is forbidden to execute. A curve with
/// no measured point at or below `P` is unavailable at this budget and is omitted from the plan.
fn speedup_within_budget(speedup: &StepSpeedup, core_budget: i64) -> Option<StepSpeedup> {
    let p = core_budget.max(1);
    let measured: Vec<&SpeedupLevel> = speedup
        .levels
        .iter()
        .filter(|level| level.inner_jobs <= p)
        .collect();
    if measured.is_empty() {
        return None;
    }
    // Re-fit from the points this run can actually execute. Merely clamping a recommendation
    // fitted against wider points can select a local regression (for example
    // T={1:100,2:50,4:70,8:10} under P=4 would incorrectly recommend 4 instead of 2).
    let aggregates: Vec<LevelAggregate> = measured
        .iter()
        .map(|level| LevelAggregate {
            inner_jobs: level.inner_jobs,
            samples: level.samples,
            wall_s: level.wall_s,
            raw_wall_s: level.raw_wall_s,
            wall_min_s: level.wall_min_s,
            wall_max_s: level.wall_max_s,
            cpu_s: level.cpu_s,
            effective_cores: level.effective_cores,
            throttled_s: level.throttled_s,
            peak_bytes: level.peak_bytes,
            peak_samples: level.peak_samples,
            peak_floor_bytes: level.peak_floor_bytes,
            peak_floor_samples: level.peak_floor_samples,
        })
        .collect();
    let mut fitted = build_step_speedup(speedup.step.clone(), &aggregates, None);
    // Keep above-budget measurements visible for diagnostics; only the recommendation, regression,
    // and achieved-core marker are fitted to the executable subset.
    fitted.levels = speedup.levels.clone();
    Some(fitted)
}

fn cpu_work_eligible(level: &SpeedupLevel, baseline_cpu: Option<f64>) -> bool {
    match (level.cpu_s, baseline_cpu) {
        (Some(cpu), Some(base)) if base > 0.0 => cpu / base <= SPEEDUP_MAX_WORK_GROWTH,
        _ => true,
    }
}

// --------------------------------------------------------------------------- CPA allocator

// Stop-reason labels the CPA gradient loop can end on (PLANNER_DESIGN.md §5.8). Deterministic
// given the same store + DAG + budgets, so the two builds report the same one bit-for-bit.
const CPA_BALANCED: &str = "balanced";
const CPA_KNEE_EXHAUSTED: &str = "knee-exhausted";
const CPA_MEM_CAPPED: &str = "mem-capped";
const CPA_FIXED_POINT: &str = "fixed-point";
const CPA_INFEASIBLE_FIXED_WIDTH: &str = "infeasible-fixed-width";
const CPA_INFEASIBLE_MEMORY: &str = "infeasible-memory";

/// A CPA seed allocation cannot fit inside its core or memory budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfeasibleAllocationError {
    /// Total core-equivalent budget supplied to the allocator.
    pub core_budget: i64,
    /// Sorted `(step tag, fixed width)` pairs that exceed the budget.
    pub fixed_widths: Vec<(String, i64)>,
    /// Memory budget for an infeasible seed allocation, when memory is the failed dimension.
    pub mem_budget: Option<i64>,
    /// Minimum runnable footprint that exceeded `mem_budget`.
    pub memory_footprint: Option<i64>,
}

impl std::fmt::Display for InfeasibleAllocationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let (Some(budget), Some(footprint)) = (self.mem_budget, self.memory_footprint) {
            return write!(
                f,
                "CPA allocation is infeasible under memory budget {budget}: minimum runnable \
                 footprint is {footprint}"
            );
        }
        let detail = self
            .fixed_widths
            .iter()
            .map(|(tag, width)| format!("{tag}={width}"))
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "CPA allocation is infeasible under core budget {}: self-managed fixed width(s) \
             exceed the budget: {detail}",
            self.core_budget
        )
    }
}

impl std::error::Error for InfeasibleAllocationError {}

/// A step's positive configured/default width before any runner-controlled budget cap.
fn cpa_configured_width(cfg: &DagConfig, step: &Step) -> i64 {
    step.hint
        .preferred_inner_jobs
        .filter(|value| *value > 0)
        .or(cfg.default_step_cpu_count.filter(|value| *value > 0))
        .unwrap_or(1)
}

fn infeasible_fixed_widths(
    cfg: &DagConfig,
    widths: &HashMap<String, i64>,
    core_budget: i64,
) -> Vec<(String, i64)> {
    let p = core_budget.max(1);
    let mut bad: Vec<(String, i64)> = cfg
        .steps
        .iter()
        .filter_map(|step| {
            let width = widths[&step.tag()];
            (step.skip_reason.is_none()
                && !step_width_is_resizable(step, &cfg.default_jobs_flag, &cfg.default_jobs_env)
                && width > p)
                .then(|| (step.tag(), width))
        })
        .collect();
    bad.sort_by(|a, b| a.0.cmp(&b.0));
    bad
}

// Per-step admissible width set `W_i` (ascending) and the MEASURED wall `T_i(p)` at each admissible
// width. Mirrors Python's `_cpa_admissible` (PLANNER_DESIGN.md §5.2): a curve step admits its
// measured widths up to the knee (`recommended_inner_jobs`) and the core budget `P`; a curveless
// step is rigid at `min(hint or default_step_cpu_count or 1, P)` with `T_i` the resolved scalar
// estimate.
#[allow(clippy::type_complexity)]
fn cpa_admissible(
    cfg: &DagConfig,
    speedups: &HashMap<String, StepSpeedup>,
    est: &HashMap<String, f64>,
    core_budget: i64,
) -> (
    HashMap<String, Vec<i64>>,
    HashMap<String, HashMap<i64, f64>>,
) {
    let mut admissible: HashMap<String, Vec<i64>> = HashMap::new();
    let mut wall: HashMap<String, HashMap<i64, f64>> = HashMap::new();
    for step in &cfg.steps {
        let tag = step.tag();
        if step.skip_reason.is_some() {
            admissible.insert(tag.clone(), vec![1]);
            wall.insert(tag, HashMap::from([(1, 0.0)]));
            continue;
        }
        match speedups.get(&tag) {
            Some(sp)
                if !sp.levels.is_empty()
                    && step_width_is_resizable(
                        step,
                        &cfg.default_jobs_flag,
                        &cfg.default_jobs_env,
                    ) =>
            {
                let baseline_cpu = sp.levels[0].cpu_s;
                let knee_ok: Vec<&SpeedupLevel> = sp
                    .levels
                    .iter()
                    .filter(|l| {
                        l.inner_jobs <= sp.recommended_inner_jobs
                            && cpu_work_eligible(l, baseline_cpu)
                    })
                    .collect();
                let within: Vec<&SpeedupLevel> = knee_ok
                    .iter()
                    .copied()
                    .filter(|l| l.inner_jobs <= core_budget)
                    .collect();
                if within.is_empty() {
                    // A measured curve entirely above P cannot justify violating the strict run
                    // budget. Treat it as unavailable at this budget and keep the step rigid at
                    // its effective configured width, capped to P, using the scalar estimate.
                    let width = cpa_configured_width(cfg, step).min(core_budget.max(1));
                    admissible.insert(tag.clone(), vec![width]);
                    wall.insert(
                        tag.clone(),
                        HashMap::from([(width, est.get(&tag).copied().unwrap_or(0.0))]),
                    );
                } else {
                    let mut widths: Vec<i64> =
                        within.iter().map(|level| level.inner_jobs).collect();
                    widths.sort_unstable();
                    let w: HashMap<i64, f64> = within
                        .iter()
                        .map(|level| (level.inner_jobs, level.wall_s))
                        .collect();
                    admissible.insert(tag.clone(), widths);
                    wall.insert(tag, w);
                }
            }
            _ => {
                // No curve, or no effective width channel: without a way to rewrite the guest
                // width CPA cannot vary its worker count and must charge it as a rigid step.
                let self_managed =
                    !step_width_is_resizable(step, &cfg.default_jobs_flag, &cfg.default_jobs_env);
                // Preserve a self-managed command's declared width even above P so the allocator
                // can report infeasibility instead of inventing a guest-width rewrite.
                let declared = step.hint.preferred_inner_jobs.filter(|value| *value > 0);
                let ww = match (self_managed, declared) {
                    (true, Some(width)) => width,
                    _ => cpa_configured_width(cfg, step).min(core_budget.max(1)),
                };
                admissible.insert(tag.clone(), vec![ww]);
                let mut w: HashMap<i64, f64> = HashMap::new();
                let modeled_wall = speedups
                    .get(&tag)
                    .and_then(|speedup| speedup.levels.iter().find(|level| level.inner_jobs == ww))
                    .map(|level| level.wall_s)
                    .unwrap_or_else(|| est.get(&tag).copied().unwrap_or(0.0));
                w.insert(ww, modeled_wall);
                wall.insert(tag, w);
            }
        }
    }
    (admissible, wall)
}

/// The next-larger admissible width after `current` in the ascending `widths`, or `None`.
fn cpa_next_width(widths: &[i64], current: i64) -> Option<i64> {
    widths.iter().copied().find(|&w| w > current)
}

/// Modeled CPU service `C_i(p)` at one allocated point. Boxed profile CPU seconds are the direct
/// work-conservation signal; `p*T(p)` is the conservative fallback for older/unboxed rows.
fn cpu_work_at(speedups: &HashMap<String, StepSpeedup>, tag: &str, width: i64, wall_s: f64) -> f64 {
    speedups
        .get(tag)
        .and_then(|speedup| {
            speedup
                .levels
                .iter()
                .find(|level| level.inner_jobs == width)
        })
        .and_then(|level| level.cpu_s)
        .unwrap_or(width as f64 * wall_s)
}

/// Return `(exact_M_p, censored_floor)` for one measured width. Exact evidence exists only after
/// the replication threshold and is raised by any censored floor; a floor by itself remains a
/// lower bound and must not replace a larger conservative fallback.
fn memory_evidence_at(
    speedups: &HashMap<String, StepSpeedup>,
    tag: &str,
    width: i64,
) -> (Option<i64>, Option<i64>) {
    let Some(level) = speedups.get(tag).and_then(|speedup| {
        speedup
            .levels
            .iter()
            .find(|level| level.inner_jobs == width)
    }) else {
        return (None, None);
    };
    let exact = (level.peak_samples >= MEMORY_MIN_SAMPLES_PER_LEVEL)
        .then_some(level.peak_bytes)
        .flatten();
    let exact = match (exact, level.peak_floor_bytes) {
        (Some(value), Some(floor)) => Some(value.max(floor)),
        (value, _) => value,
    };
    (exact, level.peak_floor_bytes)
}

/// Resolve the displayed/applied memory value and whether it is exact at `width`. Replicated
/// width-specific evidence is still ordinary feedback: it may raise the fallback, but only the
/// explicit censor-aware memory-feedback mode may lower an authored floor.
fn modeled_memory_at(
    speedups: &HashMap<String, StepSpeedup>,
    tag: &str,
    width: i64,
    fallback: Option<i64>,
) -> (Option<i64>, bool) {
    let (exact, floor) = memory_evidence_at(speedups, tag, width);
    if exact.is_some_and(|value| fallback.is_none_or(|base| value > base)) {
        return (exact, true);
    }
    ([fallback, floor].into_iter().flatten().max(), false)
}

// Largest scheduler-reachable concurrent footprint at the given widths, including outer
// safety/floor policy. `max_steps` is an upper bound on the active set, matching runtime sizing.
fn cpa_footprint(
    cfg: &DagConfig,
    widths: &HashMap<String, i64>,
    speedups: &HashMap<String, StepSpeedup>,
    max_steps: i64,
) -> i64 {
    let mut active = cfg.clone();
    for step in &mut active.steps {
        if step.skip_reason.is_some() {
            continue;
        }
        let tag = step.tag();
        let (exact, floor) = memory_evidence_at(speedups, &tag, widths[&tag]);
        let authored = step.hint.rss_baseline_bytes;
        if let Some(peak) = exact.filter(|value| authored.is_none_or(|base| *value > base)) {
            step.hint.rss_baseline_bytes = Some(peak);
            step.hint.rss_baseline_inner_jobs = Some(widths[&tag]);
        } else if exact.is_some() || floor.is_some() {
            step.hint.rss_baseline_bytes = Some(authored.unwrap_or(0).max(floor.unwrap_or(0)));
            step.hint.rss_baseline_inner_jobs = None;
        }
    }
    let peak = schedulable_peak_mem_bytes_widths(&active, max_steps.max(1), widths).0;
    outer_mem_footprint_bytes(&active, peak)
}

/// `(widths, admissible, wall, stop_reason)` from the CPA gradient loop.
type CpaResult = (
    HashMap<String, i64>,
    HashMap<String, Vec<i64>>,
    HashMap<String, HashMap<i64, f64>>,
    String,
    i64,
);

// Run the CPA gradient at one fixed active-step overlap. `None` means even the narrow seed cannot
// fit the memory budget at this overlap. Keeping the overlap fixed makes the result comparable with
// the other candidates considered by `cpa_allocate`; otherwise the first feasible wide-overlap
// seed can strand CPA at a narrow-width local optimum.
#[allow(clippy::too_many_arguments)]
fn cpa_allocate_at_overlap(
    cfg: &DagConfig,
    speedups: &HashMap<String, StepSpeedup>,
    admissible: &HashMap<String, Vec<i64>>,
    wall: &HashMap<String, HashMap<i64, f64>>,
    succ: &HashMap<String, Vec<String>>,
    core_budget: i64,
    mem_budget: Option<i64>,
    modeled_max_steps: i64,
) -> Option<(HashMap<String, i64>, String)> {
    let p = if core_budget > 0 { core_budget } else { 1 };
    let mut widths: HashMap<String, i64> = cfg
        .steps
        .iter()
        .map(|s| {
            let tag = s.tag();
            let w0 = admissible[&tag][0];
            (tag, w0)
        })
        .collect();
    if let Some(budget) = mem_budget {
        if !memory_footprint_fits(
            cpa_footprint(cfg, &widths, speedups, modeled_max_steps),
            budget,
        ) {
            return None;
        }
    }
    let mut stop_reason = CPA_FIXED_POINT.to_string();
    let max_iters: usize = cfg
        .steps
        .iter()
        .map(|s| admissible[&s.tag()].len().saturating_sub(1))
        .sum::<usize>()
        + 2;
    for _ in 0..max_iters {
        let weight: HashMap<String, f64> = cfg
            .steps
            .iter()
            .map(|s| {
                let tag = s.tag();
                let w = widths[&tag];
                (tag.clone(), wall[&tag][&w])
            })
            .collect();
        let bottom = bottom_levels(cfg, &weight, succ);
        let (cp, t_cp) = critical_path(cfg, &bottom, succ);
        let area: f64 = cfg
            .steps
            .iter()
            .map(|step| {
                let tag = step.tag();
                cpu_work_at(speedups, &tag, widths[&tag], weight[&tag])
            })
            .sum();
        let t_a = area / p as f64;
        if t_cp <= t_a {
            stop_reason = CPA_BALANCED.to_string();
            break;
        }
        let mut widenable: Vec<String> = cfg
            .steps
            .iter()
            .map(|s| s.tag())
            .filter(|tag| {
                cp.contains(tag) && cpa_next_width(&admissible[tag], widths[tag]).is_some()
            })
            .collect();
        if widenable.is_empty() {
            stop_reason = CPA_KNEE_EXHAUSTED.to_string();
            break;
        }
        widenable.sort();
        let mut best_tag: Option<String> = None;
        let mut best_gain = 0.0f64;
        let mut blocked_mem = false;
        let mut positive_candidate = false;
        // Iterate tag-ascending and keep the FIRST maximum, so ties resolve to the smallest tag.
        for tag in &widenable {
            let nxt = cpa_next_width(&admissible[tag], widths[tag]).unwrap();
            if nxt > p {
                continue; // defensive per-step ceiling; admissible curves are already truncated
            }
            let cur = widths[tag];
            let gain = (wall[tag][&cur] - wall[tag][&nxt]) / (nxt - cur) as f64;
            if gain <= 0.0 {
                continue;
            }
            positive_candidate = true;
            if let Some(budget) = mem_budget {
                let mut tentative = widths.clone();
                tentative.insert(tag.clone(), nxt);
                if !memory_footprint_fits(
                    cpa_footprint(cfg, &tentative, speedups, modeled_max_steps),
                    budget,
                ) {
                    blocked_mem = true;
                    continue;
                }
            }
            if best_tag.is_none() || gain > best_gain {
                best_tag = Some(tag.clone());
                best_gain = gain;
            }
        }
        match best_tag {
            None => {
                stop_reason = if blocked_mem && positive_candidate {
                    CPA_MEM_CAPPED.to_string()
                } else {
                    CPA_KNEE_EXHAUSTED.to_string()
                };
                break;
            }
            Some(tag) => {
                let nxt = cpa_next_width(&admissible[&tag], widths[&tag]).unwrap();
                widths.insert(tag, nxt);
            }
        }
    }
    Some((widths, stop_reason))
}

// Score one fixed-overlap allocation with the plan's deterministic no-overcommit reference
// schedule.
fn cpa_candidate_makespan(
    cfg: &DagConfig,
    widths: &HashMap<String, i64>,
    wall: &HashMap<String, HashMap<i64, f64>>,
    succ: &HashMap<String, Vec<String>>,
    core_budget: i64,
    modeled_max_steps: i64,
) -> f64 {
    let weight: HashMap<String, f64> = cfg
        .steps
        .iter()
        .map(|step| {
            let tag = step.tag();
            (tag.clone(), wall[&tag][&widths[&tag]])
        })
        .collect();
    let bottom = bottom_levels(cfg, &weight, succ);
    let order = plan_order(cfg, Planner::CriticalPath, &weight, &bottom);
    cpa_simulate_makespan(
        cfg,
        widths,
        &weight,
        &order,
        core_budget.max(1),
        modeled_max_steps,
    )
}

// Choose the best fixed-overlap CPA allocation up to the active-step ceiling. Each feasible
// overlap gets the ordinary monotone gradient and a deterministic no-overcommit makespan score.
// Iterating from the ceiling down and replacing only on a strict improvement retains the larger
// overlap on an exact tie. Without a memory constraint the gradient is overlap-independent and
// reducing the scheduler concurrency cannot improve makespan, so evaluate the ceiling only.
fn cpa_allocate(
    cfg: &DagConfig,
    speedups: &HashMap<String, StepSpeedup>,
    est: &HashMap<String, f64>,
    core_budget: i64,
    mem_budget: Option<i64>,
    max_steps: i64,
) -> CpaResult {
    let p = core_budget.max(1);
    let (admissible, wall) = cpa_admissible(cfg, speedups, est, p);
    let seed: HashMap<String, i64> = cfg
        .steps
        .iter()
        .map(|step| {
            let tag = step.tag();
            (tag.clone(), admissible[&tag][0])
        })
        .collect();
    let succ = successors(cfg);
    let ceiling = max_steps.max(1);
    if !infeasible_fixed_widths(cfg, &seed, p).is_empty() {
        return (
            seed,
            admissible,
            wall,
            CPA_INFEASIBLE_FIXED_WIDTH.to_string(),
            ceiling,
        );
    }

    let overlaps: Box<dyn Iterator<Item = i64>> = if mem_budget.is_none() {
        Box::new(std::iter::once(ceiling))
    } else {
        Box::new((1..=ceiling).rev())
    };
    let mut best: Option<(HashMap<String, i64>, String, i64)> = None;
    let mut best_makespan = f64::INFINITY;
    for modeled_max_steps in overlaps {
        let Some((widths, stop_reason)) = cpa_allocate_at_overlap(
            cfg,
            speedups,
            &admissible,
            &wall,
            &succ,
            p,
            mem_budget,
            modeled_max_steps,
        ) else {
            continue;
        };
        let makespan = cpa_candidate_makespan(cfg, &widths, &wall, &succ, p, modeled_max_steps);
        if best.is_none() || makespan < best_makespan {
            best = Some((widths, stop_reason, modeled_max_steps));
            best_makespan = makespan;
        }
    }

    let Some((widths, stop_reason, modeled_max_steps)) = best else {
        // Footprint is monotone in the overlap ceiling, so no feasible candidate means one runnable
        // step at its narrowest admissible width exceeds the memory budget.
        return (seed, admissible, wall, CPA_INFEASIBLE_MEMORY.to_string(), 1);
    };
    (widths, admissible, wall, stop_reason, modeled_max_steps)
}

/// Allocate an inner-job width to every step using measured speedup curves.
///
/// The allocator balances critical-path reduction against total work while respecting the core
/// and optional memory budgets.
pub fn allocate_widths(
    cfg: &DagConfig,
    speedups: &HashMap<String, StepSpeedup>,
    est: &HashMap<String, f64>,
    core_budget: i64,
    mem_budget: Option<i64>,
) -> Result<HashMap<String, i64>, InfeasibleAllocationError> {
    allocate_widths_with_max_steps(cfg, speedups, est, core_budget, mem_budget, None)
}

/// [`allocate_widths`] with an explicit ceiling on concurrently active steps for memory modeling.
pub fn allocate_widths_with_max_steps(
    cfg: &DagConfig,
    speedups: &HashMap<String, StepSpeedup>,
    est: &HashMap<String, f64>,
    core_budget: i64,
    mem_budget: Option<i64>,
    max_steps: Option<i64>,
) -> Result<HashMap<String, i64>, InfeasibleAllocationError> {
    let active_budget = max_steps.unwrap_or_else(|| core_budget.max(1)).max(1);
    let (widths, _admissible, _wall, reason, _modeled_max_steps) =
        cpa_allocate(cfg, speedups, est, core_budget, mem_budget, active_budget);
    if reason == CPA_INFEASIBLE_FIXED_WIDTH {
        return Err(InfeasibleAllocationError {
            core_budget: core_budget.max(1),
            fixed_widths: infeasible_fixed_widths(cfg, &widths, core_budget),
            mem_budget: None,
            memory_footprint: None,
        });
    }
    if reason == CPA_INFEASIBLE_MEMORY {
        let budget = mem_budget.expect("infeasible-memory requires a memory budget");
        return Err(InfeasibleAllocationError {
            core_budget: core_budget.max(1),
            fixed_widths: Vec::new(),
            mem_budget: Some(budget),
            memory_footprint: Some(cpa_footprint(cfg, &widths, speedups, 1)),
        });
    }
    Ok(widths)
}

// A deterministic no-overcommit reference schedule of the allocated widths (mirrors
// Python's `_cpa_simulate_makespan`). Launches ready steps (deps done, core budget
// `Σ running widths + p_i <= P`, named resources free) in `order`, advancing to the next finish
// event. Allocated widths must lie in `1..=P`; there is no over-budget run-alone escape.
// Respecting deps AND the reference capacity makes the result `>= max(T_CP, area/P)`
// (PLANNER_DESIGN.md §2). The live scheduler intentionally permits wider aggregate overlap.
// Same f64 ops in canonical `order` as the Python build, so the 3-decimal makespan is byte-identical.
fn cpa_simulate_makespan(
    cfg: &DagConfig,
    widths: &HashMap<String, i64>,
    weight: &HashMap<String, f64>,
    order: &[String],
    core_budget: i64,
    max_steps: i64,
) -> f64 {
    let p = if core_budget > 0 { core_budget } else { 1 };
    let by_tag: HashMap<String, &Step> = cfg.steps.iter().map(|s| (s.tag(), s)).collect();
    let mut done: HashMap<String, f64> = HashMap::new();
    let mut running: HashMap<String, f64> = HashMap::new();
    let mut res_avail: HashMap<String, i64> = cfg
        .resource_caps
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect();
    let mut cores_used: i64 = 0;
    let mut now = 0.0f64;
    let mut pending: std::collections::HashSet<String> =
        cfg.steps.iter().map(|s| s.tag()).collect();
    assert!(pending
        .iter()
        .all(|tag| widths[tag] >= 1 && widths[tag] <= p));
    while !pending.is_empty() || !running.is_empty() {
        let mut launched = true;
        while launched {
            launched = false;
            for tag in order {
                if !pending.contains(tag) {
                    continue;
                }
                if running.len() as i64 >= max_steps.max(1) {
                    break;
                }
                let step = by_tag[tag];
                if !step.deps.iter().all(|d| done.contains_key(d)) {
                    continue;
                }
                let w = widths[tag];
                if cores_used + w > p {
                    continue;
                }
                let res_free = step
                    .hint
                    .resources
                    .iter()
                    .all(|(r, n)| res_avail.get(r).copied().unwrap_or(0) >= *n);
                if !res_free {
                    continue;
                }
                running.insert(tag.clone(), now + weight[tag]);
                pending.remove(tag);
                cores_used += w;
                for (r, n) in &step.hint.resources {
                    *res_avail.entry(r.clone()).or_insert(0) -= n;
                }
                launched = true;
            }
        }
        if running.is_empty() {
            break;
        }
        let finish = running.values().copied().fold(f64::INFINITY, f64::min);
        now = finish;
        for tag in order {
            if running.get(tag).copied() == Some(finish) {
                done.insert(tag.clone(), finish);
                running.remove(tag);
                cores_used -= widths[tag];
                let step = by_tag[tag];
                for (r, n) in &step.hint.resources {
                    *res_avail.entry(r.clone()).or_insert(0) += n;
                }
            }
        }
    }
    done.values().copied().fold(0.0, f64::max)
}

// Two-phase CPA plan (PLANNER_DESIGN.md §4): allocate widths, then critical-path list-schedule at
// the allocated weights `T_i(p_i)`. Mirrors Python's `_build_cpa_plan`.
#[allow(clippy::too_many_arguments)] // The allocator consumes each independently tested model input.
fn build_cpa_plan(
    cfg: &DagConfig,
    resolved: &HashMap<String, Resolved>,
    est: &HashMap<String, f64>,
    speedups: &HashMap<String, StepSpeedup>,
    succ: &HashMap<String, Vec<String>>,
    core_budget: Option<i64>,
    mem_budget: Option<i64>,
    max_steps: Option<i64>,
) -> Plan {
    let p = match core_budget {
        Some(b) if b > 0 => b,
        _ => 1,
    };
    let active_budget = max_steps.unwrap_or(p).max(1);
    // Allocate against the same learned RSS values that apply_plan_to_config installs for
    // execution and ordinary --max-mem sizing. Otherwise CPA can approve a width against a stale
    // authored hint and only afterward replace it with a larger store estimate.
    let mut memory_cfg = cfg.clone();
    for step in &mut memory_cfg.steps {
        let r = &resolved[&step.tag()];
        if r.rss_source == "store" {
            step.hint.rss_baseline_bytes = r.rss;
            step.hint.rss_baseline_inner_jobs = None;
        }
    }
    let (widths, _admissible, wall, stop_reason, modeled_max_steps) =
        cpa_allocate(&memory_cfg, speedups, est, p, mem_budget, active_budget);
    let weight: HashMap<String, f64> = cfg
        .steps
        .iter()
        .map(|s| {
            let tag = s.tag();
            let w = widths[&tag];
            (tag.clone(), wall[&tag][&w])
        })
        .collect();
    let bottom = bottom_levels(cfg, &weight, succ);
    let (critical, t_cp) = critical_path(cfg, &bottom, succ);
    let order = plan_order(cfg, Planner::CriticalPath, &weight, &bottom);
    let area: f64 = cfg
        .steps
        .iter()
        .map(|step| {
            let tag = step.tag();
            cpu_work_at(speedups, &tag, widths[&tag], weight[&tag])
        })
        .sum();
    let t_a = area / p as f64;
    let lower_bound = if t_cp >= t_a { t_cp } else { t_a };
    let modeled = if matches!(
        stop_reason.as_str(),
        CPA_INFEASIBLE_FIXED_WIDTH | CPA_INFEASIBLE_MEMORY
    ) {
        f64::INFINITY
    } else {
        cpa_simulate_makespan(cfg, &widths, &weight, &order, p, modeled_max_steps)
    };
    let infeasible_memory = stop_reason == CPA_INFEASIBLE_MEMORY;
    let allocation = Allocation {
        core_budget: p,
        area_s: area,
        area_bound_s: t_a,
        critical_path_s: t_cp,
        lower_bound_s: lower_bound,
        modeled_makespan_s: modeled,
        stop_reason: stop_reason.clone(),
        modeled_max_steps,
    };
    let entries: Vec<PlanEntry> = cfg
        .steps
        .iter()
        .map(|s| {
            let tag = s.tag();
            let r = &resolved[&tag];
            let curve_level = if s.skip_reason.is_none() {
                speedups.get(&tag).and_then(|sp| {
                    sp.levels
                        .iter()
                        .find(|level| level.inner_jobs == widths[&tag])
                })
            } else {
                None
            };
            let uses_curve = curve_level.is_some();
            let (modeled_rss, rss_is_exact) =
                modeled_memory_at(speedups, &tag, widths[&tag], r.rss);
            let (_exact, floor) = memory_evidence_at(speedups, &tag, widths[&tag]);
            PlanEntry {
                tag: tag.clone(),
                est_duration_s: weight[&tag],
                est_source: if uses_curve {
                    "store".to_string()
                } else {
                    r.est_source.to_string()
                },
                rss_estimate_bytes: modeled_rss,
                rss_source: if rss_is_exact || floor.is_some() {
                    "store".to_string()
                } else {
                    r.rss_source.to_string()
                },
                bottom_level_s: bottom.get(&tag).copied().unwrap_or(0.0),
                samples: curve_level.map(|level| level.samples).unwrap_or(r.samples),
                speedup: if s.skip_reason.is_some() {
                    None
                } else {
                    speedups.get(&tag).cloned()
                },
                // A step with no width channel opts out of guest-width rewriting. CPA still
                // charges the fixed width in its model, but must not publish a value that
                // apply_plan_to_config could misrepresent as executable.
                alloc_inner_jobs: if s.skip_reason.is_some()
                    || infeasible_memory
                    || !step_width_is_resizable(s, &cfg.default_jobs_flag, &cfg.default_jobs_env)
                {
                    None
                } else {
                    Some(widths[&tag])
                },
                rss_estimate_inner_jobs: rss_is_exact.then_some(widths[&tag]),
            }
        })
        .collect();
    Plan {
        planner: Planner::Cpa,
        order,
        critical_path: critical,
        critical_path_length_s: t_cp,
        entries,
        allocation: Some(allocation),
    }
}

/// Resolve estimates and build a complete execution plan.
///
/// Speedup curves are attached for display, with their recommendation and regression marker
/// restricted to the supplied core budget for every planner. Under [`Planner::Cpa`] the bounded
/// curves also drive runner-controlled width allocation within the supplied core and memory
/// budgets; self-managed commands remain fixed and can make the plan explicitly infeasible. The
/// other planners use only dispatch ordering.
pub fn build_plan(
    cfg: &DagConfig,
    store_samples: &HashMap<String, StepSamples>,
    planner: Planner,
    min_samples: i64,
    speedups: &HashMap<String, StepSpeedup>,
    core_budget: Option<i64>,
    mem_budget: Option<i64>,
) -> Plan {
    build_plan_with_max_steps(
        cfg,
        store_samples,
        planner,
        min_samples,
        speedups,
        core_budget,
        mem_budget,
        None,
    )
}

/// [`build_plan`] with an explicit active-step ceiling for CPA's concurrent-memory model.
#[allow(clippy::too_many_arguments)] // Backward-compatible extension of the established plan API.
pub fn build_plan_with_max_steps(
    cfg: &DagConfig,
    store_samples: &HashMap<String, StepSamples>,
    planner: Planner,
    min_samples: i64,
    speedups: &HashMap<String, StepSpeedup>,
    core_budget: Option<i64>,
    mem_budget: Option<i64>,
    max_steps: Option<i64>,
) -> Plan {
    crate::model::assert_valid_jobs_env_config(cfg);
    crate::model::assert_valid_cmdtype_config(cfg);
    let mut resolved: HashMap<String, Resolved> = HashMap::new();
    let mut est: HashMap<String, f64> = HashMap::new();
    for step in &cfg.steps {
        let r = resolved_estimate(step, store_samples.get(&step.tag()), min_samples);
        est.insert(step.tag(), r.est);
        resolved.insert(step.tag(), r);
    }
    let succ = successors(cfg);
    // A CPA plan without an explicit budget has always been a one-core allocation. Preserve that
    // contract while allowing the ordering-only planners to remain unbounded when no run budget
    // exists (for example, the standalone `plan` command).
    let speedup_budget = match (planner, core_budget) {
        (_, Some(budget)) => Some(budget.max(1)),
        (Planner::Cpa, None) => Some(1),
        (_, None) => None,
    };
    let bounded_speedups: HashMap<String, StepSpeedup> = match speedup_budget {
        Some(budget) => speedups
            .iter()
            .filter_map(|(tag, speedup)| {
                if let Some(bounded) = speedup_within_budget(speedup, budget) {
                    return Some((tag.clone(), bounded));
                }
                let step = cfg.steps.iter().find(|step| step.tag() == *tag)?;
                (planner == Planner::Cpa
                    && !step_width_is_resizable(
                        step,
                        &cfg.default_jobs_flag,
                        &cfg.default_jobs_env,
                    ))
                .then(|| (tag.clone(), speedup.clone()))
            })
            .collect(),
        None => speedups.clone(),
    };
    if planner == Planner::Cpa {
        return build_cpa_plan(
            cfg,
            &resolved,
            &est,
            &bounded_speedups,
            &succ,
            core_budget,
            mem_budget,
            max_steps,
        );
    }
    let mut modeled_est = est.clone();
    let mut level_by_tag: HashMap<String, SpeedupLevel> = HashMap::new();
    for step in &cfg.steps {
        let tag = step.tag();
        let Some(speedup) = bounded_speedups.get(&tag) else {
            continue;
        };
        let Some(width) = effective_cpu_count(step, cfg.default_step_cpu_count) else {
            continue;
        };
        let Some(level) = speedup
            .levels
            .iter()
            .find(|level| level.inner_jobs == width)
        else {
            continue;
        };
        modeled_est.insert(tag.clone(), level.wall_s);
        level_by_tag.insert(tag, level.clone());
    }
    let bottom = bottom_levels(cfg, &modeled_est, &succ);
    let (critical, length) = critical_path(cfg, &bottom, &succ);
    let order = plan_order(cfg, planner, &modeled_est, &bottom);
    let entries: Vec<PlanEntry> = cfg
        .steps
        .iter()
        .map(|step| {
            let tag = step.tag();
            let r = &resolved[&tag];
            let level = level_by_tag.get(&tag);
            let effective = effective_cpu_count(step, cfg.default_step_cpu_count);
            let (width_memory, rss_is_exact) = effective.map_or((r.rss, false), |width| {
                modeled_memory_at(&bounded_speedups, &tag, width, r.rss)
            });
            let (_exact, floor) = effective.map_or((None, None), |width| {
                memory_evidence_at(&bounded_speedups, &tag, width)
            });
            PlanEntry {
                tag: tag.clone(),
                est_duration_s: level.map_or(r.est, |point| point.wall_s),
                est_source: if level.is_some() {
                    "store".to_string()
                } else {
                    r.est_source.to_string()
                },
                rss_estimate_bytes: width_memory,
                rss_source: if rss_is_exact || floor.is_some() {
                    "store".to_string()
                } else {
                    r.rss_source.to_string()
                },
                bottom_level_s: bottom.get(&tag).copied().unwrap_or(0.0),
                samples: level.map_or(r.samples, |point| point.samples),
                speedup: if step.skip_reason.is_some() {
                    None
                } else {
                    bounded_speedups.get(&tag).cloned()
                },
                alloc_inner_jobs: None,
                rss_estimate_inner_jobs: if rss_is_exact { effective } else { None },
            }
        })
        .collect();
    Plan {
        planner,
        order,
        critical_path: critical,
        critical_path_length_s: length,
        entries,
        allocation: None,
    }
}

/// Return a configuration whose resource hints contain a plan's resolved estimates.
///
/// Stored memory replaces the original hint only when it was selected as the estimate source.
/// Allocating plans install each chosen inner-job width only for runner-controlled jobs flags;
/// self-managed commands retain their declared fixed width so run-budget validation cannot be
/// bypassed by applying a plan.
///
/// THE TOP-LEVEL POLICY IS CARRIED BY CONSTRUCTION, via [`DagConfig::with_steps`]. This is one of
/// the two places in the product that rebuilds a `DagConfig` around a new step list, and it is
/// exactly the shape of the dropped-field bug in #21 scarce-resource-deadlock: a field-by-field
/// reconstruction here once discarded `default_step_cpu_count` immediately before the run-budget
/// clamp read it.
pub fn apply_plan_to_config(cfg: &DagConfig, plan: &Plan) -> DagConfig {
    if plan
        .allocation
        .as_ref()
        .is_some_and(|allocation| allocation.stop_reason == CPA_INFEASIBLE_MEMORY)
    {
        return cfg.clone();
    }
    let by_tag = plan.by_tag();
    let planned_steps: Vec<Step> = cfg
        .steps
        .iter()
        .map(|step| {
            let tag = step.tag();
            let mut s = step.clone();
            if let Some(entry) = by_tag.get(&tag) {
                if step.skip_reason.is_some() {
                    return s;
                }
                let rss = if entry.rss_source == "store" {
                    entry.rss_estimate_bytes
                } else {
                    step.hint.rss_baseline_bytes
                };
                let inner = if !step_width_is_resizable(
                    step,
                    &cfg.default_jobs_flag,
                    &cfg.default_jobs_env,
                ) {
                    step.hint.preferred_inner_jobs
                } else {
                    entry.alloc_inner_jobs.or(step.hint.preferred_inner_jobs)
                };
                s.hint = ResourceHint {
                    resources: step.hint.resources.clone(),
                    est_duration_s: entry.est_duration_s,
                    rss_baseline_bytes: rss,
                    rss_baseline_inner_jobs: if entry.rss_source == "store" {
                        entry.rss_estimate_inner_jobs
                    } else {
                        None
                    },
                    hard_mem_max_bytes: step.hint.hard_mem_max_bytes,
                    classification: step.hint.classification,
                    preferred_inner_jobs: inner,
                    measured_effective_cores: step.hint.measured_effective_cores,
                    measured_cpu_utilization: step.hint.measured_cpu_utilization,
                };
            }
            s
        })
        .collect();
    cfg.with_steps(planned_steps)
}

// --------------------------------------------------------------------------- rendering

// Fixed 3-decimal seconds, byte-identical to the Python `f"{value:.3f}"`. Shared with the summary
// serializer so every float in the summary JSON uses the SAME fixed-precision formatting.
pub(crate) fn fmt_secs(value: f64) -> String {
    format!("{value:.3}")
}

/// JSON value for an optional fixed-3-decimal number: `null` or a quoted string.
fn opt_secs_json(value: Option<f64>) -> String {
    match value {
        None => "null".to_string(),
        Some(v) => format!("\"{}\"", fmt_secs(v)),
    }
}

// An optional integer as a JSON literal: the number, or `null`.
fn opt_int_json(value: Option<i64>) -> String {
    match value {
        None => "null".to_string(),
        Some(v) => v.to_string(),
    }
}

// One speedup-curve level as a single-line JSON object (byte-identical to the Python build).
fn speedup_level_json(level: &SpeedupLevel, baseline_inner_jobs: i64) -> String {
    let width_ratio = level.inner_jobs as f64 / baseline_inner_jobs.max(1) as f64;
    let parallel_efficiency = if width_ratio > 0.0 {
        level.speedup / width_ratio
    } else {
        0.0
    };
    format!(
        "{{\"inner_jobs\": {}, \"wall_s\": \"{}\", \"raw_wall_s\": {}, \"wall_min_s\": {}, \"wall_max_s\": {}, \"speedup\": \"{}\", \"cpu_s\": {}, \"effective_cores\": {}, \"throttled_s\": {}, \"peak_bytes\": {}, \"peak_samples\": {}, \"peak_floor_bytes\": {}, \"peak_floor_samples\": {}, \"parallel_efficiency\": \"{}\", \"samples\": {}}}",
        level.inner_jobs,
        fmt_secs(level.wall_s),
        opt_secs_json(level.raw_wall_s),
        opt_secs_json(level.wall_min_s),
        opt_secs_json(level.wall_max_s),
        fmt_secs(level.speedup),
        opt_secs_json(level.cpu_s),
        opt_secs_json(level.effective_cores),
        opt_secs_json(level.throttled_s),
        opt_int_json(level.peak_bytes),
        level.peak_samples,
        opt_int_json(level.peak_floor_bytes),
        level.peak_floor_samples,
        fmt_secs(parallel_efficiency),
        level.samples,
    )
}

// The `"speedup"` field value for a step in the plan JSON: `null` or a nested object with the
// recommended width, achieved cores, and the full measured curve. Indented to embed after
// `"speedup": ` at the step object's 6-space field indent. Mirrors Python's `_speedup_to_json`.
fn speedup_to_json(speedup: &Option<StepSpeedup>) -> String {
    let sp = match speedup {
        None => return "null".to_string(),
        Some(s) => s,
    };
    let levels: Vec<String> = sp
        .levels
        .iter()
        .map(|l| {
            format!(
                "          {}",
                speedup_level_json(l, sp.baseline_inner_jobs)
            )
        })
        .collect();
    format!(
        "{{\n        \"baseline_inner_jobs\": {},\n        \"recommended_inner_jobs\": {},\n        \"measured_effective_cores\": {},\n        \"regression_inner_jobs\": {},\n        \"levels\": [\n{}\n        ]\n      }}",
        sp.baseline_inner_jobs,
        sp.recommended_inner_jobs,
        opt_secs_json(sp.measured_effective_cores),
        opt_int_json(sp.regression_inner_jobs),
        levels.join(",\n"),
    )
}

/// Path of the rebuildable, machine/container-specific scaling-model sidecar.
pub fn scaling_model_path(profile_dir: &Path, machine_id: &str, container_class: &str) -> PathBuf {
    profile_dir.join(format!("scaling_model_{machine_id}_{container_class}.json"))
}

/// Serialize the fitted empirical scaling model as deterministic JSON.
///
/// The authored DAG remains portable policy. This sidecar is an inspectable, rebuildable cache of
/// the machine-specific model derived from the raw profile store.
pub fn scaling_model_to_json(
    machine_id: &str,
    container_class: &str,
    speedups: &HashMap<String, StepSpeedup>,
) -> String {
    scaling_model_to_json_for_workloads(machine_id, container_class, speedups, None)
}

/// Serialize a scaling model with the current DAG's workload identities embedded per step.
pub fn scaling_model_to_json_for_workloads(
    machine_id: &str,
    container_class: &str,
    speedups: &HashMap<String, StepSpeedup>,
    workload_digests: Option<&HashMap<String, String>>,
) -> String {
    let mut tags: Vec<&String> = speedups.keys().collect();
    tags.sort();
    let steps: Vec<String> = tags
        .into_iter()
        .map(|tag| {
            let speedup = &speedups[tag];
            let levels = speedup
                .levels
                .iter()
                .map(|level| {
                    format!(
                        "        {}",
                        speedup_level_json(level, speedup.baseline_inner_jobs)
                    )
                })
                .collect::<Vec<_>>()
                .join(",\n");
            format!(
                "    {{\n      \"step\": {},\n      \"workload_digest\": {},\n      \"baseline_inner_jobs\": {},\n      \"recommended_inner_jobs\": {},\n      \"regression_inner_jobs\": {},\n      \"levels\": [\n{}\n      ]\n    }}",
                json_str(tag),
                json_str(
                    workload_digests
                        .and_then(|digests| digests.get(tag.as_str()))
                        .map(String::as_str)
                        .unwrap_or("")
                ),
                speedup.baseline_inner_jobs,
                speedup.recommended_inner_jobs,
                opt_int_json(speedup.regression_inner_jobs),
                levels,
            )
        })
        .collect();
    format!(
        "{{\n  \"schema\": 2,\n  \"machine_id\": {},\n  \"container_class\": {},\n  \"plateau_wall_tolerance\": \"0.100\",\n  \"max_cpu_work_growth\": \"{}\",\n  \"memory_min_samples_per_width\": {},\n  \"steps\": [\n{}\n  ]\n}}\n",
        json_str(machine_id),
        json_str(container_class),
        fmt_secs(SPEEDUP_MAX_WORK_GROWTH),
        MEMORY_MIN_SAMPLES_PER_LEVEL,
        steps.join(",\n"),
    )
}

/// Atomically refresh the derived scaling-model sidecar and return its path.
pub fn write_scaling_model(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
    speedups: &HashMap<String, StepSpeedup>,
) -> Result<PathBuf, String> {
    write_scaling_model_for_workloads(profile_dir, machine_id, container_class, speedups, None)
}

/// Atomically refresh a derived model carrying the workload identity of every modeled step.
pub fn write_scaling_model_for_workloads(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
    speedups: &HashMap<String, StepSpeedup>,
    workload_digests: Option<&HashMap<String, String>>,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(profile_dir)
        .map_err(|error| format!("cannot create {}: {error}", profile_dir.display()))?;
    let path = scaling_model_path(profile_dir, machine_id, container_class);
    let temporary = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("scaling-model"),
        std::process::id()
    ));
    std::fs::write(
        &temporary,
        scaling_model_to_json_for_workloads(
            machine_id,
            container_class,
            speedups,
            workload_digests,
        ),
    )
    .map_err(|error| format!("cannot write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("cannot publish {}: {error}", path.display()))?;
    Ok(path)
}

fn human_bytes(n: Option<i64>) -> String {
    let mut value = match n {
        Some(v) => v as f64,
        None => return "-".to_string(),
    };
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if value < 1024.0 || unit == "TiB" {
            return if unit == "B" {
                format!("{} B", value as i64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
        value /= 1024.0;
    }
    format!("{} B", n.unwrap_or(0))
}

fn json_str_list(items: &[String]) -> String {
    if items.is_empty() {
        return "[]".to_string();
    }
    let inner: Vec<String> = items
        .iter()
        .map(|item| format!("    {}", json_str(item)))
        .collect();
    format!("[\n{}\n  ]", inner.join(",\n"))
}

// The `"allocation"` field value in the plan JSON: `null` (non-CPA planners) or a nested object
// with the core budget, area / critical-path terms, makespan lower bound + modeled makespan, and
// the stop reason. Mirrors Python's `_allocation_to_json`.
fn allocation_to_json(alloc: &Option<Allocation>) -> String {
    let a = match alloc {
        None => return "null".to_string(),
        Some(a) => a,
    };
    format!(
        "{{\n    \"stop_reason\": {},\n    \"core_budget\": {},\n    \"modeled_max_steps\": {},\n    \"area_s\": \"{}\",\n    \"area_bound_s\": \"{}\",\n    \"critical_path_s\": \"{}\",\n    \"lower_bound_s\": \"{}\",\n    \"modeled_makespan_s\": \"{}\"\n  }}",
        json_str(&a.stop_reason),
        a.core_budget,
        a.modeled_max_steps,
        fmt_secs(a.area_s),
        fmt_secs(a.area_bound_s),
        fmt_secs(a.critical_path_s),
        fmt_secs(a.lower_bound_s),
        fmt_secs(a.modeled_makespan_s),
    )
}

/// Serialize a plan to canonical two-space-indented JSON.
///
/// Computed floating-point values are fixed-three-decimal strings. Allocation fields are `null`
/// for planners that do not allocate parallel widths.
pub fn plan_to_json(plan: &Plan) -> String {
    let by_tag = plan.by_tag();
    let mut parts: Vec<String> = vec![
        "{".to_string(),
        format!("  \"planner\": {},", json_str(plan.planner.value())),
        format!(
            "  \"critical_path\": {},",
            json_str_list(&plan.critical_path)
        ),
        format!(
            "  \"critical_path_length_s\": \"{}\",",
            fmt_secs(plan.critical_path_length_s)
        ),
        format!("  \"order\": {},", json_str_list(&plan.order)),
        format!(
            "  \"allocation\": {},",
            allocation_to_json(&plan.allocation)
        ),
    ];
    let mut steps_json: Vec<String> = Vec::with_capacity(plan.order.len());
    for tag in &plan.order {
        let entry = by_tag[tag];
        let rss = match entry.rss_estimate_bytes {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        let alloc = match entry.alloc_inner_jobs {
            Some(n) => n.to_string(),
            None => "null".to_string(),
        };
        steps_json.push(format!(
            "    {{\n      \"tag\": {},\n      \"est_duration_s\": \"{}\",\n      \"est_source\": {},\n      \"rss_estimate_bytes\": {},\n      \"rss_source\": {},\n      \"bottom_level_s\": \"{}\",\n      \"samples\": {},\n      \"alloc_inner_jobs\": {},\n      \"speedup\": {}\n    }}",
            json_str(&entry.tag),
            fmt_secs(entry.est_duration_s),
            json_str(&entry.est_source),
            rss,
            json_str(&entry.rss_source),
            fmt_secs(entry.bottom_level_s),
            entry.samples,
            alloc,
            speedup_to_json(&entry.speedup),
        ));
    }
    if steps_json.is_empty() {
        parts.push("  \"steps\": []".to_string());
    } else {
        parts.push("  \"steps\": [".to_string());
        parts.push(steps_json.join(",\n"));
        parts.push("  ]".to_string());
    }
    parts.push("}".to_string());
    parts.join("\n")
}

/// Render a compact, deterministic plan table for a terminal.
pub fn plan_to_text(plan: &Plan) -> String {
    let by_tag = plan.by_tag();
    let is_cpa = plan.allocation.is_some();
    let mut headers: Vec<&str> = vec![
        "step",
        "est_duration_s",
        "source",
        "rss_estimate",
        "rss_source",
        "bottom_level_s",
        "samples",
    ];
    if is_cpa {
        headers.push("alloc_inner_jobs");
    }
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(plan.order.len());
    for tag in &plan.order {
        let entry = by_tag[tag];
        let mut row = vec![
            tag.clone(),
            fmt_secs(entry.est_duration_s),
            entry.est_source.clone(),
            human_bytes(entry.rss_estimate_bytes),
            entry.rss_source.clone(),
            fmt_secs(entry.bottom_level_s),
            entry.samples.to_string(),
        ];
        if is_cpa {
            row.push(match entry.alloc_inner_jobs {
                Some(n) => n.to_string(),
                None => "-".to_string(),
            });
        }
        rows.push(row);
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt_row = |cells: &[String]| -> String {
        let mut parts: Vec<String> = Vec::with_capacity(headers.len());
        for (i, cell) in cells.iter().enumerate() {
            let w = widths[i];
            if i == 0 {
                parts.push(format!("{cell:<w$}"));
            } else {
                parts.push(format!("{cell:>w$}"));
            }
        }
        parts.join("  ")
    };
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut lines: Vec<String> = vec![
        format!("plan: {}", plan.planner.value()),
        "per-step estimates (source: store = learned from the profile store; hint = DAG-authored; default = none; skip = intentional pre-execution skip):".to_string(),
        fmt_row(&header_cells),
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    ];
    for row in &rows {
        lines.push(fmt_row(row));
    }
    let crit = if plan.critical_path.is_empty() {
        "(none)".to_string()
    } else {
        plan.critical_path.join(" -> ")
    };
    lines.push(format!(
        "critical path ({}s): {}",
        fmt_secs(plan.critical_path_length_s),
        crit
    ));
    let order = if plan.order.is_empty() {
        "(none)".to_string()
    } else {
        plan.order.join(", ")
    };
    lines.push(format!("scheduled order: {order}"));
    lines.extend(allocation_text_lines(plan));
    lines.extend(speedup_text_lines(plan));
    lines.join("\n") + "\n"
}

// The one-line CPA allocator summary for [`plan_to_text`] (`--planner cpa` only): the stop reason,
// the core budget, and the balancing terms — critical path vs. per-core area, the makespan lower
// bound, and the modeled makespan. Empty for the non-allocating planners. Mirrors Python's
// `_allocation_text_lines`.
fn allocation_text_lines(plan: &Plan) -> Vec<String> {
    let a = match &plan.allocation {
        None => return Vec::new(),
        Some(a) => a,
    };
    vec![format!(
        "allocator (cpa): {}; P={} cores; modeled-max-steps={}; critical-path={}s, area/P={}s, lower-bound={}s, no-overcommit-model={}s",
        a.stop_reason,
        a.core_budget,
        a.modeled_max_steps,
        fmt_secs(a.critical_path_s),
        fmt_secs(a.area_bound_s),
        fmt_secs(a.lower_bound_s),
        fmt_secs(a.modeled_makespan_s),
    )]
}

// The optional parallel-speedup section for [`plan_to_text`]: one row per step that HAS a learned
// curve (>=2 inner_jobs widths). Empty when no step has a model, so a store without multi-width
// samples renders exactly as before. Mirrors Python's `_speedup_text_lines`.
fn speedup_text_lines(plan: &Plan) -> Vec<String> {
    let by_tag = plan.by_tag();
    let modeled: Vec<(&String, &StepSpeedup)> = plan
        .order
        .iter()
        .filter_map(|tag| {
            by_tag
                .get(tag)
                .and_then(|e| e.speedup.as_ref().map(|s| (tag, s)))
        })
        .collect();
    if modeled.is_empty() {
        return Vec::new();
    }
    let headers = [
        "step",
        "rec_inner_jobs",
        "regress_at",
        "eff_cores",
        "speedup@rec",
        "par_eff@rec",
        "cpu_growth@rec",
        "memory@rec",
        "wall@rec discounted/raw",
        "curve(inner_jobs->speedup)",
    ];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(modeled.len());
    for (tag, sp) in &modeled {
        let knee = sp
            .levels
            .iter()
            .find(|l| l.inner_jobs == sp.recommended_inner_jobs)
            .map(|l| l.speedup)
            .unwrap_or(1.0);
        let eff = match sp.measured_effective_cores {
            Some(e) => format!("{e:.3}"),
            None => "-".to_string(),
        };
        let curve = sp
            .levels
            .iter()
            .map(|l| format!("{}:{:.2}x", l.inner_jobs, l.speedup))
            .collect::<Vec<_>>()
            .join(" ");
        let at_rec = sp
            .levels
            .iter()
            .find(|l| l.inner_jobs == sp.recommended_inner_jobs);
        let baseline = &sp.levels[0];
        let par_eff = at_rec.map_or(0.0, |level| {
            let width_ratio = level.inner_jobs as f64 / sp.baseline_inner_jobs.max(1) as f64;
            if width_ratio > 0.0 {
                level.speedup / width_ratio
            } else {
                0.0
            }
        });
        let cpu_growth = match (at_rec.and_then(|level| level.cpu_s), baseline.cpu_s) {
            (Some(cpu), Some(base)) if base > 0.0 => Some(cpu / base),
            _ => None,
        };
        // Both terms, always: the left number is discounted (modelled), the right one measured.
        let walls = match at_rec {
            None => "-".to_string(),
            Some(l) => match l.raw_wall_s {
                None => format!("{:.3}/-", l.wall_s),
                Some(raw) => format!("{:.3}/{:.3}", l.wall_s, raw),
            },
        };
        let regress = match sp.regression_inner_jobs {
            None => "-".to_string(),
            Some(w) => w.to_string(),
        };
        rows.push(vec![
            (*tag).clone(),
            sp.recommended_inner_jobs.to_string(),
            regress,
            eff,
            format!("{knee:.2}x"),
            format!("{par_eff:.2}"),
            cpu_growth.map_or_else(|| "-".to_string(), |growth| format!("{growth:.2}x")),
            human_bytes(at_rec.and_then(|level| level.peak_bytes)),
            walls,
            curve,
        ]);
    }
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt_row = |cells: &[String]| -> String {
        let mut parts: Vec<String> = Vec::with_capacity(headers.len());
        for (i, cell) in cells.iter().enumerate() {
            let w = widths[i];
            if i == 0 {
                parts.push(format!("{cell:<w$}"));
            } else {
                parts.push(format!("{cell:>w$}"));
            }
        }
        parts.join("  ")
    };
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut out: Vec<String> = vec![
        String::new(),
        "parallel-speedup model (recommended inner_jobs = narrowest width within 10% of the best wall, subject to CPU-work + core budgets):".to_string(),
        fmt_row(&header_cells),
        widths
            .iter()
            .map(|w| "-".repeat(*w))
            .collect::<Vec<_>>()
            .join("  "),
    ];
    for row in &rows {
        out.push(fmt_row(row));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IntentionalSkipReason, ResourceHint};
    use std::collections::BTreeMap;

    fn mk(group: &str, job: &str, deps: &[&str], est: f64) -> Step {
        Step {
            group: group.into(),
            job: job.into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: deps.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint {
                est_duration_s: est,
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    #[test]
    fn apply_plan_preserves_all_non_hint_step_fields() {
        // Recurrence guard, mirroring the Python test of the same name (environment-independent, so
        // it fires without cgroups too): the planner rewrites ONLY a step's hint; every other field
        // must survive verbatim. The Python build once dropped `cpu_timeout` here by rebuilding
        // Step field-by-field, silently disabling per-step CPU-time enforcement while this Rust
        // build kept it via clone-and-override. This locks the contract in for BOTH engines.
        let mut step = mk("g", "burn", &[], 3.0);
        step.cmd = "while :; do :; done".into();
        step.env.insert("K".into(), "V".into());
        step.networkonly = true;
        step.engine_only = true;
        step.timeout = 123;
        step.cpu_timeout = 7;
        step.jobs_flag = Some("-J".into());
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        let empty: HashMap<String, StepSamples> = HashMap::new();
        let no_speedups: HashMap<String, StepSpeedup> = HashMap::new();
        let plan = build_plan(
            &cfg,
            &empty,
            Planner::GreedyLpt,
            DEFAULT_MIN_SAMPLES,
            &no_speedups,
            None,
            None,
        );
        let applied = apply_plan_to_config(&cfg, &plan);
        let out = &applied.steps[0];
        assert_eq!(
            out.cpu_timeout, 7,
            "planner dropped cpu_timeout -> CPU-time enforcement silently off"
        );
        assert_eq!(out.timeout, 123);
        assert!(out.networkonly);
        assert!(out.engine_only);
        assert_eq!(out.jobs_flag.as_deref(), Some("-J"));
        assert_eq!(out.cmd, "while :; do :; done");
        assert_eq!(out.env.get("K").map(String::as_str), Some("V"));
    }

    #[test]
    fn authored_rss_is_the_floor_for_ordinary_profile_feedback() {
        // One quiet sample may refine duration, but it is not evidence that a subprocess-heavy
        // step's enforced memory ceiling may shrink. Ordinary feedback can still raise an
        // underestimate; deliberate lowering belongs to --profile-memory-feedback.
        let mut step = mk("g", "variable", &[], 10.0);
        step.hint.rss_baseline_bytes = Some(8_000);
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        let no_speedups: HashMap<String, StepSpeedup> = HashMap::new();

        let quiet: HashMap<String, StepSamples> = [(
            "g.variable".to_string(),
            StepSamples {
                step: "g.variable".to_string(),
                samples: 1,
                est_duration_s: Some(3.0),
                rss_estimate_bytes: Some(2_000),
            },
        )]
        .into_iter()
        .collect();
        let quiet_plan = build_plan(
            &cfg,
            &quiet,
            Planner::GreedyLpt,
            DEFAULT_MIN_SAMPLES,
            &no_speedups,
            None,
            None,
        );
        assert_eq!(quiet_plan.entries[0].est_source, "store");
        assert_eq!(quiet_plan.entries[0].est_duration_s, 3.0);
        assert_eq!(quiet_plan.entries[0].rss_source, "hint");
        assert_eq!(quiet_plan.entries[0].rss_estimate_bytes, Some(8_000));
        assert_eq!(
            apply_plan_to_config(&cfg, &quiet_plan).steps[0]
                .hint
                .rss_baseline_bytes,
            Some(8_000)
        );

        let high: HashMap<String, StepSamples> = [(
            "g.variable".to_string(),
            StepSamples {
                step: "g.variable".to_string(),
                samples: 1,
                est_duration_s: Some(3.0),
                rss_estimate_bytes: Some(12_000),
            },
        )]
        .into_iter()
        .collect();
        let high_plan = build_plan(
            &cfg,
            &high,
            Planner::GreedyLpt,
            DEFAULT_MIN_SAMPLES,
            &no_speedups,
            None,
            None,
        );
        assert_eq!(high_plan.entries[0].rss_source, "store");
        assert_eq!(high_plan.entries[0].rss_estimate_bytes, Some(12_000));
        assert_eq!(
            apply_plan_to_config(&cfg, &high_plan).steps[0]
                .hint
                .rss_baseline_bytes,
            Some(12_000)
        );
    }

    #[test]
    fn applying_a_plan_carries_the_whole_lane_policy_forward() {
        // Applying a plan is one of the two places the PRODUCT rebuilds a `DagConfig` around a
        // new step list, and it happens on EVERY run. The test above guards the per-STEP fields;
        // this guards the TOP-LEVEL policy with the same carry assertion the Python edition
        // applies, because a field-by-field rebuild here once discarded `default_step_cpu_count`
        // immediately before the run-budget clamp read it (#21 scarce-resource-deadlock).
        let mut caps = BTreeMap::new();
        caps.insert("widget_guest".to_string(), 1);
        let cfg = DagConfig {
            steps: vec![mk("g", "burn", &[], 3.0)],
            description: "a real lane".to_string(),
            resource_caps: caps,
            mem_cap_factor: 1.5,
            mem_cap_floor_bytes: 4 * 1024i64.pow(3),
            outer_mem_safety_factor: 1.2,
            default_step_timeout: 600,
            default_jobs_flag: "--jobs {n}".to_string(),
            default_jobs_env: "BUILD_JOBS".to_string(),
            default_step_mem_cap_bytes: None,
            default_step_cpu_count: Some(4),
            default_step_cpu_timeout: 120,
            cpu_timeout_multiplier: 2.0,
            cpu_timeout_platform: "github-hosted".to_string(),
            write_domain_policy: Default::default(),
        };
        let empty: HashMap<String, StepSamples> = HashMap::new();
        let no_speedups: HashMap<String, StepSpeedup> = HashMap::new();
        let plan = build_plan(
            &cfg,
            &empty,
            Planner::GreedyLpt,
            DEFAULT_MIN_SAMPLES,
            &no_speedups,
            None,
            None,
        );
        let applied = apply_plan_to_config(&cfg, &plan);
        // Only the steps may differ (the plan writes their hints); every top-level field carries.
        assert_eq!(
            crate::model::dag_config_carry_diff(&cfg, &applied),
            Vec::<String>::new()
        );
        // ...and the plan really was applied, so handing the argument straight back cannot pass.
        assert_eq!(applied.steps[0].hint.est_duration_s, 3.0);
        // The field that a rebuild here actually dropped once, spelled out.
        assert_eq!(applied.default_step_cpu_count, Some(4));
        assert_eq!(applied.default_step_timeout, 600);
    }

    #[test]
    fn robust_median_discards_outlier() {
        // At n>=3 one huge outlier must not move the median (MAD-trim).
        assert_eq!(robust_median(&[3.0, 3.0, 3.0, 3.0, 100.0]), 3.0);
        assert_eq!(robust_median(&[5.0, 5.0, 100.0]), 5.0);
        // At n<3 MAD-trim cannot reject an outlier; the robust estimate is the MINIMUM (intrinsic
        // value), NOT the mean, so a single slow sample cannot invert a real speedup.
        assert_eq!(robust_median(&[5.0]), 5.0);
        assert_eq!(robust_median(&[5.0, 100.0]), 5.0);
        assert_eq!(robust_median(&[100.0, 5.0]), 5.0);
        assert_eq!(robust_median(&[2.0, 4.0]), 2.0);
    }

    #[test]
    fn parse_float_rejects_non_finite() {
        // inf / -inf / nan / overflowing literals must be dropped so a bogus cell cannot poison a
        // median or contention fraction (they pass a bare `>= 0.0` filter otherwise).
        assert_eq!(parse_float(Some(&"inf".to_string())), None);
        assert_eq!(parse_float(Some(&"-inf".to_string())), None);
        assert_eq!(parse_float(Some(&"nan".to_string())), None);
        assert_eq!(parse_float(Some(&"1e400".to_string())), None);
        assert_eq!(parse_float(Some(&"5.0".to_string())), Some(5.0));
    }

    #[test]
    fn high_percentile_nearest_rank() {
        assert_eq!(high_percentile(&[10]), 10);
        // n=10, rank = (9*10+9)/10 = 9 -> 9th smallest (1-indexed) = 90.
        assert_eq!(
            high_percentile(&[10, 20, 30, 40, 50, 60, 70, 80, 90, 100]),
            90
        );
        // n=3, rank = (27+9)/10 = 3 -> the max.
        assert_eq!(high_percentile(&[5, 9, 7]), 9);
    }

    #[test]
    fn parse_helpers_trim_and_reject() {
        // Surrounding ASCII whitespace is trimmed (matches Python's str.strip over the same set).
        assert_eq!(parse_float(Some(&" 50.0 ".to_string())), Some(50.0));
        assert_eq!(parse_int(Some(&"  1000\t".to_string())), Some(1000));
        // PEP-515 underscore separators are rejected in both builds.
        assert_eq!(parse_float(Some(&"1_0.0".to_string())), None);
        assert_eq!(parse_int(Some(&"1_000".to_string())), None);
        // Out-of-i64 magnitudes are rejected (Python's bigint would otherwise keep them).
        assert_eq!(parse_int(Some(&"9999999999999999999999".to_string())), None);
        assert_eq!(parse_int(Some(&(i64::MAX).to_string())), Some(i64::MAX));
        // Empty / whitespace-only / absent cells are None.
        assert_eq!(parse_float(Some(&"   ".to_string())), None);
        assert_eq!(parse_float(None), None);
    }

    #[test]
    fn affinity_width_is_ascii_digit_only() {
        assert_eq!(affinity_width("affinity8_cpu-max"), Some(8));
        assert_eq!(affinity_width("affinity316_x"), Some(316));
        // A Unicode-digit container_class must NOT be misread (returns None, never panics).
        assert_eq!(affinity_width("affinity\u{00b2}_cpu"), None); // superscript two
        assert_eq!(affinity_width("nonaffinity"), None);
        assert_eq!(affinity_width("affinity_cpu"), None);
    }

    #[test]
    fn load_step_samples_trims_and_rejects_hostile_cells() {
        let dir = std::env::temp_dir().join(format!("dagrun_est_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,\
                   enforcement_kind,runner_name,step,classification,inner_jobs,elapsed_s,returncode,\
                   ok,timed_out,oom_kills,peak_bytes,thread_peak,pct_other\n\
                   t,m,c,abc,1,abc,unverified,local,g.a,light,1, 8.0 ,0,True,False,0,1000,,0.0\n\
                   t,m,c,abc,1,abc,unverified,local,g.a,light,1,4.0,0,True,False,0,2000,,0.0\n\
                   t,m,c,abc,1,abc,unverified,local,g.a,light,1,10.0,0,True,False,0,3000,, 50.0 \n\
                   t,m,c,abc,1,abc,unverified,local,g.b,light,1,1_0.0,0,True,False,0,1_000,,0.0\n\
                   t,m,c,abc,1,abc,unverified,local,g.b,light,1,4.0,0,True,False,0,9999999999999999999999,,0.0\n\
                   t,m,c,abc,1,abc,unverified,local,g.b,light,1,6.0,0,True,False,0,5000,,0.0\n";
        let path = dir.join("step_profiles_m_c.csv");
        std::fs::write(&path, csv).unwrap();
        let samples = load_step_samples(&dir, "m", "c");
        // g.a: whitespace elapsed trimmed, padded ' 50.0 '% contention trimmed+applied -> [8,4,5].
        let a = &samples["g.a"];
        assert_eq!(a.samples, 3);
        assert_eq!(a.est_duration_s, Some(5.0));
        assert_eq!(a.rss_estimate_bytes, Some(3000));
        // g.b: '1_0.0' rejected -> [4,6]. Two samples cannot MAD-reject an outlier, so the robust
        // estimate is the minimum (4.0); '1_000' + out-of-i64 peak rejected -> [5000].
        let b = &samples["g.b"];
        assert_eq!(b.samples, 3);
        assert_eq!(b.est_duration_s, Some(4.0));
        assert_eq!(b.rss_estimate_bytes, Some(5000));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn contention_discounts_duration() {
        // A step run under 50% other-work contention: intrinsic ~= elapsed * 0.5.
        let mut row: HashMap<String, String> = HashMap::new();
        row.insert("pct_other".into(), "50.0".into());
        assert!((contention_fraction(&row, Some(8)) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn speedup_model_detects_knee_and_linear() {
        let dir = std::env::temp_dir().join(format!("dagrun_sp_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // b.app: wall halves 1->2 but flattens 2->4 while total CPU-s blows up -> knee at 2.
        // l.step: near-linear halving with flat CPU-s -> widest measured width (4), under budget 16.
        let csv = "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,\
                   enforcement_kind,runner_name,step,classification,inner_jobs,elapsed_s,returncode,\
                   ok,timed_out,oom_kills,peak_bytes,thread_peak,effective_cores,user_s,sys_s,throttled_s\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,b.app,cpu-bound,1,10.0,0,True,False,0,1000,,1.0,10.0,0.2,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,b.app,cpu-bound,1,10.1,0,True,False,0,1000,,1.0,10.1,0.2,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,b.app,cpu-bound,2,5.0,0,True,False,0,1000,,1.98,10.0,0.4,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,b.app,cpu-bound,2,5.05,0,True,False,0,1000,,1.98,10.1,0.4,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,b.app,cpu-bound,4,4.5,0,True,False,0,1000,,3.2,17.5,0.8,1.2\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,b.app,cpu-bound,4,4.6,0,True,False,0,1000,,3.2,17.6,0.9,1.3\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,l.step,cpu-bound,1,8.0,0,True,False,0,1000,,,8.0,0.0,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,l.step,cpu-bound,2,4.0,0,True,False,0,1000,,,8.0,0.0,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,l.step,cpu-bound,4,2.0,0,True,False,0,1000,,,8.1,0.0,0.0\n\
                   t,m,affinity16_cpu-max-max,a,1,a,u,l,s.one,cpu-bound,1,5.0,0,True,False,0,1000,,,5.0,0.0,0.0\n";
        let path = dir.join("step_profiles_m_affinity16_cpu-max-max.csv");
        std::fs::write(&path, csv).unwrap();
        let models = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let knee = &models["b.app"];
        assert_eq!(knee.baseline_inner_jobs, 1);
        assert_eq!(knee.recommended_inner_jobs, 2);
        assert_eq!(knee.measured_effective_cores, Some(1.98));
        assert_eq!(
            knee.levels.iter().map(|l| l.inner_jobs).collect::<Vec<_>>(),
            vec![1, 2, 4]
        );
        assert_eq!(models["l.step"].recommended_inner_jobs, 4);
        // A single measured width is not enough to model a curve.
        assert!(!models.contains_key("s.one"));
        let _ = std::fs::remove_file(&path);
    }

    fn speedup_from_walls(points: &[(i64, f64)]) -> StepSpeedup {
        let mut rows: Vec<HashMap<String, String>> = Vec::new();
        for (width, wall) in points {
            let mut row = HashMap::new();
            row.insert("step".to_string(), "g.step".to_string());
            row.insert("inner_jobs".to_string(), width.to_string());
            row.insert("elapsed_s".to_string(), wall.to_string());
            row.insert("ok".to_string(), "True".to_string());
            row.insert("returncode".to_string(), "0".to_string());
            rows.push(row);
        }
        step_speedups_from_buckets(&bucketize_rows(&rows, None), None)
            .remove("g.step")
            .expect("speedup")
    }

    #[test]
    fn plateau_is_global_and_grid_invariant() {
        let coarse = speedup_from_walls(&[(1, 56.88), (8, 7.9), (64, 7.2)]);
        let refined = speedup_from_walls(&[(1, 56.88), (8, 7.9), (32, 7.35), (64, 7.2)]);
        assert_eq!(coarse.recommended_inner_jobs, 8);
        assert_eq!(refined.recommended_inner_jobs, 8);
    }

    #[test]
    fn width_specific_memory_excludes_censored_modern_rows() {
        let mut rows = Vec::new();
        for (width, peak, cap, reclaim) in [(1, 1000, "max", "0"), (2, 2000, "2000", "1")] {
            for repeat in 0..3 {
                rows.push(HashMap::from([
                    ("step".to_string(), "mem.step".to_string()),
                    ("inner_jobs".to_string(), width.to_string()),
                    ("elapsed_s".to_string(), (4.0 / width as f64).to_string()),
                    ("peak_bytes".to_string(), peak.to_string()),
                    ("memory_max_bytes".to_string(), cap.to_string()),
                    ("memory_events_high".to_string(), "0".to_string()),
                    ("memory_events_max".to_string(), reclaim.to_string()),
                    ("memory_events_oom".to_string(), "0".to_string()),
                    ("memory_events_oom_kill".to_string(), "0".to_string()),
                    ("ok".to_string(), "True".to_string()),
                    ("returncode".to_string(), "0".to_string()),
                    ("observation_id".to_string(), format!("{width}-{repeat}")),
                ]));
            }
        }
        let model = &step_speedups_from_buckets(&bucketize_rows(&rows, None), Some(2))["mem.step"];
        let points: HashMap<i64, &SpeedupLevel> = model
            .levels
            .iter()
            .map(|level| (level.inner_jobs, level))
            .collect();
        assert_eq!(points[&1].peak_bytes, Some(1000));
        assert_eq!(points[&1].peak_samples, 3);
        assert_eq!(points[&2].peak_bytes, None);
        assert_eq!(points[&2].peak_samples, 0);
        assert_eq!(points[&2].peak_floor_bytes, Some(2000));
        assert_eq!(points[&2].peak_floor_samples, 3);
    }

    #[test]
    fn exact_width_memory_is_not_scaled_twice_when_plan_is_applied() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut rows = Vec::new();
        for (width, wall, peak) in [(1, 8.0, GIB), (8, 1.0, 3 * GIB)] {
            for repeat in 0..3 {
                rows.push(HashMap::from([
                    ("step".to_string(), "m.scaling".to_string()),
                    ("inner_jobs".to_string(), width.to_string()),
                    ("elapsed_s".to_string(), wall.to_string()),
                    ("user_s".to_string(), "8.0".to_string()),
                    ("sys_s".to_string(), "0.0".to_string()),
                    ("peak_bytes".to_string(), peak.to_string()),
                    ("observation_id".to_string(), format!("{width}-{repeat}")),
                ]));
            }
        }
        let speedups = step_speedups_from_buckets(&bucketize_rows(&rows, None), Some(8));
        let mut step = mk("m", "scaling", &[], 8.0);
        step.jobs_flag = Some("-j%d".to_string());
        step.hint.classification = crate::model::StepClass::CpuBound;
        let cfg = DagConfig {
            steps: vec![step],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(8),
            Some(4 * GIB),
        );
        assert_eq!(plan.entries[0].alloc_inner_jobs, Some(8));
        assert_eq!(plan.entries[0].rss_estimate_bytes, Some(3 * GIB));
        assert_eq!(plan.entries[0].rss_estimate_inner_jobs, Some(8));
        let applied = apply_plan_to_config(&cfg, &plan);
        assert_eq!(applied.steps[0].hint.rss_baseline_inner_jobs, Some(8));
        assert_eq!(
            crate::sizing::step_mem_cap_for_inner_jobs(&applied.steps[0], Some(8), 1.0),
            3 * GIB
        );
    }

    #[test]
    fn replicated_width_memory_cannot_lower_an_authored_rss_floor() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut rows = Vec::new();
        for (width, wall) in [(1, 8.0), (2, 4.0)] {
            for repeat in 0..3 {
                rows.push(HashMap::from([
                    ("step".to_string(), "m.variable".to_string()),
                    ("inner_jobs".to_string(), width.to_string()),
                    ("elapsed_s".to_string(), wall.to_string()),
                    ("user_s".to_string(), "8.0".to_string()),
                    ("sys_s".to_string(), "0.0".to_string()),
                    ("peak_bytes".to_string(), GIB.to_string()),
                    ("observation_id".to_string(), format!("{width}-{repeat}")),
                ]));
            }
        }
        let speedups = step_speedups_from_buckets(&bucketize_rows(&rows, None), Some(2));
        let mut step = mk("m", "variable", &[], 8.0);
        step.hint.classification = crate::model::StepClass::CpuBound;
        step.hint.preferred_inner_jobs = Some(1);
        step.hint.rss_baseline_bytes = Some(8 * GIB);
        let cfg = DagConfig {
            steps: vec![step],
            mem_cap_factor: 1.0,
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::GreedyLpt,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(2),
            None,
        );
        assert_eq!(plan.entries[0].rss_estimate_bytes, Some(8 * GIB));
        assert_eq!(plan.entries[0].rss_source, "hint");
        assert_eq!(plan.entries[0].rss_estimate_inner_jobs, None);
        let applied = apply_plan_to_config(&cfg, &plan);
        assert_eq!(applied.steps[0].hint.rss_baseline_bytes, Some(8 * GIB));
        assert_eq!(applied.steps[0].hint.rss_baseline_inner_jobs, None);
    }

    #[test]
    fn scaling_model_sidecar_is_deterministic() {
        let model = speedup_from_walls(&[(1, 8.0), (2, 4.0)]);
        let speedups = HashMap::from([("g.step".to_string(), model)]);
        let encoded = scaling_model_to_json("m", "affinity8_cpu-max-max", &speedups);
        assert!(encoded.contains("\"schema\": 2"));
        assert!(encoded.contains("\"workload_digest\": \"\""));
        assert!(encoded.contains("\"recommended_inner_jobs\": 2"));
        let dir = std::env::temp_dir().join(format!("dagrun_scaling_model_{}", std::process::id()));
        let path = write_scaling_model(&dir, "m", "affinity8_cpu-max-max", &speedups)
            .expect("write model");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), encoded);
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn critical_path_planner_differs_from_lpt() {
        // prep(1) -> heavy(10); solo(5) independent. LPT dispatches heavy/solo/prep by est;
        // critical-path dispatches prep first (bottom_level 11 > solo 5 > heavy 10 order).
        let cfg = DagConfig {
            steps: vec![
                mk("g", "prep", &[], 1.0),
                mk("g", "heavy", &["g.prep"], 10.0),
                mk("g", "solo", &[], 5.0),
            ],
            ..Default::default()
        };
        let empty: HashMap<String, StepSamples> = HashMap::new();
        let no_speedups: HashMap<String, StepSpeedup> = HashMap::new();
        let lpt = build_plan(
            &cfg,
            &empty,
            Planner::GreedyLpt,
            DEFAULT_MIN_SAMPLES,
            &no_speedups,
            None,
            None,
        );
        let cp = build_plan(
            &cfg,
            &empty,
            Planner::CriticalPath,
            DEFAULT_MIN_SAMPLES,
            &no_speedups,
            None,
            None,
        );
        assert_eq!(lpt.order, vec!["g.heavy", "g.solo", "g.prep"]);
        assert_eq!(cp.order, vec!["g.prep", "g.heavy", "g.solo"]);
        assert_eq!(cp.critical_path, vec!["g.prep", "g.heavy"]);
        assert_ne!(lpt.order, cp.order);
    }

    // ----------------------------------------------------------------- Budgeted speedup planning / CPA allocator

    /// Write a synthetic multi-inner_jobs speedup store (affinity16 identity) into a unique temp
    /// dir and return that dir.
    fn write_speedup_store(name: &str, rows: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dagrun_speedup_{}_{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let header = "timestamp,machine_id,container_class,git_sha,outer_jobs,profile_base_sha,\
                      enforcement_kind,runner_name,step,classification,inner_jobs,elapsed_s,\
                      returncode,ok,timed_out,oom_kills,peak_bytes,thread_peak,effective_cores,\
                      user_s,sys_s,throttled_s\n";
        std::fs::write(
            dir.join("step_profiles_m_affinity16_cpu-max-max.csv"),
            format!("{header}{rows}"),
        )
        .unwrap();
        dir
    }

    fn cpa_widths(plan: &Plan) -> HashMap<String, Option<i64>> {
        plan.entries
            .iter()
            .map(|e| (e.tag.clone(), e.alloc_inner_jobs))
            .collect()
    }

    fn speedup_level(width: i64, wall: f64, cpu_s: f64, peak_bytes: Option<i64>) -> SpeedupLevel {
        SpeedupLevel {
            inner_jobs: width,
            samples: 3,
            wall_s: wall,
            raw_wall_s: Some(wall),
            wall_min_s: Some(wall),
            wall_max_s: Some(wall),
            cpu_s: Some(cpu_s),
            effective_cores: Some(width as f64),
            throttled_s: Some(0.0),
            peak_bytes,
            peak_samples: if peak_bytes.is_some() { 3 } else { 0 },
            peak_floor_bytes: None,
            peak_floor_samples: 0,
            speedup: 1.0,
        }
    }

    #[test]
    fn every_planner_bounds_profile_recommendations_to_the_run_core_budget() {
        // The learned curve scales through width 8 and then regresses at 16. With P=4 every
        // planner may retain the wider measurements for display, but its actionable recommendation
        // and regression marker must describe only widths this run can execute efficiently.
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.scaling,cpu-bound,1,8.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.scaling,cpu-bound,2,4.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.scaling,cpu-bound,4,2.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.scaling,cpu-bound,8,1.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.scaling,cpu-bound,16,2.0,0,True,False,0,1000,,,16.0,0.0,0.0
";
        let dir = write_speedup_store("all-planner-budget", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        assert_eq!(speedups["g.scaling"].recommended_inner_jobs, 8);
        assert_eq!(speedups["g.scaling"].regression_inner_jobs, Some(16));
        let cfg = DagConfig {
            steps: vec![mk("g", "scaling", &[], 8.0)],
            ..Default::default()
        };

        for planner in [Planner::GreedyLpt, Planner::CriticalPath, Planner::Cpa] {
            let plan = build_plan(
                &cfg,
                &HashMap::new(),
                planner,
                DEFAULT_MIN_SAMPLES,
                &speedups,
                Some(4),
                None,
            );
            let bounded = plan.entries[0]
                .speedup
                .as_ref()
                .expect("profile-derived curve should remain available");
            assert_eq!(bounded.recommended_inner_jobs, 4, "{planner:?}");
            assert_eq!(bounded.regression_inner_jobs, None, "{planner:?}");
            assert!(
                bounded.levels.iter().any(|level| level.inner_jobs == 16),
                "{planner:?} should retain measurements above P for diagnosis"
            );
            if planner == Planner::Cpa {
                assert!(plan.entries[0].alloc_inner_jobs.unwrap() <= 4);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_refits_the_plateau_to_the_executable_budget() {
        let mut levels: Vec<SpeedupLevel> = [(1, 100.0), (2, 50.0), (4, 70.0), (8, 10.0)]
            .into_iter()
            .map(|(width, wall)| speedup_level(width, wall, 100.0, None))
            .collect();
        for level in &mut levels {
            level.speedup = 100.0 / level.wall_s;
        }
        let speedup = StepSpeedup {
            step: "g.scaling".to_string(),
            baseline_inner_jobs: 1,
            recommended_inner_jobs: 8,
            measured_effective_cores: Some(8.0),
            regression_inner_jobs: Some(4),
            levels,
        };
        let mut step = mk("g", "scaling", &[], 100.0);
        step.jobs_flag = Some("-j%d".to_string());
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::from([("g.scaling".to_string(), speedup)]),
            Some(4),
            None,
        );
        assert_eq!(
            plan.entries[0]
                .speedup
                .as_ref()
                .unwrap()
                .recommended_inner_jobs,
            2
        );
        assert_eq!(plan.entries[0].alloc_inner_jobs, Some(2));
    }

    #[test]
    fn cpa_excludes_cpu_inefficient_widths_individually() {
        let mut levels = vec![
            speedup_level(1, 100.0, 100.0, None),
            speedup_level(2, 55.0, 200.0, None),
            speedup_level(4, 25.0, 100.0, None),
        ];
        for level in &mut levels {
            level.speedup = 100.0 / level.wall_s;
        }
        let speedup = StepSpeedup {
            step: "g.scaling".to_string(),
            baseline_inner_jobs: 1,
            recommended_inner_jobs: 4,
            measured_effective_cores: Some(4.0),
            regression_inner_jobs: None,
            levels,
        };
        let mut step = mk("g", "scaling", &[], 100.0);
        step.jobs_flag = Some("-j%d".to_string());
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        let speedups = HashMap::from([("g.scaling".to_string(), speedup)]);
        let narrow = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(2),
            None,
        );
        let wide = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(4),
            None,
        );
        assert_eq!(narrow.entries[0].alloc_inner_jobs, Some(1));
        assert_eq!(wide.entries[0].alloc_inner_jobs, Some(4));
        assert_eq!(
            cpa_admissible(
                &cfg,
                &speedups,
                &HashMap::from([("g.scaling".to_string(), 100.0)]),
                4,
            )
            .0["g.scaling"],
            vec![1, 4]
        );
    }

    #[test]
    fn cpa_never_accepts_a_negative_gain_widening() {
        let mut one = speedup_level(1, 10.0, 10.0, None);
        one.speedup = 1.0;
        let mut two = speedup_level(2, 12.0, 10.0, None);
        two.speedup = 10.0 / 12.0;
        let speedup = StepSpeedup {
            step: "g.scaling".to_string(),
            baseline_inner_jobs: 1,
            recommended_inner_jobs: 2,
            measured_effective_cores: Some(2.0),
            regression_inner_jobs: Some(2),
            levels: vec![one, two],
        };
        let mut step = mk("g", "scaling", &[], 10.0);
        step.jobs_flag = Some("-j%d".to_string());
        let cfg = DagConfig {
            steps: vec![step],
            ..Default::default()
        };
        let widths = allocate_widths(
            &cfg,
            &HashMap::from([("g.scaling".to_string(), speedup)]),
            &HashMap::from([("g.scaling".to_string(), 10.0)]),
            2,
            None,
        )
        .unwrap();
        assert_eq!(widths["g.scaling"], 1);
    }

    #[test]
    fn every_planner_omits_a_profile_curve_wholly_above_the_run_core_budget() {
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.wide,cpu-bound,4,8.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.wide,cpu-bound,8,4.0,0,True,False,0,1000,,,8.0,0.0,0.0
";
        let dir = write_speedup_store("all-planner-above-budget", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let mut wide = mk("g", "wide", &[], 12.0);
        wide.hint.preferred_inner_jobs = Some(8);
        let cfg = DagConfig {
            steps: vec![wide],
            ..Default::default()
        };

        for planner in [Planner::GreedyLpt, Planner::CriticalPath, Planner::Cpa] {
            let plan = build_plan(
                &cfg,
                &HashMap::new(),
                planner,
                DEFAULT_MIN_SAMPLES,
                &speedups,
                Some(2),
                None,
            );
            assert!(plan.entries[0].speedup.is_none(), "{planner:?}");
            if planner == Planner::Cpa {
                assert_eq!(cpa_widths(&plan)["g.wide"], Some(2));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_keeps_an_empty_jobs_flag_step_at_its_rigid_declared_width() {
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.managed,cpu-bound,1,8.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.managed,cpu-bound,2,4.0,0,True,False,0,1000,,,8.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.managed,cpu-bound,4,2.0,0,True,False,0,1000,,,8.0,0.0,0.0
";
        let dir = write_speedup_store("empty-jobs-flag-rigid", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let mut managed = mk("g", "managed", &[], 8.0);
        managed.hint.preferred_inner_jobs = Some(2);
        managed.jobs_flag = Some(String::new());
        let cfg = DagConfig {
            steps: vec![managed],
            ..Default::default()
        };

        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(4),
            None,
        );
        assert_eq!(speedups["g.managed"].recommended_inner_jobs, 4);
        assert_eq!(cpa_widths(&plan)["g.managed"], None);
        assert_eq!(plan.entries[0].est_duration_s, 4.0);
        assert_eq!(plan.entries[0].est_source, "store");
        assert_eq!(
            apply_plan_to_config(&cfg, &plan).steps[0]
                .hint
                .preferred_inner_jobs,
            Some(2)
        );
        assert!(
            plan.entries[0].speedup.is_some(),
            "the measured curve remains diagnostic even though CPA cannot apply it"
        );

        let mut no_exact = cfg.clone();
        no_exact.steps[0].hint.est_duration_s = 13.0;
        no_exact.steps[0].hint.preferred_inner_jobs = Some(3);
        let no_exact_plan = build_plan(
            &no_exact,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(4),
            None,
        );
        assert_eq!(no_exact_plan.entries[0].est_duration_s, 13.0);
        assert_eq!(no_exact_plan.entries[0].est_source, "hint");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_allocates_and_applies_width_for_an_env_only_step() {
        let mut step = mk("g", "env", &[], 8.0);
        step.hint.preferred_inner_jobs = Some(4);
        step.jobs_flag = Some(String::new());
        let cfg = DagConfig {
            steps: vec![step],
            default_jobs_env: "CARGO_BUILD_JOBS".to_string(),
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::new(),
            Some(1),
            None,
        );
        assert_eq!(plan.entries[0].alloc_inner_jobs, Some(1));
        assert_eq!(
            apply_plan_to_config(&cfg, &plan).steps[0]
                .hint
                .preferred_inner_jobs,
            Some(1)
        );
    }

    #[test]
    #[should_panic(expected = "invalid jobs-env configuration")]
    fn cpa_refuses_malformed_programmatic_jobs_env_before_publishing_an_allocation() {
        let mut step = mk("g", "env", &[], 8.0);
        step.hint.preferred_inner_jobs = Some(4);
        step.jobs_flag = Some(String::new());
        let cfg = DagConfig {
            steps: vec![step],
            default_jobs_env: "bad=name".to_string(),
            ..Default::default()
        };
        let _ = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::new(),
            Some(1),
            None,
        );
    }

    #[test]
    fn cpa_excludes_intentional_skips_from_cpu_memory_and_curve_allocation() {
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.live,cpu-bound,1,10.0,0,True,False,0,1000,,,10.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.live,cpu-bound,2,5.0,0,True,False,0,1000,,,10.0,0.0,0.0
";
        let dir = write_speedup_store("intentional-skip", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let mut skipped = mk("g", "skipped", &[], 100.0);
        skipped.hint.rss_baseline_bytes = Some(1_000_000_000_000);
        skipped.hint.preferred_inner_jobs = Some(8);
        skipped.jobs_flag = Some(String::new());
        skipped.skip_reason = Some(IntentionalSkipReason::EmptyManifestBucket);
        let skipped_hint = skipped.hint.clone();
        let mut live = mk("g", "live", &[], 10.0);
        live.hint.preferred_inner_jobs = Some(1);
        live.jobs_flag = Some("-j%d".to_string());
        let cfg = DagConfig {
            steps: vec![skipped, live],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            ..Default::default()
        };
        let est = HashMap::from([("g.skipped".to_string(), 0.0), ("g.live".to_string(), 10.0)]);
        let widths = allocate_widths(&cfg, &speedups, &est, 2, None).unwrap();
        assert_eq!(widths["g.skipped"], 1);
        assert_eq!(widths["g.live"], 2);
        assert_eq!(
            crate::sizing::schedulable_peak_mem_bytes_widths(&cfg, 2, &widths).0,
            1024i64.pow(3)
        );
        assert_eq!(
            crate::sizing::stress_copy_footprint_bytes(&cfg, Some(123)),
            123
        );
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(2),
            None,
        );
        let entries = plan.by_tag();
        assert_eq!(entries["g.skipped"].est_duration_s, 0.0);
        assert_eq!(entries["g.skipped"].est_source, "skip");
        assert_eq!(entries["g.skipped"].alloc_inner_jobs, None);
        assert!(entries["g.skipped"].speedup.is_none());
        assert_eq!(entries["g.live"].alloc_inner_jobs, Some(2));
        assert_eq!(plan.allocation.as_ref().unwrap().modeled_makespan_s, 5.0);
        let applied = apply_plan_to_config(&cfg, &plan);
        assert_eq!(
            applied.steps[0].hint.est_duration_s,
            skipped_hint.est_duration_s
        );
        assert_eq!(
            applied.steps[0].hint.rss_baseline_bytes,
            skipped_hint.rss_baseline_bytes
        );
        assert_eq!(
            applied.steps[0].hint.preferred_inner_jobs,
            skipped_hint.preferred_inner_jobs
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_plan_application_cannot_launder_an_overbudget_self_managed_width() {
        let dir = std::env::temp_dir().join(format!(
            "dagrun_cpa_fixed_width_{}_{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("spawned");
        let mut fixed = mk("g", "fixed", &[], 8.0);
        fixed.cmd = format!("touch {}", marker.display());
        fixed.hint.preferred_inner_jobs = Some(8);
        fixed.jobs_flag = Some(String::new());
        let cfg = DagConfig {
            steps: vec![fixed],
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::new(),
            Some(2),
            None,
        );
        assert_eq!(plan.entries[0].alloc_inner_jobs, None);
        let error = allocate_widths(
            &cfg,
            &HashMap::new(),
            &HashMap::from([("g.fixed".to_string(), 8.0)]),
            2,
            None,
        )
        .unwrap_err();
        assert_eq!(error.core_budget, 2);
        assert_eq!(error.fixed_widths, vec![("g.fixed".to_string(), 8)]);
        let allocation = plan.allocation.as_ref().unwrap();
        assert_eq!(allocation.stop_reason, CPA_INFEASIBLE_FIXED_WIDTH);
        assert!(allocation.modeled_makespan_s.is_infinite());

        let applied = apply_plan_to_config(&cfg, &plan);
        assert_eq!(applied.steps[0].hint.preferred_inner_jobs, Some(8));
        let result = crate::scheduler::run_dag_limited(&applied, 1, 2, false, 0);
        assert!(!result.ok);
        assert!(result.outcomes.is_empty());
        assert!(!marker.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cpa_spreads_cores_on_independent_tasks() {
        // Two INDEPENDENT linear-scaling steps, P=4: the allocator balances T_CP vs area/P and
        // SPREADS cores -> both land at width 2 (not one hogging 4), stopping "balanced".
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.a,cpu-bound,1,10.0,0,True,False,0,1000,,,10.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.a,cpu-bound,2,5.0,0,True,False,0,1000,,,10.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.a,cpu-bound,4,2.5,0,True,False,0,1000,,,10.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.b,cpu-bound,1,10.0,0,True,False,0,1000,,,10.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.b,cpu-bound,2,5.0,0,True,False,0,1000,,,10.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,g.b,cpu-bound,4,2.5,0,True,False,0,1000,,,10.0,0.0,0.0
";
        let dir = write_speedup_store("spread", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let cfg = DagConfig {
            steps: vec![mk("g", "a", &[], 10.0), mk("g", "b", &[], 10.0)],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            ..Default::default()
        };
        let empty: HashMap<String, StepSamples> = HashMap::new();
        let plan = build_plan(
            &cfg,
            &empty,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(4),
            None,
        );
        let w = cpa_widths(&plan);
        assert_eq!(w["g.a"], Some(2));
        assert_eq!(w["g.b"], Some(2));
        let alloc = plan.allocation.as_ref().unwrap();
        assert_eq!(alloc.stop_reason, "balanced");
        assert!(alloc.modeled_makespan_s >= alloc.lower_bound_s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_charges_the_undeclared_step_cpu_default_as_rigid_width() {
        let mut defaulted = mk("g", "defaulted", &[], 3.0);
        defaulted.hint.preferred_inner_jobs = Some(0);
        let cfg = DagConfig {
            steps: vec![defaulted],
            default_step_cpu_count: Some(4),
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::new(),
            Some(8),
            None,
        );
        assert_eq!(cpa_widths(&plan)["g.defaulted"], Some(4));

        let mut self_managed = mk("g", "defaulted", &[], 1.0);
        self_managed.hint.preferred_inner_jobs = None;
        self_managed.jobs_flag = Some(String::new());
        let self_managed_cfg = DagConfig {
            steps: vec![self_managed],
            default_step_cpu_count: Some(8),
            ..Default::default()
        };
        let widths = allocate_widths(
            &self_managed_cfg,
            &HashMap::new(),
            &HashMap::from([("g.defaulted".to_string(), 1.0)]),
            2,
            None,
        )
        .unwrap();
        assert_eq!(widths["g.defaulted"], 2);
        let default_plan = build_plan(
            &self_managed_cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::new(),
            Some(2),
            None,
        );
        assert_eq!(cpa_widths(&default_plan)["g.defaulted"], None);
        assert_ne!(
            default_plan.allocation.as_ref().unwrap().stop_reason,
            CPA_INFEASIBLE_FIXED_WIDTH
        );
    }

    #[test]
    fn cpa_piles_cores_on_the_chain_and_leaves_plateau_narrow() {
        // Chain prep(curveless) -> build(scaling) -> test(scaling) plus independent plateau (side).
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,1,40.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,2,20.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,4,10.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,8,5.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.test,cpu-bound,1,16.0,0,True,False,0,1000,,,16.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.test,cpu-bound,2,8.0,0,True,False,0,1000,,,16.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.test,cpu-bound,4,4.0,0,True,False,0,1000,,,16.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.side,cpu-bound,1,9.0,0,True,False,0,1000,,,9.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.side,cpu-bound,2,8.7,0,True,False,0,1000,,,9.0,0.0,0.0
";
        let dir = write_speedup_store("chain", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let cfg = DagConfig {
            steps: vec![
                mk("c", "prep", &[], 2.0),
                mk("c", "build", &["c.prep"], 40.0),
                mk("c", "test", &["c.build"], 16.0),
                mk("c", "side", &[], 9.0),
            ],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            ..Default::default()
        };
        let empty: HashMap<String, StepSamples> = HashMap::new();
        let plan = build_plan(
            &cfg,
            &empty,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(16),
            None,
        );
        let w = cpa_widths(&plan);
        assert_eq!(w["c.build"], Some(8)); // piled onto the chain
        assert_eq!(w["c.test"], Some(4));
        assert_eq!(w["c.side"], Some(1)); // plateau: never widened
        assert_eq!(w["c.prep"], Some(1)); // curveless: rigid
        let alloc = plan.allocation.as_ref().unwrap();
        assert_eq!(alloc.stop_reason, "knee-exhausted");
        assert!(alloc.modeled_makespan_s < 58.0); // beats the width-1 critical path
        assert!(alloc.modeled_makespan_s >= alloc.lower_bound_s);
    }

    #[test]
    fn cpa_memory_blocks_widening_even_with_free_cores() {
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,m.heavy,cpu-bound,1,40.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,m.heavy,cpu-bound,2,20.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,m.heavy,cpu-bound,4,10.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,m.heavy,cpu-bound,8,5.0,0,True,False,0,1000,,,40.0,0.0,0.0
";
        let dir = write_speedup_store("mem", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let heavy = Step {
            group: "m".into(),
            job: "heavy".into(),
            desc: String::new(),
            description: String::new(),
            labels: Vec::new(),
            cmd: "true".into(),
            cmdtype: crate::model::CmdType::Unknown,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            deps: vec!["m.prep".into()],
            env: BTreeMap::new(),
            hint: ResourceHint {
                est_duration_s: 40.0,
                rss_baseline_bytes: Some(3 * 1024i64.pow(3)),
                classification: crate::model::StepClass::CpuBound,
                ..Default::default()
            },
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 1800,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        };
        let cfg = DagConfig {
            steps: vec![mk("m", "prep", &[], 2.0), heavy],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            ..Default::default()
        };
        let empty: HashMap<String, StepSamples> = HashMap::new();
        // 5 GiB budget blocks widening m.heavy 4 -> 8 (footprint 3 GiB -> 6 GiB), cores free.
        let capped = build_plan(
            &cfg,
            &empty,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(16),
            Some(5 * 1024i64.pow(3)),
        );
        assert_eq!(cpa_widths(&capped)["m.heavy"], Some(4));
        assert_eq!(
            capped.allocation.as_ref().unwrap().stop_reason,
            "mem-capped"
        );
        // With no RAM budget it widens all the way to the knee (8).
        let free = build_plan(
            &cfg,
            &empty,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(16),
            None,
        );
        assert_eq!(cpa_widths(&free)["m.heavy"], Some(8));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_trades_overlap_for_width_when_that_lowers_makespan() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let curve = |tag: &str| {
            let mut one = speedup_level(1, 100.0, 100.0, Some(GIB));
            one.speedup = 1.0;
            let mut eight = speedup_level(8, 12.5, 100.0, Some(3 * GIB));
            eight.speedup = 8.0;
            StepSpeedup {
                step: tag.to_string(),
                baseline_inner_jobs: 1,
                recommended_inner_jobs: 8,
                measured_effective_cores: Some(8.0),
                regression_inner_jobs: None,
                levels: vec![one, eight],
            }
        };
        let mut a = mk("g", "a", &[], 100.0);
        a.jobs_flag = Some("-j%d".to_string());
        let mut b = mk("g", "b", &[], 100.0);
        b.jobs_flag = Some("-j%d".to_string());
        let cfg = DagConfig {
            steps: vec![a, b],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let speedups = HashMap::from([
            ("g.a".to_string(), curve("g.a")),
            ("g.b".to_string(), curve("g.b")),
        ]);
        let plan = build_plan_with_max_steps(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(16),
            Some(4 * GIB),
            Some(2),
        );
        let widths = cpa_widths(&plan);
        assert_eq!(widths["g.a"], Some(8));
        assert_eq!(widths["g.b"], Some(8));
        assert_eq!(plan.allocation.as_ref().unwrap().stop_reason, CPA_BALANCED);
        assert_eq!(plan.allocation.as_ref().unwrap().modeled_max_steps, 1);
        assert_eq!(plan.allocation.as_ref().unwrap().modeled_makespan_s, 25.0);
    }

    #[test]
    fn cpa_searches_past_a_barely_feasible_ninety_mib_overlap_seed() {
        const MIB: i64 = 1024 * 1024;
        let curve = |tag: &str, points: &[(i64, f64, f64, i64)], recommended: i64| {
            let baseline_wall = points[0].1;
            let levels = points
                .iter()
                .map(|(width, wall, cpu, peak)| {
                    let mut level = speedup_level(*width, *wall, *cpu, Some(*peak));
                    level.speedup = baseline_wall / wall;
                    level
                })
                .collect();
            StepSpeedup {
                step: tag.to_string(),
                baseline_inner_jobs: 1,
                recommended_inner_jobs: recommended,
                measured_effective_cores: Some(recommended as f64),
                regression_inner_jobs: None,
                levels,
            }
        };
        let a_curve = curve(
            "g.a",
            &[
                (1, 2.586, 2.525, 46_833_664),
                (2, 1.420, 2.562, 46_563_328),
                (4, 0.815, 2.562, 47_398_912),
                (8, 0.508, 2.559, 55_177_216),
                (16, 0.347, 2.624, 77_008_896),
            ],
            16,
        );
        let b_curve = curve(
            "g.b",
            &[
                (1, 2.596, 2.516, 47_005_696),
                (2, 1.406, 2.552, 48_287_744),
                (4, 0.829, 2.576, 49_242_112),
            ],
            4,
        );
        let mut a = mk("g", "a", &[], 2.586);
        a.jobs_flag = Some("-j%d".to_string());
        let mut b = mk("g", "b", &[], 2.596);
        b.jobs_flag = Some("-j%d".to_string());
        let cfg = DagConfig {
            steps: vec![a, b],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let speedups = HashMap::from([("g.a".to_string(), a_curve), ("g.b".to_string(), b_curve)]);

        let plan = build_plan_with_max_steps(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(32),
            Some(90 * MIB),
            Some(2),
        );

        let widths = cpa_widths(&plan);
        assert_eq!(widths["g.a"], Some(4));
        assert_eq!(widths["g.b"], Some(4));
        let allocation = plan.allocation.as_ref().unwrap();
        assert_eq!(allocation.stop_reason, CPA_KNEE_EXHAUSTED);
        assert_eq!(allocation.modeled_max_steps, 1);
        assert!((allocation.modeled_makespan_s - 1.644).abs() < 1e-12);
    }

    #[test]
    fn cpa_overlap_search_retains_larger_ceiling_on_makespan_tie() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut a = mk("m", "a", &[], 1.0);
        a.hint.hard_mem_max_bytes = Some(GIB);
        let mut b = mk("m", "b", &["m.a"], 1.0);
        b.hint.hard_mem_max_bytes = Some(GIB);
        let cfg = DagConfig {
            steps: vec![a, b],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };

        let plan = build_plan_with_max_steps(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &HashMap::new(),
            Some(2),
            Some(GIB),
            Some(2),
        );

        let allocation = plan.allocation.as_ref().unwrap();
        assert_eq!(allocation.modeled_makespan_s, 2.0);
        assert_eq!(allocation.modeled_max_steps, 2);
    }

    #[test]
    fn censored_width_peak_remains_a_planning_floor() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut rows = Vec::new();
        for (width, wall, peak, cap, reclaim) in [
            (1, 8.0, GIB, "max".to_string(), "0"),
            (8, 1.0, 4 * GIB, (4 * GIB).to_string(), "2"),
        ] {
            for repeat in 0..3 {
                rows.push(HashMap::from([
                    ("step".to_string(), "m.scaling".to_string()),
                    ("inner_jobs".to_string(), width.to_string()),
                    ("elapsed_s".to_string(), wall.to_string()),
                    ("user_s".to_string(), "8.0".to_string()),
                    ("sys_s".to_string(), "0.0".to_string()),
                    ("peak_bytes".to_string(), peak.to_string()),
                    ("memory_max_bytes".to_string(), cap.clone()),
                    ("memory_events_high".to_string(), "0".to_string()),
                    ("memory_events_max".to_string(), reclaim.to_string()),
                    ("memory_events_oom".to_string(), "0".to_string()),
                    ("memory_events_oom_kill".to_string(), "0".to_string()),
                    ("ok".to_string(), "True".to_string()),
                    ("returncode".to_string(), "0".to_string()),
                    ("observation_id".to_string(), format!("{width}-{repeat}")),
                ]));
            }
        }
        let speedups = step_speedups_from_buckets(&bucketize_rows(&rows, None), Some(8));
        let mut step = mk("m", "scaling", &[], 8.0);
        step.jobs_flag = Some("-j%d".to_string());
        step.hint.rss_baseline_bytes = Some(GIB);
        step.hint.classification = crate::model::StepClass::CpuBound;
        let cfg = DagConfig {
            steps: vec![step],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(8),
            Some(3 * GIB),
        );
        assert_eq!(plan.entries[0].alloc_inner_jobs, Some(1));
        assert_eq!(
            plan.allocation.as_ref().unwrap().stop_reason,
            CPA_MEM_CAPPED
        );
    }

    #[test]
    fn censored_floor_never_replaces_a_larger_width_fallback() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut rows = Vec::new();
        for (width, wall, peak, cap, reclaim) in [
            (1, 8.0, 4 * GIB, "max".to_string(), "0"),
            (8, 1.0, 5 * GIB, (5 * GIB).to_string(), "1"),
        ] {
            for repeat in 0..3 {
                rows.push(HashMap::from([
                    ("step".to_string(), "m.scaling".to_string()),
                    ("inner_jobs".to_string(), width.to_string()),
                    ("elapsed_s".to_string(), wall.to_string()),
                    ("user_s".to_string(), "8.0".to_string()),
                    ("sys_s".to_string(), "0.0".to_string()),
                    ("peak_bytes".to_string(), peak.to_string()),
                    ("memory_max_bytes".to_string(), cap.clone()),
                    ("memory_events_high".to_string(), "0".to_string()),
                    ("memory_events_max".to_string(), reclaim.to_string()),
                    ("memory_events_oom".to_string(), "0".to_string()),
                    ("memory_events_oom_kill".to_string(), "0".to_string()),
                    ("ok".to_string(), "True".to_string()),
                    ("returncode".to_string(), "0".to_string()),
                    ("observation_id".to_string(), format!("{width}-{repeat}")),
                ]));
            }
        }
        let speedups = step_speedups_from_buckets(&bucketize_rows(&rows, None), Some(8));
        let mut step = mk("m", "scaling", &[], 8.0);
        step.jobs_flag = Some("-j%d".to_string());
        step.hint.rss_baseline_bytes = Some(4 * GIB);
        step.hint.classification = crate::model::StepClass::CpuBound;
        let cfg = DagConfig {
            steps: vec![step],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let plan = build_plan(
            &cfg,
            &HashMap::new(),
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(8),
            Some(6 * GIB),
        );
        assert_eq!(plan.entries[0].alloc_inner_jobs, Some(1));
        assert_eq!(
            plan.allocation.as_ref().unwrap().stop_reason,
            CPA_MEM_CAPPED
        );
    }

    #[test]
    fn cpa_memory_uses_learned_rss_before_allocating() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,m.heavy,cpu-bound,1,40.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,m.heavy,cpu-bound,8,5.0,0,True,False,0,1000,,,40.0,0.0,0.0
";
        let dir = write_speedup_store("learned-mem", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let mut heavy = mk("m", "heavy", &[], 40.0);
        heavy.hint.rss_baseline_bytes = Some(GIB);
        heavy.hint.classification = crate::model::StepClass::CpuBound;
        heavy.hint.preferred_inner_jobs = Some(1);
        let cfg = DagConfig {
            steps: vec![heavy],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let samples = HashMap::from([(
            "m.heavy".to_string(),
            StepSamples {
                step: "m.heavy".to_string(),
                samples: 3,
                est_duration_s: Some(40.0),
                rss_estimate_bytes: Some(8 * GIB),
            },
        )]);

        let plan = build_plan(
            &cfg,
            &samples,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &speedups,
            Some(8),
            Some(4 * GIB),
        );

        let allocation = plan.allocation.as_ref().unwrap();
        assert_eq!(allocation.stop_reason, CPA_INFEASIBLE_MEMORY);
        assert!(allocation.modeled_makespan_s.is_infinite());
        assert_eq!(plan.entries[0].rss_source, "store");
        assert_eq!(plan.entries[0].rss_estimate_bytes, Some(8 * GIB));
        assert_eq!(plan.entries[0].alloc_inner_jobs, None);
        assert_eq!(
            crate::io::dag_to_json(&apply_plan_to_config(&cfg, &plan)),
            crate::io::dag_to_json(&cfg)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cpa_seed_applies_outer_memory_envelope_and_returns_typed_error() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut heavy = mk("m", "heavy", &[], 1.0);
        heavy.hint.rss_baseline_bytes = Some(3 * GIB);
        let cfg = DagConfig {
            steps: vec![heavy],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 2.0,
            ..Default::default()
        };
        let empty_samples: HashMap<String, StepSamples> = HashMap::new();
        let empty_speedups: HashMap<String, StepSpeedup> = HashMap::new();
        let est = HashMap::from([("m.heavy".to_string(), 1.0)]);

        let plan = build_plan(
            &cfg,
            &empty_samples,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &empty_speedups,
            Some(8),
            Some(5 * GIB),
        );
        let allocation = plan.allocation.as_ref().unwrap();
        assert_eq!(allocation.stop_reason, CPA_INFEASIBLE_MEMORY);
        assert!(allocation.modeled_makespan_s.is_infinite());
        assert_eq!(plan.entries[0].alloc_inner_jobs, None);
        assert_eq!(
            crate::io::dag_to_json(&apply_plan_to_config(&cfg, &plan)),
            crate::io::dag_to_json(&cfg)
        );

        let error = allocate_widths(&cfg, &empty_speedups, &est, 8, Some(5 * GIB))
            .expect_err("seed footprint must exceed memory budget");
        assert_eq!(error.mem_budget, Some(5 * GIB));
        assert_eq!(error.memory_footprint, Some(6 * GIB));
    }

    #[test]
    fn cpa_memory_treats_max_steps_as_a_ceiling_and_models_serial_fallback() {
        const GIB: i64 = 1024 * 1024 * 1024;
        let mut a = mk("m", "a", &[], 1.0);
        a.hint.hard_mem_max_bytes = Some(4 * GIB);
        let mut b = mk("m", "b", &[], 1.0);
        b.hint.hard_mem_max_bytes = Some(4 * GIB);
        let cfg = DagConfig {
            steps: vec![a, b],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            outer_mem_safety_factor: 1.0,
            ..Default::default()
        };
        let empty_samples: HashMap<String, StepSamples> = HashMap::new();
        let empty_speedups: HashMap<String, StepSpeedup> = HashMap::new();

        let plan = build_plan(
            &cfg,
            &empty_samples,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &empty_speedups,
            Some(2),
            Some(4 * GIB),
        );

        assert_ne!(
            plan.allocation.as_ref().unwrap().stop_reason,
            CPA_INFEASIBLE_MEMORY
        );
        assert_eq!(plan.allocation.as_ref().unwrap().modeled_max_steps, 1);
        assert_eq!(plan.allocation.as_ref().unwrap().modeled_makespan_s, 2.0);
        assert_eq!(
            crate::sizing::jobs_for_budget(&apply_plan_to_config(&cfg, &plan), 4 * GIB),
            (1, 4 * GIB)
        );

        let explicitly_serial = build_plan_with_max_steps(
            &cfg,
            &empty_samples,
            Planner::Cpa,
            DEFAULT_MIN_SAMPLES,
            &empty_speedups,
            Some(2),
            Some(4 * GIB),
            Some(1),
        );
        assert_eq!(
            explicitly_serial
                .allocation
                .as_ref()
                .unwrap()
                .modeled_max_steps,
            1
        );
    }

    #[test]
    fn cpa_allocation_is_idempotent() {
        let rows = "\
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,1,40.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,2,20.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,4,10.0,0,True,False,0,1000,,,40.0,0.0,0.0
t,m,affinity16_cpu-max-max,a,1,a,u,l,c.build,cpu-bound,8,5.0,0,True,False,0,1000,,,40.0,0.0,0.0
";
        let dir = write_speedup_store("idem", rows);
        let speedups = load_step_speedups(&dir, "m", "affinity16_cpu-max-max");
        let cfg = DagConfig {
            steps: vec![
                mk("c", "prep", &[], 2.0),
                mk("c", "build", &["c.prep"], 40.0),
            ],
            mem_cap_factor: 1.0,
            mem_cap_floor_bytes: 0,
            ..Default::default()
        };
        let est: HashMap<String, f64> = cfg
            .steps
            .iter()
            .map(|s| (s.tag(), s.hint.est_duration_s))
            .collect();
        let w1 = allocate_widths(&cfg, &speedups, &est, 16, None).unwrap();
        let w2 = allocate_widths(&cfg, &speedups, &est, 16, None).unwrap();
        assert_eq!(w1, w2);
        assert_eq!(w1["c.build"], 8);
        assert_eq!(w1["c.prep"], 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workload_selection_prefers_matching_digest_and_never_mixes_revisions() {
        let row = |step: &str, digest: &str, elapsed: &str| {
            HashMap::from([
                ("step".to_string(), step.to_string()),
                ("workload_digest".to_string(), digest.to_string()),
                ("elapsed_s".to_string(), elapsed.to_string()),
            ])
        };
        let rows = vec![
            row("g.a", "", "1"),
            row("g.a", "old", "2"),
            row("g.a", "current", "3"),
            row("g.b", "", "4"),
            row("g.b", "old", "5"),
        ];
        let expected = HashMap::from([
            ("g.a".to_string(), "current".to_string()),
            ("g.b".to_string(), "current".to_string()),
        ]);

        let selected = select_workload_rows(&rows, &expected);
        let evidence: Vec<(&str, &str)> = selected
            .iter()
            .map(|row| (row["step"].as_str(), row["elapsed_s"].as_str()))
            .collect();

        assert_eq!(evidence, [("g.a", "3"), ("g.b", "4")]);
    }
}
