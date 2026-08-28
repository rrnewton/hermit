#!/usr/bin/env -S rust-script --force
//! Convert nextest's versioned libtest JSON into dagrun's shared test-result file.
//!
//! ```cargo
//! [dependencies]
//! dagrun = { path = "../agent-utils/rs/dagrun" }
//! serde_json = "1"
//! ```

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use dagrun::TestResult;
use dagrun::TestResults;
use serde_json::Value;

#[path = "../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

fn usage() {
    println!("usage: nextest-test-results.rs EVENTS_JSONL EXECUTED_TESTS FILTERED_TESTS OUTPUT");
}

fn required_string<'a>(value: &'a Value, field: &str, line: usize) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("nextest-test-results line {line}: {field} must be a string"))
}

fn test_identity(name: &str, line: usize) -> Result<((String, String), String, u64), String> {
    let (name, attempts) = match name.rsplit_once('#') {
        Some((prefix, suffix)) if suffix.chars().all(|character| character.is_ascii_digit()) => {
            let attempts = suffix.parse::<u64>().map_err(|error| {
                format!("nextest-test-results line {line}: invalid retry count: {error}")
            })?;
            (prefix, attempts)
        }
        _ => (name, 1),
    };
    if attempts == 0 {
        return Err(format!(
            "nextest-test-results line {line}: retry count must be positive"
        ));
    }
    let (package, binary_and_test) = name.split_once("::").ok_or_else(|| {
        format!("nextest-test-results line {line}: name has no package separator")
    })?;
    let (binary, test) = binary_and_test
        .split_once('$')
        .ok_or_else(|| format!("nextest-test-results line {line}: name has no test separator"))?;
    if package.is_empty() || binary.is_empty() || test.is_empty() {
        return Err(format!(
            "nextest-test-results line {line}: name has an empty package, binary, or test"
        ));
    }
    Ok((
        (package.to_string(), binary.to_string()),
        test.to_string(),
        attempts,
    ))
}

fn displayed_test_id(package: &str, binary: &str, test: &str, suite_kind: &str) -> String {
    let display_binary = if suite_kind == "lib" {
        package.to_string()
    } else {
        format!("{package}::{binary}")
    };
    format!("{display_binary}${test}")
}

fn parse_events(path: &Path) -> Result<Vec<TestResult>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("nextest-test-results-read {}: {error}", path.display()))?;
    let mut results = BTreeMap::new();
    let mut suite_kinds = BTreeMap::new();
    for (offset, line) in text.lines().enumerate() {
        let line_number = offset + 1;
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).map_err(|error| {
            format!("nextest-test-results line {line_number}: invalid JSON: {error}")
        })?;
        let kind = required_string(&value, "type", line_number)?;
        let event = required_string(&value, "event", line_number)?;
        match (kind, event) {
            ("suite", "started" | "ok" | "failed") => {
                let nextest = value
                    .get("nextest")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        format!(
                            "nextest-test-results line {line_number}: nextest must be an object"
                        )
                    })?;
                let package = nextest.get("crate").and_then(Value::as_str).ok_or_else(|| {
                    format!("nextest-test-results line {line_number}: nextest.crate must be a string")
                })?;
                let binary = nextest
                    .get("test_binary")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        format!("nextest-test-results line {line_number}: nextest.test_binary must be a string")
                    })?;
                let suite_kind = nextest.get("kind").and_then(Value::as_str).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: nextest.kind must be a string"
                    )
                })?;
                if package.is_empty() || binary.is_empty() || suite_kind.is_empty() {
                    return Err(format!(
                        "nextest-test-results line {line_number}: suite identity fields must be nonempty"
                    ));
                }
                let key = (package.to_string(), binary.to_string());
                if let Some(prior) = suite_kinds.insert(key.clone(), suite_kind.to_string()) {
                    if prior != suite_kind {
                        return Err(format!(
                            "nextest-test-results line {line_number}: suite {key:?} changed kind from {prior:?} to {suite_kind:?}"
                        ));
                    }
                }
            }
            ("test", "started" | "ignored") => {}
            ("test", "ok" | "failed") => {
                let name = required_string(&value, "name", line_number)?;
                let (key, test, attempts) = test_identity(name, line_number)?;
                let suite_kind = suite_kinds.get(&key).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: test names suite {key:?} before its typed suite metadata"
                    )
                })?;
                let id = displayed_test_id(&key.0, &key.1, &test, suite_kind);
                let result = TestResult::new(id.clone(), event == "ok", attempts)?;
                if results.insert(id.clone(), result).is_some() {
                    return Err(format!(
                        "nextest-test-results line {line_number}: duplicate terminal test id {id:?}"
                    ));
                }
            }
            _ => {
                return Err(format!(
                    "nextest-test-results line {line_number}: unsupported {kind} event {event:?}"
                ));
            }
        }
    }
    Ok(results.into_values().collect())
}

fn parse_u64(value: String, name: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .map_err(|error| format!("nextest-test-results {name}: {error}"))
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let Some(first) = args.next() else {
        usage();
        return Err("nextest-test-results requires four arguments".into());
    };
    if first == "--help" || first == "-h" {
        usage();
        return Ok(());
    }
    let events = first;
    let executed = parse_u64(
        args.next()
            .ok_or_else(|| "nextest-test-results missing EXECUTED_TESTS".to_string())?,
        "executed_tests",
    )?;
    let filtered = parse_u64(
        args.next()
            .ok_or_else(|| "nextest-test-results missing FILTERED_TESTS".to_string())?,
        "filtered_tests",
    )?;
    let output = args
        .next()
        .ok_or_else(|| "nextest-test-results missing OUTPUT".to_string())?;
    if args.next().is_some() {
        return Err("nextest-test-results received more than four arguments".into());
    }
    let results = parse_events(Path::new(&events))?;
    TestResults::current(executed, filtered, results)?.write_current(Path::new(&output))
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("nextest-test-results: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_names_keep_the_suite_identity_and_retry_count() {
        assert_eq!(
            test_identity("pkg::internal_lib$module::case#2", 1).unwrap(),
            (
                ("pkg".into(), "internal_lib".into()),
                "module::case".into(),
                2
            )
        );
        assert_eq!(
            test_identity("pkg::integration_case$module::case", 1).unwrap(),
            (
                ("pkg".into(), "integration_case".into()),
                "module::case".into(),
                1
            )
        );
        assert_eq!(
            displayed_test_id("hermit-detcore", "detcore", "module::case", "lib"),
            "hermit-detcore$module::case"
        );
        assert_eq!(
            displayed_test_id("hermit", "cli", "module::case", "test"),
            "hermit::cli$module::case"
        );
    }
}
