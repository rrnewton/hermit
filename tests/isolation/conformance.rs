#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! ```cargo
//! [package]
//! edition = "2024"
//! ```

pub mod driver;
#[path = "../../scripts/lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeSet;
use std::path::PathBuf;

fn selection(value: &str) -> Vec<&str> {
    value.split(',').collect()
}

fn validate_selection(
    name: &str,
    selected: &Option<String>,
    allowed: BTreeSet<String>,
) -> Result<(), String> {
    let Some(selected) = selected else {
        return Ok(());
    };
    let mut seen = BTreeSet::new();
    for value in selection(selected) {
        if !allowed.contains(value) {
            return Err(format!(
                "{name} contains an unknown or empty selection {value:?}"
            ));
        }
        if !seen.insert(value) {
            return Err(format!("{name} repeats selection {value:?}"));
        }
    }
    Ok(())
}

fn run() -> Result<bool, String> {
    let mut repository = std::env::current_dir().map_err(|error| format!("repository: {error}"))?;
    let mut hermit = None;
    let mut safehermit = None;
    let mut artifacts = None;
    let mut installation = None;
    let mut backends = None;
    let mut roots = None;
    let mut paths = None;
    let mut policies = None;
    let mut scope = "full".to_owned();
    let mut list = false;
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < arguments.len() {
        let argument = &arguments[index];
        if matches!(argument.as_str(), "--help" | "-h") {
            println!("Usage: conformance.rs --hermit PATH --artifacts DIR [OPTIONS]");
            println!("Defaults to every required backend, root, path, and namespace policy.");
            println!(
                "Options: --repository DIR --safehermit PATH --installation DIR --backends LIST --roots LIST --paths LIST --policies LIST --assert-scope full|launch-migration --list"
            );
            println!(
                "Missing resources and incomplete observations fail requested cells. A selected scope makes no broader parity claim."
            );
            return Ok(true);
        }
        if argument == "--list" {
            list = true;
            index += 1;
            continue;
        }
        let (name, value, consumed) = if let Some((name, value)) = argument.split_once('=') {
            (name, value, 1)
        } else {
            (
                argument.as_str(),
                arguments
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument} requires a value"))?
                    .as_str(),
                2,
            )
        };
        match name {
            "--repository" => repository = PathBuf::from(value),
            "--hermit" => hermit = Some(PathBuf::from(value)),
            "--safehermit" => safehermit = Some(PathBuf::from(value)),
            "--artifacts" => artifacts = Some(PathBuf::from(value)),
            "--installation" => installation = Some(PathBuf::from(value)),
            "--backends" => backends = Some(value.to_owned()),
            "--roots" => roots = Some(value.to_owned()),
            "--paths" => paths = Some(value.to_owned()),
            "--policies" => policies = Some(value.to_owned()),
            "--assert-scope" => scope = value.to_owned(),
            _ => return Err(format!("unknown option {name}")),
        }
        index += consumed;
    }
    if !matches!(scope.as_str(), "full" | "launch-migration") {
        return Err("--assert-scope must be full or launch-migration".to_owned());
    }
    let required = driver::required_cells();
    validate_selection(
        "--backends",
        &backends,
        required.iter().map(|cell| cell.backend.clone()).collect(),
    )?;
    validate_selection(
        "--roots",
        &roots,
        required.iter().map(|cell| cell.root.clone()).collect(),
    )?;
    validate_selection(
        "--paths",
        &paths,
        required.iter().map(|cell| cell.path.clone()).collect(),
    )?;
    validate_selection(
        "--policies",
        &policies,
        required
            .iter()
            .map(|cell| driver::policy_name(cell.policy).to_owned())
            .collect(),
    )?;
    let cells: Vec<_> = required
        .into_iter()
        .filter(|cell| {
            backends
                .as_ref()
                .is_none_or(|value| selection(value).contains(&cell.backend.as_str()))
                && roots
                    .as_ref()
                    .is_none_or(|value| selection(value).contains(&cell.root.as_str()))
                && paths
                    .as_ref()
                    .is_none_or(|value| selection(value).contains(&cell.path.as_str()))
                && policies.as_ref().is_none_or(|value| {
                    selection(value).contains(&driver::policy_name(cell.policy))
                })
        })
        .collect();
    if cells.is_empty() {
        return Err("selection names no required cells".to_owned());
    }
    if scope == "launch-migration"
        && (backends.is_none()
            || roots.is_none()
            || paths.is_none()
            || policies.is_none()
            || cells.iter().any(|cell| {
                cell.root != "host"
                    || cell.path != "run-raw"
                    || cell.policy != driver::conformance::NamespacePolicy::Private
            }))
    {
        return Err(
            "launch-migration scope requires an explicit host/run-raw/private selection".to_owned(),
        );
    }
    if list {
        for cell in cells {
            println!("{}", cell.name());
        }
        return Ok(true);
    }
    let hermit = hermit.ok_or("--hermit is required")?;
    let artifacts = artifacts
        .ok_or("--artifacts is required and must name a fresh ignored/ or target/ directory")?;
    let safehermit = safehermit.map_or_else(|| driver::find_safehermit(&repository), Ok)?;
    let prepared = driver::Prepared::build_with_installation(
        &repository,
        &hermit,
        &safehermit,
        &artifacts,
        installation.as_deref(),
    )?;
    let mut matches = true;
    for cell in cells {
        let result = prepared.collect(cell);
        let accepted = if scope == "launch-migration" {
            result.launch_migration_matches()
        } else {
            result.full_matches()
        };
        let failures = result
            .assertions
            .iter()
            .filter(|row| row.verdict != driver::conformance::Verdict::Match)
            .count();
        println!(
            "{} scope={} verdict={} full_required_failures={} report={}",
            result.cell.name(),
            scope,
            if accepted {
                "match"
            } else {
                "mismatch_or_no_result"
            },
            failures,
            result.directory.join("assertions.json").display()
        );
        matches &= accepted;
    }
    Ok(matches)
}

fn main() {
    rust_script_prelude::init();
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(error) => {
            eprintln!("isolation conformance: {error}");
            std::process::exit(2);
        }
    }
}
