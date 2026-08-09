//! Exact-cell coverage evidence for the complete local validation profile.
//!
//! The E2E harness writes one schema-2 JSONL record per selected cell.  A DAG
//! node's zero exit status is not enough: `--allow-empty` deliberately permits
//! several bucket nodes to do no work.  This module crosses the current run's
//! records against the commit-ratcheted cell set, so an empty, stale, skipped,
//! duplicated, or caller-planted record cannot shrink the denominator.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CellKey {
    pub lane: String,
    pub category: String,
    pub test: String,
    pub mode: String,
    pub backend: String,
}

impl CellKey {
    fn from_value(value: &Value) -> Result<Self, String> {
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| format!("cell has no nonempty string {name}: {value}"))
        };
        let key = Self {
            lane: field("lane")?,
            category: field("category")?,
            test: field("test")?,
            mode: field("mode")?,
            backend: field("backend")?,
        };
        if !matches!(key.lane.as_str(), "portable" | "privileged") {
            return Err(format!("cell has invalid lane {}", key.lane));
        }
        if !key
            .category
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(format!("cell has unsafe category {}", key.category));
        }
        Ok(key)
    }

    pub fn label(&self) -> String {
        format!(
            "{}:{}/{}/{}:{}",
            self.lane, self.category, self.test, self.mode, self.backend
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct CoverageReport {
    pub planned: usize,
    pub executed: usize,
    pub planned_kvm: usize,
    pub executed_kvm: usize,
    pub missing: Vec<String>,
    pub failed: Vec<String>,
    pub duplicates: Vec<String>,
    pub unexpected: Vec<String>,
    pub invalid: Vec<String>,
}

impl CoverageReport {
    pub fn satisfied(&self) -> bool {
        self.planned > 0
            && self.executed == self.planned
            && self.planned_kvm > 0
            && self.executed_kvm == self.planned_kvm
            && self.missing.is_empty()
            && self.failed.is_empty()
            && self.duplicates.is_empty()
            && self.unexpected.is_empty()
            && self.invalid.is_empty()
    }

    pub fn evidence_json(&self) -> Value {
        serde_json::json!({
            "planned_cells": self.planned,
            "executed_cells": self.executed,
            "planned_kvm_cells": self.planned_kvm,
            "executed_kvm_cells": self.executed_kvm,
            "missing_cells": self.missing,
            "failed_cells": self.failed,
            "duplicate_cells": self.duplicates,
            "unexpected_cells": self.unexpected,
            "invalid_records": self.invalid,
            "satisfied": self.satisfied(),
        })
    }
}

pub fn load_required_cells(root: &Path) -> Result<BTreeSet<CellKey>, String> {
    let path = root.join("ci/expected-e2e-plan.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    if value.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err(format!("{} has unsupported schema", path.display()));
    }

    let mut required = BTreeSet::new();
    for field in ["cells", "local_full_kvm_cells"] {
        let cells = value
            .get(field)
            .and_then(Value::as_array)
            .filter(|cells| !cells.is_empty())
            .ok_or_else(|| format!("{} has no nonempty {field} array", path.display()))?;
        for cell in cells {
            let key = CellKey::from_value(cell)?;
            if field == "local_full_kvm_cells"
                && (key.lane != "privileged" || key.backend != "kvm")
            {
                return Err(format!(
                    "local-full KVM cell is not privileged/KVM: {}",
                    key.label()
                ));
            }
            if !required.insert(key.clone()) {
                return Err(format!("duplicate required cell {}", key.label()));
            }
        }
    }
    if !required.iter().any(|cell| cell.backend == "kvm") {
        return Err("local full E2E plan contains no required KVM cell".into());
    }
    Ok(required)
}

fn evaluate_values(
    expected: &BTreeSet<CellKey>,
    records: impl IntoIterator<Item = Result<Value, String>>,
    run_id: &str,
    commit: &str,
) -> CoverageReport {
    let mut report = CoverageReport {
        planned: expected.len(),
        planned_kvm: expected.iter().filter(|cell| cell.backend == "kvm").count(),
        ..CoverageReport::default()
    };
    let mut valid = BTreeSet::new();
    let mut seen: BTreeMap<CellKey, usize> = BTreeMap::new();

    for record in records {
        let value = match record {
            Ok(value) => value,
            Err(error) => {
                report.invalid.push(error);
                continue;
            }
        };
        // Other run ids are stale output, never evidence for this run.  The
        // harness truncates each selected result file before writing; retaining
        // this check makes a skipped node fail missing rather than reuse an old
        // green record.
        if value.get("run_id").and_then(Value::as_str) != Some(run_id) {
            continue;
        }
        let key = match CellKey::from_value(&value) {
            Ok(key) => key,
            Err(error) => {
                report.invalid.push(error);
                continue;
            }
        };
        *seen.entry(key.clone()).or_insert(0) += 1;
        if !expected.contains(&key) {
            report.unexpected.push(key.label());
            continue;
        }
        if value.get("schema").and_then(Value::as_u64) != Some(2)
            || value.get("hermit_sha").and_then(Value::as_str) != Some(commit)
            || value.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
            || value.get("classification").and_then(Value::as_str) != Some("required")
        {
            report.invalid.push(key.label());
            continue;
        }
        if value.get("outcome").and_then(Value::as_str) != Some("PASS") {
            report.failed.push(key.label());
            continue;
        }
        valid.insert(key);
    }

    report.duplicates = seen
        .into_iter()
        .filter(|(_, count)| *count != 1)
        .map(|(cell, count)| format!("{} x{count}", cell.label()))
        .collect();
    report.missing = expected.difference(&valid).map(CellKey::label).collect();
    report.executed = valid.len();
    report.executed_kvm = valid.iter().filter(|cell| cell.backend == "kvm").count();
    report.failed.sort();
    report.failed.dedup();
    report.unexpected.sort();
    report.unexpected.dedup();
    report.invalid.sort();
    report.invalid.dedup();
    report
}

pub fn verify_result_files(
    root: &Path,
    expected: &BTreeSet<CellKey>,
    run_id: &str,
    commit: &str,
) -> CoverageReport {
    let mut records: Vec<Result<Value, String>> = Vec::new();
    for lane in ["portable", "privileged"] {
        let lane_dir = root.join("ignored/e2e").join(lane);
        let entries = match std::fs::read_dir(&lane_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                records.push(Err(format!("cannot read {}: {error}", lane_dir.display())));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    records.push(Err(format!(
                        "cannot read {} entry: {error}",
                        lane_dir.display()
                    )));
                    continue;
                }
            };
            let path = entry.path().join("results.jsonl");
            if !path.is_file() {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    records.push(Err(format!("cannot read {}: {error}", path.display())));
                    continue;
                }
            };
            for (index, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                records.push(serde_json::from_str(line).map_err(|error| {
                    format!(
                        "{}:{} is malformed JSON: {error}",
                        path.display(),
                        index + 1
                    )
                }));
            }
        }
    }
    evaluate_values(expected, records, run_id, commit)
}

pub fn self_test() -> Result<String, String> {
    let portable = serde_json::json!({
        "lane":"portable", "category":"applications", "test":"applications/echo",
        "mode":"verify", "backend":"ptrace"
    });
    let kvm = serde_json::json!({
        "lane":"privileged", "category":"applications", "test":"applications/kvm-echo",
        "mode":"verify", "backend":"kvm"
    });
    let expected = [portable.clone(), kvm.clone()]
        .iter()
        .map(CellKey::from_value)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let record = |cell: &Value, run_id: &str, sha: &str| {
        let mut row = cell.clone();
        let object = row.as_object_mut().unwrap();
        object.insert("schema".into(), Value::from(2));
        object.insert("run_id".into(), Value::from(run_id));
        object.insert("hermit_sha".into(), Value::from(sha));
        object.insert("source_tree_dirty".into(), Value::from(false));
        object.insert("classification".into(), Value::from("required"));
        object.insert("outcome".into(), Value::from("PASS"));
        row
    };
    let run_id = "validate-causal-run";
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let positive = evaluate_values(
        &expected,
        [
            Ok(record(&portable, run_id, sha)),
            Ok(record(&kvm, run_id, sha)),
        ],
        run_id,
        sha,
    );
    if !positive.satisfied() || positive.executed != 2 || positive.executed_kvm != 1 {
        return Err(format!("complete E2E evidence was refused: {positive:?}"));
    }

    // Planted missing/skip negative: the ordinary cell exists and passes, but
    // the required KVM cell produced no record at all.  This must be 1/2 total,
    // 0/1 KVM, and ineligible — never a smaller-denominator green.
    let skipped = evaluate_values(&expected, [Ok(record(&portable, run_id, sha))], run_id, sha);
    if skipped.satisfied()
        || skipped.executed != 1
        || skipped.executed_kvm != 0
        || skipped.missing.len() != 1
    {
        return Err(format!("missing KVM evidence was not refused: {skipped:?}"));
    }

    // Planted caller/stale negatives: a stale run id is not current evidence,
    // and a current-looking row for the wrong SHA is invalid rather than green.
    let stale = evaluate_values(
        &expected,
        [
            Ok(record(&portable, "caller-chosen-run", sha)),
            Ok(record(&kvm, "caller-chosen-run", sha)),
        ],
        run_id,
        sha,
    );
    if stale.satisfied() || stale.executed != 0 || stale.missing.len() != 2 {
        return Err(format!("stale caller records were accepted: {stale:?}"));
    }
    let forged = evaluate_values(
        &expected,
        [
            Ok(record(&portable, run_id, sha)),
            Ok(record(
                &kvm,
                run_id,
                "ffffffffffffffffffffffffffffffffffffffff",
            )),
        ],
        run_id,
        sha,
    );
    if forged.satisfied()
        || forged.executed != 1
        || forged.executed_kvm != 0
        || forged.invalid.len() != 1
    {
        return Err(format!("wrong-SHA KVM record was accepted: {forged:?}"));
    }

    Ok("E2E coverage: positive 2/2 (KVM 1/1); skipped negative 1/2 (KVM 0/1); stale negative 0/2; wrong-SHA negative 1/2".into())
}
