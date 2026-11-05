//! Bounded, mergeable summaries of execution profiles.

// A constant-sized, MERGEABLE profile SUMMARY that closes the profiling feedback loop on
// EPHEMERAL CI.
//
// Direct port of `py/dagrun/summary.py`; the canonical JSON serialization and the MERGE are
// BYTE-IDENTICAL to the Python build (cross-tested in `cross/differential.py`) — this is the
// correctness core of the sync feature.
//
// The profile store auto-logs per-step CSVs and the planner reads them back, but that loop is
// INERT on ephemeral CI (each runner starts with an empty store). This module is the artifact a
// pluggable backend ([`crate::sync`]) uploads at end-of-run and downloads at start-of-run:
//
// * For each `(step, inner_jobs, workload_digest)` bucket it keeps a RESERVOIR of up to
//   [`DEFAULT_RESERVOIR_K`] [`Sample`]s (exactly the fields the estimator + speedup model consume).
//   Separating command revisions before sampling prevents stale data from evicting the current
//   cohort. Bucket count is hard-capped at [`DEFAULT_MAX_BUCKETS`].
// * [`merge`] unions two summaries' reservoirs per bucket and subsamples back to K by a
//   CONTENT-derived stable order (an FNV-1a hash of each sample's canonical serialization, then take
//   the first K) — deterministic, COMMUTATIVE, and ASSOCIATIVE, and identical across builds.
// * Estimates are recomputed FROM the reservoirs via the same estimator core the CSV reader uses,
//   so a summary that has not subsampled a bucket yields byte-identical estimates to the raw rows.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::Value;

use crate::estimates::{
    affinity_width, bucketize_rows, buckets_for_workloads, fmt_secs, load_store,
    step_samples_from_buckets, step_speedups_from_buckets, BucketKey, Sample, StepSamples,
    StepSpeedup,
};
use crate::io::json_str;

/// On-disk schema version (bumped only on an incompatible shape change; an unknown version is
/// refused rather than mis-parsed).
pub const SUMMARY_VERSION: i64 = 2;
/// Default reservoir size K: max samples kept per `(step, inner_jobs, workload_digest)` bucket.
pub const DEFAULT_RESERVOIR_K: usize = 64;
/// Hard cap on the number of buckets (defensive constant-size guarantee).
pub const DEFAULT_MAX_BUCKETS: usize = 4096;

/// Summary reservoir identity. The workload digest is deliberately part of the key so an old
/// command revision cannot evict samples from the current revision before planner selection.
pub type SummaryBucketKey = (String, i64, String);

const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// A malformed summary document, unknown version, or mismatched identity.
#[derive(Debug)]
pub struct SummaryError(pub String);

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SummaryError {}

fn err<T>(msg: String) -> Result<T, SummaryError> {
    Err(SummaryError(msg))
}

/// A bounded, mergeable profile summary for one machine and container-class identity.
#[derive(Debug, Clone)]
pub struct Summary {
    /// On-disk schema version.
    pub version: i64,
    /// Stable machine identifier used to select compatible samples.
    pub machine_id: String,
    /// Stable CPU-affinity and quota class used to select compatible samples.
    pub container_class: String,
    /// Bounded sample reservoirs keyed by step tag, inner-job width, and workload digest.
    pub buckets: HashMap<SummaryBucketKey, Vec<Sample>>,
}

// --------------------------------------------------------------------------- content hashing

// FNV-1a 64-bit hash (wrapping arithmetic), identical to the Python build.
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut h = FNV_OFFSET_BASIS;
    for &byte in data {
        h = (h ^ byte as u64).wrapping_mul(FNV_PRIME);
    }
    h
}

fn opt_secs_json(value: Option<f64>) -> String {
    match value {
        None => "null".to_string(),
        Some(v) => format!("\"{}\"", fmt_secs(v)),
    }
}

// The canonical one-line JSON object for a sample — used BOTH as the serialized form and the input
// to the subsample hash (so they can never drift). Mirrors Python's `_sample_canonical`.
fn sample_canonical(sample: &Sample) -> String {
    format!(
        "{{\"observation_id\": {}, \"workload_digest\": {}, \"elapsed_s\": {}, \"contention\": \"{}\", \"cpu_s\": {}, \"effective_cores\": {}, \"throttled_s\": {}, \"peak_bytes\": {}, \"uncensored_peak_bytes\": {}, \"peak_floor_bytes\": {}}}",
        json_str(&sample.observation_id),
        json_str(&sample.workload_digest),
        opt_secs_json(sample.elapsed_s),
        fmt_secs(sample.contention),
        opt_secs_json(sample.cpu_s),
        opt_secs_json(sample.effective_cores),
        opt_secs_json(sample.throttled_s),
        match sample.peak_bytes {
            None => "null".to_string(),
            Some(p) => p.to_string(),
        },
        match sample.uncensored_peak_bytes {
            None => "null".to_string(),
            Some(p) => p.to_string(),
        },
        match sample.peak_floor_bytes {
            None => "null".to_string(),
            Some(p) => p.to_string(),
        },
    )
}

// The stable observation-derived rank `(fnv_hash, canonical_json)` for a sample. New observations
// have unique ids; legacy samples fall back to canonical content. The canonical JSON tie-break has
// identical ordering in both implementations. Mirrors Python's `_sample_sort_key`.
fn sample_sort_key(sample: &Sample) -> (u64, String) {
    let canon = sample_canonical(sample);
    let identity = if sample.observation_id.is_empty() {
        canon.as_bytes()
    } else {
        sample.observation_id.as_bytes()
    };
    (fnv1a_64(identity), canon)
}

// Return deterministic bottom-k order, optionally truncated to the first `cap` observations.
// Because the rank is a fixed total order on observation identities, first-cap-of-union remains
// commutative + associative. Mirrors Python's `_ordered`.
fn ordered(samples: &[Sample], cap: Option<usize>) -> Vec<Sample> {
    let mut known: BTreeMap<String, Sample> = BTreeMap::new();
    let mut legacy: Vec<Sample> = Vec::new();
    for sample in samples {
        if sample.observation_id.is_empty() {
            legacy.push(sample.clone());
            continue;
        }
        match known.get(&sample.observation_id) {
            Some(previous) if sample_canonical(previous) <= sample_canonical(sample) => {}
            _ => {
                known.insert(sample.observation_id.clone(), sample.clone());
            }
        }
    }
    let combined = known.into_values().chain(legacy);
    let mut with_keys: Vec<((u64, String), Sample)> = combined
        .map(|sample| (sample_sort_key(&sample), sample))
        .collect();
    with_keys.sort_by(|a, b| a.0.cmp(&b.0));
    let take = match cap {
        Some(c) if with_keys.len() > c => c,
        _ => with_keys.len(),
    };
    with_keys.into_iter().take(take).map(|(_, s)| s).collect()
}

fn bucket_sort_key(key: &SummaryBucketKey) -> (u64, String) {
    let canon = format!("{}:{}:{}", json_str(&key.0), key.1, json_str(&key.2));
    (fnv1a_64(canon.as_bytes()), canon)
}

// Drop buckets beyond `max_buckets` by the content-derived stable order. A no-op for a normal
// workload. Mirrors Python's `_cap_buckets`.
fn cap_buckets(
    buckets: HashMap<SummaryBucketKey, Vec<Sample>>,
    max_buckets: usize,
) -> HashMap<SummaryBucketKey, Vec<Sample>> {
    if buckets.len() <= max_buckets {
        return buckets;
    }
    let mut keys: Vec<SummaryBucketKey> = buckets.keys().cloned().collect();
    keys.sort_by_key(bucket_sort_key);
    keys.truncate(max_buckets);
    let mut out: HashMap<SummaryBucketKey, Vec<Sample>> = HashMap::new();
    for key in keys {
        if let Some(v) = buckets.get(&key) {
            out.insert(key, v.clone());
        }
    }
    out
}

// --------------------------------------------------------------------------- construction / merge

/// An empty summary for `(machine_id, container_class)` (what a backend returns when none exists).
pub fn empty(machine_id: &str, container_class: &str) -> Summary {
    Summary {
        version: SUMMARY_VERSION,
        machine_id: machine_id.to_string(),
        container_class: container_class.to_string(),
        buckets: HashMap::new(),
    }
}

/// Build a bounded summary from raw profile rows.
pub fn summary_from_rows(
    rows: &[HashMap<String, String>],
    machine_id: &str,
    container_class: &str,
    affinity: Option<i64>,
    reservoir_cap: usize,
    max_buckets: usize,
) -> Summary {
    let identified: Vec<HashMap<String, String>> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if row
                .get("observation_id")
                .is_some_and(|value| !value.is_empty())
                || row.get("run_id").is_some_and(|value| !value.is_empty())
            {
                return row.clone();
            }
            let mut fields: Vec<(&String, &String)> = row.iter().collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            let material = fields
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\0");
            let mut identified = row.clone();
            identified.insert(
                "observation_id".to_string(),
                format!("legacy-{:016x}-{index:016x}", fnv1a_64(material.as_bytes())),
            );
            identified
        })
        .collect();
    let raw = bucketize_rows(&identified, affinity);
    let mut partitioned: HashMap<SummaryBucketKey, Vec<Sample>> = HashMap::new();
    for ((step, inner_jobs), samples) in raw {
        for sample in samples {
            partitioned
                .entry((step.clone(), inner_jobs, sample.workload_digest.clone()))
                .or_default()
                .push(sample);
        }
    }
    let mut capped: HashMap<SummaryBucketKey, Vec<Sample>> = HashMap::new();
    for (key, samples) in partitioned {
        capped.insert(key, ordered(&samples, Some(reservoir_cap)));
    }
    Summary {
        version: SUMMARY_VERSION,
        machine_id: machine_id.to_string(),
        container_class: container_class.to_string(),
        buckets: cap_buckets(capped, max_buckets),
    }
}

/// Build a summary from a CSV profile store for `(machine_id, container_class)`, or an empty summary
/// when the store file is absent. A convenience wrapper the `summary build` CLI uses.
pub fn summary_from_store(
    profile_dir: &Path,
    machine_id: &str,
    container_class: &str,
    reservoir_cap: usize,
) -> Summary {
    match load_store(profile_dir, machine_id, container_class) {
        Some((rows, affinity)) => summary_from_rows(
            &rows,
            machine_id,
            container_class,
            affinity,
            reservoir_cap,
            DEFAULT_MAX_BUCKETS,
        ),
        None => empty(machine_id, container_class),
    }
}

/// Merge two summaries with the same identity into bounded sample reservoirs.
///
/// The operation is deterministic, commutative, and associative. An identity mismatch returns an
/// error rather than combining incompatible measurements.
pub fn merge(
    a: &Summary,
    b: &Summary,
    reservoir_cap: usize,
    max_buckets: usize,
) -> Result<Summary, SummaryError> {
    if a.machine_id != b.machine_id || a.container_class != b.container_class {
        return err(format!(
            "cannot merge summaries of different identities: {}/{} vs {}/{}",
            a.machine_id, a.container_class, b.machine_id, b.container_class
        ));
    }
    let mut keys: std::collections::BTreeSet<SummaryBucketKey> = std::collections::BTreeSet::new();
    keys.extend(a.buckets.keys().cloned());
    keys.extend(b.buckets.keys().cloned());
    let mut merged: HashMap<SummaryBucketKey, Vec<Sample>> = HashMap::new();
    for key in keys {
        let mut combined: Vec<Sample> = Vec::new();
        if let Some(v) = a.buckets.get(&key) {
            combined.extend(v.iter().cloned());
        }
        if let Some(v) = b.buckets.get(&key) {
            combined.extend(v.iter().cloned());
        }
        merged.insert(key, ordered(&combined, Some(reservoir_cap)));
    }
    Ok(Summary {
        version: SUMMARY_VERSION,
        machine_id: a.machine_id.clone(),
        container_class: a.container_class.clone(),
        buckets: cap_buckets(merged, max_buckets),
    })
}

/// Fold-merge same-identity summaries starting from empty (order-independent). Mirrors `merge_all`.
pub fn merge_all(
    summaries: &[Summary],
    machine_id: &str,
    container_class: &str,
    reservoir_cap: usize,
    max_buckets: usize,
) -> Result<Summary, SummaryError> {
    let mut acc = empty(machine_id, container_class);
    for s in summaries {
        acc = merge(&acc, s, reservoir_cap, max_buckets)?;
    }
    Ok(acc)
}

// --------------------------------------------------------------------------- serialization

/// Serialize a summary to canonical two-space-indented JSON.
///
/// Buckets and samples use stable content order, and floating-point values use fixed three-decimal
/// strings.
pub fn to_json(summary: &Summary) -> String {
    let mut lines: Vec<String> = vec![
        "{".to_string(),
        format!("  \"version\": {},", summary.version),
        format!("  \"machine_id\": {},", json_str(&summary.machine_id)),
        format!(
            "  \"container_class\": {},",
            json_str(&summary.container_class)
        ),
    ];
    let mut keys: Vec<SummaryBucketKey> = summary.buckets.keys().cloned().collect();
    keys.sort();
    if keys.is_empty() {
        lines.push("  \"buckets\": []".to_string());
        lines.push("}".to_string());
        return lines.join("\n");
    }
    lines.push("  \"buckets\": [".to_string());
    let mut blocks: Vec<String> = Vec::with_capacity(keys.len());
    for (step, inner, workload_digest) in &keys {
        let samples = ordered(
            &summary.buckets[&(step.clone(), *inner, workload_digest.clone())],
            None,
        );
        let mut block: Vec<String> = vec![
            "    {".to_string(),
            format!("      \"step\": {},", json_str(step)),
            format!("      \"inner_jobs\": {},", inner),
            format!("      \"workload_digest\": {},", json_str(workload_digest)),
        ];
        if samples.is_empty() {
            block.push("      \"samples\": []".to_string());
        } else {
            block.push("      \"samples\": [".to_string());
            let sample_lines: Vec<String> = samples
                .iter()
                .map(|s| format!("        {}", sample_canonical(s)))
                .collect();
            block.push(sample_lines.join(",\n"));
            block.push("      ]".to_string());
        }
        block.push("    }".to_string());
        blocks.push(block.join("\n"));
    }
    lines.push(blocks.join(",\n"));
    lines.push("  ]".to_string());
    lines.push("}".to_string());
    lines.join("\n")
}

fn as_obj<'a>(
    value: &'a Value,
    where_: &str,
) -> Result<&'a serde_json::Map<String, Value>, SummaryError> {
    value
        .as_object()
        .ok_or_else(|| SummaryError(format!("invalid summary: {where_} must be an object")))
}

fn req_str(
    m: &serde_json::Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<String, SummaryError> {
    match m.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => err(format!("invalid summary: {where_}.{key} must be a string")),
    }
}

fn req_int(
    m: &serde_json::Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<i64, SummaryError> {
    match m.get(key) {
        // Reject bool (serde treats it distinctly) and non-integers.
        Some(Value::Number(n)) => n.as_i64().ok_or_else(|| {
            SummaryError(format!(
                "invalid summary: {where_}.{key} must be an integer"
            ))
        }),
        _ => err(format!(
            "invalid summary: {where_}.{key} must be an integer"
        )),
    }
}

/// Parse an optional fixed-decimal seconds STRING (or null) back to a finite float. Rejects a
/// non-string / non-finite value (our serializer only ever emits a quoted decimal or null).
fn opt_secs(
    m: &serde_json::Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<Option<f64>, SummaryError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            let parsed: f64 = s.parse().map_err(|_| {
                SummaryError(format!(
                    "invalid summary: {where_}.{key} is not a number ({s:?})"
                ))
            })?;
            if !parsed.is_finite() {
                return err(format!(
                    "invalid summary: {where_}.{key} is not finite ({s:?})"
                ));
            }
            Ok(Some(parsed))
        }
        _ => err(format!(
            "invalid summary: {where_}.{key} must be a decimal string or null"
        )),
    }
}

fn opt_int(
    m: &serde_json::Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<Option<i64>, SummaryError> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.as_i64().ok_or_else(|| {
            SummaryError(format!(
                "invalid summary: {where_}.{key} must be an integer or null"
            ))
        })?)),
        _ => err(format!(
            "invalid summary: {where_}.{key} must be an integer or null"
        )),
    }
}

/// Parse a canonical summary document with strict shape and version validation.
pub fn from_json(text: &str) -> Result<Summary, SummaryError> {
    let raw: Value = serde_json::from_str(text)
        .map_err(|e| SummaryError(format!("invalid summary JSON: {e}")))?;
    let doc = as_obj(&raw, "summary")?;
    let version = req_int(doc, "version", "summary")?;
    if version != 1 && version != SUMMARY_VERSION {
        return err(format!(
            "unsupported summary version {version} (this build understands 1 and {SUMMARY_VERSION})"
        ));
    }
    let machine_id = req_str(doc, "machine_id", "summary")?;
    let container_class = req_str(doc, "container_class", "summary")?;
    let arr: &[Value] = match doc.get("buckets") {
        None => &[],
        Some(v) => v
            .as_array()
            .ok_or_else(|| SummaryError("invalid summary: buckets must be a list".to_string()))?
            .as_slice(),
    };
    let mut buckets: HashMap<SummaryBucketKey, Vec<Sample>> = HashMap::new();
    for (i, bucket_val) in arr.iter().enumerate() {
        let where_ = format!("buckets[{i}]");
        let bucket = as_obj(bucket_val, &where_)?;
        let step = req_str(bucket, "step", &where_)?;
        let inner = req_int(bucket, "inner_jobs", &where_)?;
        let bucket_workload_digest = if version == 1 {
            String::new()
        } else {
            req_str(bucket, "workload_digest", &where_)?
        };
        let samples_arr: &[Value] = match bucket.get("samples") {
            None => &[],
            Some(v) => v
                .as_array()
                .ok_or_else(|| {
                    SummaryError(format!("invalid summary: {where_}.samples must be a list"))
                })?
                .as_slice(),
        };
        let mut samples: Vec<Sample> = Vec::with_capacity(samples_arr.len());
        for (j, sample_val) in samples_arr.iter().enumerate() {
            let sw = format!("{where_}.samples[{j}]");
            let sm = as_obj(sample_val, &sw)?;
            let observation_id = match sm.get("observation_id") {
                None => String::new(),
                Some(value) => value.as_str().map(str::to_string).ok_or_else(|| {
                    SummaryError(format!(
                        "invalid summary: {sw}.observation_id must be a string"
                    ))
                })?,
            };
            let workload_digest = match sm.get("workload_digest") {
                None => bucket_workload_digest.clone(),
                Some(value) => value.as_str().map(str::to_string).ok_or_else(|| {
                    SummaryError(format!(
                        "invalid summary: {sw}.workload_digest must be a string"
                    ))
                })?,
            };
            let peak_bytes = opt_int(sm, "peak_bytes", &sw)?;
            let uncensored_peak_bytes = if sm.contains_key("uncensored_peak_bytes") {
                opt_int(sm, "uncensored_peak_bytes", &sw)?
            } else {
                // Version-1 summaries predate censoring provenance and historically treated every
                // peak as exact. Preserve that behavior when upgrading them in memory.
                peak_bytes
            };
            samples.push(Sample {
                elapsed_s: opt_secs(sm, "elapsed_s", &sw)?,
                contention: opt_secs(sm, "contention", &sw)?.unwrap_or(0.0),
                cpu_s: opt_secs(sm, "cpu_s", &sw)?,
                effective_cores: opt_secs(sm, "effective_cores", &sw)?,
                throttled_s: opt_secs(sm, "throttled_s", &sw)?,
                peak_bytes,
                uncensored_peak_bytes,
                peak_floor_bytes: opt_int(sm, "peak_floor_bytes", &sw)?,
                observation_id,
                workload_digest: if version == 1 {
                    String::new()
                } else {
                    workload_digest
                },
            });
        }
        if version >= 2
            && samples
                .iter()
                .any(|sample| sample.workload_digest != bucket_workload_digest)
        {
            return err(format!(
                "invalid summary: {where_} sample workload_digest does not match its bucket"
            ));
        }
        if samples.is_empty() {
            buckets
                .entry((step, inner, bucket_workload_digest))
                .or_default();
        } else {
            for sample in samples {
                buckets
                    .entry((step.clone(), inner, sample.workload_digest.clone()))
                    .or_default()
                    .push(sample);
            }
        }
    }
    Ok(Summary {
        version: SUMMARY_VERSION,
        machine_id,
        container_class,
        buckets,
    })
}

// --------------------------------------------------------------------------- estimate recompute

/// Collapse summary keys back to the raw estimator's `(step, inner_jobs)` shape, then select one
/// workload cohort per step. Partitioning happened before reservoir sampling, so stale revisions
/// cannot evict the current cohort.
fn estimator_buckets(
    summary: &Summary,
    expected: &HashMap<String, String>,
) -> HashMap<BucketKey, Vec<Sample>> {
    let mut combined: HashMap<BucketKey, Vec<Sample>> = HashMap::new();
    for ((step, inner, _digest), samples) in &summary.buckets {
        combined
            .entry((step.clone(), *inner))
            .or_default()
            .extend(samples.iter().cloned());
    }
    buckets_for_workloads(&combined, expected)
}

/// Recompute per-step duration and memory estimates from summary reservoirs.
pub fn step_samples_from_summary(summary: &Summary) -> HashMap<String, StepSamples> {
    step_samples_from_buckets(&estimator_buckets(summary, &HashMap::new()))
}

/// Recompute scalar estimates for only the expected workload cohorts.
pub(crate) fn step_samples_from_summary_for_workloads(
    summary: &Summary,
    expected: &HashMap<String, String>,
) -> HashMap<String, StepSamples> {
    step_samples_from_buckets(&estimator_buckets(summary, expected))
}

/// Recompute per-step speedup curves from a summary's reservoirs; `core_budget` defaults to the
/// affinity width parsed from the summary's `container_class`. Mirrors `step_speedups_from_summary`.
pub fn step_speedups_from_summary(
    summary: &Summary,
    core_budget: Option<i64>,
) -> HashMap<String, StepSpeedup> {
    let budget = core_budget.or_else(|| affinity_width(&summary.container_class));
    step_speedups_from_buckets(&estimator_buckets(summary, &HashMap::new()), budget)
}

/// Recompute speedup curves for only the expected workload cohorts.
pub(crate) fn step_speedups_from_summary_for_workloads(
    summary: &Summary,
    core_budget: Option<i64>,
    expected: &HashMap<String, String>,
) -> HashMap<String, StepSpeedup> {
    let budget = core_budget.or_else(|| affinity_width(&summary.container_class));
    step_speedups_from_buckets(&estimator_buckets(summary, expected), budget)
}

/// Return `(bucket_count, total_samples, largest_bucket_samples)`.
pub fn summary_stats(summary: &Summary) -> (usize, usize, usize) {
    let bucket_count = summary.buckets.len();
    let total: usize = summary.buckets.values().map(|v| v.len()).sum();
    let largest: usize = summary.buckets.values().map(|v| v.len()).max().unwrap_or(0);
    (bucket_count, total, largest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimates::{bucketize_rows, step_samples_from_buckets};
    use std::collections::HashMap;

    const MID: &str = "m";
    const CC: &str = "affinity8_cpu-max-max";

    fn row(
        step: &str,
        inner: i64,
        elapsed: f64,
        peak: i64,
        pct_other: f64,
    ) -> HashMap<String, String> {
        let mut r = HashMap::new();
        r.insert("step".to_string(), step.to_string());
        r.insert("inner_jobs".to_string(), inner.to_string());
        r.insert("elapsed_s".to_string(), format!("{elapsed:.3}"));
        r.insert("peak_bytes".to_string(), peak.to_string());
        r.insert("pct_other".to_string(), format!("{pct_other:.3}"));
        r
    }

    fn build(rows: &[HashMap<String, String>], cap: usize) -> Summary {
        summary_from_rows(rows, MID, CC, Some(8), cap, DEFAULT_MAX_BUCKETS)
    }

    #[test]
    fn serialization_roundtrip_is_stable() {
        let rows = vec![
            row("g.a", 1, 8.0, 1000, 0.0),
            row("g.a", 1, 20.0, 1000, 60.0),
        ];
        let s = build(&rows, 64);
        let js = to_json(&s);
        let reparsed = from_json(&js).expect("parse");
        assert_eq!(to_json(&reparsed), js);
        assert!(js.contains("\"elapsed_s\": \"20.000\""));
        assert!(js.contains("\"contention\": \"0.600\""));
        assert!(js.contains("\"version\": 2"));
        assert!(js.contains("\"workload_digest\": \"\""));
    }

    #[test]
    fn workload_revisions_have_independent_reservoirs_before_selection() {
        let mut old = row("g.a", 1, 100.0, 1000, 0.0);
        old.insert("workload_digest".to_string(), "old".to_string());
        old.insert("observation_id".to_string(), "old-1".to_string());
        let mut current = row("g.a", 1, 1.0, 2000, 0.0);
        current.insert("workload_digest".to_string(), "current".to_string());
        current.insert("observation_id".to_string(), "current-1".to_string());

        let summary = build(&[old, current], 1);
        assert_eq!(summary.buckets.len(), 2);
        assert_eq!(summary_stats(&summary), (2, 2, 1));

        let selected = step_samples_from_summary_for_workloads(
            &summary,
            &HashMap::from([("g.a".to_string(), "current".to_string())]),
        );
        assert_eq!(selected["g.a"].samples, 1);
        assert_eq!(selected["g.a"].est_duration_s, Some(1.0));
        assert_eq!(selected["g.a"].rss_estimate_bytes, Some(2000));
    }

    #[test]
    fn version_one_summary_upgrades_to_blank_workload_and_legacy_exact_peak() {
        let text = r#"{
  "version": 1,
  "machine_id": "m",
  "container_class": "affinity8_cpu-max-max",
  "buckets": [
    {
      "step": "g.a",
      "inner_jobs": 1,
      "samples": [
        {"elapsed_s": "1.000", "contention": "0.000", "peak_bytes": 123}
      ]
    }
  ]
}"#;
        let summary = from_json(text).expect("v1 remains readable");
        assert_eq!(summary.version, SUMMARY_VERSION);
        let samples = &summary.buckets[&("g.a".to_string(), 1, String::new())];
        assert_eq!(samples[0].workload_digest, "");
        assert_eq!(samples[0].uncensored_peak_bytes, Some(123));
        assert!(to_json(&summary).contains("\"version\": 2"));
    }

    #[test]
    fn version_two_rejects_sample_and_bucket_workload_mismatch() {
        let text = r#"{
  "version": 2,
  "machine_id": "m",
  "container_class": "affinity8_cpu-max-max",
  "buckets": [
    {
      "step": "g.a",
      "inner_jobs": 1,
      "workload_digest": "current",
      "samples": [
        {"workload_digest": "old", "contention": "0.000"}
      ]
    }
  ]
}"#;
        assert!(from_json(text)
            .unwrap_err()
            .0
            .contains("sample workload_digest does not match its bucket"));
    }

    #[test]
    fn estimates_from_summary_match_raw_union() {
        let rows = vec![
            row("g.a", 1, 8.0, 6_000_000_000, 0.0),
            row("g.a", 1, 20.0, 6_000_000_000, 60.0),
            row("g.b", 2, 5.0, 1_000_000_000, 0.0),
            row("g.b", 2, 5.2, 1_000_000_000, 0.0),
            row("g.b", 2, 5.1, 1_000_000_000, 0.0),
        ];
        let raw = step_samples_from_buckets(&bucketize_rows(&rows, Some(8)));
        let s = build(&rows, 64);
        let got = step_samples_from_summary(&s);
        for tag in ["g.a", "g.b"] {
            assert_eq!(got[tag].est_duration_s, raw[tag].est_duration_s);
            assert_eq!(got[tag].rss_estimate_bytes, raw[tag].rss_estimate_bytes);
            assert_eq!(got[tag].samples, raw[tag].samples);
        }
    }

    #[test]
    fn merge_is_commutative_and_associative() {
        let a = build(
            &[row("g.a", 1, 1.0, 100, 0.0), row("g.a", 1, 2.0, 200, 0.0)],
            64,
        );
        let b = build(
            &[row("g.a", 1, 3.0, 300, 0.0), row("g.b", 1, 4.0, 400, 0.0)],
            64,
        );
        let c = build(
            &[row("g.b", 1, 5.0, 500, 0.0), row("g.c", 1, 6.0, 600, 0.0)],
            64,
        );
        let ab_c = merge(&merge(&a, &b, 64, 4096).unwrap(), &c, 64, 4096).unwrap();
        let a_bc = merge(&a, &merge(&b, &c, 64, 4096).unwrap(), 64, 4096).unwrap();
        let ca_b = merge(&merge(&c, &a, 64, 4096).unwrap(), &b, 64, 4096).unwrap();
        assert_eq!(to_json(&ab_c), to_json(&a_bc));
        assert_eq!(to_json(&ab_c), to_json(&ca_b));
    }

    #[test]
    fn merge_with_empty_is_identity() {
        let a = build(&[row("g.a", 1, 1.0, 100, 0.0)], 64);
        let e = empty(MID, CC);
        assert_eq!(to_json(&merge(&a, &e, 64, 4096).unwrap()), to_json(&a));
        assert_eq!(to_json(&merge(&e, &a, 64, 4096).unwrap()), to_json(&a));
    }

    #[test]
    fn merge_rejects_identity_mismatch() {
        let a = build(&[row("g.a", 1, 1.0, 100, 0.0)], 64);
        let b = summary_from_rows(
            &[row("g.a", 1, 1.0, 100, 0.0)],
            "other",
            CC,
            Some(8),
            64,
            4096,
        );
        assert!(merge(&a, &b, 64, 4096).is_err());
    }

    #[test]
    fn merge_is_bounded_across_many_runs() {
        let k = 8;
        let mut acc = empty(MID, CC);
        for run in 0..200i64 {
            let delta = build(
                &[
                    row("g.a", 1, run as f64 + 0.5, 1000 + run, 0.0),
                    row("g.b", 2, 3.0, 2000, 0.0),
                ],
                k,
            );
            acc = merge(&acc, &delta, k, 4096).unwrap();
        }
        let (buckets, total, largest) = summary_stats(&acc);
        assert_eq!(buckets, 2);
        assert!(largest <= k);
        assert!(total <= buckets * k);
    }

    #[test]
    fn reservoir_does_not_badly_bias_median_on_skewed_set() {
        let mut rows = Vec::new();
        for _ in 0..90 {
            rows.push(row("g.a", 1, 5.0, 1000, 0.0));
        }
        for _ in 0..30 {
            rows.push(row("g.a", 1, 50.0, 1000, 0.0));
        }
        let raw = step_samples_from_buckets(&bucketize_rows(&rows, Some(8)));
        let s = build(&rows, 64);
        let got = step_samples_from_summary(&s);
        assert_eq!(got["g.a"].samples, 64);
        let est = got["g.a"].est_duration_s.unwrap();
        let raw_est = raw["g.a"].est_duration_s.unwrap();
        assert!((4.5..=5.5).contains(&est), "est {est} out of range");
        assert!((est - raw_est).abs() <= 0.5);
    }

    #[test]
    fn reservoir_ranks_observations_not_distinct_values() {
        let mut rows = Vec::new();
        for _ in 0..936 {
            rows.push(row("g.a", 1, 5.0, 1000, 0.0));
        }
        for _ in 0..64 {
            rows.push(row("g.a", 1, 50.0, 1000, 0.0));
        }
        let raw = step_samples_from_buckets(&bucketize_rows(&rows, Some(8)));
        let summary = build(&rows, 64);
        let got = step_samples_from_summary(&summary);
        assert_eq!(raw["g.a"].est_duration_s, Some(5.0));
        assert_eq!(got["g.a"].est_duration_s, Some(5.0));
    }

    #[test]
    fn from_json_rejects_unknown_version_and_non_finite() {
        assert!(from_json(
            "{\"version\": 999, \"machine_id\": \"m\", \"container_class\": \"c\", \"buckets\": []}"
        )
        .is_err());
        // A quoted non-finite float string must be rejected.
        assert!(from_json(
            "{\"version\": 1, \"machine_id\": \"m\", \"container_class\": \"c\", \"buckets\": \
             [{\"step\": \"s\", \"inner_jobs\": 1, \"samples\": [{\"contention\": \"0.0\", \
             \"elapsed_s\": \"inf\"}]}]}"
        )
        .is_err());
        assert!(from_json("not json").is_err());
    }
}
