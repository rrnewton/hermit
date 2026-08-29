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
    println!("usage: nextest-test-results.rs EVENTS_JSONL NEXTEST_STATUS OUTPUT_OR_DASH");
}

fn required_string<'a>(value: &'a Value, field: &str, line: usize) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("nextest-test-results line {line}: {field} must be a string"))
}

fn required_u64(value: &Value, field: &str, line: usize) -> Result<u64, String> {
    value.get(field).and_then(Value::as_u64).ok_or_else(|| {
        format!("nextest-test-results line {line}: {field} must be an unsigned integer")
    })
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

#[derive(Debug, Eq, PartialEq)]
struct ParsedEvents {
    executed_tests: u64,
    filtered_tests: u64,
    results: Vec<TestResult>,
}

fn skipped_count(filtered_out: u64, ignored: u64, line: usize) -> Result<u64, String> {
    // nextest 0.9.100's libtest-json adapter represents `0 - ignored` as a
    // wrapping u64 when every test in a binary is ignored. The typed `ignored`
    // field already carries those tests, so admit only that exact producer
    // shape as zero additional filtered tests. Any other overflow is malformed.
    if ignored > 0 && filtered_out == 0u64.wrapping_sub(ignored) {
        return Ok(ignored);
    }
    ignored.checked_add(filtered_out).ok_or_else(|| {
        format!("nextest-test-results line {line}: ignored + filtered_out overflows u64")
    })
}

fn parse_events(text: &str) -> Result<ParsedEvents, String> {
    let mut results = BTreeMap::new();
    let mut suite_kinds = BTreeMap::new();
    let mut skipped_by_suite: BTreeMap<(String, String), u64> = BTreeMap::new();
    let mut suite_passed = 0u64;
    let mut suite_failed = 0u64;
    let mut saw_event = false;
    for (offset, line) in text.lines().enumerate() {
        let line_number = offset + 1;
        if line.trim().is_empty() {
            continue;
        }
        saw_event = true;
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
                match event {
                    "started" => {}
                    "ok" | "failed" => {
                        suite_passed = suite_passed
                            .checked_add(required_u64(&value, "passed", line_number)?)
                            .ok_or_else(|| {
                                "nextest-test-results: passed count overflows u64".to_string()
                            })?;
                        suite_failed = suite_failed
                            .checked_add(required_u64(&value, "failed", line_number)?)
                            .ok_or_else(|| {
                                "nextest-test-results: failed count overflows u64".to_string()
                            })?;
                        let ignored = required_u64(&value, "ignored", line_number)?;
                        let skipped = skipped_count(
                            required_u64(&value, "filtered_out", line_number)?,
                            ignored,
                            line_number,
                        )?;
                        // nextest may launch one exact-filtered process per test,
                        // repeating the same suite-wide skipped population in
                        // every terminal suite record. Keep one count per typed
                        // package/binary identity instead of multiplying recaps.
                        skipped_by_suite
                            .entry(key)
                            .and_modify(|current| *current = (*current).max(skipped))
                            .or_insert(skipped);
                    }
                    _ => unreachable!(),
                }
            }
            ("test", "started" | "ignored") => {
                required_string(&value, "name", line_number)?;
            }
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
    if !saw_event {
        return Err("nextest-test-results: event stream is empty".into());
    }
    for suite in suite_kinds.keys() {
        if !skipped_by_suite.contains_key(suite) {
            return Err(format!(
                "nextest-test-results: suite {suite:?} has no terminal count record"
            ));
        }
    }
    let results = results.into_values().collect::<Vec<_>>();
    let executed_tests = u64::try_from(results.len())
        .map_err(|_| "nextest-test-results: terminal result count does not fit u64".to_string())?;
    let suite_executed = suite_passed
        .checked_add(suite_failed)
        .ok_or_else(|| "nextest-test-results: suite executed count overflows u64".to_string())?;
    if suite_executed != executed_tests {
        return Err(format!(
            "nextest-test-results: typed suite records report {suite_executed} executed test(s), but typed terminal test records report {executed_tests}"
        ));
    }
    let filtered_tests = skipped_by_suite.values().try_fold(0u64, |total, count| {
        total
            .checked_add(*count)
            .ok_or_else(|| "nextest-test-results: filtered count overflows u64".to_string())
    })?;
    Ok(ParsedEvents {
        executed_tests,
        filtered_tests,
        results,
    })
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
        return Err("nextest-test-results requires three arguments".into());
    };
    if first == "--help" || first == "-h" {
        usage();
        return Ok(());
    }
    let events = first;
    let status = parse_u64(
        args.next()
            .ok_or_else(|| "nextest-test-results missing NEXTEST_STATUS".to_string())?,
        "nextest_status",
    )?;
    let output = args
        .next()
        .ok_or_else(|| "nextest-test-results missing OUTPUT".to_string())?;
    if args.next().is_some() {
        return Err("nextest-test-results received more than three arguments".into());
    }
    let text = fs::read_to_string(&events)
        .map_err(|error| format!("nextest-test-results-read {events}: {error}"))?;
    let parsed = parse_events(&text)?;
    let passed = u64::try_from(parsed.results.iter().filter(|result| result.passed).count())
        .map_err(|_| "nextest-test-results: passed result count does not fit u64".to_string())?;
    let failed = parsed
        .executed_tests
        .checked_sub(passed)
        .ok_or_else(|| "nextest-test-results: passed count exceeds executed count".to_string())?;
    if status == 0 && failed != 0 {
        return Err(format!(
            "nextest-test-results: successful nextest status disagrees with {failed} failed typed result(s)"
        ));
    }
    let report =
        TestResults::current(parsed.executed_tests, parsed.filtered_tests, parsed.results)?;
    if output != "-" {
        report.write_current(Path::new(&output))?;
    }
    println!("running {} tests", report.executed_tests);
    if status == 0 {
        println!(
            "test result: ok. {passed} passed; 0 failed; 0 ignored; {} filtered out",
            report.filtered_tests
        );
    } else {
        println!(
            "test result: FAILED. {passed} passed; {failed} failed; 0 ignored; {} filtered out",
            report.filtered_tests
        );
    }
    Ok(())
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

    #[test]
    fn aggregate_counts_come_from_typed_suite_and_test_events() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":2,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"suite::suite$passes"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"suite::suite$passes","exec_time":0.1}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"suite::suite$recovers"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"suite::suite$recovers#2","exec_time":0.2}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":2,"failed":0,"ignored":0,"measured":0,"filtered_out":7,"exec_time":0.3,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
        );
        let parsed = parse_events(events).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(parsed.filtered_tests, 7);
        assert_eq!(parsed.results.len(), 2);

        let mutated = events.replace("\"filtered_out\":7", "\"filtered_out\":11");
        let parsed = parse_events(&mutated).unwrap();
        assert_eq!(parsed.executed_tests, 2);
        assert_eq!(parsed.filtered_tests, 11);

        let extra_test = events
            .replace("\"test_count\":2", "\"test_count\":3")
            .replace("\"passed\":2", "\"passed\":3")
            .replace(
                r#"{"type":"suite","event":"ok""#,
                concat!(
                    r#"{"type":"test","event":"started","name":"suite::suite$third"}"#,
                    "\n",
                    r#"{"type":"test","event":"ok","name":"suite::suite$third","exec_time":0.1}"#,
                    "\n",
                    r#"{"type":"suite","event":"ok""#,
                ),
            );
        let parsed = parse_events(&extra_test).unwrap();
        assert_eq!(parsed.executed_tests, 3);
        assert_eq!(parsed.filtered_tests, 7);
    }

    #[test]
    fn repeated_suite_recaps_do_not_multiply_filtered_count() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":214,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"hermit::rr_suite$ignored_one"}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":213,"measured":0,"filtered_out":0,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
            "\n",
            r#"{"type":"suite","event":"started","test_count":214,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
            "\n",
            r#"{"type":"test","event":"started","name":"hermit::rr_suite$runs"}"#,
            "\n",
            r#"{"type":"test","event":"ok","name":"hermit::rr_suite$runs","exec_time":0.1}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":1,"failed":0,"ignored":213,"measured":0,"filtered_out":0,"nextest":{"crate":"hermit","test_binary":"rr_suite","kind":"test"}}"#,
        );
        let parsed = parse_events(events).unwrap();
        assert_eq!(parsed.executed_tests, 1);
        assert_eq!(parsed.filtered_tests, 213);
    }

    #[test]
    fn all_ignored_wrapping_filtered_count_keeps_the_typed_ignored_count() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":3,"nextest":{"crate":"hermit","test_binary":"analyze","kind":"test"}}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":3,"measured":0,"filtered_out":18446744073709551613,"nextest":{"crate":"hermit","test_binary":"analyze","kind":"test"}}"#,
        );
        let parsed = parse_events(events).unwrap();
        assert_eq!(parsed.executed_tests, 0);
        assert_eq!(parsed.filtered_tests, 3);
    }

    #[test]
    fn missing_typed_count_fails_by_field_name() {
        let events = concat!(
            r#"{"type":"suite","event":"started","test_count":1,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
            "\n",
            r#"{"type":"suite","event":"ok","passed":0,"failed":0,"ignored":1,"measured":0,"nextest":{"crate":"suite","test_binary":"suite","kind":"lib"}}"#,
        );
        assert!(
            parse_events(events)
                .unwrap_err()
                .contains("filtered_out must be an unsigned integer")
        );

        let disagreeing = events
            .replace("\"passed\":0", "\"passed\":1")
            .replace("\"measured\":0,", "\"measured\":0,\"filtered_out\":0,");
        assert!(parse_events(&disagreeing)
            .unwrap_err()
            .contains(
                "typed suite records report 1 executed test(s), but typed terminal test records report 0"
            ));
    }
}
