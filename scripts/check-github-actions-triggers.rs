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
//! Local exact-head validation is the landing authority. The portable and demo
//! review workflows are supplemental evidence and may run automatically only
//! after a commit is pushed to `integration`; every workflow may remain manually
//! dispatchable.
//! This checker deliberately accepts only the small YAML shape used here. An
//! unfamiliar or ambiguous trigger block is an error, never a silent pass.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

const WORKFLOW_DIR: &str = ".github/workflows";
const PORTABLE: &str = "ci-portable.yml";
const DEMO_REVIEW: &str = "demo-review-gate.yml";
const INTEGRATION_PUSH_WORKFLOWS: [&str; 2] = [PORTABLE, DEMO_REVIEW];

#[derive(Debug, PartialEq, Eq)]
struct Triggers {
    events: BTreeSet<String>,
    push_branches: Option<Vec<String>>,
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
    let mut entries = Vec::new();
    for raw in inner.split(',') {
        let raw = raw.trim();
        let token = if let Some(quoted) = raw
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
        {
            quoted
        } else if let Some(quoted) = raw
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            quoted
        } else if raw.starts_with(['\'', '"']) || raw.ends_with(['\'', '"']) {
            return None;
        } else {
            raw
        };
        if token.is_empty()
            || !token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return None;
        }
        entries.push(token.to_string());
    }
    Some(entries)
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
        }
    }

    if events.is_empty() {
        return Err("the top-level `on:` mapping has no events".to_string());
    }
    Ok(Triggers {
        events,
        push_branches,
    })
}

fn validate(name: &str, triggers: &Triggers) -> Vec<String> {
    let allowed = BTreeSet::from(["push".to_string(), "workflow_dispatch".to_string()]);
    let mut errors = Vec::new();
    for event in triggers.events.difference(&allowed) {
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

    if INTEGRATION_PUSH_WORKFLOWS.contains(&name) {
        if triggers.events != allowed {
            errors.push(format!(
                "{name} must have exactly `workflow_dispatch` and `push` triggers"
            ));
        }
        if triggers.push_branches.as_deref() != Some(&["integration".to_string()]) {
            errors.push(format!("{name} must push-trigger exactly on `integration`"));
        }
    } else {
        let manual_only = BTreeSet::from(["workflow_dispatch".to_string()]);
        if triggers.events != manual_only {
            errors.push(format!(
                "{name} must have exactly the `workflow_dispatch` trigger"
            ));
        }
        if triggers.push_branches.is_some() {
            errors.push(format!("{name} must not configure `push.branches`"));
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
    let mut seen_integration_push_workflows = BTreeSet::new();
    for path in &files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if INTEGRATION_PUSH_WORKFLOWS.contains(&name) {
            seen_integration_push_workflows.insert(name);
        }
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
    for required in INTEGRATION_PUSH_WORKFLOWS {
        if !seen_integration_push_workflows.contains(required) {
            errors.push(format!("{required} is missing"));
        }
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
            "GitHub Actions triggers OK: {count} workflows; automatic runs are limited to integration"
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
    fn integration_push_workflows_accept_integration_and_manual_dispatch() {
        for name in INTEGRATION_PUSH_WORKFLOWS {
            for branch in ["integration", "'integration'", "\"integration\""] {
                let triggers = parsed(&format!(
                    "name: workflow\non:\n  workflow_dispatch:\n  push:\n    branches: [{branch}]\njobs:\n"
                ));
                assert!(
                    validate(name, &triggers).is_empty(),
                    "{name} rejected matched quoting: {branch}"
                );
            }
        }
    }

    #[test]
    fn manual_only_workflow_is_allowed() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        assert!(validate("docs.yml", &triggers).is_empty());
    }

    #[test]
    fn wrong_workflow_integration_push_is_rejected() {
        let triggers =
            parsed("on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\njobs:\n");
        let errors = validate("demo-hot-path.yml", &triggers);
        assert!(
            errors
                .iter()
                .any(|error| error.contains("must have exactly the `workflow_dispatch` trigger")),
            "{errors:?}"
        );
    }

    #[test]
    fn pull_request_and_schedule_are_rejected_for_every_workflow_class() {
        for event in [
            "pull_request",
            "pull_request_target",
            "schedule",
            "workflow_run",
        ] {
            for name in [PORTABLE, DEMO_REVIEW, "other.yml"] {
                let push = INTEGRATION_PUSH_WORKFLOWS
                    .contains(&name)
                    .then_some("  push:\n    branches: [integration]\n")
                    .unwrap_or("");
                let triggers = parsed(&format!(
                    "on:\n  workflow_dispatch:\n{push}  {event}:\njobs:\n"
                ));
                let errors = validate(name, &triggers);
                assert!(
                    errors
                        .iter()
                        .any(|error| error.contains(&format!("automatic `{event}`"))),
                    "{event} unexpectedly passed for {name}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn main_and_unbounded_pushes_are_rejected_for_allowed_workflows() {
        for name in INTEGRATION_PUSH_WORKFLOWS {
            for push in ["  push:\n    branches: [main]\n", "  push:\n"] {
                let triggers = parsed(&format!("on:\n  workflow_dispatch:\n{push}jobs:\n"));
                let errors = validate(name, &triggers);
                assert!(
                    errors.iter().any(|error| {
                        error.contains("must push-trigger exactly on `integration`")
                    }),
                    "{name} unexpectedly accepted {push:?}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn tag_pushes_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n    tags: [v1]\njobs:\n";
        let error = parse_triggers(source).expect_err("tag trigger unexpectedly parsed");
        assert!(error.contains("only `branches` is permitted"), "{error}");
    }

    #[test]
    fn mismatched_branch_quotes_are_rejected() {
        for branch in ["'integration\"'", "\"integration'\""] {
            let source =
                format!("on:\n  workflow_dispatch:\n  push:\n    branches: [{branch}]\njobs:\n");
            assert!(
                parse_triggers(&source).is_err(),
                "mismatched branch quotes unexpectedly parsed: {branch}"
            );
        }
    }

    #[test]
    fn deeper_push_structures_are_rejected() {
        let source = "on:\n  workflow_dispatch:\n  push:\n    branches: [integration]\n      unexpected: value\njobs:\n";
        let error = parse_triggers(source).expect_err("deeper push structure unexpectedly parsed");
        assert!(error.contains("ambiguous nested structure"), "{error}");
    }

    #[test]
    fn integration_push_workflows_require_both_allowed_events() {
        let triggers = parsed("on:\n  workflow_dispatch:\njobs:\n");
        for name in INTEGRATION_PUSH_WORKFLOWS {
            assert!(!validate(name, &triggers).is_empty());
        }
    }

    #[test]
    fn ambiguous_trigger_shapes_fail_to_parse() {
        for source in [
            "on: [push, workflow_dispatch]\njobs:\n",
            "on:\n  workflow_dispatch: {}\njobs:\n",
            "on:\n  \"repository_dispatch\":\njobs:\n",
            "on:\n  'repository_dispatch':\njobs:\n",
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
            "\"on\":\n  repository_dispatch:",
            "\"o\\x6e\":\n  repository_dispatch:",
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
