// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// Retain the per-cell result population carried by one full validate run.

use std::collections::BTreeMap;
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
        && comparison.get("compare_logs")?.as_bool()?
        && comparison.get("log_scope")?.as_str()? == "info"
        && !comparison.get("strip_lines")?.as_bool()?
        && comparison.get("canonicalize_addresses")?.as_bool()?
        && comparison.get("full_trace")?.as_bool()?
        && comparison.get("exact_remainder")?.as_bool()?
        && !comparison.get("ignore_lines")?.as_bool()?
        && !comparison.get("skip_commit")?.as_bool()?
        && !comparison.get("skip_detlog")?.as_bool()?
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
    let Some(attempts) = row.get("attempts").and_then(Value::as_array) else {
        return Ok(serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "declared-but-unverifiable",
            "reason": "cell emitted no typed attempts"
        }));
    };
    let mut reports = Vec::new();
    let mut unavailable_reason = None;
    for (index, attempt) in attempts.iter().enumerate() {
        let Some(raw) = attempt.get("verification_report").and_then(Value::as_str) else {
            unavailable_reason = Some(format!(
                "attempt {} emitted no typed verification report",
                index + 1
            ));
            continue;
        };
        let expected_sha = attempt
            .get("verification_report_sha256")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("attempt {} omitted verification_report_sha256", index + 1))?;
        if hex_digest(raw.as_bytes()) != expected_sha {
            return Err(format!(
                "attempt {} verification_report_sha256 mismatch",
                index + 1
            ));
        }
        let report = serde_json::from_str::<Value>(raw).map_err(|error| {
            format!(
                "attempt {} verification report is malformed: {error}",
                index + 1
            )
        })?;
        if comparison_tier(&report).is_none() {
            unavailable_reason = Some(format!(
                "attempt {} did not compare canonical nonzero INFO evidence",
                index + 1
            ));
        }
        reports.push(report);
    }
    let classify = |report: &Value| {
        let matched = report.get("verified").and_then(Value::as_bool) == Some(true)
            && report.get("verdict").and_then(Value::as_str) == Some("matched")
            && report.get("bitwise_parity").and_then(Value::as_bool) == Some(true);
        let diverged = report.get("verdict").and_then(Value::as_str) == Some("diverged")
            && report.get("bitwise_parity").and_then(Value::as_bool) == Some(false);
        (matched, diverged)
    };
    // A genuine canonical divergence is sticky across sibling attempts. Missing
    // or weaker evidence may prevent a clean leg, but it must never erase a red
    // leg merely because it was observed before or after that divergence.
    if let Some(report) = reports
        .iter()
        .find(|report| comparison_tier(report).is_some() && classify(report).1)
    {
        return Ok(serde_json::json!({
            "state": "compared-and-diverged",
            "comparison_tier": "canonical-bitwise",
            "comparison": report.get("comparison").cloned().ok_or("report omitted comparison")?,
            "bitwise_parity": report.get("bitwise_parity").cloned().ok_or("report omitted bitwise_parity")?,
            "compared_log_messages": report.get("compared_log_messages").cloned().ok_or("report omitted compared_log_messages")?
        }));
    }
    if reports.is_empty() || unavailable_reason.is_some() {
        return Ok(serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "declared-but-unverifiable",
            "reason": unavailable_reason.unwrap_or_else(|| "cell emitted no typed verification report".into())
        }));
    }
    if reports.iter().any(|report| !classify(report).0) {
        return Ok(serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "declared-but-unverifiable",
            "reason": "typed canonical report was neither a match nor a divergence"
        }));
    }
    if string(row, "outcome")? != "PASS" {
        return Ok(serde_json::json!({
            "state": "unavailable-with-reason",
            "comparison_tier": "declared-but-unverifiable",
            "reason": row.get("reason").and_then(Value::as_str)
                .filter(|reason| !reason.trim().is_empty())
                .unwrap_or("cell outcome was not PASS despite matched comparison evidence")
        }));
    }
    let report = reports.last().expect("nonempty reports");
    Ok(serde_json::json!({
        "state": "compared-and-matched",
        "comparison_tier": "canonical-bitwise",
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

pub fn expected_plan(repo_root: &Path) -> Result<Vec<Value>, String> {
    let path = repo_root.join("ci/expected-e2e-plan.json");
    let document: Value = serde_json::from_str(
        &fs::read_to_string(&path)
            .map_err(|error| format!("cannot read expected plan {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("expected plan {} is malformed: {error}", path.display()))?;
    document
        .get("cells")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("expected plan {} has no cells array", path.display()))?
        .iter()
        .map(identity)
        .collect()
}

/// Transform all result rows for one validate invocation into the closed
/// schema-6 cell-verdict artifact and summary used by ci-hub.
pub fn retain(
    parent: &Path,
    result_root: &Path,
    commit: &str,
    expected: &[Value],
) -> Result<RetainedCellResults, String> {
    let mut files = Vec::new();
    collect_results_files(result_root, &mut files)?;
    files.sort();
    let mut run_id: Option<String> = None;
    let mut selected = Vec::new();
    let mut identities = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut terminal_rows = BTreeMap::new();
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
            let attempt = row.get("attempt").and_then(Value::as_u64).unwrap_or(1);
            if attempt == 0 {
                return Err("per-cell result attempt must be positive".into());
            }
            if !observations.insert((key.clone(), attempt)) {
                return Err("per-cell results contain a duplicate identity and attempt".into());
            }
            if identities.insert(key.clone()) {
                selected.push(id);
            }
            if terminal_rows
                .get(&key)
                .is_none_or(|(seen, _): &(u64, Value)| attempt > *seen)
            {
                terminal_rows.insert(key, (attempt, row));
            }
        }
    }
    let mut cells = terminal_rows
        .into_values()
        .map(|(_, row)| {
            let id = identity(&row)?;
            let mut cell = id.as_object().expect("identity object").clone();
            cell.insert("cell_verdict".into(), cell_verdict(&row)?);
            Ok(Value::Object(cell))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let run_id = run_id.ok_or("full validation retained zero per-cell result rows")?;
    selected.sort_by_key(|value| sort_key(value).expect("validated identity"));
    cells.sort_by_key(|value| sort_key(value).expect("validated identity"));
    let mut expected = expected.to_vec();
    expected.sort_by_key(|value| sort_key(value).expect("validated expected identity"));
    if selected != expected {
        let observed_keys = selected
            .iter()
            .map(sort_key)
            .collect::<Result<BTreeSet<_>, _>>()?;
        let expected_keys = expected
            .iter()
            .map(sort_key)
            .collect::<Result<BTreeSet<_>, _>>()?;
        let missing = expected_keys.difference(&observed_keys).count();
        let extra = observed_keys.difference(&expected_keys).count();
        return Err(format!(
            "per-cell results differ from the exact planned population: {missing} missing, {extra} extra"
        ));
    }
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

    fn report(verdict: &str, log_scope: &str) -> String {
        let matched = verdict == "matched";
        serde_json::json!({
            "verified": matched,
            "verdict": verdict,
            "bitwise_parity": matched,
            "comparison": {
                "strictness": "canonical",
                "compare_logs": true,
                "log_scope": log_scope,
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
            "compared_log_messages": {"left": 123, "right": if matched { 123 } else { 124 }}
        })
        .to_string()
    }

    fn attempt(raw: &str) -> Value {
        serde_json::json!({
            "verification_report": raw,
            "verification_report_sha256": hex_digest(raw.as_bytes())
        })
    }

    fn result_row(run_id: &str, commit: &str) -> Value {
        let matched = report("matched", "info");
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
            "attempts": [attempt(&matched)]
        })
    }

    fn expected(row: &Value) -> Vec<Value> {
        vec![identity(row).unwrap()]
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

    fn append_result_row(root: &Path, row: &Value) {
        use std::io::Write;

        let directory = root.join("bucket");
        fs::create_dir_all(&directory).unwrap();
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("results.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(row).unwrap()).unwrap();
    }

    #[test]
    fn retains_one_closed_schema6_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "1111111111111111111111111111111111111111";
        let row = result_row("validate-one", commit);
        write_result(&results, &row);
        let retained = retain(&root, &results, commit, &expected(&row)).unwrap();
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
    fn retains_terminal_attempt_without_rejecting_preserved_history() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "abababababababababababababababababababab";
        let mut first = result_row("validate-retry", commit);
        first["attempt"] = Value::from(1);
        first["outcome"] = Value::String("FAIL".into());
        first["reason"] = Value::String("forced first failure".into());
        first["duration_ms"] = Value::from(111);
        first["timeout_seconds"] = Value::from(15);
        let mut second = result_row("validate-retry", commit);
        second["attempt"] = Value::from(2);
        second["duration_ms"] = Value::from(222);
        second["timeout_seconds"] = Value::from(15);
        append_result_row(&results, &first);
        append_result_row(&results, &second);

        let retained = retain(&root, &results, commit, &expected(&second)).unwrap();
        assert_eq!(retained.evidence["recorded_count"], 1);
        assert_eq!(
            retained.evidence["cells"][0]["cell_verdict"]["state"],
            "compared-and-matched"
        );
        let raw = fs::read_to_string(results.join("bucket/results.jsonl")).unwrap();
        let observations = raw
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0]["attempt"], 1);
        assert_eq!(observations[0]["duration_ms"], 111);
        assert_eq!(observations[0]["timeout_seconds"], 15);
        assert_eq!(observations[1]["attempt"], 2);
        assert_eq!(observations[1]["duration_ms"], 222);
        assert_eq!(observations[1]["timeout_seconds"], 15);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_mixed_bucket_run_ids() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "2222222222222222222222222222222222222222";
        let first = result_row("validate-one", commit);
        write_result(&results, &first);
        let second = results.join("other");
        write_result(&second, &result_row("validate-two", commit));
        let error = retain(&root, &results, commit, &expected(&first)).unwrap_err();
        assert!(error.contains("mix run_id"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn top_level_failure_cannot_project_a_matched_attempt_as_clean() {
        let mut row = result_row("failed-row", "3333333333333333333333333333333333333333");
        row["outcome"] = Value::String("FAIL".into());
        row["reason"] = Value::String("SaBRe interception path was incomplete".into());
        let verdict = cell_verdict(&row).unwrap();
        assert_eq!(verdict["state"], "unavailable-with-reason");
    }

    #[test]
    fn earlier_divergence_cannot_be_hidden_by_a_final_match() {
        let mut row = result_row("retry-row", "4444444444444444444444444444444444444444");
        let diverged = report("diverged", "info");
        let matched = report("matched", "info");
        row["attempts"] = Value::Array(vec![attempt(&diverged), attempt(&matched)]);
        let verdict = cell_verdict(&row).unwrap();
        assert_eq!(verdict["state"], "compared-and-diverged");
    }

    #[test]
    fn missing_sibling_attempt_cannot_erase_a_divergence_in_either_order() {
        let diverged = report("diverged", "info");
        let missing = serde_json::json!({"outcome": "ERROR"});
        for attempts in [
            vec![attempt(&diverged), missing.clone()],
            vec![missing.clone(), attempt(&diverged)],
        ] {
            let mut row = result_row(
                "missing-sibling",
                "8888888888888888888888888888888888888888",
            );
            row["attempts"] = Value::Array(attempts);
            let verdict = cell_verdict(&row).unwrap();
            assert_eq!(verdict["state"], "compared-and-diverged");
        }
    }

    #[test]
    fn non_info_sibling_attempt_cannot_erase_a_divergence() {
        let diverged = report("diverged", "info");
        let weaker = report("matched", "deterministic");
        let mut row = result_row("weaker-sibling", "9999999999999999999999999999999999999999");
        row["attempts"] = Value::Array(vec![attempt(&weaker), attempt(&diverged)]);
        let verdict = cell_verdict(&row).unwrap();
        assert_eq!(verdict["state"], "compared-and-diverged");
    }

    #[test]
    fn non_info_scope_never_becomes_a_clean_leg() {
        let mut row = result_row("scope-row", "5555555555555555555555555555555555555555");
        let deterministic = report("matched", "deterministic");
        row["attempts"] = Value::Array(vec![attempt(&deterministic)]);
        let verdict = cell_verdict(&row).unwrap();
        assert_eq!(verdict["state"], "unavailable-with-reason");
    }

    #[test]
    fn mismatched_report_hash_refuses_the_receipt() {
        let mut row = result_row("hash-row", "7777777777777777777777777777777777777777");
        row["attempts"][0]["verification_report_sha256"] = Value::String("0".repeat(64));
        let error = cell_verdict(&row).unwrap_err();
        assert!(error.contains("verification_report_sha256 mismatch"));
    }

    #[test]
    fn missing_planned_cell_refuses_the_population() {
        let root = fixture_root();
        let results = root.join("results");
        let commit = "6666666666666666666666666666666666666666";
        let row = result_row("missing-row", commit);
        write_result(&results, &row);
        let mut plan = expected(&row);
        let mut missing = plan[0].clone();
        missing["test"] = Value::String("c-programs/missing".into());
        plan.push(missing);
        let error = retain(&root, &results, commit, &plan).unwrap_err();
        assert!(error.contains("1 missing, 0 extra"));
        fs::remove_dir_all(root).unwrap();
    }
}
