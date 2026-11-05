//! Per-step measurement enrichment from cgroup and ambient-load observations.

// Per-step measurement ENRICHMENT: fold a step's cgroup + ambient deltas into the rich
// profile-row columns the parallel-speedup model reads back (Rust port of
// `py/dagrun/profile_enrich.py`).
//
// The column NAMES mirror the originating `scripts/validate_perflog.py` `STEP_PROFILE_COLUMNS`
// (`effective_cores`, `quota_utilization_pct`, `throttled_s`, `external_cpu_s`/`external_cores`,
// `co_tenants_*`, `ambient_bucket`, `load*`, host/step PSI), so a later schema unification is a
// RENAME, not a redesign. Everything is best-effort and captured UNDER cgroup boxing; an
// unavailable measurement simply leaves its column out of the returned row and the writer records
// it blank (No Silent Failure).
//
// These are WRITER columns whose exact numeric values legitimately differ per host/run, so they
// are NOT byte-compared across the Python and Rust builds (only the CSV schema is). The READER
// that consumes them (`estimates::load_step_speedups`) IS cross-checked byte-identical.

use std::collections::BTreeMap;

use crate::ambient::{ambient_bucket, attribute_external_cores, AmbientSnapshot, PsiReading};
use crate::perflog::{effective_cpu_quota, nproc};

const USEC: f64 = 1_000_000.0;

/// Return the effective core budget of the current cgroup or process affinity.
///
/// Walks the current cgroup's ancestor chain for the tightest finite CPU quota, floors it to a
/// positive whole-core budget, and takes the tighter of that quota and process affinity. Flooring
/// is deliberate: a per-step width ceiling must never promise more CPU than the binding
/// bandwidth cap, and integer arithmetic avoids language-specific rounding at half-core boundaries.
pub fn container_core_budget() -> i64 {
    let affinity = nproc().max(1);
    core_budget_from_quota(&effective_cpu_quota(), affinity)
}

fn core_budget_from_quota(quota: &str, affinity: i64) -> i64 {
    let affinity = affinity.max(1);
    let mut parts = quota.split('_');
    let quota_cores = match (parts.next(), parts.next(), parts.next()) {
        (Some(q), Some(p), None) => match (q.parse::<i64>(), p.parse::<i64>()) {
            (Ok(q), Ok(p)) if q > 0 && p > 0 => Some((q / p).max(1)),
            _ => None,
        },
        _ => None,
    };
    quota_cores.map_or(affinity, |quota| quota.min(affinity))
}

/// Resolve an optional step width to a concrete positive-environment budget.
///
/// An explicit width is returned unchanged; otherwise the current container core budget is used.
pub fn resolve_effective_inner_jobs(inner_jobs: Option<i64>) -> i64 {
    inner_jobs.unwrap_or_else(container_core_budget)
}

fn psi_columns(
    row: &mut BTreeMap<String, String>,
    prefix: &str,
    start: Option<&PsiReading>,
    end: Option<&PsiReading>,
) {
    if let Some(s) = start {
        row.insert(
            format!("{prefix}_psi_avg10_start"),
            format!("{:.2}", s.avg10),
        );
        row.insert(
            format!("{prefix}_psi_avg60_start"),
            format!("{:.2}", s.avg60),
        );
    }
    if let Some(e) = end {
        row.insert(format!("{prefix}_psi_avg10_end"), format!("{:.2}", e.avg10));
        row.insert(format!("{prefix}_psi_avg60_end"), format!("{:.2}", e.avg60));
    }
}

/// Build rich profile columns from one step's cgroup and ambient measurements.
///
/// Only columns whose inputs are available are returned; the profile writer leaves the remaining
/// fields in [`crate::perflog::STEP_PROFILE_COLUMNS`] empty.
#[allow(clippy::too_many_arguments)]
pub fn step_enrichment_columns(
    elapsed_s: f64,
    inner_jobs: Option<i64>,
    cpu_stats: Option<&BTreeMap<String, i64>>,
    ambient_start: Option<&AmbientSnapshot>,
    ambient_end: Option<&AmbientSnapshot>,
    step_pressure_start: Option<&PsiReading>,
    step_pressure_end: Option<&PsiReading>,
) -> BTreeMap<String, String> {
    let mut row: BTreeMap<String, String> = BTreeMap::new();
    if let Some(stats) = cpu_stats {
        if elapsed_s > 0.0 {
            let usage_usec = *stats.get("usage_usec").unwrap_or(&0);
            let effective_cores = usage_usec as f64 / (elapsed_s * USEC);
            row.insert("effective_cores".into(), format!("{effective_cores:.4}"));
            row.insert(
                "user_s".into(),
                format!("{:.3}", *stats.get("user_usec").unwrap_or(&0) as f64 / USEC),
            );
            row.insert(
                "sys_s".into(),
                format!(
                    "{:.3}",
                    *stats.get("system_usec").unwrap_or(&0) as f64 / USEC
                ),
            );
            row.insert(
                "throttled_s".into(),
                format!(
                    "{:.3}",
                    *stats.get("throttled_usec").unwrap_or(&0) as f64 / USEC
                ),
            );
            if let Some(j) = inner_jobs {
                if j != 0 {
                    row.insert(
                        "quota_utilization_pct".into(),
                        format!("{:.2}", effective_cores / j as f64 * 100.0),
                    );
                }
            }
            if let (Some(a0), Some(a1)) = (ambient_start, ambient_end) {
                let external_cores = attribute_external_cores(
                    a0.busy_jiffies,
                    a1.busy_jiffies,
                    usage_usec,
                    elapsed_s,
                );
                row.insert(
                    "external_cpu_s".into(),
                    format!("{:.3}", external_cores * elapsed_s),
                );
                row.insert("external_cores".into(), format!("{external_cores:.3}"));
                row.insert(
                    "ambient_bucket".into(),
                    ambient_bucket(external_cores, a1).value().to_string(),
                );
            }
        }
    }
    if let (Some(a0), Some(a1)) = (ambient_start, ambient_end) {
        row.insert("co_tenants_start".into(), a0.co_tenants.to_string());
        row.insert("co_tenants_end".into(), a1.co_tenants.to_string());
        row.insert("load1_start".into(), format!("{:.3}", a0.load1));
        row.insert("load1_end".into(), format!("{:.3}", a1.load1));
        row.insert("load5_start".into(), format!("{:.3}", a0.load5));
        row.insert("load5_end".into(), format!("{:.3}", a1.load5));
        psi_columns(
            &mut row,
            "host_cpu",
            a0.cpu_psi.as_ref(),
            a1.cpu_psi.as_ref(),
        );
        psi_columns(
            &mut row,
            "host_memory",
            a0.memory_psi.as_ref(),
            a1.memory_psi.as_ref(),
        );
        psi_columns(&mut row, "host_io", a0.io_psi.as_ref(), a1.io_psi.as_ref());
    }
    psi_columns(&mut row, "step_cpu", step_pressure_start, step_pressure_end);
    row
}

#[cfg(test)]
mod tests {
    use super::core_budget_from_quota;

    #[test]
    fn fractional_cpu_quota_is_conservatively_floored() {
        assert_eq!(core_budget_from_quota("50000_100000", 64), 1);
        assert_eq!(core_budget_from_quota("150000_100000", 64), 1);
        assert_eq!(core_budget_from_quota("250000_100000", 64), 2);
        assert_eq!(core_budget_from_quota("350000_100000", 2), 2);
        assert_eq!(core_budget_from_quota("max", 7), 7);
        assert_eq!(core_budget_from_quota("unknown", 7), 7);
    }
}
