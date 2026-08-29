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

fn displayed_test_id(
    package: &str,
    binary: &str,
    test: &str,
    suite_kind: &str,
    stress_index: Option<u64>,
) -> String {
    let stress = stress_index.map_or_else(String::new, |index| format!("@stress-{index}"));
    let display_binary = if suite_kind == "lib" {
        format!("{package}{stress}")
    } else {
        format!("{package}::{binary}{stress}")
    };
    format!("{display_binary}${test}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SuiteIdentity {
    package: String,
    binary: String,
    kind: String,
    stress_index: Option<u64>,
}

impl SuiteIdentity {
    fn from_event(value: &Value, line: usize) -> Result<Self, String> {
        let nextest = value
            .get("nextest")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                format!("nextest-test-results line {line}: nextest must be an object")
            })?;
        let package = nextest.get("crate").and_then(Value::as_str).ok_or_else(|| {
            format!("nextest-test-results line {line}: nextest.crate must be a string")
        })?;
        let binary = nextest
            .get("test_binary")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                format!(
                    "nextest-test-results line {line}: nextest.test_binary must be a string"
                )
            })?;
        let kind = nextest.get("kind").and_then(Value::as_str).ok_or_else(|| {
            format!("nextest-test-results line {line}: nextest.kind must be a string")
        })?;
        if package.is_empty() || binary.is_empty() || kind.is_empty() {
            return Err(format!(
                "nextest-test-results line {line}: suite identity fields must be nonempty"
            ));
        }
        let stress_index = match nextest.get("stress_index") {
            Some(value) => Some(value.as_u64().ok_or_else(|| {
                format!(
                    "nextest-test-results line {line}: nextest.stress_index must be an unsigned integer"
                )
            })?),
            None => None,
        };
        Ok(Self {
            package: package.into(),
            binary: binary.into(),
            kind: kind.into(),
            stress_index,
        })
    }

    fn test_name_key(&self) -> (String, String) {
        let binary = match self.stress_index {
            Some(index) => format!("{}@stress-{index}", self.binary),
            None => self.binary.clone(),
        };
        (self.package.clone(), binary)
    }
}

fn parse_event_text(text: &str) -> Result<Vec<TestResult>, String> {
    let mut results = BTreeMap::new();
    // Test rows carry package/binary but not target kind. Keep every currently
    // open typed suite by that rendered key so interleaved distinct suites stay
    // attributable. Two open suites with the same key are ambiguous and must
    // refuse: no field on their test rows could distinguish them.
    let mut open_suites = BTreeMap::<(String, String), SuiteIdentity>::new();
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
            ("suite", "started") => {
                let suite = SuiteIdentity::from_event(&value, line_number)?;
                let key = suite.test_name_key();
                if let Some(active) = open_suites.insert(key.clone(), suite.clone()) {
                    return Err(format!(
                        "nextest-test-results line {line_number}: suite key {key:?} is ambiguous because {suite:?} started before {active:?} ended"
                    ));
                }
            }
            ("suite", "ok" | "failed") => {
                let ended = SuiteIdentity::from_event(&value, line_number)?;
                let key = ended.test_name_key();
                let started = open_suites.remove(&key).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: suite {ended:?} ended before its typed start metadata"
                    )
                })?;
                if started != ended {
                    if started.package == ended.package
                        && started.binary == ended.binary
                        && started.kind != ended.kind
                    {
                        return Err(format!(
                            "nextest-test-results line {line_number}: suite ({:?}, {:?}) changed kind from {:?} to {:?}",
                            started.package, started.binary, started.kind, ended.kind
                        ));
                    }
                    return Err(format!(
                        "nextest-test-results line {line_number}: suite ended as {ended:?} after starting as {started:?}"
                    ));
                }
            }
            ("test", "started" | "ignored" | "ok" | "failed") => {
                let name = required_string(&value, "name", line_number)?;
                let (key, test, attempts) = test_identity(name, line_number)?;
                let suite = open_suites.get(&key).ok_or_else(|| {
                    format!(
                        "nextest-test-results line {line_number}: test names suite {key:?} without one unambiguous open typed suite"
                    )
                })?;
                if matches!(event, "started" | "ignored") {
                    continue;
                }
                let id = displayed_test_id(
                    &suite.package,
                    &suite.binary,
                    &test,
                    &suite.kind,
                    suite.stress_index,
                );
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
    if !open_suites.is_empty() {
        return Err(format!(
            "nextest-test-results ended before open suites published terminal events: {:?}",
            open_suites.into_values().collect::<Vec<_>>()
        ));
    }
    Ok(results.into_values().collect())
}

fn parse_events(path: &Path) -> Result<Vec<TestResult>, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("nextest-test-results-read {}: {error}", path.display()))?;
    parse_event_text(&text)
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
            displayed_test_id("hermit-detcore", "detcore", "module::case", "lib", None),
            "hermit-detcore$module::case"
        );
        assert_eq!(
            displayed_test_id("hermit", "cli", "module::case", "test", None),
            "hermit::cli$module::case"
        );
    }

    #[test]
    fn same_named_lib_and_bin_suites_keep_distinct_typed_identities() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"hermit::hermit$shared_case#2\",\"exec_time\":0.1}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"measured\":0,\"filtered_out\":0,\"exec_time\":0.1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"hermit::hermit$shared_case\",\"exec_time\":0.1}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":1,\"failed\":0,\"ignored\":0,\"measured\":0,\"filtered_out\":0,\"exec_time\":0.1,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
        );
        assert_eq!(
            parse_event_text(events).unwrap(),
            vec![
                TestResult::new("hermit$shared_case".into(), true, 2).unwrap(),
                TestResult::new("hermit::hermit$shared_case".into(), true, 1).unwrap(),
            ]
        );
    }

    #[test]
    fn one_suite_changing_kind_is_still_refused() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"test_count\":0,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"passed\":0,\"failed\":0,\"ignored\":0,\"measured\":0,\"filtered_out\":0,\"exec_time\":0.0,\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
        );
        let error = parse_event_text(events).unwrap_err();
        assert!(error.contains("changed kind from \"lib\" to \"bin\""), "{error}");
    }

    #[test]
    fn distinct_suites_may_interleave_and_ignore_additive_fields() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"future\":true,\"nextest\":{\"crate\":\"alpha\",\"test_binary\":\"alpha\",\"kind\":\"lib\",\"future\":\"ok\"}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"beta\",\"test_binary\":\"tool\",\"kind\":\"bin\"}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"alpha::alpha$case\"}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"beta::tool$case\"}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"nextest\":{\"crate\":\"beta\",\"test_binary\":\"tool\",\"kind\":\"bin\"}}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"nextest\":{\"crate\":\"alpha\",\"test_binary\":\"alpha\",\"kind\":\"lib\"}}\n",
        );
        assert_eq!(
            parse_event_text(events).unwrap(),
            vec![
                TestResult::new("alpha$case".into(), true, 1).unwrap(),
                TestResult::new("beta::tool$case".into(), true, 1).unwrap(),
            ]
        );
    }

    #[test]
    fn lib_stress_suites_keep_distinct_ids() {
        let events = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":1}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"pkg::pkg@stress-1$case\"}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":1}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":2}}\n",
            "{\"type\":\"test\",\"event\":\"ok\",\"name\":\"pkg::pkg@stress-2$case\"}\n",
            "{\"type\":\"suite\",\"event\":\"ok\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\",\"stress_index\":2}}\n",
        );
        assert_eq!(
            parse_event_text(events).unwrap(),
            vec![
                TestResult::new("pkg@stress-1$case".into(), true, 1).unwrap(),
                TestResult::new("pkg@stress-2$case".into(), true, 1).unwrap(),
            ]
        );
    }

    #[test]
    fn ambiguous_wrong_and_incomplete_suite_streams_refuse() {
        let ambiguous = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"hermit\",\"test_binary\":\"hermit\",\"kind\":\"bin\"}}\n",
        );
        assert!(parse_event_text(ambiguous).unwrap_err().contains("ambiguous"));

        let wrong_row = concat!(
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"alpha\",\"test_binary\":\"alpha\",\"kind\":\"lib\"}}\n",
            "{\"type\":\"test\",\"event\":\"failed\",\"name\":\"beta::beta$case\"}\n",
        );
        assert!(
            parse_event_text(wrong_row)
                .unwrap_err()
                .contains("without one unambiguous open typed suite")
        );

        let end_without_start =
            "{\"type\":\"suite\",\"event\":\"ok\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\"}}\n";
        assert!(
            parse_event_text(end_without_start)
                .unwrap_err()
                .contains("ended before its typed start metadata")
        );

        let unterminated =
            "{\"type\":\"suite\",\"event\":\"started\",\"nextest\":{\"crate\":\"pkg\",\"test_binary\":\"pkg\",\"kind\":\"lib\"}}\n";
        assert!(
            parse_event_text(unterminated)
                .unwrap_err()
                .contains("before open suites published terminal events")
        );
    }
}
