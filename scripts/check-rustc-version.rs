#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Refuse drift between every place this tree names its Rust version:
//!
//!   1. `rust-toolchain.toml`'s channel (what rustup and Cargo honor),
//!   2. `[hermit] rustc_version` in `.buckconfig` and `.buckconfig.oss`,
//!      plus `.buckconfig.local` when present (what the shim Rust
//!      toolchain stamps into every Buck rust action key, shim/BUCK;
//!      Buck uses the last definition, and the local file layers last,
//!      so this checker reads them in that order), and
//!   3. with `--host`, the compiler that will actually run: the full
//!      `rustc -vV` release **and commit hash** must equal what
//!      `rustup run <channel> rustc -vV` reports for the pinned channel.
//!
//! Only an exact pin is accepted: `X.Y.Z` or `nightly-YYYY-MM-DD`. A
//! floating channel (`stable`, bare `nightly`) is refused even when all
//! files agree, because the stamp would then stay constant while the
//! compiler changes underneath it — the exact condition that let a
//! shared action cache serve a nightly-built rlib to the pinned build,
//! which rustc refused with E0514.
//!
//! What this cannot see: a `--config` command-line override of the pin.
//! Callers who pass one own the consequences; the checked-in files and
//! the local layer are what every normal build in this tree uses.
//!
//! Usage: check-rustc-version.rs [--host] [ROOT]
//! Exit 0 and one OK line on agreement; exit 1 naming every disagreement.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Whether `v` is an exact pin: X.Y.Z (all numeric) or nightly-YYYY-MM-DD.
fn is_exact_pin(v: &str) -> bool {
    if let Some(date) = v.strip_prefix("nightly-") {
        let parts: Vec<&str> = date.split('-').collect();
        return parts.len() == 3
            && parts[0].len() == 4
            && parts[1].len() == 2
            && parts[2].len() == 2
            && parts
                .iter()
                .all(|p| p.chars().all(|c| c.is_ascii_digit()));
    }
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// Last `[hermit] rustc_version` in a buckconfig text (Buck uses the last
/// definition). The value is the raw remainder, whitespace-trimmed: Buck
/// keeps an inline `#...` as part of the value, so this checker must too,
/// or it would pass while Buck stamps something else.
fn parse_buckconfig_pin(text: &str) -> Option<String> {
    let mut in_hermit = false;
    let mut found = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_hermit = line == "[hermit]";
            continue;
        }
        if in_hermit {
            if let Some(rest) = line.strip_prefix("rustc_version") {
                if let Some(v) = rest.trim_start().strip_prefix('=') {
                    found = Some(v.trim().trim_matches('"').to_owned());
                }
            }
        }
    }
    found
}

fn parse_channel(text: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(v) = line.strip_prefix("channel") {
            if let Some(v) = v.trim().strip_prefix('=') {
                let v = v.trim();
                // TOML recognizes the comment; take the quoted token.
                if let Some(stripped) = v.strip_prefix('"') {
                    return stripped.split('"').next().map(str::to_owned);
                }
                return Some(v.split('#').next().unwrap_or("").trim().to_owned());
            }
        }
    }
    None
}

/// (release, commit-hash) from `rustc -vV` output.
fn parse_verbose_version(text: &str) -> Option<(String, String)> {
    let mut release = None;
    let mut hash = None;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("release:") {
            release = Some(v.trim().to_owned());
        }
        if let Some(v) = line.strip_prefix("commit-hash:") {
            hash = Some(v.trim().to_owned());
        }
    }
    Some((release?, hash?))
}

fn read_pin_file(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_buckconfig_pin(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: unreadable: {e}", path.display())),
    }
}

/// Pure verdict over already-read values; every disagreement is named.
fn evaluate(
    channel: &str,
    base_pins: &[(&str, Option<String>)],
    effective_pin: Option<&str>,
    host: Option<(&str, &str)>,
    expected_host: Option<(&str, &str)>,
) -> Vec<String> {
    let mut errors = Vec::new();
    if !is_exact_pin(channel) {
        errors.push(format!(
            "rust-toolchain.toml channel {channel:?} is floating, not an exact pin \
             (X.Y.Z or nightly-YYYY-MM-DD): the stamp would stay constant while \
             the compiler moves"
        ));
    }
    for (file, pin) in base_pins {
        match pin {
            None => errors.push(format!("{file}: no [hermit] rustc_version line")),
            Some(pin) if pin != channel => errors.push(format!(
                "{file} [hermit] rustc_version = {pin} != rust-toolchain.toml channel {channel}"
            )),
            _ => {}
        }
    }
    if let (Some(eff), Some((_, base))) = (
        effective_pin,
        base_pins.iter().find(|(_, p)| p.is_some()),
    ) {
        if let Some(base) = base {
            if eff != base.as_str() {
                errors.push(format!(
                    "effective pin {eff} (after .buckconfig.local) != checked-in pin {base}: \
                     this checkout stamps a pin the tree does not record"
                ));
            }
        }
    }
    if let (Some((h_rel, h_hash)), Some((e_rel, e_hash))) = (host, expected_host) {
        if h_rel != e_rel || h_hash != e_hash {
            errors.push(format!(
                "host rustc is {h_rel} ({h_hash}) but the pinned channel resolves to \
                 {e_rel} ({e_hash}): this host would upload under a pin its compiler does not match"
            ));
        }
    }
    errors
}

fn verbose_of(cmd: &mut Command, what: &str) -> Result<(String, String), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("{what}: cannot run: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what}: exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    parse_verbose_version(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| format!("{what}: unparsable -vV output"))
}

fn run(root: &Path, check_host: bool) -> Result<String, Vec<String>> {
    run_io(root, check_host, &|channel| {
        verbose_of(
            Command::new("rustc").arg("-vV"),
            "rustc -vV",
        )
        .map(|host| {
            let expected = verbose_of(
                Command::new("rustup")
                    .args(["run", channel, "rustc", "-vV"]),
                "rustup run <channel> rustc -vV",
            )?;
            Ok((host, expected))
        })
        .and_then(|x| x)
    })
}

/// `host_probe` is split out so tests never execute a real compiler.
fn run_io(
    root: &Path,
    check_host: bool,
    host_probe: &dyn Fn(&str) -> Result<((String, String), (String, String)), String>,
) -> Result<String, Vec<String>> {
    let mut errors = Vec::new();
    let channel = match std::fs::read_to_string(root.join("rust-toolchain.toml")) {
        Ok(text) => match parse_channel(&text) {
            Some(c) => Some(c),
            None => {
                errors.push("rust-toolchain.toml: no channel line".to_owned());
                None
            }
        },
        Err(e) => {
            errors.push(format!("rust-toolchain.toml: unreadable: {e}"));
            None
        }
    };
    let mut base_pins = Vec::new();
    for f in [".buckconfig", ".buckconfig.oss"] {
        match read_pin_file(&root.join(f)) {
            Ok(pin) => base_pins.push((f, pin)),
            Err(e) => errors.push(e),
        }
    }
    // Effective pin: last base definition, overridden by the local layer.
    let mut effective: Option<String> =
        base_pins.iter().rev().find_map(|(_, p)| p.clone());
    match read_pin_file(&root.join(".buckconfig.local")) {
        Ok(Some(local)) => effective = Some(local),
        Ok(None) => {}
        Err(e) => errors.push(e),
    }
    let (host, expected) = if check_host {
        match &channel {
            Some(c) => match host_probe(c) {
                Ok((h, e)) => (Some(h), Some(e)),
                Err(e) => {
                    errors.push(e);
                    (None, None)
                }
            },
            None => (None, None),
        }
    } else {
        (None, None)
    };
    if let Some(channel) = &channel {
        errors.extend(evaluate(
            channel,
            &base_pins
                .iter()
                .map(|(f, p)| (*f, p.clone()))
                .collect::<Vec<_>>(),
            effective.as_deref(),
            host.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
            expected.as_ref().map(|(a, b)| (a.as_str(), b.as_str())),
        ));
        if errors.is_empty() {
            return Ok(channel.clone());
        }
    }
    Err(errors)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check_host = args.iter().any(|a| a == "--host");
    let root = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    match run(&root, check_host) {
        Ok(v) => {
            println!(
                "OK: rustc pin {v} agrees in rust-toolchain.toml, .buckconfig, .buckconfig.oss{}",
                if check_host { " and host" } else { "" }
            );
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("RUSTC VERSION PIN DISAGREEMENT: {e}");
            }
            eprintln!(
                "RUSTC PIN REGRESSION - BLOCKED: bump [hermit] rustc_version \
                 in both .buckconfig files in the same change as \
                 rust-toolchain.toml (see the toolchain stamp in shim/BUCK)"
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn last_pin_wins_and_inline_comment_is_kept() {
        let t = "[hermit]\nrustc_version = 1.0.0\nrustc_version = 1.97.1\n";
        assert_eq!(parse_buckconfig_pin(t).as_deref(), Some("1.97.1"));
        // Buck keeps an inline # as part of the value, so the checker
        // must see (and reject by mismatch) the same bytes Buck stamps.
        let t2 = "[hermit]\nrustc_version = 1.97.1 # note\n";
        assert_eq!(
            parse_buckconfig_pin(t2).as_deref(),
            Some("1.97.1 # note")
        );
        assert_eq!(parse_buckconfig_pin("[rust]\nrustc_version = 9.9.9\n"), None);
    }

    #[test]
    fn exact_pin_acceptance() {
        assert!(is_exact_pin("1.97.1"));
        assert!(is_exact_pin("nightly-2026-07-14"));
        assert!(!is_exact_pin("nightly"));
        assert!(!is_exact_pin("stable"));
        assert!(!is_exact_pin("1.97"));
        assert!(!is_exact_pin("1.97.1-beta"));
    }

    #[test]
    fn verbose_version_parses() {
        let v = parse_verbose_version(
            "rustc 1.97.1 (8bab26f4f 2026-07-14)\n\
             binary: rustc\ncommit-hash: 8bab26f4f68e0e26f0bb7960be334d5b520ea452\n\
             commit-date: 2026-07-14\nrelease: 1.97.1\n",
        );
        assert_eq!(
            v,
            Some((
                "1.97.1".to_owned(),
                "8bab26f4f68e0e26f0bb7960be334d5b520ea452".to_owned()
            ))
        );
    }

    fn tree(
        channel: &str,
        buckconfig: &str,
        oss: &str,
        local: Option<&str>,
    ) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "check-rustc-version-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("rust-toolchain.toml"),
            format!("[toolchain]\nchannel = \"{channel}\"\n"),
        )
        .unwrap();
        fs::write(
            dir.join(".buckconfig"),
            format!("[hermit]\nrustc_version = {buckconfig}\n"),
        )
        .unwrap();
        fs::write(
            dir.join(".buckconfig.oss"),
            format!("[hermit]\nrustc_version = {oss}\n"),
        )
        .unwrap();
        if let Some(local) = local {
            fs::write(
                dir.join(".buckconfig.local"),
                format!("[hermit]\nrustc_version = {local}\n"),
            )
            .unwrap();
        }
        dir
    }

    fn no_host(_: &str) -> Result<((String, String), (String, String)), String> {
        Err("host probe must not run".to_owned())
    }

    #[test]
    fn run_accepts_matching_tree() {
        let d = tree("1.97.1", "1.97.1", "1.97.1", None);
        assert_eq!(run_io(&d, false, &no_host).unwrap(), "1.97.1");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn run_refuses_each_mismatch_kind() {
        // pin != channel
        let d = tree("1.99.1", "1.97.1", "1.97.1", None);
        let e = run_io(&d, false, &no_host).unwrap_err().join("\n");
        assert!(e.contains("!= rust-toolchain.toml channel"), "{e}");
        fs::remove_dir_all(&d).unwrap();
        // floating channel, everything agreeing: still refused
        let d = tree("nightly", "nightly", "nightly", None);
        let e = run_io(&d, false, &no_host).unwrap_err().join("\n");
        assert!(e.contains("floating"), "{e}");
        fs::remove_dir_all(&d).unwrap();
        // .buckconfig.oss split
        let d = tree("1.97.1", "1.97.1", "1.98.0", None);
        let e = run_io(&d, false, &no_host).unwrap_err().join("\n");
        assert!(e.contains(".buckconfig.oss"), "{e}");
        fs::remove_dir_all(&d).unwrap();
        // local override drift
        let d = tree("1.97.1", "1.97.1", "1.97.1", Some("1.99.0-nightly"));
        let e = run_io(&d, false, &no_host).unwrap_err().join("\n");
        assert!(e.contains("effective pin"), "{e}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn run_refuses_wrong_host_commit() {
        let d = tree("1.97.1", "1.97.1", "1.97.1", None);
        let probe = |_: &str| {
            Ok((
                ("1.97.1".to_owned(), "deadbeef".to_owned()),
                ("1.97.1".to_owned(), "8bab26f4f".to_owned()),
            ))
        };
        let e = run_io(&d, true, &probe).unwrap_err().join("\n");
        assert!(e.contains("host rustc is"), "{e}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn run_fails_closed_on_missing_files() {
        let d = tree("1.97.1", "1.97.1", "1.97.1", None);
        fs::remove_file(d.join(".buckconfig.oss")).unwrap();
        let e = run_io(&d, false, &no_host).unwrap_err().join("\n");
        assert!(e.contains("no [hermit] rustc_version"), "{e}");
        fs::remove_dir_all(&d).unwrap();
    }
}
