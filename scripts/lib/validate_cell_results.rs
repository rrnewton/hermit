// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the per-cell result population carried by one full validate run.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;

const IDENTITY_KEYS: [&str; 5] = ["lane", "category", "test", "mode", "backend"];

#[derive(Debug)]
pub struct RetainedCellResults {
    pub run_id: String,
    pub evidence: Value,
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn collect_results_files(path: &Path, output: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(path).map_err(|error| {
        format!(
            "cannot read per-cell result root {}: {error}",
            path.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read per-cell result entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot classify {}: {error}", entry.path().display()))?;
        if file_type.is_dir() {
            collect_results_files(&entry.path(), output)?;
        } else if file_type.is_file() && entry.file_name() == "results.jsonl" {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("per-cell result has no nonempty {key}"))
}

fn identity(value: &Value) -> Result<Value, String> {
    let mut identity = serde_json::Map::new();
    for key in IDENTITY_KEYS {
        identity.insert(key.into(), Value::String(string(value, key)?.into()));
    }
    Ok(Value::Object(identity))
}

fn comparison_tier(report: &Value) -> Option<&'static str> {
    let comparison = report.get("comparison")?;
    let counts = report.get("compared_log_messages")?;
    let canonical = comparison.get("strictness")?.as_str()? == "canonical"
        && comparison.get("compare_logs")?.as_bool()? == true
        && comparison.get("strip_lines")?.as_bool()? == false
        && comparison.get("canonicalize_addresses")?.as_bool()? == true
        && comparison.get("full_trace")?.as_bool()? == true
        && comparison.get("exact_remainder")?.as_bool()? == true
        && comparison.get("ignore_lines")?.as_bool()? == false
        && comparison.get("skip_commit")?.as_bool()? == false
        && comparison.get("skip_detlog")?.as_bool()? == false
        && counts.get("left")?.as_u64()? > 0
        && counts.get("right")?.as_u64()? > 0;
    canonical.then_some("canonical-bitwise")
}

fn cell_verdict(row: &Value) -> Result<Value, String> {
    let mode = string(row, "mode")?;
    if mode == "naked" || mode == "custom" {
        return Ok(serde_json::json!({
            "state": "performs-no-comparison-by-design",
            "comparison_tier": "declared-but-unverifiable",
            "reason": format!("{mode} mode does not perform canonical two-run comparison")
        }));
    }
    let reports = row
        .get("attempts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|attempt| attempt.get("verification_report").and_then(Value::as_str))
        .map(|report| {
            serde_json::from_str::<Value>(report)
                .map_err(|error| format!("embedded verification report is malformed: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let Some(report) = reports.last() else {
        return Ok(serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "declared-but-unverifiable",
            "reason": row.get("reason").and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("cell emitted no typed verification report")
        }));
    };
    let Some(tier) = comparison_tier(report) else {
        return Ok(serde_json::json!({
            "state": "performs-no-comparison-by-design",
            "comparison_tier": "declared-but-unverifiable",
            "reason": "typed report did not compare canonical nonzero INFO evidence"
        }));
    };
    let matched = report.get("verified").and_then(Value::as_bool) == Some(true)
        && report.get("verdict").and_then(Value::as_str) == Some("matched")
        && report.get("bitwise_parity").and_then(Value::as_bool) == Some(true);
    let diverged = report.get("verdict").and_then(Value::as_str) == Some("diverged")
        && report.get("bitwise_parity").and_then(Value::as_bool) == Some(false);
    if !matched && !diverged {
        return Ok(serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "declared-but-unverifiable",
            "reason": "typed canonical report was neither a match nor a divergence"
        }));
    }
    Ok(serde_json::json!({
        "state": if matched { "compared-and-matched" } else { "compared-and-diverged" },
        "comparison_tier": tier,
        "comparison": report.get("comparison").cloned().ok_or("report omitted comparison")?,
        "bitwise_parity": report.get("bitwise_parity").cloned().ok_or("report omitted bitwise_parity")?,
        "compared_log_messages": report.get("compared_log_messages").cloned().ok_or("report omitted compared_log_messages")?
    }))
}

fn sort_key(value: &Value) -> Result<(String, String, String, String, String), String> {
    Ok((
        string(value, "lane")?.into(),
        string(value, "category")?.into(),
        string(value, "test")?.into(),
        string(value, "mode")?.into(),
        string(value, "backend")?.into(),
    ))
}

fn parent_root(repo_root: &Path) -> PathBuf {
    std::env::var_os("DEV_HERMIT_PARENT")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.to_path_buf())
}

/// Transform all result rows for one validate invocation into the closed
/// schema-6 cell-verdict artifact and summary used by ci-hub.
pub fn retain(
    repo_root: &Path,
    result_root: &Path,
    commit: &str,
) -> Result<RetainedCellResults, String> {
    retain_at(&parent_root(repo_root), result_root, commit)
}

fn retain_at(
    parent: &Path,
    result_root: &Path,
    commit: &str,
) -> Result<RetainedCellResults, String> {
    let mut files = Vec::new();
    collect_results_files(result_root, &mut files)?;
    files.sort();
    let mut run_id: Option<String> = None;
    let mut cells = Vec::new();
    let mut selected = Vec::new();
    let mut identities = BTreeSet::new();
    for file in files {
        let text = fs::read_to_string(&file)
            .map_err(|error| format!("cannot read {}: {error}", file.display()))?;
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let row: Value = serde_json::from_str(line).map_err(|error| {
                format!(
                    "{}:{} malformed result row: {error}",
                    file.display(),
                    line_number + 1
                )
            })?;
            if row.get("schema").and_then(Value::as_u64) != Some(4)
                || string(&row, "hermit_sha")? != commit
                || row.get("source_tree_dirty").and_then(Value::as_bool) != Some(false)
            {
                return Err(format!(
                    "{}:{} is not an exact clean schema-4 cell result for {commit}",
                    file.display(),
                    line_number + 1
                ));
            }
            let row_run_id = string(&row, "run_id")?;
            match run_id.as_deref() {
                None => run_id = Some(row_run_id.into()),
                Some(existing) if existing == row_run_id => {}
                Some(existing) => {
                    return Err(format!(
                        "per-cell results mix run_id {existing} with {row_run_id}"
                    ));
                }
            }
            let id = identity(&row)?;
            let key = sort_key(&id)?;
            if !identities.insert(key) {
                return Err("per-cell results contain a duplicate identity".into());
            }
            let mut cell = id.as_object().expect("identity object").clone();
            cell.insert("cell_verdict".into(), cell_verdict(&row)?);
            selected.push(id);
            cells.push(Value::Object(cell));
        }
    }
    let run_id = run_id.ok_or("full validation retained zero per-cell result rows")?;
    selected.sort_by_key(|value| sort_key(value).expect("validated identity"));
    cells.sort_by_key(|value| sort_key(value).expect("validated identity"));
    let population_bytes = serde_json::to_vec(&selected)
        .map_err(|error| format!("cannot encode selected cell population: {error}"))?;
    let artifact_dir = parent
        .join("ignored")
        .join("validate")
        .join("artifacts")
        .join(&run_id);
    fs::create_dir_all(&artifact_dir).map_err(|error| {
        format!(
            "cannot create retained cell artifact {}: {error}",
            artifact_dir.display()
        )
    })?;
    let artifact = artifact_dir.join("cell-results.jsonl");
    let mut artifact_bytes = Vec::new();
    for cell in &cells {
        let mut record = serde_json::Map::new();
        record.insert("run_id".into(), Value::String(run_id.clone()));
        record.insert("hermit_sha".into(), Value::String(commit.into()));
        record.insert("source_tree_dirty".into(), Value::Bool(false));
        for key in IDENTITY_KEYS {
            record.insert(key.into(), cell[key].clone());
        }
        record.insert("cell_verdict".into(), cell["cell_verdict"].clone());
        serde_json::to_writer(&mut artifact_bytes, &record)
            .map_err(|error| format!("cannot encode retained cell row: {error}"))?;
        artifact_bytes.push(b'\n');
    }
    fs::write(&artifact, &artifact_bytes).map_err(|error| {
        format!(
            "cannot publish retained cell artifact {}: {error}",
            artifact.display()
        )
    })?;
    let relative = artifact
        .strip_prefix(parent)
        .map_err(|_| "retained cell artifact is outside parent root")?
        .to_string_lossy()
        .into_owned();
    Ok(RetainedCellResults {
        run_id: run_id.clone(),
        evidence: serde_json::json!({
            "run_id": run_id,
            "hermit_sha": commit,
            "source_tree_dirty": false,
            "selected_count": selected.len(),
            "recorded_count": cells.len(),
            "population_sha256": hex_digest(&population_bytes),
            "artifact": {
                "path": relative,
                "sha256": hex_digest(&artifact_bytes),
                "row_count": cells.len()
            },
            "selected": selected,
            "cells": cells
        }),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture_root() -> PathBuf {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("validate-cell-results-{}-{id}", std::process::id()))
    }

    fn result_row(run_id: &str, commit: &str) -> Value {
        serde_json::json!({
            "schema": 4,
            "run_id": run_id,
            "hermit_sha": commit,
            "source_tree_dirty": false,
            "lane": "portable",
            "category": "c-programs",
            "test": "uname",
            "mode": "verify",
            "backend": "ptrace",
            "outcome": "PASS",
            "reason": null,
            "attempts": [{
                "verification_report": serde_json::json!({
                    "verified": true,
                    "verdict": "matched",
                    "bitwise_parity": true,
                    "comparison": {
                        "strictness": "canonical",
                        "compare_logs": true,
                        "log_scope": "info",
                        "strip_lines": false,
                        "canonicalize_addresses": true,
                        "full_trace": true,
                        "exact_remainder": true,
                        "stripped_prefixes": [],
                        "canonicalizations": ["bitwise-info-v1"],
                        "ignore_lines": false,
                        "skip_commit": false,
                        "skip_detlog": false
                    },
                    "compared_log_messages": {"left": 123, "right": 123}
                }).to_string()
            }]
        })
    }

    fn write_result(root: &Path, row: &Value) {
        let directory = root.join("bucket");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("results.jsonl"),
            format!("{}\n", serde_json::to_string(row).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn retains_one_closed_schema6_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1111111111111111111111111111111111111111";
        write_result(&results, &result_row("validate-one", commit));
        let retained = retain_at(&root, &results, commit).unwrap();
        assert_eq!(retained.run_id, "validate-one");
        assert_eq!(retained.evidence["selected_count"], 1);
        assert_eq!(retained.evidence["recorded_count"], 1);
        assert_eq!(
            retained.evidence["population_sha256"],
            "6134692d215e0bd6a4da5514206e3670a377cf32dae1b507453560e62cb41557"
        );
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        let artifact = root.join(retained.evidence["artifact"]["path"].as_str().unwrap());
        let bytes = fs::read(&artifact).unwrap();
        assert_eq!(hex_digest(&bytes), retained.evidence["artifact"]["sha256"]);
        assert!(bytes.ends_with(b"\n"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_mixed_bucket_run_ids() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "2222222222222222222222222222222222222222";
        write_result(&results, &result_row("validate-one", commit));
        let second = results.join("other");
        write_result(&second, &result_row("validate-two", commit));
        let error = retain_at(&root, &results, commit).unwrap_err();
        assert!(error.contains("mix run_id"));
        fs::remove_dir_all(root).unwrap();
    }
}
