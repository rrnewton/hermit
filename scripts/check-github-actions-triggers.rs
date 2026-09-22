#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Enforce the repository's GitHub Actions trigger policy.
//!
//! Local exact-head validation is the landing authority. The portable workflow
//! is supplemental evidence and may run automatically only after a commit is
//! pushed to `integration`. Two owner-approved maintenance workflows also run
//! at their exact reviewed schedules; every workflow may remain manually
//! dispatchable. This checker deliberately accepts only the small YAML shape
//! used here. An unfamiliar or ambiguous trigger block is an error, never a
//! silent pass.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const WORKFLOW_DIR: &str = ".github/workflows";
const PORTABLE: &str = "ci-portable.yml";

#[derive(Debug, PartialEq, Eq)]
struct Triggers {
    events: BTreeSet<String>,
    push_branches: Option<Vec<String>>,
    schedule_crons: Option<Vec<String>>,
}

fn indentation(line: &str) -> Result<usize, String> {
    let prefix = line.len() - line.trim_start_matches([' ', '\t']).len();
    if line[..prefix].contains('\t') {
        return Err("tabs are not accepted in YAML indentation".to_string());
    }
    Ok(prefix)
}

fn code(line: &str) -> &str {
    line.split_once('#')
        .map_or(line, |(before, _)| before)
        .trim_end()
}

fn key(line: &str) -> Option<&str> {
    let trimmed = code(line).trim();
    let (name, value) = trimmed.split_once(':')?;
    if !value.trim().is_empty()
        || name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name)
}

fn inline_list(value: &str) -> Option<Vec<String>> {
    let value = value.trim();
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    let entries = inner
        .split(',')
        .map(|entry| entry.trim().trim_matches(['\'', '"']).to_string())
        .collect::<Vec<_>>();
    if entries.is_empty() || entries.iter().any(String::is_empty) {
        return None;
    }
    Some(entries)
}

fn quoted_scalar(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.is_empty() || inner.contains('"') {
        return None;
    }
    Some(inner.to_string())
}

fn parse_triggers(source: &str) -> Result<Triggers, String> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut on_entries = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = code(raw);
        if line.trim().is_empty() || indentation(line)? != 0 {
            continue;
        }
        let trimmed = line.trim();
        if matches!(trimmed, "---" | "...") {
            continue;
        }
        let (spelling, value) = trimmed
            .split_once(':')
            .ok_or_else(|| format!("unsupported top-level YAML structure: `{trimmed}`"))?;
        let spelling = spelling.trim();
        if spelling.is_empty()
            || !spelling
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "top-level YAML keys must use an unquoted simple spelling: `{spelling}`"
            ));
        }
        if spelling == "on" {
            on_entries.push((index, value.trim()));
        }
    }
    if on_entries.len() != 1 {
        return Err(format!(
            "expected exactly one top-level `on` key, found {}",
            on_entries.len()
        ));
    }
    let (on_index, value) = on_entries[0];
    if !value.is_empty() {
        return Err("the top-level `on` value must be a mapping".to_string());
    }

    let mut events = BTreeSet::new();
    let mut event_indent = None;
    let mut push_indent = None;
    let mut push_child_indent = None;
    let mut push_branches = None;
    let mut schedule_indent = None;
    let mut schedule_child_indent = None;
    let mut schedule_crons = None;
    for raw in &lines[on_index + 1..] {
        let line = code(raw);
        if line.trim().is_empty() {
            continue;
        }
        let indent = indentation(line)?;
        if indent == 0 {
            break;
        }
        let direct_indent = *event_indent.get_or_insert(indent);
        if indent < direct_indent {
            return Err("inconsistent indentation in top-level `on:` mapping".to_string());
        }
        if indent == direct_indent {
            let event = key(line).ok_or_else(|| {
                format!(
                    "event entries must use a mapping key on its own line; found `{}`",
                    line.trim()
                )
            })?;
            if !events.insert(event.to_string()) {
                return Err(format!("duplicate `{event}` trigger"));
            }
            push_indent = (event == "push").then_some(indent);
            push_child_indent = None;
            schedule_indent = (event == "schedule").then_some(indent);
            schedule_child_indent = None;
            if event == "schedule" {
                schedule_crons = Some(Vec::new());
            }
            continue;
        }

        if let Some(indent_of_push) = push_indent {
            if indent > indent_of_push {
                let direct_push_indent = *push_child_indent.get_or_insert(indent);
                if indent != direct_push_indent {
                    return Err(format!(
                        "ambiguous nested structure in `push`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix("branches:") {
                    if push_branches.is_some() {
                        return Err("duplicate `push.branches` entry".to_string());
                    }
                    push_branches = Some(inline_list(value).ok_or_else(|| {
                        "`push.branches` must be an explicit inline list".to_string()
                    })?);
                } else {
                    return Err(format!(
                        "unsupported direct `push` key; only `branches` is permitted: `{trimmed}`"
                    ));
                }
            }
        } else if let Some(indent_of_schedule) = schedule_indent {
            if indent > indent_of_schedule {
                let direct_schedule_indent = *schedule_child_indent.get_or_insert(indent);
                if indent != direct_schedule_indent {
                    return Err(format!(
                        "ambiguous nested structure in `schedule`: `{}`",
                        line.trim()
                    ));
                }
                let trimmed = line.trim();
                let value = trimmed.strip_prefix("- cron:").ok_or_else(|| {
                    format!(
                        "unsupported direct `schedule` entry; only `- cron: \"...\"` is permitted: `{trimmed}`"
                    )
                })?;
                let cron = quoted_scalar(value).ok_or_else(|| {
                    "`schedule` cron must be an explicit nonempty double-quoted scalar".to_string()
                })?;
                schedule_crons
                    .as_mut()
                    .expect("schedule trigger initializes cron storage")
                    .push(cron);
            }
        }
    }

    if events.is_empty() {
        return Err("the top-level `on:` mapping has no events".to_string());
    }
    Ok(Triggers {
        events,
        push_branches,
        schedule_crons,
    })
}

fn approved_schedule(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "buck2-oss-nightly.yml" => Some(&["17 10 * * *"]),
        "docs.yml" => Some(&["23 8 * * *"]),
        _ => None,
    }
}

fn validate(name: &str, triggers: &Triggers) -> Vec<String> {
    let recognized = BTreeSet::from([
        "push".to_string(),
        "schedule".to_string(),
        "workflow_dispatch".to_string(),
    ]);
    let mut errors = Vec::new();
    for event in triggers.events.difference(&recognized) {
        errors.push(format!("automatic `{event}` trigger is not permitted"));
    }
    if !triggers.events.contains("workflow_dispatch") {
        errors.push("missing `workflow_dispatch` trigger".to_string());
    }
    if triggers.events.contains("push") {
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push("`push.branches` must contain exactly `integration`".to_string());
        }
    } else if triggers.push_branches.is_some() {
        errors.push("found `push.branches` without a `push` trigger".to_string());
    }

    if let Some(expected_crons) = approved_schedule(name) {
        let required_events =
            BTreeSet::from(["schedule".to_string(), "workflow_dispatch".to_string()]);
        if triggers.events != required_events {
            errors.push(format!(
                "{name} must have exactly `workflow_dispatch` and `schedule` triggers"
            ));
        }
        let actual_crons = triggers.schedule_crons.as_deref().unwrap_or_default();
        if !actual_crons
            .iter()
            .map(String::as_str)
            .eq(expected_crons.iter().copied())
        {
            errors.push(format!(
                "{name} must use exactly the approved cron schedule: {}",
                expected_crons.join(", ")
            ));
        }
    } else if triggers.events.contains("schedule") {
        errors.push("automatic `schedule` trigger is not permitted".to_string());
    }

    if name == PORTABLE {
        let allowed = BTreeSet::from(["push".to_string(), "workflow_dispatch".to_string()]);
        if triggers.events != allowed {
            errors.push(
                "ci-portable.yml must have exactly `workflow_dispatch` and `push` triggers"
                    .to_string(),
            );
        }
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push("ci-portable.yml must push-trigger exactly on `integration`".to_string());
        }
    }
    errors
}

fn workflow_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("cannot read workflow directory entry: {error}"))?
            .path();
        if matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yml" | "yaml")
        ) {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(format!("no YAML workflows found under {}", dir.display()));
    }
    Ok(files)
}

fn run(dir: &Path) -> Result<usize, Vec<String>> {
    let files = workflow_files(dir).map_err(|error| vec![error])?;
    let mut errors = Vec::new();
    let mut saw_portable = false;
    for path in &files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        saw_portable |= name == PORTABLE;
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                errors.push(format!("{}: cannot read: {error}", path.display()));
                continue;
            }
        };
        match parse_triggers(&source) {
            Ok(triggers) => {
                errors.extend(
                    validate(name, &triggers)
                        .into_iter()
                        .map(|error| format!("{}: {error}", path.display())),
                );
            }
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    if !saw_portable {
        errors.push(format!("{PORTABLE} is missing"));
    }
    if errors.is_empty() {
        Ok(files.len())
    } else {
        Err(errors)
    }
}

fn main() {
    rust_script_prelude::init();
    match run(Path::new(WORKFLOW_DIR)) {
        Ok(count) => println!(
            "GitHub Actions triggers OK: {count} workflows; automatic runs are limited to integration and exact approved schedules"
        ),
        Err(errors) => {
            for error in errors {
                eprintln!("check-github-actions-triggers: {error}");
            }
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> Triggers {
        parse_triggers(source).expect("fixture should parse")
    }

    #[test]
    fn portable_policy_accepts_integration_and_manual_only() {
        let triggers = parsed(
            "name: portable\non:\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n",
        );
        assert!(validate(PORTABLE, &triggers).is_empty());
    }

    #[test]
    fn manual_only_workflow_is_allowed() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(validate("other.yml", &triggers).is_empty());
    }

    #[test]
    fn exact_approved_schedules_are_allowed() {
        for (name, cron) in [
            ("buck2-oss-nightly.yml", "17 10 * * *"),
            ("docs.yml", "23 8 * * *"),
        ] {
            let triggers = parsed(&format!(
                "on:\n  schedule:\n    - cron: \"{cron}\"\n  workflow_dispatch:\njobs:\n"
            ));
            assert!(validate(name, &triggers).is_empty(), "{name}");
        }
    }

    #[test]
    fn pull_request_and_schedule_are_rejected() {
        for event in [
            "pull_request",
            "pull_request_target",
            "schedule",
            "workflow_run",
        ] {
            let triggers = parsed(&format!("on:\n  workflow_dispatch:\n  {event}:\njobs:\n"));
            assert!(
                !validate("other.yml", &triggers).is_empty(),
                "{event} unexpectedly passed"
            );
        }
    }

    #[test]
    fn copied_or_modified_schedules_are_rejected() {
        for (name, cron) in [
            ("other.yml", "17 10 * * *"),
            ("buck2-oss-nightly.yml", "18 10 * * *"),
            ("docs.yml", "23 8 * * 1"),
        ] {
            let triggers = parsed(&format!(
                "on:\n  schedule:\n    - cron: \"{cron}\"\n  workflow_dispatch:\njobs:\n"
            ));
            assert!(!validate(name, &triggers).is_empty(), "{name} {cron}");
        }
    }

    #[test]
    fn approved_schedule_rejects_extra_automatic_trigger() {
        let triggers = parsed(
            "on:\n  schedule:\n    - cron: \"17 10 * * *\"\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n",
        );
        assert!(!validate("buck2-oss-nightly.yml", &triggers).is_empty());
    }

    #[test]
    fn ambiguous_schedule_shapes_fail_to_parse() {
        for source in [
            "on:\n  schedule:\n    cron: \"17 10 * * *\"\n  workflow_dispatch:\njobs:\n",
            "on:\n  schedule:\n    - cron: 17 10 * * *\n  workflow_dispatch:\njobs:\n",
            "on:\n  schedule:\n    - cron: \"17 10 * * *\"\n      timezone: UTC\n  workflow_dispatch:\njobs:\n",
        ] {
            assert!(
                parse_triggers(source).is_err(),
                "schedule fixture unexpectedly parsed"
            );
        }
    }

    #[test]
    fn main_and_unbounded_pushes_are_rejected() {
        for push in ["  push:\n    branches: [main]\n", "  push:\n"] {
            let triggers = parsed(&format!("on:\n  workflow_dispatch:\n{push}jobs:\n"));
            assert!(!validate("other.yml", &triggers).is_empty());
        }
    }

    #[test]
    fn tag_pushes_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n    tags: [v1]\njobs:\n";
        let error = parse_triggers(source).expect_err("tag trigger unexpectedly parsed");
        assert!(error.contains("only `branches` is permitted"), "{error}");
    }

    #[test]
    fn deeper_push_structures_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n      unexpected: value\njobs:\n";
        let error = parse_triggers(source).expect_err("deeper push structure unexpectedly parsed");
        assert!(error.contains("ambiguous nested structure"), "{error}");
    }

    #[test]
    fn portable_requires_both_allowed_events() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(!validate(PORTABLE, &triggers).is_empty());
    }

    #[test]
    fn ambiguous_trigger_shapes_fail_to_parse() {
        for source in [
            "on: [push, workflow_dispatch]\njobs:\n",
            "on:\n  workflow_dispatch: {}\njobs:\n",
            "on:\n  push:\n    branches:\n      - integration\njobs:\n",
        ] {
            assert!(
                parse_triggers(source).is_err(),
                "fixture unexpectedly parsed"
            );
        }
    }

    #[test]
    fn duplicate_alternate_on_form_is_rejected() {
        for alternate in [
            "on: [pull_request]",
            "on : [pull_request]",
            "\"on\": [pull_request]",
            "'on': [pull_request]",
            "!!str on: [pull_request]",
            "? on\n: [pull_request]",
        ] {
            let source = format!("on:\n  workflow_dispatch:\n{alternate}\njobs:\n  test:\n");
            assert!(
                parse_triggers(&source).is_err(),
                "alternate top-level on unexpectedly parsed: {alternate}"
            );
        }
    }

    #[test]
    fn quoted_top_level_on_is_rejected() {
        for spelling in ["\"on\"", "'on'"] {
            let source = format!("{spelling}:\n  workflow_dispatch:\njobs:\n");
            let error =
                parse_triggers(&source).expect_err("quoted top-level on unexpectedly parsed");
            assert!(error.contains("unquoted simple spelling"), "{error}");
        }
    }
}
