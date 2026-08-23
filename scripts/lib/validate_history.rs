// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Two readers of the validate-run ledger: the TREE-KEYED result cache
//! (`cache_lookup_record`, validate.sh:620) and the runtime ESTIMATE
//! (`history_estimate`, validate.sh:936).
//!
//! # The cache predicate must match the PRODUCER and its evidence
//!
//! Both producers require `executed_tests > 0`: a missing count and a measured
//! zero are distinct facts, but neither proves that a test-bearing run completed.
//! The Rust driver carries that count from typed step outcomes, independently of
//! human-facing log verbosity. It also writes `executed_nodes`, because a
//! ~47-NODE DAG run must never be readable as a 47-TEST pass (see `write_ledger`
//! in `validate.rs`).
//!
//! So the predicate is dispatched on the row's own `producer` field: a Rust
//! validate row must carry `executed_tests > 0`, `executed_nodes > 0`,
//! **and** a satisfied coverage record; the versioned Git-provenance producer
//! must additionally carry its complete, landing-qualified frozen Git snapshot;
//! a bash row must carry `executed_tests > 0`. That is one verifier per
//! authority rather than one generic field test, and a Rust row cannot
//! substitute its node count for test evidence.
//!
//! # Fail-open, never fail-hit
//!
//! Every unreadable/absent/ambiguous condition yields "no hit" and a real run.
//! A missing ledger, a malformed line, an unknown producer, an absent coverage
//! block — none of them can manufacture a reuse.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// A qualifying prior run, with enough context that the printed banner cannot
/// misdescribe what was reused.
#[derive(Clone, Debug)]
pub struct CacheHit {
    pub finished_at: String,
    pub real_seconds: f64,
    pub cpu_seconds: f64,
    /// The count the producer actually recorded.
    pub executed: i64,
    /// What that count COUNTS. Never collapsed: "test(s)" for a bash row,
    /// "node(s)" for a validate.rs row.
    pub executed_unit: &'static str,
    pub commit: String,
    pub producer: String,
}

/// Read the one logical ledger into rows, skipping unparseable lines.
///
/// In an admitted dev-hermit run, `ledger` is the parent's logical `ledger/`
/// root.  The parent adapter owns sharding and union semantics; opening one
/// shard (or the retired raw shadow file) here would create a second receipt
/// authority.  An ordinary file remains supported only for isolated fixtures
/// and genuinely standalone checkouts.
pub fn read_rows(ledger: &Path) -> Vec<serde_json::Value> {
    let explicit = std::env::var("HERMIT_VALIDATE_LEDGER")
        .ok()
        .filter(|value| !value.is_empty())
        .is_some_and(|value| Path::new(&value) == ledger);
    let text = if !explicit && ledger.file_name().is_some_and(|name| name == "ledger") {
        let Some(parent) = ledger.parent() else { return Vec::new() };
        let adapter = parent.join("ci-hub/ledger/validate_rows.py");
        let Ok(output) = Command::new("python3").arg(&adapter).arg("rows").output() else {
            eprintln!(
                "validate: warning: cannot launch canonical ledger reader {}",
                adapter.display()
            );
            return Vec::new();
        };
        if !output.status.success() {
            eprintln!(
                "validate: warning: canonical ledger reader {} refused: {}",
                adapter.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            );
            return Vec::new();
        }
        let Ok(text) = String::from_utf8(output.stdout) else {
            eprintln!("validate: warning: canonical ledger reader emitted non-UTF-8 data");
            return Vec::new();
        };
        text
    } else {
        let Ok(text) = std::fs::read_to_string(ledger) else { return Vec::new() };
        text
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

fn s<'a>(row: &'a serde_json::Value, k: &str) -> &'a str {
    row.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

fn i(row: &serde_json::Value, k: &str) -> Option<i64> {
    row.get(k).and_then(|v| v.as_i64())
}

fn f(row: &serde_json::Value, k: &str) -> f64 {
    row.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

/// Identity of the run whose result may be reused. Every field here is part of
/// the cache KEY, so a value that differs on any of them is a different run.
pub struct CacheKey<'a> {
    pub tree: &'a str,
    pub profile: &'a str,
    pub host: &'a str,
    pub toolchain: &'a str,
}

/// The gate-coverage half of the predicate, shared by both producers.
fn gate_coverage_ok(row: &serde_json::Value) -> bool {
    match (i(row, "gates_expected"), i(row, "gates_run")) {
        (None, _) => true, // gates_expected null: no obligation was recorded
        (Some(exp), Some(run)) => run >= exp,
        (Some(_), None) => false,
    }
}

fn rust_producer(producer: &str) -> bool {
    matches!(
        producer,
        "validate.rs" | "hermit-validate-rs" | "hermit-validate-rs-git-provenance-v1"
    )
}

const GIT_PROVENANCE_PRODUCER: &str = "hermit-validate-rs-git-provenance-v1";

fn sha40(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The versioned writer promises a complete frozen Git snapshot. A cache hit
/// skips the real validator, so it must enforce the same landing-qualified
/// evidence as the canonical receipt consumer rather than trusting the slug.
fn git_provenance_cache_ok(row: &serde_json::Value) -> bool {
    if row.get("git_provenance_version").and_then(|v| v.as_u64()) != Some(1)
        || !row
            .get("git_depth")
            .and_then(|v| v.as_u64())
            .is_some_and(|v| v > 0)
        || row.get("git_is_shallow").and_then(|v| v.as_bool()) != Some(false)
        || s(row, "git_comparison_ref") != "origin/main"
        || row.get("git_ahead").and_then(|v| v.as_u64()).is_none()
        || row.get("git_behind").and_then(|v| v.as_u64()) != Some(0)
    {
        return false;
    }
    let comparison = s(row, "git_comparison_sha");
    let base = s(row, "base_sha");
    sha40(comparison) && comparison == base
}

fn legacy_rust_coverage_cache_ok(row: &serde_json::Value) -> bool {
    match row.get("coverage") {
        None => false,
        Some(coverage) => {
            let absent = coverage
                .get("absent_nodes")
                .and_then(|value| value.as_array())
                .map(Vec::len);
            let executed = coverage
                .get("executed_test_nodes")
                .and_then(|value| value.as_i64());
            matches!(absent, Some(0)) && executed.is_some()
        }
    }
}

/// The capability-bearing writer always emits the complete canonical coverage
/// object. Reusing its PASS requires that exact positive claim: a nonempty plan,
/// every planned test node accounted as executed, and both explicit failure
/// arrays present and empty. Legacy Rust slugs retain their historical reader.
fn versioned_rust_coverage_cache_ok(row: &serde_json::Value) -> bool {
    let Some(coverage) = row.get("coverage").and_then(|value| value.as_object()) else {
        return false;
    };
    let Some(planned) = coverage.get("planned_test_nodes").and_then(|value| value.as_u64()) else {
        return false;
    };
    let Some(executed) = coverage.get("executed_test_nodes").and_then(|value| value.as_u64()) else {
        return false;
    };
    let explicit_empty = |field: &str| {
        coverage
            .get(field)
            .and_then(|value| value.as_array())
            .is_some_and(Vec::is_empty)
    };
    planned > 0
        && executed == planned
        && explicit_empty("zero_executed_nodes")
        && explicit_empty("absent_nodes")
}

/// Does this PASS row carry everything a reuse needs?
///
/// Dispatched on `producer`; an unrecognized producer is REFUSED rather than
/// guessed at, so a future third writer cannot be silently cached under
/// whichever field name happens to be present.
fn pass_row_qualifies(row: &serde_json::Value) -> bool {
    if i(row, "failures") != Some(0) {
        return false;
    }
    if !gate_coverage_ok(row) {
        return false;
    }
    match s(row, "producer") {
        producer if rust_producer(producer) => {
            if i(row, "executed_tests").unwrap_or(0) <= 0 {
                return false;
            }
            if i(row, "executed_nodes").unwrap_or(0) <= 0 {
                return false;
            }
            if producer == GIT_PROVENANCE_PRODUCER && !git_provenance_cache_ok(row) {
                return false;
            }
            // Coverage is a first-class part of the claim. The new capability
            // slug is held to its complete canonical shape; historical Rust
            // rows retain the evidence contract their writers actually made.
            if producer == GIT_PROVENANCE_PRODUCER {
                versioned_rust_coverage_cache_ok(row)
            } else {
                legacy_rust_coverage_cache_ok(row)
            }
        }
        // A validate.sh-era row has no typed node/coverage evidence, so its
        // positive test count is the producer-specific execution proof.
        "" | "validate.sh" => i(row, "executed_tests").unwrap_or(0) > 0,
        _ => false,
    }
}

/// Newest qualifying record for `want_result`, or `None`.
///
/// Port of `cache_lookup_record` (validate.sh:620) with the producer-aware
/// predicate described in the module doc.
pub fn cache_lookup(
    rows: &[serde_json::Value],
    want_result: &str,
    key: &CacheKey,
) -> Option<CacheHit> {
    if key.tree.is_empty() || key.tree == "unknown" {
        return None;
    }
    let mut best: Option<&serde_json::Value> = None;
    for row in rows {
        if s(row, "tree") != key.tree
            || s(row, "profile") != key.profile
            || s(row, "host") != key.host
            || s(row, "toolchain") != key.toolchain
            || s(row, "selection_mode") != "full"
            || s(row, "result") != want_result
        {
            continue;
        }
        if row.get("commit_anchored").and_then(|v| v.as_bool()) != Some(true) {
            continue;
        }
        if row.get("tree_dirty").and_then(|v| v.as_bool()) != Some(false) {
            continue;
        }
        if want_result == "pass" && !pass_row_qualifies(row) {
            continue;
        }
        let newer = match best {
            None => true,
            Some(b) => s(row, "finished_at") >= s(b, "finished_at"),
        };
        if newer {
            best = Some(row);
        }
    }
    let row = best?;
    let producer = s(row, "producer");
    let (executed, unit) = if rust_producer(producer) {
        (i(row, "executed_nodes").unwrap_or(0), "node(s)")
    } else {
        (i(row, "executed_tests").unwrap_or(0), "test(s)")
    };
    Some(CacheHit {
        finished_at: s(row, "finished_at").to_string(),
        real_seconds: f(row, "real_seconds"),
        cpu_seconds: f(row, "user_seconds") + f(row, "sys_seconds"),
        executed,
        executed_unit: unit,
        commit: {
            let c = s(row, "commit");
            if c.is_empty() { "unknown".to_string() } else { c.to_string() }
        },
        producer: if producer.is_empty() { "validate.sh".into() } else { producer.into() },
    })
}

// ------------------------------------------------------------------ estimate

/// Minimum samples before a scope is reported (`MIN` in validate.sh:993).
const MIN_SAMPLES: usize = 3;

fn human(secs: f64) -> String {
    let x = (secs + 0.5) as i64;
    let (h, m, s) = (x / 3600, (x % 3600) / 60, x % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

fn emit(mut v: Vec<f64>, scope: &str) -> String {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    let lo = v[0];
    let hi = v[n - 1];
    let md = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
    if (lo - hi).abs() < f64::EPSILON {
        format!("~{} ({scope}, n={n})", human(md))
    } else {
        format!("~{} (median; range {}-{}; {scope}, n={n})", human(md), human(lo), human(hi))
    }
}

/// Port of `history_estimate` (validate.sh:936).
///
/// Only successful runs of the SAME profile count — a fast-failing or timed-out
/// run is not a representative completion time. Buckets degrade from
/// (cache, host) to (cache, any host) to (any cache, any host) and, when even
/// the broadest is too thin, SAY SO rather than fabricating a number.
pub fn history_estimate(
    rows: &[serde_json::Value],
    profile: &str,
    cache_state: &str,
    host: &str,
    have_ledger: bool,
) -> String {
    if !have_ledger {
        return "no measured estimate yet (no run-history ledger; this run seeds it)".into();
    }
    let (mut t1, mut t2, mut t3) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows {
        if s(row, "profile") != profile || s(row, "result") != "pass" {
            continue;
        }
        let w = f(row, "real_seconds");
        if w <= 0.0 {
            continue;
        }
        t3.push(w);
        if s(row, "cache_state") == cache_state {
            t2.push(w);
            if s(row, "host") == host {
                t1.push(w);
            }
        }
    }
    if t1.len() >= MIN_SAMPLES {
        emit(t1, &format!("{cache_state} cache, {host}, this profile"))
    } else if t2.len() >= MIN_SAMPLES {
        emit(t2, &format!("{cache_state} cache, any host, this profile"))
    } else if t3.len() >= MIN_SAMPLES {
        emit(
            t3,
            &format!(
                "MIXED warm/cold -- no {cache_state}-specific history yet, treat as a wide prior; \
                 this profile"
            ),
        )
    } else {
        format!(
            "insufficient history to estimate (only {} prior successful {profile} run(s); need \
             >={MIN_SAMPLES}). Current cache: {cache_state}. This run seeds the estimate.",
            t3.len()
        )
    }
}

// ----------------------------------------------------------------- selective

/// Resolve the last-known-green baseline for `--selective`
/// (`resolve_selective_baseline`, validate.sh:4364).
///
/// Precedence: explicit `--baseline`, then `$HERMIT_LAST_GREEN_SHA`, then the
/// most recent passing ledger row (preferring this slot). Only a commit that
/// EXISTS locally is returned; anything else yields `None` so selection fails
/// safe to the full lane. Never fail-open on a stale or missing baseline.
pub fn selective_baseline(
    rows: &[serde_json::Value],
    explicit: Option<&str>,
    slot: &str,
    commit_exists: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let mut sha: Option<String> = explicit.map(|s| s.to_string());
    if sha.is_none() {
        sha = std::env::var("HERMIT_LAST_GREEN_SHA").ok().filter(|v| !v.is_empty());
    }
    if sha.is_none() {
        // `tail -n 1` in the bash: LAST matching line, i.e. append order, not
        // finished_at order. Preserved so both drivers pick the same baseline
        // from the same shard.
        let pick = |want_slot: Option<&str>| -> Option<String> {
            rows.iter()
                .rev()
                .find(|r| {
                    s(r, "result") == "pass"
                        && s(r, "commit") != "unknown"
                        && !s(r, "commit").is_empty()
                        && want_slot.map(|w| s(r, "slot") == w).unwrap_or(true)
                })
                .map(|r| s(r, "commit").to_string())
        };
        sha = pick(Some(slot)).or_else(|| pick(None));
    }
    let sha = sha?;
    if commit_exists(&sha) { Some(sha) } else { None }
}

// ----------------------------------------------------------------- self-test

/// Inert brackets. These construct synthetic ledger rows in memory; nothing here
/// reads the real ledger, runs a gate, or publishes anything.
pub fn self_test() -> Result<String, String> {
    let base = |extra: serde_json::Value| -> serde_json::Value {
        let mut v = serde_json::json!({
            "tree": "T", "profile": "full", "host": "h1", "toolchain": "rustc 1.0",
            "selection_mode": "full", "result": "pass", "commit_anchored": true,
            "tree_dirty": false, "failures": 0, "commit": "c0ffee",
            "finished_at": "2026-08-07T00:00:00Z", "real_seconds": 100,
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                o.insert(k.clone(), val.clone());
            }
        }
        v
    };
    let key = CacheKey { tree: "T", profile: "full", host: "h1", toolchain: "rustc 1.0" };

    // POSITIVE, both producers. A predicate that refuses everything would look
    // correct with negatives alone, so each authority gets a counted accept.
    let rs_pass = base(serde_json::json!({
        "producer": "hermit-validate-rs-git-provenance-v1", "executed_tests": 873, "executed_nodes": 47,
        "gates_expected": 47, "gates_run": 47,
        "coverage": {
            "planned_test_nodes": 20,
            "executed_test_nodes": 20,
            "zero_executed_nodes": [],
            "absent_nodes": []
        },
        "git_provenance_version": 1, "git_depth": 1881, "git_is_shallow": false,
        "git_comparison_ref": "origin/main",
        "git_comparison_sha": "1111111111111111111111111111111111111111",
        "git_ahead": 1, "git_behind": 0,
        "base_sha": "1111111111111111111111111111111111111111",
    }));
    let sh_pass = base(serde_json::json!({
        "producer": "validate.sh", "executed_tests": 1234, "gates_expected": 12, "gates_run": 12,
    }));
    let mut accepted = 0usize;
    for (why, row) in [("versioned Rust row", &rs_pass), ("validate.sh row", &sh_pass)] {
        if cache_lookup(std::slice::from_ref(row), "pass", &key).is_none() {
            return Err(format!("cache: a fully qualifying {why} must be a HIT"));
        }
        accepted += 1;
    }
    for producer in ["validate.rs", "hermit-validate-rs"] {
        let legacy = base(serde_json::json!({
            "producer": producer, "executed_tests": 873, "executed_nodes": 47,
            "gates_expected": 47, "gates_run": 47,
            "coverage": {"planned_test_nodes": 20, "executed_test_nodes": 20, "absent_nodes": []},
        }));
        if cache_lookup(std::slice::from_ref(&legacy), "pass", &key).is_none() {
            return Err(format!("cache: legacy Rust producer {producer} must remain readable"));
        }
        accepted += 1;
    }

    // NEGATIVE: every single missing condition must REFUSE. Each row below is
    // the positive row with exactly one field spoiled, so a refusal is
    // attributable to that field and nothing else.
    let negatives: Vec<(&str, serde_json::Value)> = vec![
        ("different tree", base(serde_json::json!({"tree": "OTHER", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("different profile", base(serde_json::json!({"profile": "quick", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("different host", base(serde_json::json!({"host": "h2", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("different toolchain", base(serde_json::json!({"toolchain": "rustc 2.0", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("selective run", base(serde_json::json!({"selection_mode": "selective", "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("not commit-anchored", base(serde_json::json!({"commit_anchored": false, "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("dirty tree", base(serde_json::json!({"tree_dirty": true, "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("nonzero failures", base(serde_json::json!({"failures": 1, "producer": "validate.rs", "executed_tests": 873, "executed_nodes": 1, "coverage": {"executed_test_nodes": 1, "absent_nodes": []}}))),
        ("validate.rs row with no executed_tests", base(serde_json::json!({"producer": "validate.rs", "executed_nodes": 47, "gates_expected": 47, "gates_run": 47, "coverage": {"planned_test_nodes": 20, "executed_test_nodes": 20, "absent_nodes": []}}))),
        ("validate.rs row with zero executed_tests", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 0, "executed_nodes": 47, "gates_expected": 47, "gates_run": 47, "coverage": {"planned_test_nodes": 20, "executed_test_nodes": 20, "absent_nodes": []}}))),
        ("zero executed nodes", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 0, "coverage": {"executed_test_nodes": 0, "absent_nodes": []}}))),
        ("absent coverage block", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 5}))),
        ("planned node never ran", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 5, "coverage": {"executed_test_nodes": 4, "absent_nodes": ["test.x"]}}))),
        ("gates_run below gates_expected", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 873, "executed_nodes": 5, "gates_expected": 47, "gates_run": 12, "coverage": {"executed_test_nodes": 5, "absent_nodes": []}}))),
        ("bash row with zero executed_tests", base(serde_json::json!({"producer": "validate.sh", "executed_tests": 0}))),
        ("bash row with no executed_tests", base(serde_json::json!({"producer": "validate.sh"}))),
        // The cross-producer trap this module exists to close: a validate.rs row
        // must NOT be admitted by the bash counter, and vice versa.
        ("validate.rs row carrying only executed_tests", base(serde_json::json!({"producer": "validate.rs", "executed_tests": 999}))),
        ("bash row carrying only executed_nodes", base(serde_json::json!({"producer": "validate.sh", "executed_nodes": 999}))),
        ("unknown producer", base(serde_json::json!({"producer": "some-other-tool", "executed_nodes": 9, "executed_tests": 9}))),
    ];
    let mut refused = 0usize;
    for (why, row) in &negatives {
        if cache_lookup(std::slice::from_ref(row), "pass", &key).is_some() {
            return Err(format!("cache: a row with {why} must NOT be a hit"));
        }
        refused += 1;
    }

    // The current producer's complete extension is part of cache authority.
    // Remove each field independently, then mutate the most dangerous
    // legal-looking values: shallow checkout, a different base, and behind.
    for field in [
        "git_provenance_version",
        "git_depth",
        "git_is_shallow",
        "git_comparison_ref",
        "git_comparison_sha",
        "git_ahead",
        "git_behind",
        "base_sha",
    ] {
        let mut row = rs_pass.clone();
        row.as_object_mut().unwrap().remove(field);
        if cache_lookup(std::slice::from_ref(&row), "pass", &key).is_some() {
            return Err(format!(
                "cache: versioned Rust row missing {field} must NOT be a hit"
            ));
        }
        refused += 1;
    }
    for (why, field, value) in [
        ("zero Git depth", "git_depth", serde_json::json!(0)),
        (
            "shallow Git checkout",
            "git_is_shallow",
            serde_json::json!(true),
        ),
        (
            "wrong Git comparison ref",
            "git_comparison_ref",
            serde_json::json!("refs/remotes/origin/main"),
        ),
        (
            "malformed Git comparison SHA",
            "git_comparison_sha",
            serde_json::json!("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ),
        (
            "Git comparison/base mismatch",
            "git_comparison_sha",
            serde_json::json!("2222222222222222222222222222222222222222"),
        ),
        ("negative Git ahead", "git_ahead", serde_json::json!(-1)),
        ("nonzero Git behind", "git_behind", serde_json::json!(1)),
    ] {
        let mut row = rs_pass.clone();
        row.as_object_mut().unwrap().insert(field.into(), value);
        if cache_lookup(std::slice::from_ref(&row), "pass", &key).is_some() {
            return Err(format!(
                "cache: versioned Rust row with {why} must NOT be a hit"
            ));
        }
        refused += 1;
    }

    for field in [
        "planned_test_nodes",
        "executed_test_nodes",
        "zero_executed_nodes",
        "absent_nodes",
    ] {
        let mut row = rs_pass.clone();
        row.get_mut("coverage")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .remove(field);
        if cache_lookup(std::slice::from_ref(&row), "pass", &key).is_some() {
            return Err(format!(
                "cache: versioned Rust coverage missing {field} must NOT be a hit"
            ));
        }
        refused += 1;
    }
    for (why, field, value) in [
        (
            "zero planned test nodes",
            "planned_test_nodes",
            serde_json::json!(0),
        ),
        (
            "executed/planned mismatch",
            "executed_test_nodes",
            serde_json::json!(19),
        ),
        (
            "nonempty zero-executed failures",
            "zero_executed_nodes",
            serde_json::json!(["test.zero"]),
        ),
        (
            "nonempty absent failures",
            "absent_nodes",
            serde_json::json!(["test.absent"]),
        ),
    ] {
        let mut row = rs_pass.clone();
        row.get_mut("coverage")
            .and_then(serde_json::Value::as_object_mut)
            .unwrap()
            .insert(field.into(), value);
        if cache_lookup(std::slice::from_ref(&row), "pass", &key).is_some() {
            return Err(format!(
                "cache: versioned Rust coverage with {why} must NOT be a hit"
            ));
        }
        refused += 1;
    }

    // The reused count must never be relabelled: a node count must print as
    // node(s) and a test count as test(s).
    let hit = cache_lookup(std::slice::from_ref(&rs_pass), "pass", &key).unwrap();
    if hit.executed_unit != "node(s)" || hit.executed != 47 {
        return Err("cache: a Rust validate hit must report 47 node(s), not tests".into());
    }
    let hit = cache_lookup(std::slice::from_ref(&sh_pass), "pass", &key).unwrap();
    if hit.executed_unit != "test(s)" || hit.executed != 1234 {
        return Err("cache: a validate.sh hit must report 1234 test(s), not nodes".into());
    }

    // A FAIL lookup must not require the pass conditions (a fail is noted, not reused).
    let failing = base(serde_json::json!({"result": "fail", "failures": 3, "producer": "validate.rs"}));
    if cache_lookup(std::slice::from_ref(&failing), "fail", &key).is_none() {
        return Err("cache: a prior FAIL record must be findable so it can be reported".into());
    }

    // Estimate brackets: below MIN it must SAY it is insufficient; at/above MIN
    // it must produce a median. A silently-fabricated number is the failure mode.
    let sample = |secs: i64| base(serde_json::json!({"cache_state": "warm", "real_seconds": secs, "producer": "validate.rs"}));
    let thin: Vec<serde_json::Value> = (0..MIN_SAMPLES - 1).map(|i| sample(100 + i as i64)).collect();
    let est = history_estimate(&thin, "full", "warm", "h1", true);
    if !est.contains("insufficient history") {
        return Err(format!("estimate: {} samples must be reported as insufficient", thin.len()));
    }
    let enough: Vec<serde_json::Value> = vec![sample(60), sample(120), sample(180)];
    let est = history_estimate(&enough, "full", "warm", "h1", true);
    if !est.starts_with("~2m00s") || !est.contains("n=3") {
        return Err(format!("estimate: median of 60/120/180 must be ~2m00s, got {est}"));
    }
    if !history_estimate(&enough, "full", "warm", "h1", false).contains("no run-history ledger") {
        return Err("estimate: a missing ledger must say so".into());
    }
    // A failing run must never contribute to a completion-time estimate.
    let poisoned: Vec<serde_json::Value> = vec![
        sample(60),
        sample(120),
        base(serde_json::json!({"cache_state": "warm", "real_seconds": 5, "result": "fail", "producer": "validate.rs"})),
    ];
    if !history_estimate(&poisoned, "full", "warm", "h1", true).contains("insufficient history") {
        return Err("estimate: a failing run must not count as a completion sample".into());
    }

    // Selective-baseline brackets: a nonexistent commit must be REFUSED (so the
    // caller falls back to the full lane) and an existing one ACCEPTED.
    let ledger_rows = vec![
        base(serde_json::json!({"slot": "other", "commit": "aaa", "producer": "validate.rs"})),
        base(serde_json::json!({"slot": "mine", "commit": "bbb", "producer": "validate.rs"})),
    ];
    let exists_all = |_: &str| true;
    let exists_none = |_: &str| false;
    let seen: BTreeSet<String> = ledger_rows.iter().map(|r| s(r, "commit").to_string()).collect();
    if seen.len() != 2 {
        return Err("selective: fixture rows must carry distinct commits".into());
    }
    if selective_baseline(&ledger_rows, None, "mine", &exists_all).as_deref() != Some("bbb") {
        return Err("selective: this slot's newest passing commit must win".into());
    }
    if selective_baseline(&ledger_rows, Some("cafe"), "mine", &exists_all).as_deref() != Some("cafe") {
        return Err("selective: an explicit --baseline must win".into());
    }
    if selective_baseline(&ledger_rows, Some("cafe"), "mine", &exists_none).is_some() {
        return Err("selective: a baseline absent from this checkout must be REFUSED".into());
    }
    refused += 1;
    accepted += 2;
    Ok(format!(
        "history: cache bracketed {accepted} accept / {refused} refuse (incl. both \
         cross-producer counter traps), estimate bracketed thin/median/no-ledger/fail-poison, \
         selective baseline bracketed slot-preference/explicit/missing-commit"
    ))
}
