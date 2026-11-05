//! Native, standalone HTML reports for accumulated step-profiling data.
//!
//! A report embeds the authored DAG, every successful aggregate record from
//! `step_profiles_*.csv`, and every interval trace from `traces/*.csv`.  The browser performs
//! filtering and normalization, so samples from distinct commits, machine/container shapes, and
//! workload revisions are never compared against an unrelated baseline.  The generated document
//! has no network, CDN, font, or JavaScript-package dependency.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::io::dag_from_path;
use crate::model::DagConfig;
use crate::perflog::{parse_csv_records, ProfileFileLock};
use crate::sweep::stable_topological_order;

const PROFILE_PREFIX: &str = "step_profiles_";
const PROFILE_SUFFIX: &str = ".csv";
/// Canonical rebuildable report sidecar in a profile store.
pub const PROFILE_REPORT_FILENAME: &str = "profile_report.html";
const REPORT_DATA_MARKER: &[u8] = br#"<script id="dagrun-report-data" type="application/json">"#;

/// A malformed or unreadable report input.
#[derive(Debug)]
pub struct ProfileReportError(String);

impl ProfileReportError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProfileReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProfileReportError {}

/// Counts and notices from a generated report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileReportSummary {
    /// Successful aggregate trials embedded in the page.
    pub aggregate_samples: usize,
    /// Individual step executions with interval samples embedded in the page.
    pub trace_series: usize,
    /// Non-fatal exclusions and absent-input notices shown in the page.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct AggregateRecord {
    step: String,
    commit: String,
    timestamp: String,
    jobs: i64,
    elapsed_s: f64,
    cpu_s: Option<f64>,
    peak_bytes: Option<i64>,
    effective_cores: Option<f64>,
    run_id: String,
    machine: String,
    container: String,
    workload: String,
    runner: String,
    source: String,
}

impl AggregateRecord {
    fn environment(&self) -> String {
        environment_key(&self.machine, &self.container)
    }

    fn to_json(&self) -> Value {
        json!({
            "step": self.step,
            "commit": self.commit,
            "timestamp": self.timestamp,
            "jobs": self.jobs,
            "elapsed_s": self.elapsed_s,
            "cpu_s": self.cpu_s,
            "peak_bytes": self.peak_bytes,
            "effective_cores": self.effective_cores,
            "run_id": self.run_id,
            "machine": self.machine,
            "container": self.container,
            "environment": self.environment(),
            "workload": self.workload,
            "runner": self.runner,
            "source": self.source,
        })
    }
}

#[derive(Debug, Clone)]
struct TracePoint {
    sample_index: i64,
    sample_kind: String,
    elapsed_s: f64,
    interval_s: Option<f64>,
    effective_cores: Option<f64>,
    user_cores: Option<f64>,
    system_cores: Option<f64>,
    thread_count: Option<i64>,
    throttled_s: Option<f64>,
}

impl TracePoint {
    fn to_json(&self) -> Value {
        json!({
            "sample_index": self.sample_index,
            "sample_kind": self.sample_kind,
            "elapsed_s": self.elapsed_s,
            "interval_s": self.interval_s,
            "effective_cores": self.effective_cores,
            "user_cores": self.user_cores,
            "system_cores": self.system_cores,
            "thread_count": self.thread_count,
            "throttled_s": self.throttled_s,
        })
    }
}

#[derive(Debug, Clone)]
struct TraceSeries {
    key: String,
    step: String,
    commit: String,
    timestamp: String,
    jobs: i64,
    run_id: String,
    machine: String,
    container: String,
    workload: String,
    source: String,
    points: Vec<TracePoint>,
}

impl TraceSeries {
    fn environment(&self) -> String {
        environment_key(&self.machine, &self.container)
    }

    fn to_json(&self) -> Value {
        json!({
            "key": self.key,
            "step": self.step,
            "commit": self.commit,
            "timestamp": self.timestamp,
            "jobs": self.jobs,
            "run_id": self.run_id,
            "machine": self.machine,
            "container": self.container,
            "environment": self.environment(),
            "workload": self.workload,
            "source": self.source,
            "points": self.points.iter().map(TracePoint::to_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone)]
struct CaptureArtifactView {
    role: String,
    path: String,
    href: String,
    size_bytes: i64,
    mode: String,
    exists: bool,
}

impl CaptureArtifactView {
    fn to_json(&self) -> Value {
        json!({
            "role": self.role,
            "path": self.path,
            "href": self.href,
            "size_bytes": self.size_bytes,
            "mode": self.mode,
            "exists": self.exists,
        })
    }
}

#[derive(Debug, Clone)]
struct CaptureTrialView {
    trial_id: String,
    kind: String,
    state: String,
    inner_jobs: i64,
    started_at: String,
    finished_at: String,
    measured_wall_s: Option<f64>,
    workload_returncode: Option<i64>,
    profiler_returncode: Option<i64>,
    error: String,
    included_in_model: bool,
    artifacts: Vec<CaptureArtifactView>,
}

impl CaptureTrialView {
    fn to_json(&self) -> Value {
        json!({
            "trial_id": self.trial_id,
            "kind": self.kind,
            "state": self.state,
            "inner_jobs": self.inner_jobs,
            "started_at": self.started_at,
            "finished_at": self.finished_at,
            "measured_wall_s": self.measured_wall_s,
            "workload_returncode": self.workload_returncode,
            "profiler_returncode": self.profiler_returncode,
            "error": self.error,
            "included_in_model": self.included_in_model,
            "artifacts": self.artifacts.iter().map(CaptureArtifactView::to_json).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone)]
struct CaptureView {
    capture_id: String,
    state: String,
    created_at: String,
    finished_at: String,
    step: String,
    commit: String,
    machine: String,
    container: String,
    environment: String,
    workload: String,
    jobs: i64,
    expected_wall_s: Option<f64>,
    speedup: Option<f64>,
    manifest_path: String,
    errors: Vec<String>,
    trials: Vec<CaptureTrialView>,
}

impl CaptureView {
    fn to_json(&self) -> Value {
        json!({
            "capture_id": self.capture_id,
            "state": self.state,
            "created_at": self.created_at,
            "finished_at": self.finished_at,
            "step": self.step,
            "commit": self.commit,
            "machine": self.machine,
            "container": self.container,
            "environment": self.environment,
            "workload": self.workload,
            "jobs": self.jobs,
            "expected_wall_s": self.expected_wall_s,
            "speedup": self.speedup,
            "manifest_path": self.manifest_path,
            "errors": self.errors,
            "trials": self.trials.iter().map(CaptureTrialView::to_json).collect::<Vec<_>>(),
        })
    }
}

type CsvRow = BTreeMap<String, String>;

fn cell<'a>(row: &'a CsvRow, name: &str) -> &'a str {
    row.get(name).map(String::as_str).unwrap_or("").trim()
}

fn nonnegative_float(value: &str) -> Option<f64> {
    let parsed = value.trim().parse::<f64>().ok()?;
    (parsed.is_finite() && parsed >= 0.0).then_some(parsed)
}

fn integer_at_least(value: &str, minimum: i64) -> Option<i64> {
    let parsed = value.trim().parse::<i64>().ok()?;
    (parsed >= minimum).then_some(parsed)
}

fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes"
    )
}

fn failed_measurement(row: &CsvRow) -> bool {
    let ok = cell(row, "ok").to_ascii_lowercase();
    if !ok.is_empty() && !matches!(ok.as_str(), "true" | "1" | "yes") {
        return true;
    }
    if cell(row, "returncode")
        .parse::<i64>()
        .is_ok_and(|returncode| returncode != 0)
    {
        return true;
    }
    if truthy(cell(row, "timed_out")) || truthy(cell(row, "cpu_timed_out")) {
        return true;
    }
    cell(row, "oom_kills")
        .parse::<i64>()
        .is_ok_and(|count| count > 0)
}

fn environment_key(machine: &str, container: &str) -> String {
    format!("{machine}\u{241f}{container}")
}

fn environment_label(machine: &str, container: &str) -> String {
    format!(
        "{} / {}",
        if machine.is_empty() {
            "unknown machine"
        } else {
            machine
        },
        if container.is_empty() {
            "unknown container"
        } else {
            container
        }
    )
}

fn relative_source(path: &Path, profile_dir: &Path) -> String {
    path.strip_prefix(profile_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn sorted_files(
    directory: &Path,
    predicate: impl Fn(&str) -> bool,
) -> Result<Vec<PathBuf>, ProfileReportError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        ProfileReportError::new(format!(
            "cannot read profile directory {}: {error}",
            directory.display()
        ))
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            ProfileReportError::new(format!("cannot enumerate {}: {error}", directory.display()))
        })?;
        let path = entry.path();
        if path.is_file() {
            let name = entry.file_name();
            if predicate(&name.to_string_lossy()) {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_csv(path: &Path, required: &[&str]) -> Result<Vec<CsvRow>, ProfileReportError> {
    let text = fs::read_to_string(path).map_err(|error| {
        ProfileReportError::new(format!(
            "cannot read profile CSV {}: {error}",
            path.display()
        ))
    })?;
    let records = parse_csv_records(&text);
    let Some(header) = records.first().cloned() else {
        return Err(ProfileReportError::new(format!(
            "{}: profile CSV has no header",
            path.display()
        )));
    };
    let fields: HashSet<&str> = header.iter().map(String::as_str).collect();
    let mut missing: Vec<&str> = required
        .iter()
        .copied()
        .filter(|field| !fields.contains(field))
        .collect();
    missing.sort_unstable();
    if !missing.is_empty() {
        return Err(ProfileReportError::new(format!(
            "{}: profile CSV is missing columns: {}",
            path.display(),
            missing.join(", ")
        )));
    }
    Ok(records
        .into_iter()
        .skip(1)
        .map(|values| {
            header
                .iter()
                .enumerate()
                .map(|(index, name)| (name.clone(), values.get(index).cloned().unwrap_or_default()))
                .collect()
        })
        .collect())
}

fn load_aggregates(
    profile_dir: &Path,
    known_steps: &HashSet<String>,
) -> Result<(Vec<AggregateRecord>, Vec<String>), ProfileReportError> {
    let paths = sorted_files(profile_dir, |name| {
        name.starts_with(PROFILE_PREFIX) && name.ends_with(PROFILE_SUFFIX)
    })?;
    let mut warnings = Vec::new();
    if paths.is_empty() {
        warnings.push("No step_profiles_*.csv files were found in the profile store.".to_string());
    }
    let mut records = Vec::new();
    let mut skipped_failed = 0usize;
    let mut skipped_invalid = 0usize;
    let mut skipped_unknown = 0usize;
    for path in paths {
        for row in read_csv(&path, &["step", "inner_jobs", "elapsed_s"])? {
            let step = cell(&row, "step").to_string();
            if !known_steps.contains(&step) {
                skipped_unknown += 1;
                continue;
            }
            if failed_measurement(&row) {
                skipped_failed += 1;
                continue;
            }
            let Some(jobs) = integer_at_least(cell(&row, "inner_jobs"), 1) else {
                skipped_invalid += 1;
                continue;
            };
            let Some(elapsed_s) =
                nonnegative_float(cell(&row, "elapsed_s")).filter(|elapsed| *elapsed > 0.0)
            else {
                skipped_invalid += 1;
                continue;
            };
            let user_s = nonnegative_float(cell(&row, "user_s"));
            let sys_s = nonnegative_float(cell(&row, "sys_s"));
            let effective_cores = nonnegative_float(cell(&row, "effective_cores"));
            let cpu_s = if user_s.is_some() || sys_s.is_some() {
                Some(user_s.unwrap_or(0.0) + sys_s.unwrap_or(0.0))
            } else {
                effective_cores.map(|cores| cores * elapsed_s)
            };
            records.push(AggregateRecord {
                step,
                commit: nonempty_or(cell(&row, "git_sha"), "(unknown)"),
                timestamp: cell(&row, "timestamp").to_string(),
                jobs,
                elapsed_s,
                cpu_s,
                peak_bytes: integer_at_least(cell(&row, "peak_bytes"), 0),
                effective_cores,
                run_id: cell(&row, "run_id").to_string(),
                machine: cell(&row, "machine_id").to_string(),
                container: cell(&row, "container_class").to_string(),
                workload: cell(&row, "workload_digest").to_string(),
                runner: cell(&row, "runner_name").to_string(),
                source: relative_source(&path, profile_dir),
            });
        }
    }
    if skipped_failed > 0 {
        warnings.push(format!(
            "Excluded {skipped_failed} failed or interrupted aggregate sample(s)."
        ));
    }
    if skipped_invalid > 0 {
        warnings.push(format!(
            "Excluded {skipped_invalid} aggregate row(s) without a positive width and wall time."
        ));
    }
    if skipped_unknown > 0 {
        warnings.push(format!(
            "Excluded {skipped_unknown} aggregate row(s) for steps absent from the supplied DAG."
        ));
    }
    records.sort_by(|left, right| {
        (
            &left.step,
            &left.timestamp,
            &left.commit,
            left.environment(),
            &left.workload,
            left.jobs,
            &left.run_id,
            &left.source,
        )
            .cmp(&(
                &right.step,
                &right.timestamp,
                &right.commit,
                right.environment(),
                &right.workload,
                right.jobs,
                &right.run_id,
                &right.source,
            ))
    });
    Ok((records, warnings))
}

fn nonempty_or(value: &str, fallback: &str) -> String {
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TraceGroupKey {
    run_id: String,
    step: String,
    jobs: i64,
    commit: String,
    machine: String,
    container: String,
    workload: String,
}

fn load_traces(
    profile_dir: &Path,
    known_steps: &HashSet<String>,
) -> Result<Vec<TraceSeries>, ProfileReportError> {
    let trace_dir = profile_dir.join("traces");
    if !trace_dir.is_dir() {
        return Ok(Vec::new());
    }
    let paths = sorted_files(&trace_dir, |name| name.ends_with(PROFILE_SUFFIX))?;
    let mut grouped: BTreeMap<TraceGroupKey, Vec<(TracePoint, String, String)>> = BTreeMap::new();
    for path in paths {
        for row in read_csv(
            &path,
            &["run_id", "step", "inner_jobs", "sample_index", "elapsed_s"],
        )? {
            let step = cell(&row, "step").to_string();
            if !known_steps.contains(&step) {
                continue;
            }
            let Some(jobs) = integer_at_least(cell(&row, "inner_jobs"), 1) else {
                continue;
            };
            let Some(sample_index) = integer_at_least(cell(&row, "sample_index"), 0) else {
                continue;
            };
            let Some(elapsed_s) = nonnegative_float(cell(&row, "elapsed_s")) else {
                continue;
            };
            let run_id = nonempty_or(
                cell(&row, "run_id"),
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("trace"),
            );
            let key = TraceGroupKey {
                run_id,
                step,
                jobs,
                commit: nonempty_or(cell(&row, "git_sha"), "(unknown)"),
                machine: cell(&row, "machine_id").to_string(),
                container: cell(&row, "container_class").to_string(),
                workload: cell(&row, "workload_digest").to_string(),
            };
            let point = TracePoint {
                sample_index,
                sample_kind: nonempty_or(cell(&row, "sample_kind"), "periodic"),
                elapsed_s,
                interval_s: nonnegative_float(cell(&row, "interval_s")),
                effective_cores: nonnegative_float(cell(&row, "effective_cores")),
                user_cores: nonnegative_float(cell(&row, "user_cores")),
                system_cores: nonnegative_float(cell(&row, "system_cores")),
                thread_count: integer_at_least(cell(&row, "thread_count"), 0),
                throttled_s: nonnegative_float(cell(&row, "throttled_s")),
            };
            grouped.entry(key).or_default().push((
                point,
                cell(&row, "timestamp").to_string(),
                relative_source(&path, profile_dir),
            ));
        }
    }

    let mut result = Vec::new();
    for (key, mut rows) in grouped {
        rows.sort_by(|left, right| {
            left.0
                .sample_index
                .cmp(&right.0.sample_index)
                .then_with(|| left.0.elapsed_s.total_cmp(&right.0.elapsed_s))
        });
        let timestamp = rows
            .iter()
            .map(|(_, timestamp, _)| timestamp.as_str())
            .max()
            .unwrap_or("")
            .to_string();
        let source = rows.first().map(|row| row.2.clone()).unwrap_or_default();
        let series_key = [
            key.run_id.as_str(),
            key.step.as_str(),
            &key.jobs.to_string(),
            key.commit.as_str(),
            key.machine.as_str(),
            key.container.as_str(),
            key.workload.as_str(),
        ]
        .join("\u{241f}");
        result.push(TraceSeries {
            key: series_key,
            step: key.step,
            commit: key.commit,
            timestamp,
            jobs: key.jobs,
            run_id: key.run_id,
            machine: key.machine,
            container: key.container,
            workload: key.workload,
            source,
            points: rows.into_iter().map(|(point, _, _)| point).collect(),
        });
    }
    result.sort_by(|left, right| {
        (
            &left.step,
            &left.timestamp,
            &left.commit,
            left.jobs,
            &left.run_id,
            &left.source,
        )
            .cmp(&(
                &right.step,
                &right.timestamp,
                &right.commit,
                right.jobs,
                &right.run_id,
                &right.source,
            ))
    });
    Ok(result)
}

const CAPTURE_SCHEMA: &str = "dagrun-profile-capture-v1";

fn capture_object<'a>(
    value: &'a Value,
    where_: &str,
) -> Result<&'a serde_json::Map<String, Value>, ProfileReportError> {
    value
        .as_object()
        .ok_or_else(|| ProfileReportError::new(format!("{where_} must be an object")))
}

fn capture_string(
    value: Option<&Value>,
    where_: &str,
    allow_empty: bool,
) -> Result<String, ProfileReportError> {
    let value = value.and_then(Value::as_str).ok_or_else(|| {
        let qualifier = if allow_empty { "" } else { "non-empty " };
        ProfileReportError::new(format!("{where_} must be a {qualifier}string"))
    })?;
    if !allow_empty && value.is_empty() {
        return Err(ProfileReportError::new(format!(
            "{where_} must be a non-empty string"
        )));
    }
    Ok(value.to_string())
}

fn capture_int(
    value: Option<&Value>,
    where_: &str,
    minimum: Option<i64>,
) -> Result<i64, ProfileReportError> {
    let value = value
        .and_then(Value::as_i64)
        .ok_or_else(|| ProfileReportError::new(format!("{where_} must be an integer")))?;
    if minimum.is_some_and(|minimum| value < minimum) {
        return Err(ProfileReportError::new(format!(
            "{where_} must be >= {}",
            minimum.unwrap_or_default()
        )));
    }
    Ok(value)
}

fn capture_optional_int(
    value: Option<&Value>,
    where_: &str,
) -> Result<Option<i64>, ProfileReportError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => capture_int(value, where_, None).map(Some),
    }
}

fn capture_optional_float(
    value: Option<&Value>,
    where_: &str,
) -> Result<Option<f64>, ProfileReportError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let parsed = value.as_f64().ok_or_else(|| {
                ProfileReportError::new(format!("{where_} must be a number or null"))
            })?;
            if !parsed.is_finite() || parsed < 0.0 {
                return Err(ProfileReportError::new(format!(
                    "{where_} must be finite and >= 0"
                )));
            }
            Ok(Some(parsed))
        }
    }
}

fn capture_strings(value: Option<&Value>, where_: &str) -> Result<Vec<String>, ProfileReportError> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileReportError::new(format!("{where_} must be a list of strings")))?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| capture_string(Some(value), &format!("{where_}[{index}]"), true))
        .collect()
}

fn capture_state(value: Option<&Value>, where_: &str) -> Result<String, ProfileReportError> {
    let state = capture_string(value, where_, false)?;
    if !matches!(
        state.as_str(),
        "running" | "complete" | "failed" | "skipped"
    ) {
        return Err(ProfileReportError::new(format!(
            "{where_} has unknown state '{state}'"
        )));
    }
    Ok(state)
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, ProfileReportError> {
    let source = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                ProfileReportError::new(format!("cannot resolve report paths: {error}"))
            })?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in source.components() {
        match component {
            std::path::Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            std::path::Component::RootDir => normalized.push(Path::new("/")),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

fn relative_link(target: &Path, base: &Path) -> Result<PathBuf, ProfileReportError> {
    let target = lexical_absolute(target)?;
    let base = lexical_absolute(base)?;
    let target_parts: Vec<_> = target.components().collect();
    let base_parts: Vec<_> = base.components().collect();
    let common = target_parts
        .iter()
        .zip(&base_parts)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for _ in common..base_parts.len() {
        relative.push("..");
    }
    for component in &target_parts[common..] {
        relative.push(component.as_os_str());
    }
    if relative.as_os_str().is_empty() {
        relative.push(".");
    }
    Ok(relative)
}

fn url_quote_path(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let normalized = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(normalized.len());
    for byte in normalized.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/@:-._~".contains(byte) {
            encoded.push(char::from(*byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn capture_artifact_view(
    raw: &Value,
    where_: &str,
    manifest_path: &Path,
    profile_dir: &Path,
    report_path: Option<&Path>,
) -> Result<CaptureArtifactView, ProfileReportError> {
    let artifact = capture_object(raw, where_)?;
    let raw_path = capture_string(artifact.get("path"), &format!("{where_}.path"), false)?;
    let relative = Path::new(&raw_path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(ProfileReportError::new(format!(
            "{where_}.path must stay within its capture directory"
        )));
    }
    let artifact_path = manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(relative);
    let link_base = report_path
        .and_then(Path::parent)
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(profile_dir);
    let href = url_quote_path(&relative_link(&artifact_path, link_base)?);
    Ok(CaptureArtifactView {
        role: capture_string(artifact.get("role"), &format!("{where_}.role"), false)?,
        path: relative_source(&artifact_path, profile_dir),
        href,
        size_bytes: capture_int(
            artifact.get("size_bytes"),
            &format!("{where_}.size_bytes"),
            Some(0),
        )?,
        mode: match artifact.get("mode") {
            Some(value) => capture_string(Some(value), &format!("{where_}.mode"), true)?,
            None => String::new(),
        },
        exists: artifact_path.is_file(),
    })
}

fn capture_trial_view(
    raw: &Value,
    where_: &str,
    manifest_path: &Path,
    profile_dir: &Path,
    report_path: Option<&Path>,
) -> Result<CaptureTrialView, ProfileReportError> {
    let trial = capture_object(raw, where_)?;
    let kind = capture_string(trial.get("kind"), &format!("{where_}.kind"), false)?;
    if !matches!(kind.as_str(), "perf" | "wprof") {
        return Err(ProfileReportError::new(format!(
            "{where_}.kind has unknown profiler '{kind}'"
        )));
    }
    let artifacts = trial
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileReportError::new(format!("{where_}.artifacts must be a list")))?
        .iter()
        .enumerate()
        .map(|(index, artifact)| {
            capture_artifact_view(
                artifact,
                &format!("{where_}.artifacts[{index}]"),
                manifest_path,
                profile_dir,
                report_path,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let included_in_model = trial
        .get("included_in_model")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            ProfileReportError::new(format!("{where_}.included_in_model must be a boolean"))
        })?;
    Ok(CaptureTrialView {
        trial_id: capture_string(trial.get("trial_id"), &format!("{where_}.trial_id"), false)?,
        kind,
        state: capture_state(trial.get("state"), &format!("{where_}.state"))?,
        inner_jobs: capture_int(
            trial.get("inner_jobs"),
            &format!("{where_}.inner_jobs"),
            Some(1),
        )?,
        started_at: match trial.get("started_at") {
            Some(value) => capture_string(Some(value), &format!("{where_}.started_at"), true)?,
            None => String::new(),
        },
        finished_at: match trial.get("finished_at") {
            Some(value) => capture_string(Some(value), &format!("{where_}.finished_at"), true)?,
            None => String::new(),
        },
        measured_wall_s: capture_optional_float(
            trial.get("measured_wall_s"),
            &format!("{where_}.measured_wall_s"),
        )?,
        workload_returncode: capture_optional_int(
            trial.get("workload_returncode"),
            &format!("{where_}.workload_returncode"),
        )?,
        profiler_returncode: capture_optional_int(
            trial.get("profiler_returncode"),
            &format!("{where_}.profiler_returncode"),
        )?,
        error: match trial.get("error") {
            Some(value) => capture_string(Some(value), &format!("{where_}.error"), true)?,
            None => String::new(),
        },
        included_in_model,
        artifacts,
    })
}

fn parse_capture_manifest(
    raw: &Value,
    manifest_path: &Path,
    profile_dir: &Path,
    report_path: Option<&Path>,
) -> Result<CaptureView, ProfileReportError> {
    let where_ = relative_source(manifest_path, profile_dir);
    let manifest = capture_object(raw, &where_)?;
    let schema = capture_string(manifest.get("schema"), &format!("{where_}.schema"), false)?;
    if schema != CAPTURE_SCHEMA {
        return Err(ProfileReportError::new(format!(
            "{where_}.schema must be '{CAPTURE_SCHEMA}', got '{schema}'"
        )));
    }
    let selection = capture_object(
        manifest.get("selection").ok_or_else(|| {
            ProfileReportError::new(format!("{where_}.selection must be an object"))
        })?,
        &format!("{where_}.selection"),
    )?;
    let trials_raw = manifest
        .get("trials")
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileReportError::new(format!("{where_}.trials must be a list")))?;
    let mut trials = trials_raw
        .iter()
        .enumerate()
        .map(|(index, trial)| {
            capture_trial_view(
                trial,
                &format!("{where_}.trials[{index}]"),
                manifest_path,
                profile_dir,
                report_path,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    trials.sort_by(|left, right| {
        (&left.started_at, &left.trial_id, &left.kind).cmp(&(
            &right.started_at,
            &right.trial_id,
            &right.kind,
        ))
    });
    let commit = match selection.get("git_sha") {
        Some(value) => capture_string(Some(value), &format!("{where_}.selection.git_sha"), true)?,
        None => String::new(),
    };
    let machine = match manifest.get("machine_id") {
        Some(value) => capture_string(Some(value), &format!("{where_}.machine_id"), true)?,
        None => String::new(),
    };
    let container = match manifest.get("container_class") {
        Some(value) => capture_string(Some(value), &format!("{where_}.container_class"), true)?,
        None => String::new(),
    };
    let environment = if machine.is_empty() && container.is_empty() {
        String::new()
    } else {
        environment_key(&machine, &container)
    };
    Ok(CaptureView {
        capture_id: capture_string(
            manifest.get("capture_id"),
            &format!("{where_}.capture_id"),
            false,
        )?,
        state: capture_state(manifest.get("state"), &format!("{where_}.state"))?,
        created_at: match manifest.get("created_at") {
            Some(value) => capture_string(Some(value), &format!("{where_}.created_at"), true)?,
            None => String::new(),
        },
        finished_at: match manifest.get("finished_at") {
            Some(value) => capture_string(Some(value), &format!("{where_}.finished_at"), true)?,
            None => String::new(),
        },
        step: capture_string(
            selection.get("step"),
            &format!("{where_}.selection.step"),
            false,
        )?,
        commit: nonempty_or(&commit, "(unknown)"),
        machine,
        container,
        environment,
        workload: match selection.get("workload_digest") {
            Some(value) => capture_string(
                Some(value),
                &format!("{where_}.selection.workload_digest"),
                true,
            )?,
            None => String::new(),
        },
        jobs: capture_int(
            selection.get("inner_jobs"),
            &format!("{where_}.selection.inner_jobs"),
            Some(1),
        )?,
        expected_wall_s: capture_optional_float(
            selection.get("expected_wall_s"),
            &format!("{where_}.selection.expected_wall_s"),
        )?,
        speedup: capture_optional_float(
            selection.get("speedup"),
            &format!("{where_}.selection.speedup"),
        )?,
        manifest_path: where_.clone(),
        errors: match manifest.get("errors") {
            Some(value) => capture_strings(Some(value), &format!("{where_}.errors"))?,
            None => Vec::new(),
        },
        trials,
    })
}

fn load_capture_manifests(
    profile_dir: &Path,
    known_steps: &HashSet<String>,
    report_path: Option<&Path>,
) -> Result<(Vec<CaptureView>, Vec<String>), ProfileReportError> {
    let capture_dir = profile_dir.join("captures");
    if !capture_dir.is_dir() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(&capture_dir).map_err(|error| {
        ProfileReportError::new(format!(
            "cannot read capture directory {}: {error}",
            capture_dir.display()
        ))
    })? {
        let entry = entry.map_err(|error| ProfileReportError::new(error.to_string()))?;
        let manifest_path = entry.path().join("manifest.json");
        if manifest_path.exists() {
            paths.push(manifest_path);
        }
    }
    paths.sort();
    let mut captures = Vec::new();
    let mut warnings = Vec::new();
    for manifest_path in paths {
        let relative = relative_source(&manifest_path, profile_dir);
        let parsed = fs::read_to_string(&manifest_path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())
            })
            .and_then(|raw| {
                parse_capture_manifest(&raw, &manifest_path, profile_dir, report_path)
                    .map_err(|error| error.to_string())
            });
        let capture = match parsed {
            Ok(capture) => capture,
            Err(_) => {
                // Keep report payloads interoperable across implementations. Parser and I/O
                // exception text is runtime-specific and must not leak into serialized data.
                warnings.push(format!("Ignored malformed capture manifest {relative}."));
                continue;
            }
        };
        if !known_steps.contains(&capture.step) {
            warnings.push(format!(
                "Ignored capture manifest {relative} for step '{}', which is absent from the supplied DAG.",
                capture.step
            ));
            continue;
        }
        captures.push(capture);
    }
    captures.sort_by(|left, right| {
        (
            &left.step,
            &left.created_at,
            &left.capture_id,
            &left.manifest_path,
        )
            .cmp(&(
                &right.step,
                &right.created_at,
                &right.capture_id,
                &right.manifest_path,
            ))
    });
    Ok((captures, warnings))
}

fn graph_json(config: &DagConfig) -> Result<Value, ProfileReportError> {
    if config.steps.is_empty() {
        return Err(ProfileReportError::new(
            "the supplied DAG has no steps to display",
        ));
    }
    let order = stable_topological_order(&config.steps).map_err(ProfileReportError::new)?;
    let by_tag: HashMap<String, usize> = config
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| (step.tag(), index))
        .collect();
    let mut layer_by_index = vec![0usize; config.steps.len()];
    for index in &order {
        let step = &config.steps[*index];
        layer_by_index[*index] = step
            .deps
            .iter()
            .filter_map(|tag| by_tag.get(tag).copied())
            .map(|dependency| layer_by_index[dependency] + 1)
            .max()
            .unwrap_or(0);
    }
    let layer_count = layer_by_index.iter().max().copied().unwrap_or(0) + 1;
    let mut members = vec![Vec::<usize>::new(); layer_count];
    for index in &order {
        members[layer_by_index[*index]].push(*index);
    }
    let max_members = members.iter().map(Vec::len).max().unwrap_or(1);
    let width = 760usize.max(220 + layer_count.saturating_sub(1) * 250);
    let height = 320usize.max(130 + max_members * 145);
    let mut position = HashMap::new();
    for (layer, indices) in members.iter().enumerate() {
        let x = if layer_count == 1 {
            width as f64 / 2.0
        } else {
            110.0 + layer as f64 * ((width - 220) as f64 / (layer_count - 1) as f64)
        };
        for (member, index) in indices.iter().enumerate() {
            let y = (member + 1) as f64 * height as f64 / (indices.len() + 1) as f64;
            position.insert(*index, (round3(x), round3(y)));
        }
    }
    let mut steps = Vec::new();
    for (topological_order, index) in order.iter().enumerate() {
        let step = &config.steps[*index];
        let (x, y) = position[index];
        steps.push(json!({
            "tag": step.tag(),
            "desc": step.desc,
            "description": step.description,
            "deps": step.deps,
            "order": topological_order,
            "layer": layer_by_index[*index],
            "x": x,
            "y": y,
        }));
    }
    let edges = order
        .iter()
        .flat_map(|index| {
            let step = &config.steps[*index];
            let target = step.tag();
            step.deps
                .iter()
                .map(move |dependency| json!({"from": dependency, "to": target}))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "description": config.description,
        "width": width,
        "height": height,
        "steps": steps,
        "edges": edges,
    }))
}

fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Load a DAG document through dagrun's strict interchange parser.
pub fn load_report_dag(path: &Path) -> Result<DagConfig, ProfileReportError> {
    dag_from_path(path).map_err(|error| ProfileReportError::new(error.to_string()))
}

/// Build the stable JSON payload embedded in a profile report.
pub fn build_report_data(
    config: &DagConfig,
    dag_path: &Path,
    profile_dir: &Path,
) -> Result<Value, ProfileReportError> {
    build_report_data_for_output(config, dag_path, profile_dir, None)
}

fn build_report_data_for_output(
    config: &DagConfig,
    dag_path: &Path,
    profile_dir: &Path,
    report_path: Option<&Path>,
) -> Result<Value, ProfileReportError> {
    if !profile_dir.is_dir() {
        return Err(ProfileReportError::new(format!(
            "profile store is not a directory: {}",
            profile_dir.display()
        )));
    }
    let known_steps: HashSet<String> = config.steps.iter().map(|step| step.tag()).collect();
    let (records, profile_warnings) = load_aggregates(profile_dir, &known_steps)?;
    let traces = load_traces(profile_dir, &known_steps)?;
    let (captures, capture_warnings) =
        load_capture_manifests(profile_dir, &known_steps, report_path)?;
    let warnings: Vec<String> = profile_warnings
        .into_iter()
        .chain(capture_warnings)
        .collect();
    let mut commit_latest = BTreeMap::<String, String>::new();
    let mut environments = BTreeMap::<String, String>::new();
    let mut workloads = BTreeSet::<String>::new();
    for record in &records {
        commit_latest
            .entry(record.commit.clone())
            .and_modify(|timestamp| *timestamp = timestamp.clone().max(record.timestamp.clone()))
            .or_insert_with(|| record.timestamp.clone());
        environments.insert(
            record.environment(),
            environment_label(&record.machine, &record.container),
        );
        if !record.workload.is_empty() {
            workloads.insert(record.workload.clone());
        }
    }
    for trace in &traces {
        commit_latest
            .entry(trace.commit.clone())
            .and_modify(|timestamp| *timestamp = timestamp.clone().max(trace.timestamp.clone()))
            .or_insert_with(|| trace.timestamp.clone());
        environments.insert(
            trace.environment(),
            environment_label(&trace.machine, &trace.container),
        );
        if !trace.workload.is_empty() {
            workloads.insert(trace.workload.clone());
        }
    }
    for capture in &captures {
        commit_latest
            .entry(capture.commit.clone())
            .and_modify(|timestamp| *timestamp = timestamp.clone().max(capture.created_at.clone()))
            .or_insert_with(|| capture.created_at.clone());
        if !capture.environment.is_empty() {
            environments.insert(
                capture.environment.clone(),
                environment_label(&capture.machine, &capture.container),
            );
        }
        if !capture.workload.is_empty() {
            workloads.insert(capture.workload.clone());
        }
    }
    let mut commits: Vec<(String, String)> = commit_latest.into_iter().collect();
    commits.sort_by(|left, right| (&left.1, &left.0).cmp(&(&right.1, &right.0)));
    let mut environments: Vec<(String, String)> = environments.into_iter().collect();
    environments.sort_by(|left, right| (&left.1, &left.0).cmp(&(&right.1, &right.0)));

    Ok(json!({
        "schema": 1,
        "profile_dir": profile_dir.to_string_lossy(),
        "dag_path": dag_path.to_string_lossy(),
        "graph": graph_json(config)?,
        "commits": commits.into_iter().map(|(sha, timestamp)| json!({"sha": sha, "timestamp": timestamp})).collect::<Vec<_>>(),
        "environments": environments.into_iter().map(|(key, label)| json!({"key": key, "label": label})).collect::<Vec<_>>(),
        "workloads": workloads,
        "records": records.iter().map(AggregateRecord::to_json).collect::<Vec<_>>(),
        "traces": traces.iter().map(TraceSeries::to_json).collect::<Vec<_>>(),
        "captures": captures.iter().map(CaptureView::to_json).collect::<Vec<_>>(),
        "warnings": warnings,
    }))
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn script_safe_json(value: &Value) -> Result<String, ProfileReportError> {
    serde_json::to_string(value)
        .map(|text| {
            text.replace('&', "\\u0026")
                .replace('<', "\\u003c")
                .replace('>', "\\u003e")
        })
        .map_err(|error| ProfileReportError::new(format!("cannot encode report data: {error}")))
}

/// Render an already-loaded payload as a standalone interactive HTML document.
pub fn render_report(data: &Value, title: &str) -> Result<String, ProfileReportError> {
    let warnings = data
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|warning| format!("<li>{}</li>", html_escape(warning)))
        .collect::<String>();
    let warning_html = if warnings.is_empty() {
        String::new()
    } else {
        format!("<ul class=\"warnings\" aria-label=\"Input notices\">{warnings}</ul>")
    };
    let profile_dir = html_escape(
        data.get("profile_dir")
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    let dag_path = html_escape(data.get("dag_path").and_then(Value::as_str).unwrap_or(""));
    Ok(format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title><style>{STYLE}</style></head><body>
<header><h1>{title}</h1><p>Explore scaling behavior across profiling generations. Points are successful trials; fitted lines are per-width medians recomputed from the active filters.</p><code>DAG: {dag_path}<br>Profile store: {profile_dir}</code></header>
<main>{warning_html}
<section class="panel"><h2>Historical window</h2><p>All controls update the DAG and fitted curves together.</p><div class="controls"><label>Commit history<select id="commit-limit"></select></label><label>Machine / container<select id="environment-filter"></select></label><label>Workload revision<select id="workload-filter"></select></label></div></section>
<section class="panel"><h2>DAG CPU-work map</h2><p>Click a node to drill down. Circle area is proportional to median CPU-seconds in the current view.</p><div class="dag-wrap"><svg id="dag-svg"></svg></div></section>
<section class="panel"><div class="step-head"><div><h2 id="step-title"></h2><p id="step-description"></p></div><label>Step<select id="step-select"></select></label></div>
<div class="cards"><div><small>Samples</small><strong id="card-samples">—</strong></div><div><small>Commits</small><strong id="card-commits">—</strong></div><div><small>Best median speedup</small><strong id="card-best">—</strong></div><div><small>Economic sweet spot</small><strong id="card-sweet">—</strong></div><div><small>RSS at sweet spot</small><strong id="card-memory">—</strong></div></div>
<div class="capture-section"><h3>Profiler captures</h3><p>Perf and wprof follow-up trials taken at a selected scaling width. Artifact paths are relative to the profile store.</p><div id="capture-list" class="capture-list" aria-live="polite"></div></div>
<div class="charts"><article><h3>Parallel speedup</h3><p>Normalized within commit, environment, and workload. Dashed line is ideal.</p><svg id="speedup-chart"></svg></article><article><h3>Memory response</h3><p>Peak resident memory by requested inner width.</p><svg id="memory-chart"></svg></article><article><h3>CPU-work efficiency</h3><p>Baseline CPU-seconds / measured CPU-seconds; 100% conserves work.</p><svg id="efficiency-chart"></svg></article></div></section>
<section class="panel"><h2>Parallelism over time</h2><p>Inspect sequential startup/shutdown and effective CPU occupancy.</p><div class="trace-controls"><label>Trace run<select id="trace-select"></select></label><span id="trace-meta"></span></div><article class="wide"><svg id="timeline-chart"></svg></article></section>
</main><script id="dagrun-report-data" type="application/json">{payload}</script><script>{SCRIPT}</script></body></html>"##,
        title = html_escape(title),
        payload = script_safe_json(data)?,
    ))
}

fn paths_resolve_same(left: &Path, right: &Path) -> Result<bool, ProfileReportError> {
    if lexical_absolute(left)? == lexical_absolute(right)? {
        return Ok(true);
    }
    if let (Ok(left), Ok(right)) = (fs::canonicalize(left), fs::canonicalize(right)) {
        if left == right {
            return Ok(true);
        }
    }
    #[cfg(unix)]
    if let (Ok(left), Ok(right)) = (fs::metadata(left), fs::metadata(right)) {
        use std::os::unix::fs::MetadataExt;
        if left.dev() == right.dev() && left.ino() == right.ino() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_report_marker(path: &Path) -> Result<bool, ProfileReportError> {
    let mut source = fs::File::open(path).map_err(|error| {
        ProfileReportError::new(format!(
            "cannot inspect existing report destination {}: {error}",
            path.display()
        ))
    })?;
    let mut overlap = Vec::<u8>::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut chunk).map_err(|error| {
            ProfileReportError::new(format!(
                "cannot inspect existing report destination {}: {error}",
                path.display()
            ))
        })?;
        if count == 0 {
            return Ok(false);
        }
        overlap.extend_from_slice(&chunk[..count]);
        if overlap
            .windows(REPORT_DATA_MARKER.len())
            .any(|window| window == REPORT_DATA_MARKER)
        {
            return Ok(true);
        }
        let keep = REPORT_DATA_MARKER.len().saturating_sub(1);
        if overlap.len() > keep {
            overlap.drain(..overlap.len() - keep);
        }
    }
}

fn validate_report_destination(
    output_path: &Path,
    dag_path: &Path,
) -> Result<(), ProfileReportError> {
    if paths_resolve_same(output_path, dag_path)? {
        return Err(ProfileReportError::new(format!(
            "report output resolves to DAG input: {}",
            output_path.display()
        )));
    }
    let metadata = match fs::symlink_metadata(output_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(ProfileReportError::new(format!(
                "cannot inspect existing report destination {}: {error}",
                output_path.display()
            )))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(ProfileReportError::new(format!(
            "refusing to replace report destination symlink: {}",
            output_path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(ProfileReportError::new(format!(
            "refusing to replace non-regular report destination: {}",
            output_path.display()
        )));
    }
    if !has_report_marker(output_path)? {
        return Err(ProfileReportError::new(format!(
            "refusing to replace existing file without dagrun report marker: {}",
            output_path.display()
        )));
    }
    Ok(())
}

fn create_report_temporary(output_path: &Path) -> Result<(PathBuf, fs::File), ProfileReportError> {
    let name = output_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("profile-report.html");
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    for _ in 0..128 {
        let counter = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = output_path.with_file_name(format!(
            ".{name}.{}.{}.{counter}.tmp",
            std::process::id(),
            clock
        ));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(ProfileReportError::new(format!(
                    "cannot create temporary report beside {}: {error}",
                    output_path.display()
                )))
            }
        }
    }
    Err(ProfileReportError::new(format!(
        "cannot create a unique temporary report beside {}",
        output_path.display()
    )))
}

fn write_report_document(
    output_path: &Path,
    dag_path: &Path,
    document: &str,
) -> Result<(), ProfileReportError> {
    validate_report_destination(output_path, dag_path)?;
    let (temporary, mut destination) = create_report_temporary(output_path)?;
    let result = (|| {
        destination
            .write_all(document.as_bytes())
            .map_err(|error| {
                ProfileReportError::new(format!(
                    "cannot write report {}: {error}",
                    output_path.display()
                ))
            })?;
        #[cfg(unix)]
        destination
            .set_permissions({
                use std::os::unix::fs::PermissionsExt;
                fs::Permissions::from_mode(0o644)
            })
            .map_err(|error| {
                ProfileReportError::new(format!(
                    "cannot finalize report permissions {}: {error}",
                    output_path.display()
                ))
            })?;
        destination.sync_all().map_err(|error| {
            ProfileReportError::new(format!(
                "cannot write report {}: {error}",
                output_path.display()
            ))
        })?;
        drop(destination);
        validate_report_destination(output_path, dag_path)?;
        fs::rename(&temporary, output_path).map_err(|error| {
            ProfileReportError::new(format!(
                "cannot write report {}: {error}",
                output_path.display()
            ))
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Generate a standalone report from an already parsed DAG.
pub fn generate_report(
    config: &DagConfig,
    dag_path: &Path,
    profile_dir: &Path,
    output_path: &Path,
    title: &str,
) -> Result<ProfileReportSummary, ProfileReportError> {
    validate_report_destination(output_path, dag_path)?;
    if let Some(parent) = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            ProfileReportError::new(format!(
                "cannot create report directory {}: {error}",
                parent.display()
            ))
        })?;
    }
    let _lock = ProfileFileLock::acquire(output_path).map_err(|error| {
        ProfileReportError::new(format!(
            "cannot lock report {}: {error}",
            output_path.display()
        ))
    })?;
    validate_report_destination(output_path, dag_path)?;
    let data = build_report_data_for_output(config, dag_path, profile_dir, Some(output_path))?;
    let document = render_report(&data, title)?;
    write_report_document(output_path, dag_path, &document)?;
    Ok(ProfileReportSummary {
        aggregate_samples: data["records"].as_array().map(Vec::len).unwrap_or(0),
        trace_series: data["traces"].as_array().map(Vec::len).unwrap_or(0),
        warnings: data["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    })
}

/// Load a JSON/YAML DAG and generate its standalone report directly.
pub fn generate_report_from_path(
    dag_path: &Path,
    profile_dir: &Path,
    output_path: &Path,
    title: &str,
) -> Result<ProfileReportSummary, ProfileReportError> {
    if paths_resolve_same(output_path, dag_path)? {
        return Err(ProfileReportError::new(format!(
            "report output resolves to DAG input: {}",
            output_path.display()
        )));
    }
    let config = load_report_dag(dag_path)?;
    generate_report(&config, dag_path, profile_dir, output_path, title)
}

/// Refresh the canonical interactive report stored alongside the profile CSVs.
pub fn write_profile_report(
    profile_dir: &Path,
    config: &DagConfig,
    dag_path: &Path,
    title: &str,
) -> Result<PathBuf, ProfileReportError> {
    let output_path = profile_dir.join(PROFILE_REPORT_FILENAME);
    generate_report(config, dag_path, profile_dir, &output_path, title)?;
    Ok(output_path)
}

const STYLE: &str = r#"
:root{color-scheme:dark;--bg:#08111d;--panel:#101d2e;--ink:#edf5ff;--muted:#9db0c6;--line:#29415d;--cyan:#59d8e6;--blue:#6ba7ff;--gold:#ffc96b;--rose:#ff7e9b}
*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at 12% -10%,#234d7855,transparent 34rem),var(--bg);color:var(--ink);font:14px/1.45 system-ui,sans-serif}header,main{padding:28px clamp(18px,4vw,62px)}header{padding-bottom:8px}h1{font-size:clamp(28px,4vw,46px);margin:0}header p,.panel>p,.step-head p,article p{color:var(--muted)}code{color:#7890aa}.panel{background:linear-gradient(150deg,#14243af5,#0d1929f5);border:1px solid #739bc533;border-radius:15px;margin:18px 0;padding:20px;box-shadow:0 18px 42px #0005}.controls,.step-head,.trace-controls{display:flex;flex-wrap:wrap;gap:14px;align-items:end}.controls label{flex:1;min-width:180px}label{color:var(--muted);font-size:12px;font-weight:700;text-transform:uppercase;letter-spacing:.04em}select{display:block;width:100%;margin-top:6px;padding:9px;color:var(--ink);background:#091727;border:1px solid #34516f;border-radius:8px}.warnings{list-style:none;padding:0}.warnings li{padding:10px;border-left:3px solid var(--gold);background:#ffc96b16}.dag-wrap{overflow:auto}#dag-svg{display:block;width:100%;min-width:720px;max-height:640px}.edge{stroke:#456482;stroke-width:2;fill:none}.node{cursor:pointer}.node circle{fill:#173653;stroke:#6596c2;stroke-width:2}.node:hover circle,.node.selected circle{fill:#14546a;stroke:var(--cyan);stroke-width:4}.node text{fill:white;text-anchor:middle;font-weight:700;pointer-events:none}.node .cpu{fill:#b7c9dc;font-size:10px}.step-head{justify-content:space-between}.step-head label{min-width:260px}.cards{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:1px;background:var(--line);margin:18px -20px}.cards div{background:var(--panel);padding:15px}.cards small{display:block;color:var(--muted);text-transform:uppercase}.cards strong{display:block;font-size:21px;margin-top:5px}.capture-section{padding:8px 0 18px}.capture-section>p,.capture-meta,.capture-path{color:var(--muted);font-size:12px}.capture-list,.capture-trials{display:grid;gap:9px}.capture{border:1px solid #739bc533;border-radius:10px;background:#050e1988;padding:13px}.capture-head{display:flex;flex-wrap:wrap;gap:8px;align-items:center}.capture-head strong{margin-right:auto}.status{border:1px solid currentColor;border-radius:999px;padding:2px 8px;font-size:10px;font-weight:800;text-transform:uppercase}.status.complete{color:#73dfa4}.status.failed{color:var(--rose)}.status.running{color:var(--gold)}.status.skipped{color:var(--muted)}.capture-path{font-family:ui-monospace,monospace;overflow-wrap:anywhere}.capture-errors{color:#ffc2cf;margin:7px 0}.capture-trials{margin-top:9px}.capture-trial{border-left:2px solid #3d5875;padding:5px 0 5px 11px}.capture-artifacts{display:flex;flex-wrap:wrap;gap:6px 14px;margin-top:4px}.capture-artifacts code,.capture-artifacts a{color:#b9d7ed;font:11px ui-monospace,monospace;overflow-wrap:anywhere}.capture-artifacts a:hover{color:var(--cyan)}.charts{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,390px),1fr));gap:14px}.charts article,.wide{background:#050e1995;border:1px solid #739bc52b;border-radius:11px;padding:13px}article svg{display:block;width:100%;min-height:280px}.axis{stroke:#617b97}.grid{stroke:#263b52}.tick{fill:#90a6be;font-size:10px}.label{fill:#c2d1e1;font-size:11px}.point{stroke:#09111d;stroke-width:1.5;opacity:.76}.fit{fill:none;stroke:var(--cyan);stroke-width:3}.reference{fill:none;stroke:#8292a7;stroke-width:1.5;stroke-dasharray:7 5}.empty{fill:var(--muted);text-anchor:middle}.trace-controls{margin:12px 0}.trace-controls label{min-width:340px}.trace-controls span{color:var(--muted)}.trace-core{fill:none;stroke:var(--cyan);stroke-width:3}.trace-thread{fill:none;stroke:var(--gold);stroke-width:2}.warnings+section{margin-top:0}
"#;

const SCRIPT: &str = r##"
"use strict";
const DATA=JSON.parse(document.getElementById("dagrun-report-data").textContent);
const $=id=>document.getElementById(id), NS="http://www.w3.org/2000/svg";
const state={step:DATA.graph.steps[0]?.tag||"",commitLimit:0,environment:"all",workload:"all",trace:null};
const finite=Number.isFinite, median=a=>{if(!a.length)return null;const b=[...a].sort((x,y)=>x-y),m=Math.floor(b.length/2);return b.length%2?b[m]:(b[m-1]+b[m])/2}, fmt=(x,n=2)=>finite(x)?x.toFixed(n):"—";
const bytes=x=>!finite(x)?"—":x>=2**30?`${fmt(x/2**30,2)} GiB`:x>=2**20?`${fmt(x/2**20,1)} MiB`:x>=2**10?`${fmt(x/2**10,1)} KiB`:`${x} B`;
const short=s=>s==="(unknown)"?s:s.slice(0,10), el=(tag,a={},text=null)=>{const n=document.createElementNS(NS,tag);for(const[k,v]of Object.entries(a))n.setAttribute(k,v);if(text!==null)n.textContent=text;return n}, htmlEl=(tag,className="",text=null)=>{const n=document.createElement(tag);if(className)n.className=className;if(text!==null)n.textContent=text;return n};
function option(select,label,value){select.append(new Option(label,value))}
function controls(){const cs=$("commit-limit");option(cs,"All commits","0");[1,3,5,10,20].filter(n=>n<DATA.commits.length).forEach(n=>option(cs,`Latest ${n} commits`,String(n)));if(DATA.commits.length)option(cs,`Latest ${DATA.commits.length} commits`,String(DATA.commits.length));const es=$("environment-filter");option(es,"All environments","all");DATA.environments.forEach(x=>option(es,x.label,x.key));const ws=$("workload-filter");option(ws,"All workloads","all");if(DATA.records.some(x=>x.workload==="")||DATA.captures.some(x=>x.workload===""))option(ws,"Legacy data (no digest)","legacy");DATA.workloads.forEach(x=>option(ws,x,x));const ss=$("step-select");DATA.graph.steps.forEach(x=>option(ss,x.tag,x.tag));[cs,es,ws].forEach(x=>x.addEventListener("change",()=>{state.commitLimit=+cs.value;state.environment=es.value;state.workload=ws.value;update()}));ss.addEventListener("change",()=>select(ss.value))}
function commits(){return state.commitLimit?new Set(DATA.commits.slice(-state.commitLimit).map(x=>x.sha)):null}
function base(r){const c=commits();if(c&&!c.has(r.commit))return false;if(state.environment!=="all"&&r.environment!==state.environment)return false;if(state.workload==="legacy")return r.workload==="";return state.workload==="all"||r.workload===state.workload}
function rows(tag=state.step){return DATA.records.filter(r=>r.step===tag&&base(r))}
function normalized(){const rs=rows(), groups=new Map();for(const r of rs){const k=[r.commit,r.environment,r.workload].join("\x1f");if(!groups.has(k))groups.set(k,[]);groups.get(k).push(r)}const out=[];for(const g of groups.values()){const lo=Math.min(...g.map(r=>r.jobs)),wall=median(g.filter(r=>r.jobs===lo).map(r=>r.elapsed_s)),cpu=median(g.filter(r=>r.jobs===lo).map(r=>r.cpu_s).filter(finite));for(const r of g)out.push({...r,speedup:wall/r.elapsed_s,cpu_efficiency:finite(cpu)&&finite(r.cpu_s)&&r.cpu_s>0?100*cpu/r.cpu_s:null})}return out}
function medians(points,field){const m=new Map();for(const p of points){if(!finite(p[field]))continue;if(!m.has(p.jobs))m.set(p.jobs,[]);m.get(p.jobs).push(p[field])}return[...m].sort((a,b)=>a[0]-b[0]).map(([jobs,v])=>({jobs,value:median(v)}))}
function graph(){const svg=$("dag-svg"),by=new Map(DATA.graph.steps.map(s=>[s.tag,s]));svg.setAttribute("viewBox",`0 0 ${DATA.graph.width} ${DATA.graph.height}`);svg.replaceChildren();const defs=el("defs"),marker=el("marker",{id:"arrow",markerWidth:8,markerHeight:8,refX:7,refY:3,orient:"auto"});marker.append(el("path",{d:"M0,0 L0,6 L8,3 z",fill:"#456482"}));defs.append(marker);svg.append(defs);for(const e of DATA.graph.edges){const a=by.get(e.from),b=by.get(e.to);svg.append(el("line",{class:"edge",x1:a.x,y1:a.y,x2:b.x,y2:b.y,"marker-end":"url(#arrow)"}))}for(const s of DATA.graph.steps){const g=el("g",{class:"node",transform:`translate(${s.x} ${s.y})`,tabindex:0});g.dataset.step=s.tag;g.append(el("circle",{r:24}),el("text",{y:-2},s.tag.length>22?s.tag.slice(0,20)+"…":s.tag),el("text",{class:"cpu",y:16},"no samples"));g.addEventListener("click",()=>select(s.tag));g.addEventListener("keydown",e=>{if(e.key==="Enter"||e.key===" ")select(s.tag)});svg.append(g)}}
function weights(){const values=new Map(DATA.graph.steps.map(s=>[s.tag,median(rows(s.tag).map(r=>r.cpu_s).filter(finite))])),mx=Math.max(0,...[...values.values()].filter(finite));document.querySelectorAll(".node").forEach(g=>{const v=values.get(g.dataset.step),area=finite(v)&&mx?Math.max(1450,9000*v/mx):1200;g.querySelector("circle").setAttribute("r",Math.sqrt(area/Math.PI));g.querySelector(".cpu").textContent=finite(v)?`${fmt(v,1)} CPU-s`:"no samples";g.classList.toggle("selected",g.dataset.step===state.step)})}
function color(commit){const i=DATA.commits.findIndex(x=>x.sha===commit),t=DATA.commits.length<2?0.5:i/(DATA.commits.length-1);return `hsl(${215-170*t} 84% ${64+4*t}%)`}
function scatter(svg,points,field,opt){svg.replaceChildren();const W=640,H=330,m={l:58,r:18,t:18,b:48},valid=points.filter(p=>finite(p[field]));svg.setAttribute("viewBox",`0 0 ${W} ${H}`);if(!valid.length){svg.append(el("text",{class:"empty",x:W/2,y:H/2},"No measurements match these filters"));return}const jobs=[...new Set(valid.map(p=>p.jobs))].sort((a,b)=>a-b),lx=Math.log2(jobs[0]),hx=Math.log2(jobs.at(-1)),x=v=>m.l+(hx===lx?.5:(Math.log2(v)-lx)/(hx-lx))*(W-m.l-m.r),vals=valid.map(p=>p[field]);let lo=opt.zero?0:Math.min(...vals),hi=Math.max(...vals,opt.reference||0);const pad=Math.max((hi-lo)*.12,hi*.04,.01);if(opt.zero)hi*=1.12;else{lo=Math.max(0,lo-pad);hi+=pad}if(hi<=lo)hi=lo+1;const y=v=>H-m.b-(v-lo)/(hi-lo)*(H-m.t-m.b);for(let i=0;i<=4;i++){const v=lo+i*(hi-lo)/4,p=y(v);svg.append(el("line",{class:"grid",x1:m.l,x2:W-m.r,y1:p,y2:p}),el("text",{class:"tick",x:m.l-8,y:p+4,"text-anchor":"end"},opt.format(v)))}for(const j of jobs){const p=x(j);svg.append(el("line",{class:"grid",x1:p,x2:p,y1:m.t,y2:H-m.b}),el("text",{class:"tick",x:p,y:H-m.b+19,"text-anchor":"middle"},j))}svg.append(el("line",{class:"axis",x1:m.l,x2:W-m.r,y1:H-m.b,y2:H-m.b}),el("text",{class:"label",x:W/2,y:H-10,"text-anchor":"middle"},"Requested inner jobs (log₂ scale)"));if(opt.ideal){const d=jobs.map((j,i)=>`${i?"L":"M"} ${x(j)} ${y(j/jobs[0])}`).join(" ");svg.append(el("path",{class:"reference",d}))}if(finite(opt.reference)&&opt.reference>=lo&&opt.reference<=hi)svg.append(el("line",{class:"reference",x1:m.l,x2:W-m.r,y1:y(opt.reference),y2:y(opt.reference)}));const fit=medians(valid,field);if(fit.length>1)svg.append(el("path",{class:"fit",d:fit.map((p,i)=>`${i?"L":"M"} ${x(p.jobs)} ${y(p.value)}`).join(" ")}));for(const p of valid){const c=el("circle",{class:"point",cx:x(p.jobs),cy:y(p[field]),r:4.5,fill:color(p.commit)});c.append(el("title",{},`${short(p.commit)} · ${p.jobs} jobs\n${opt.tip(p[field])}\nwall ${fmt(p.elapsed_s,3)}s · CPU ${fmt(p.cpu_s,3)}s · RSS ${bytes(p.peak_bytes)}`));svg.append(c)}}
function sweet(points){const s=medians(points,"speedup");if(!s.length)return null;const e=new Map(medians(points,"cpu_efficiency").map(x=>[x.jobs,x.value])),best=Math.max(...s.map(x=>x.value));return s.find(x=>x.value>=best/1.1&&(!e.has(x.jobs)||e.get(x.jobs)>=100/1.5))||s.find(x=>x.value>=best/1.1)||s.at(-1)}
function visibleCaptures(){return DATA.captures.filter(x=>x.step===state.step&&base(x))}
function renderCaptures(){const container=$("capture-list"),captures=visibleCaptures().sort((a,b)=>b.created_at.localeCompare(a.created_at)||b.capture_id.localeCompare(a.capture_id));container.replaceChildren();if(!captures.length){container.append(htmlEl("div","capture-meta","No perf or wprof captures match this step and history window."));return}for(const capture of captures){const card=htmlEl("article","capture"),head=htmlEl("div","capture-head");head.append(htmlEl("strong","",`${capture.capture_id} · ${capture.jobs} jobs`),htmlEl("span",`status ${capture.state}`,capture.state));card.append(head,htmlEl("div","capture-meta",`${short(capture.commit)} · ${capture.created_at||"unknown time"} · selected speedup ${fmt(capture.speedup,2)}×`),htmlEl("div","capture-path",`manifest: ${capture.manifest_path}`));if(capture.errors.length){const errors=htmlEl("ul","capture-errors");for(const error of capture.errors)errors.append(htmlEl("li","",error));card.append(errors)}const trials=htmlEl("div","capture-trials");for(const trial of capture.trials){const item=htmlEl("div","capture-trial");item.append(htmlEl("strong","",`${trial.kind} · ${trial.state} · ${trial.inner_jobs} jobs · ${fmt(trial.measured_wall_s,3)}s`));if(trial.error)item.append(htmlEl("div","capture-errors",trial.error));const artifacts=htmlEl("div","capture-artifacts");if(!trial.artifacts.length)artifacts.append(htmlEl("span","capture-meta","No retained artifacts"));for(const artifact of trial.artifacts){const suffix=artifact.exists?"":" (missing)",label=`${artifact.role}: ${artifact.path} · ${bytes(artifact.size_bytes)}${suffix}`;if(artifact.exists){const link=htmlEl("a","",label);link.setAttribute("href",artifact.href);artifacts.append(link)}else artifacts.append(htmlEl("code","",label))}item.append(artifacts);trials.append(item)}card.append(trials);container.append(card)}}
function charts(){const p=normalized(),spot=sweet(p),best=Math.max(0,...medians(p,"speedup").map(x=>x.value)),at=spot?p.filter(x=>x.jobs===spot.jobs):[];$("card-samples").textContent=p.length;$("card-commits").textContent=new Set(p.map(x=>x.commit)).size;$("card-best").textContent=best?`${fmt(best)}×`:"—";$("card-sweet").textContent=spot?`${spot.jobs} jobs`:"—";$("card-memory").textContent=bytes(median(at.map(x=>x.peak_bytes).filter(finite)));scatter($("speedup-chart"),p,"speedup",{zero:true,ideal:true,format:x=>fmt(x,1)+"×",tip:x=>fmt(x,3)+"× speedup"});scatter($("memory-chart"),p,"peak_bytes",{zero:true,format:bytes,tip:bytes});scatter($("efficiency-chart"),p,"cpu_efficiency",{reference:100,format:x=>fmt(x,0)+"%",tip:x=>fmt(x,1)+"% CPU efficiency"});renderCaptures();traceSelect(spot?.jobs)}
function select(tag){state.step=tag;$("step-select").value=tag;const s=DATA.graph.steps.find(x=>x.tag===tag);$("step-title").textContent=tag;$("step-description").textContent=s?.description||s?.desc||"No authored description.";weights();charts()}
function traceSelect(sweetJobs){const s=$("trace-select"),ts=DATA.traces.filter(t=>t.step===state.step&&base(t)).sort((a,b)=>b.timestamp.localeCompare(a.timestamp));s.replaceChildren();if(!ts.length){s.disabled=true;option(s,"No matching traces","");timeline(null);return}s.disabled=false;for(const t of ts)option(s,`${short(t.commit)} · ${t.jobs} jobs · ${t.timestamp||t.run_id}`,t.key);const t=ts.find(x=>x.key===state.trace)||ts.find(x=>x.jobs===sweetJobs)||ts[0];state.trace=t.key;s.value=t.key;timeline(t)}
function timeline(t){const svg=$("timeline-chart"),W=1120,H=350,m={l:58,r:58,t:18,b:46};svg.replaceChildren();svg.setAttribute("viewBox",`0 0 ${W} ${H}`);if(!t||!t.points.length){svg.append(el("text",{class:"empty",x:W/2,y:H/2},"No interval trace matches these filters"));$("trace-meta").textContent="Enable profile time-series collection to record interval data.";return}const cp=t.points.filter(p=>finite(p.effective_cores)),tp=t.points.filter(p=>finite(p.thread_count)),mt=Math.max(1e-9,...t.points.map(p=>p.elapsed_s)),mc=Math.max(1,t.jobs,...cp.map(p=>p.effective_cores))*1.12,mth=Math.max(1,...tp.map(p=>p.thread_count))*1.12,x=v=>m.l+v/mt*(W-m.l-m.r),yc=v=>H-m.b-v/mc*(H-m.t-m.b),yt=v=>H-m.b-v/mth*(H-m.t-m.b);for(let i=0;i<=4;i++){const py=m.t+i*(H-m.t-m.b)/4;svg.append(el("line",{class:"grid",x1:m.l,x2:W-m.r,y1:py,y2:py}),el("text",{class:"tick",x:m.l-8,y:py+4,"text-anchor":"end"},fmt(mc*(1-i/4),1)),el("text",{class:"tick",x:W-m.r+8,y:py+4},fmt(mth*(1-i/4),0)))}const path=(ps,get,y)=>ps.map((p,i)=>`${i?"L":"M"} ${x(p.elapsed_s)} ${y(get(p))}`).join(" ");if(cp.length)svg.append(el("path",{class:"trace-core",d:path(cp,p=>p.effective_cores,yc)}));if(tp.length)svg.append(el("path",{class:"trace-thread",d:path(tp,p=>p.thread_count,yt)}));svg.append(el("line",{class:"reference",x1:m.l,x2:W-m.r,y1:yc(t.jobs),y2:yc(t.jobs)}));for(const p of cp){const c=el("circle",{class:"point",cx:x(p.elapsed_s),cy:yc(p.effective_cores),r:3.5,fill:"#59d8e6"});c.append(el("title",{},`${fmt(p.elapsed_s,3)}s · ${fmt(p.effective_cores,2)} effective cores · ${p.thread_count??"—"} threads`));svg.append(c)}$("trace-meta").textContent=`${short(t.commit)} · ${t.jobs} jobs · cyan effective cores · amber threads · dashed requested width · ${t.source}`}
function update(){weights();charts()}controls();graph();$("trace-select").addEventListener("change",e=>{state.trace=e.target.value;timeline(DATA.traces.find(t=>t.key===state.trace)||null)});select(state.step);
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CmdType, ResourceHint, Step};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn step(tag: &str, deps: &[&str]) -> Step {
        let (group, job) = tag.split_once('.').unwrap();
        Step {
            group: group.to_string(),
            job: job.to_string(),
            desc: format!("{tag} short"),
            description: format!("details for {tag}"),
            labels: Vec::new(),
            cmd: "true".to_string(),
            cmdtype: CmdType::Unknown,
            deps: deps.iter().map(|value| value.to_string()).collect(),
            env: BTreeMap::new(),
            hint: ResourceHint::default(),
            networkonly: false,
            engine_only: false,
            delegated_children: false,
            timeout: 0,
            cpu_timeout: 0,
            jobs_flag: None,
            jobs_env: None,
            manifest: None,
            result_manifests: None,
            integration_test_binaries: None,
            skip_reason: None,
            write_domains: None,
            write_domain_guarantee: None,
            explains: Vec::new(),
            fail_fast_family: None,
        }
    }

    fn fixture_dir() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("dagrun-report-{}-{nonce}", std::process::id()));
        fs::create_dir_all(path.join("traces")).unwrap();
        path
    }

    #[test]
    fn report_embeds_historical_records_graph_and_traces() {
        let dir = fixture_dir();
        fs::write(dir.join("step_profiles_m_c.csv"), concat!(
            "timestamp,machine_id,container_class,git_sha,runner_name,step,inner_jobs,elapsed_s,user_s,sys_s,effective_cores,peak_bytes,returncode,ok,timed_out,cpu_timed_out,oom_kills,run_id,workload_digest\n",
            "2026-01-01T00:00:00Z,m,c,aaa,rust,g.a,1,10,8,1,0.9,1000,0,true,false,false,0,r1,w1\n",
            "2026-01-02T00:00:00Z,m,c,bbb,python,g.a,4,3,9,2,3.6,2000,0,true,false,false,0,r2,w1\n",
            "2026-01-02T00:00:00Z,m,c,bbb,rust,g.a,8,2,1,1,1,3000,1,false,false,false,0,bad,w1\n",
            "2026-01-02T00:00:00Z,m,c,bbb,rust,not.in.graph,1,1,1,0,1,1,0,true,false,false,0,x,w1\n"
        )).unwrap();
        fs::write(dir.join("traces/r2.csv"), concat!(
            "timestamp,machine_id,container_class,git_sha,run_id,step,inner_jobs,sample_index,sample_kind,elapsed_s,interval_s,effective_cores,user_cores,system_cores,throttled_s,thread_count,workload_digest\n",
            "2026-01-02T00:00:00Z,m,c,bbb,r2,g.a,4,1,periodic,0.5,0.5,3.2,3.0,0.2,0,5,w1\n",
            "2026-01-02T00:00:00Z,m,c,bbb,r2,g.a,4,0,periodic,0.0,0.5,0.2,0.1,0.1,0,2,w1\n"
        )).unwrap();
        let config = DagConfig {
            steps: vec![step("g.a", &[]), step("g.b", &["g.a"])],
            description: "demo".to_string(),
            ..DagConfig::default()
        };
        let data = build_report_data(&config, Path::new("graph.yaml"), &dir).unwrap();
        assert_eq!(data["records"].as_array().unwrap().len(), 2);
        assert_eq!(data["traces"].as_array().unwrap().len(), 1);
        assert_eq!(data["traces"][0]["points"][0]["sample_index"], 0);
        assert_eq!(data["graph"]["edges"][0], json!({"from":"g.a","to":"g.b"}));
        let warnings = data["warnings"].as_array().unwrap();
        assert!(warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("failed")));
        assert!(warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("absent")));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn generated_html_is_offline_interactive_and_script_safe() {
        let dir = fixture_dir();
        fs::write(
            dir.join("step_profiles_m_c.csv"),
            concat!(
                "step,inner_jobs,elapsed_s,git_sha,machine_id,container_class,ok\n",
                "g.a,1,1,abc,m,c,true\n"
            ),
        )
        .unwrap();
        let config = DagConfig {
            steps: vec![step("g.a", &[])],
            ..DagConfig::default()
        };
        let output = dir.join("site/report.html");
        let summary = generate_report(
            &config,
            Path::new("dag<unsafe>.json"),
            &dir,
            &output,
            "A&B <report>",
        )
        .unwrap();
        let html = fs::read_to_string(&output).unwrap();
        assert_eq!(summary.aggregate_samples, 1);
        assert!(html.contains("A&amp;B &lt;report&gt;"));
        assert!(html.contains("id=\"speedup-chart\""));
        assert!(html.contains("id=\"timeline-chart\""));
        assert!(html.contains("const DATA=JSON.parse"));
        assert!(!html.contains("https://"));
        assert!(!html.contains("dag<unsafe>"));
        let canonical = write_profile_report(
            &dir,
            &config,
            Path::new("pipeline.json"),
            "Canonical report",
        )
        .unwrap();
        assert_eq!(canonical, dir.join(PROFILE_REPORT_FILENAME));
        assert!(canonical.is_file());
        assert!(dir
            .join(".locks")
            .join("profile_report.html.lock")
            .is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn report_destination_guard_preserves_inputs_and_allows_reports() {
        let dir = fixture_dir();
        let profile = dir.join("step_profiles_m_c.csv");
        fs::write(
            &profile,
            "step,inner_jobs,elapsed_s,git_sha,machine_id,container_class,ok\n\
             g.a,1,1,abc,m,c,true\n",
        )
        .unwrap();
        let dag = dir.join("pipeline.json");
        fs::write(&dag, r#"{"steps":[{"group":"g","job":"a","cmd":"true"}]}"#).unwrap();
        let config = DagConfig {
            steps: vec![step("g.a", &[])],
            ..DagConfig::default()
        };

        let probe_output = dir.join("site/probe.html");
        fs::create_dir_all(probe_output.parent().unwrap()).unwrap();
        let (probe_path, probe_file) = create_report_temporary(&probe_output).unwrap();
        assert_eq!(probe_path.parent(), probe_output.parent());
        assert!(fs::symlink_metadata(&probe_path)
            .unwrap()
            .file_type()
            .is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                probe_file.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        drop(probe_file);
        fs::remove_file(probe_path).unwrap();

        let output = dir.join("site/new-report.html");
        generate_report(&config, &dag, &dir, &output, "First").unwrap();
        let first = fs::read(&output).unwrap();
        generate_report(&config, &dag, &dir, &output, "Second").unwrap();
        let refreshed = fs::read(&output).unwrap();
        assert_ne!(refreshed, first);
        assert!(refreshed
            .windows(REPORT_DATA_MARKER.len())
            .any(|window| window == REPORT_DATA_MARKER));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o644
            );
        }
        assert!(!fs::read_dir(output.parent().unwrap())
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            }));

        let dag_before = fs::read(&dag).unwrap();
        let error = generate_report(&config, &dag, &dir, &dag, "Unsafe").unwrap_err();
        assert!(error
            .to_string()
            .contains("report output resolves to DAG input"));
        assert_eq!(fs::read(&dag).unwrap(), dag_before);

        let hardlink = dir.join("dag-hardlink.json");
        fs::hard_link(&dag, &hardlink).unwrap();
        let error = generate_report(&config, &dag, &dir, &hardlink, "Unsafe").unwrap_err();
        assert!(error
            .to_string()
            .contains("report output resolves to DAG input"));
        assert_eq!(fs::read(&dag).unwrap(), dag_before);

        let trace = dir.join("traces/raw.csv");
        fs::write(&trace, b"trace-data").unwrap();
        let manifest = dir.join("captures/capture-001/manifest.json");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, b"manifest-data").unwrap();
        let artifact = dir.join("captures/capture-001/perf.data");
        fs::write(&artifact, b"artifact-data").unwrap();
        let arbitrary = dir.join("keep.txt");
        fs::write(&arbitrary, b"do not overwrite").unwrap();
        for destination in [&profile, &trace, &manifest, &artifact, &arbitrary] {
            let before = fs::read(destination).unwrap();
            let error = generate_report(&config, &dag, &dir, destination, "Unsafe").unwrap_err();
            assert!(error
                .to_string()
                .contains("refusing to replace existing file without dagrun report marker"));
            assert_eq!(fs::read(destination).unwrap(), before);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = dir.join("report-link.html");
            symlink(&output, &link).unwrap();
            let error = generate_report(&config, &dag, &dir, &link, "Unsafe").unwrap_err();
            assert!(error
                .to_string()
                .contains("refusing to replace report destination symlink"));
            assert!(fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
        }

        let directory = dir.join("existing-directory");
        fs::create_dir(&directory).unwrap();
        let error = generate_report(&config, &dag, &dir, &directory, "Unsafe").unwrap_err();
        assert!(error
            .to_string()
            .contains("refusing to replace non-regular report destination"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn capture_manifests_are_filtered_sorted_and_linked_safely() {
        let dir = fixture_dir();
        fs::write(
            dir.join("step_profiles_m_c.csv"),
            "step,inner_jobs,elapsed_s,git_sha,machine_id,container_class,ok\n\
             g.a,1,1,commit1,m,c,true\n",
        )
        .unwrap();
        let capture = dir.join("captures/capture-001");
        let artifact = capture.join("perf-001/perf data#1.data");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(&artifact, b"PERFILE2").unwrap();
        let manifest = json!({
            "schema": "dagrun-profile-capture-v1",
            "capture_id": "capture-001",
            "state": "complete",
            "machine_id": "capture-machine",
            "container_class": "capture-container",
            "created_at": "2026-08-22T00:01:00Z",
            "finished_at": "2026-08-22T00:01:03Z",
            "artifact_root": ".",
            "selection": {
                "step": "g.a",
                "workload_digest": "digest-a",
                "inner_jobs": 4,
                "expected_wall_s": 2.5,
                "speedup": 3.2,
                "git_sha": "commit2"
            },
            "preflight": [],
            "trials": [{
                "trial_id": "perf-001",
                "kind": "perf",
                "state": "complete",
                "inner_jobs": 4,
                "started_at": "2026-08-22T00:01:00Z",
                "finished_at": "2026-08-22T00:01:03Z",
                "measured_wall_s": 2.6,
                "workload_returncode": 0,
                "profiler_returncode": 0,
                "included_in_model": false,
                "artifacts": [
                    {"role":"perf-data","path":"perf-001/perf data#1.data","size_bytes":8,"mode":"0o600"},
                    {"role":"perf-log","path":"perf-001/missing.log","size_bytes":0,"mode":"0o600"}
                ],
                "error": ""
            }],
            "errors": []
        });
        fs::write(
            capture.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        let legacy_dir = dir.join("captures/legacy-capture");
        fs::create_dir_all(&legacy_dir).unwrap();
        let mut legacy = manifest.clone();
        legacy["capture_id"] = json!("legacy-capture");
        legacy["created_at"] = json!("2026-08-23T00:01:00Z");
        legacy.as_object_mut().unwrap().remove("machine_id");
        legacy.as_object_mut().unwrap().remove("container_class");
        fs::write(
            legacy_dir.join("manifest.json"),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();
        fs::create_dir_all(dir.join("captures/bad-json")).unwrap();
        fs::write(dir.join("captures/bad-json/manifest.json"), "{not json").unwrap();
        fs::create_dir_all(dir.join("captures/removed-step")).unwrap();
        let mut removed = manifest.clone();
        removed["capture_id"] = json!("removed-step");
        removed["selection"]["step"] = json!("old.removed");
        fs::write(
            dir.join("captures/removed-step/manifest.json"),
            serde_json::to_string(&removed).unwrap(),
        )
        .unwrap();

        let config = DagConfig {
            steps: vec![step("g.a", &[])],
            ..DagConfig::default()
        };
        let output = dir.join("out/report.html");
        let data =
            build_report_data_for_output(&config, Path::new("pipeline.json"), &dir, Some(&output))
                .unwrap();
        assert_eq!(data["captures"].as_array().unwrap().len(), 2);
        let capture = data["captures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|capture| capture["capture_id"] == "capture-001")
            .unwrap();
        assert_eq!(capture["step"], "g.a");
        assert_eq!(capture["machine"], "capture-machine");
        assert_eq!(capture["container"], "capture-container");
        assert_eq!(capture["environment"], "capture-machine␟capture-container");
        assert_eq!(
            capture["manifest_path"],
            "captures/capture-001/manifest.json"
        );
        assert_eq!(
            capture["trials"][0]["artifacts"][0],
            json!({
                "exists": true,
                "href": "../captures/capture-001/perf-001/perf%20data%231.data",
                "mode": "0o600",
                "path": "captures/capture-001/perf-001/perf data#1.data",
                "role": "perf-data",
                "size_bytes": 8
            })
        );
        assert_eq!(capture["trials"][0]["artifacts"][1]["exists"], false);
        let legacy = data["captures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|capture| capture["capture_id"] == "legacy-capture")
            .unwrap();
        assert_eq!(legacy["machine"], "");
        assert_eq!(legacy["container"], "");
        assert_eq!(legacy["environment"], "");
        assert!(data["environments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|environment| environment["key"] == "capture-machine␟capture-container"));
        assert!(!data["environments"]
            .as_array()
            .unwrap()
            .iter()
            .any(|environment| environment["key"] == "␟"));
        let warnings = data["warnings"].as_array().unwrap();
        assert!(warnings.iter().any(|warning| warning.as_str()
            == Some("Ignored malformed capture manifest captures/bad-json/manifest.json.")));
        assert!(warnings
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("old.removed")));

        let canonical = build_report_data_for_output(
            &config,
            Path::new("pipeline.json"),
            &dir,
            Some(&dir.join(PROFILE_REPORT_FILENAME)),
        )
        .unwrap();
        let canonical_capture = canonical["captures"]
            .as_array()
            .unwrap()
            .iter()
            .find(|capture| capture["capture_id"] == "capture-001")
            .unwrap();
        assert_eq!(
            canonical_capture["trials"][0]["artifacts"][0]["href"],
            "captures/capture-001/perf-001/perf%20data%231.data"
        );
        let html = render_report(&data, "capture report").unwrap();
        assert!(html.contains("id=\"capture-list\""));
        assert!(html.contains("Perf and wprof follow-up trials"));
        assert!(html.contains("link.setAttribute(\"href\",artifact.href)"));
        assert!(html.contains("x.step===state.step&&base(x)"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_required_csv_columns_are_reported() {
        let dir = fixture_dir();
        fs::write(dir.join("step_profiles_m_c.csv"), "step,elapsed_s\ng.a,1\n").unwrap();
        let config = DagConfig {
            steps: vec![step("g.a", &[])],
            ..DagConfig::default()
        };
        let error = build_report_data(&config, Path::new("dag.json"), &dir).unwrap_err();
        assert!(error.to_string().contains("inner_jobs"));
        fs::remove_dir_all(dir).unwrap();
    }
}
