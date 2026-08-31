#!/usr/bin/env -S rust-script --force
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Run one guest under two backends and report the FIRST DETLOG DIVERGENCE.
//!
//! This is a diagnostic comparison of one run from each execution path. It does
//! not establish L2, full parity, or repeat determinism: those require
//! `hermit run --strict --verify --verify-strict --verify-json ...` for each
//! in-scope backend. In particular, matching guest output is not DETLOG parity.
//!
//! **The first-divergence report is the point.** A boolean ("these backends
//! differ") is not actionable. "They agree for 157 records and then ptrace logs
//! `openat(...) = Ok(3)` where DBT logs `openat(...) = Ok(4)`" is a bug report.
//! So this prints the divergent pair with surrounding context from both sides.
//!
//! ## Two things that make this harder than `hermit log-diff`
//!
//! `hermit log-diff` compares two logs. It does not produce them, and producing
//! them across backends is where the sharp edges are:
//!
//! 1. **Evidence must come from an authoritative sink.** `ptrace` honours the
//!    host-opened `--log-file`. DBT's ordinary single-run adapter deliberately
//!    refuses that option and shares stderr with the guest, so this tool refuses
//!    DBT rather than accepting guest-forgeable records. It never chooses a
//!    stderr stream merely because it contains more record-looking lines.
//! 2. **Normalization is where fake parity gets manufactured.** Every
//!    normalization here is opt-in except the wall-clock prefix, and every one
//!    actually applied is listed in the output. If a lossy normalization could
//!    hide a real difference, an apparent match is a refusal rather than rc 0.
//!
//! ## Usage
//!
//! ```text
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,ptrace -- /bin/true
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,sabre --detlog-heap -- ./guest arg
//! ./scripts/cross-backend-detlog-diff.rs --normalize host-addresses,virtual-time -- ./guest
//! ./scripts/cross-backend-detlog-diff.rs --self-test
//! ./scripts/cross-backend-detlog-diff.rs --list-normalizations
//! ```
//!
//! Exit codes: `0` streams agree · `1` they diverge · `2` the harness could not
//! produce a comparable pair (a run failed, evidence was incomplete/untrusted,
//! or a backend emitted no authoritative trace).
//! Divergence is exit 1, not an error: finding one is a successful measurement.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Exact in-band line emitted by Hermit's bounded log writer at end-of-file.
/// Keep this byte-for-byte aligned with `detcore::logdiff::TRUNCATION_MARKER`.
const TRUNCATION_MARKER: &str = "=== HERMIT LOG TRUNCATED: reached the configured size bound \
     (HERMIT_LOG_MAX_BYTES). Output beyond this point was DISCARDED. The run itself continued and \
     was NOT affected. ===";

/// A normalization the caller can switch on, and the reason it is or is not
/// safe. `default_on` is reserved for rewrites that erase something with no
/// deterministic content at all.
struct Normalization {
    name: &'static str,
    default_on: bool,
    what: &'static str,
    /// Set when enabling this can mask a real deterministic difference. Printed loudly.
    masks: Option<&'static str>,
}

const NORMALIZATIONS: &[Normalization] = &[
    Normalization {
        name: "wall-clock",
        default_on: true,
        what: "the leading real-time `2026-01-01T00:00:00.000000Z` prefix",
        masks: None,
    },
    Normalization {
        name: "host-addresses",
        default_on: false,
        what: "explicit <hostaddr 0x...> markers -> first-appearance ordinals, preserving identity and aliasing; bare hex remains exact",
        masks: None,
    },
    Normalization {
        name: "virtual-time",
        default_on: false,
        what: "virtual timestamps like 1_767_225_600.007_940_575s -> <VTIME>",
        masks: Some(
            "virtual time is a deterministic function of work done; a difference here is a REAL \
             divergence in syscall/RCB counts, not noise",
        ),
    },
    Normalization {
        name: "thread-identity",
        default_on: false,
        what: "dtid/dettid values -> first-appearance ordinals",
        masks: Some(
            "thread identities are deterministic evidence; ordinalizing them can hide a real \
             backend disagreement",
        ),
    },
];

struct Config {
    hermit: PathBuf,
    backends: Vec<String>,
    guest: Vec<String>,
    detlog_stack: bool,
    detlog_heap: bool,
    context: usize,
    normalize: Vec<String>,
    keep: Option<PathBuf>,
}

fn backend_description(backend: &str) -> Result<&'static str, String> {
    match backend {
        "ptrace" => Ok("ptrace backend"),
        "dbt" => Ok("DynamoRIO DBT backend"),
        "liteinst" => Ok("ptrace-hosted LiteInst backend"),
        "sabre" => Ok("SaBRe backend"),
        "kvm" => Ok("KVM backend"),
        "e9patch" => Ok("e9patch preprocessing with the ptrace backend"),
        _ => Err(format!(
            "unknown backend {backend:?}; expected ptrace, dbt, liteinst, sabre, kvm, or e9patch"
        )),
    }
}

fn supports_single_run_log_file(backend: &str) -> bool {
    // Live hermit-cli policy: DBT's ordinary single-run adapter refuses
    // --log-file. Its stderr is shared with the guest and is not accepted as
    // authoritative evidence by this diagnostic.
    backend != "dbt"
}

fn usage() -> String {
    let mut s = String::from(
        "Usage: scripts/cross-backend-detlog-diff.rs [OPTIONS] -- <guest> [guest-args...]\n\n\
         Diagnostic only: this compares one selected DETLOG stream per execution path.\n\
         It does not establish L2, full parity, or repeat determinism.\n\n\
         Options:\n\
         \x20 --backends A,B          two execution paths (required)\n\
         \x20 --hermit PATH           hermit binary (default: target/debug/hermit)\n\
         \x20 --detlog-stack          pass --detlog-stack to both runs\n\
         \x20 --detlog-heap           pass --detlog-heap to both runs\n\
         \x20 --context N             records of context around the divergence (default: 5)\n\
         \x20 --normalize LIST        comma-separated; see --list-normalizations\n\
         \x20 --keep DIR              keep the raw captured streams in DIR\n\
         \x20 --self-test             run inert parser/normalization/selection checks\n\
         \x20 --list-normalizations   describe every normalization and exit\n\n\
         Backends: ptrace, dbt, liteinst, sabre, kvm. `e9patch` means\n\
         preprocessing followed by the ptrace runtime; it is not a backend.\n\
         DBT currently refuses here because its one-run stderr is guest-\n\
         controllable and no isolated authenticated sink is exposed.\n\n\
         Exit: 0 agree, 1 diverge, 2 could not compare.\n",
    );
    s.push_str("\nNormalizations:\n");
    for n in NORMALIZATIONS {
        s.push_str(&format!(
            "  {:<16} {} {}\n",
            n.name,
            if n.default_on { "[on] " } else { "[off]" },
            n.what
        ));
    }
    s
}

fn parse_args() -> Result<Config, String> {
    let mut cfg = Config {
        hermit: PathBuf::from("target/debug/hermit"),
        backends: Vec::new(),
        guest: Vec::new(),
        detlog_stack: false,
        detlog_heap: false,
        context: 5,
        normalize: NORMALIZATIONS
            .iter()
            .filter(|n| n.default_on)
            .map(|n| n.name.to_string())
            .collect(),
        keep: None,
    };
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(usage()),
            "--list-normalizations" => {
                let mut out = String::new();
                for n in NORMALIZATIONS {
                    out.push_str(&format!(
                        "{}\n  default: {}\n  rewrites: {}\n",
                        n.name,
                        if n.default_on { "ON" } else { "off" },
                        n.what
                    ));
                    if let Some(m) = n.masks {
                        out.push_str(&format!("  CAUTION: {}\n", m));
                    }
                    out.push('\n');
                }
                return Err(out);
            }
            "--backends" => {
                let v = args.next().ok_or("--backends needs a value")?;
                cfg.backends = v.split(',').map(|s| s.trim().to_string()).collect();
                if cfg.backends.len() != 2 {
                    return Err("--backends takes exactly two, e.g. ptrace,dbt".into());
                }
                for backend in &cfg.backends {
                    backend_description(backend)?;
                }
            }
            "--hermit" => cfg.hermit = PathBuf::from(args.next().ok_or("--hermit needs a value")?),
            "--detlog-stack" => cfg.detlog_stack = true,
            "--detlog-heap" => cfg.detlog_heap = true,
            "--context" => {
                cfg.context = args
                    .next()
                    .ok_or("--context needs a value")?
                    .parse()
                    .map_err(|_| "--context takes a number")?;
            }
            "--keep" => cfg.keep = Some(PathBuf::from(args.next().ok_or("--keep needs a value")?)),
            "--normalize" => {
                let v = args.next().ok_or("--normalize needs a value")?;
                for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    if !NORMALIZATIONS.iter().any(|n| n.name == name) {
                        return Err(format!(
                            "unknown normalization {name:?}; see --list-normalizations"
                        ));
                    }
                    if !cfg.normalize.iter().any(|n| n == name) {
                        cfg.normalize.push(name.to_string());
                    }
                }
            }
            "--self-test" => {
                return Err("--self-test must be the only argument".into());
            }
            "--" => {
                cfg.guest = args.collect();
                break;
            }
            other => return Err(format!("unexpected argument {other:?}\n\n{}", usage())),
        }
    }
    if cfg.backends.is_empty() {
        return Err(format!(
            "--backends is required; name two execution paths explicitly\n\n{}",
            usage()
        ));
    }
    if cfg.guest.is_empty() {
        return Err(format!("no guest command given\n\n{}", usage()));
    }
    Ok(cfg)
}

fn log_was_truncated(log_text: &str) -> bool {
    let trimmed = log_text.trim_end_matches(['\n', '\r']);
    if !trimmed.ends_with(TRUNCATION_MARKER) {
        return false;
    }
    let marker_start = trimmed.len() - TRUNCATION_MARKER.len();
    marker_start == 0 || trimmed.as_bytes()[marker_start - 1] == b'\n'
}

fn ensure_complete_evidence(side: &str, source: &str, raw: &str) -> Result<(), String> {
    if log_was_truncated(raw) {
        return Err(format!(
            "{side} authoritative {source} ends with the canonical {TRUNCATION_MARKER}; refusing to compare a retained prefix"
        ));
    }
    Ok(())
}

fn ensure_complete_pair(left: &str, right: &str) -> Result<(), String> {
    match (log_was_truncated(left), log_was_truncated(right)) {
        (false, false) => Ok(()),
        (true, false) => Err("left synthetic stream is truncated".into()),
        (false, true) => Err("right synthetic stream is truncated".into()),
        (true, true) => Err("left and right synthetic streams are truncated".into()),
    }
}

fn select_authoritative_stream<'a>(
    side: &str,
    backend: &str,
    from_file: &'a str,
    stderr: &'a str,
) -> Result<(&'static str, &'a str, usize), String> {
    if backend == "dbt" {
        return Err(format!(
            "{side} DynamoRIO DBT ordinary single-run evidence is available only on stderr, which is shared with the guest and can be forged; no isolated typed/authenticated one-run sink is exposed, so this diagnostic refuses DBT (use strict canonical verify instead)"
        ));
    }
    if from_file.is_empty() {
        let stderr_records = stderr.lines().filter(|line| is_record(line)).count();
        return Err(format!(
            "{side} {} produced no authoritative --log-file evidence; refusing {} guest-controllable stderr record(s)",
            backend_description(backend).unwrap_or("unknown execution path"),
            stderr_records
        ));
    }
    let evidence_label = format!(
        "{side} {}",
        backend_description(backend).unwrap_or("unknown execution path")
    );
    ensure_complete_evidence(&evidence_label, "log-file", from_file)?;
    Ok(("log-file", from_file, stderr.len()))
}

/// One backend's captured deterministic trace, plus where it actually came from.
struct Capture {
    backend: String,
    /// "log-file" or "stderr" — which stream carried the trace.
    source: &'static str,
    /// Bytes seen on the other stream, so a silent split can be reported.
    other_stream_bytes: usize,
    exit_code: Option<i32>,
    records: Vec<String>,
}

/// A DETLOG record is any line carrying a deterministic fact: a `DETLOG` entry
/// or a scheduler `COMMIT turn`. Everything else in the trace (build chatter,
/// backend banners, DEBUG diagnostics) is not part of the deterministic
/// contract and is not compared.
fn is_record(line: &str) -> bool {
    line.contains("DETLOG") || line.contains("COMMIT turn")
}

fn capture(cfg: &Config, backend: &str, side: &str, tmpdir: &Path) -> Result<Capture, String> {
    let log_file = tmpdir.join(format!("{side}-{backend}.log-file"));
    let mut cmd = Command::new(&cfg.hermit);
    cmd.arg("--log").arg("info");
    if supports_single_run_log_file(backend) {
        cmd.arg("--log-file").arg(&log_file);
    }
    cmd.arg("run")
        .arg(format!("--backend={backend}"))
        .arg("--strict");
    if cfg.detlog_stack {
        cmd.arg("--detlog-stack");
    }
    if cfg.detlog_heap {
        cmd.arg("--detlog-heap");
    }
    cmd.arg("--").args(&cfg.guest);

    let out = cmd
        .output()
        .map_err(|e| format!("could not run {} for {backend}: {e}", cfg.hermit.display()))?;
    let stderr = String::from_utf8(out.stderr).map_err(|error| {
        format!(
            "{} emitted non-UTF-8 stderr, so its DETLOG cannot be compared: {error}",
            backend_description(backend).unwrap_or("unknown execution path")
        )
    })?;
    let from_file = match fs::read(&log_file) {
        Ok(bytes) => String::from_utf8(bytes).map_err(|error| {
            format!(
                "{} emitted a non-UTF-8 --log-file, so its DETLOG cannot be compared: {error}",
                backend_description(backend).unwrap_or("unknown execution path")
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(format!(
                "cannot read {} --log-file {}: {error}",
                backend_description(backend).unwrap_or("unknown execution path"),
                log_file.display()
            ));
        }
    };

    if !out.status.success() {
        let last_stderr = stderr.lines().last().unwrap_or("<empty stderr>");
        return Err(format!(
            "{} run failed with {}; refusing to compare its {} captured log bytes as successful evidence (last stderr line: {last_stderr})",
            backend_description(backend).unwrap_or("unknown execution path"),
            out.status,
            stderr.len() + from_file.len()
        ));
    }

    // `--log-file` is opened by Hermit in the host namespace and is the only
    // authoritative single-run sink available here. Stderr is shared with the
    // guest: record-looking lines there are diagnostics, never evidence. Check
    // the COMPLETE raw authoritative stream for the anchored truncation marker
    // before filtering it down to deterministic records.
    let (source, authoritative, other_bytes) =
        select_authoritative_stream(side, backend, &from_file, &stderr)?;
    let records: Vec<String> = authoritative
        .lines()
        .filter(|line| is_record(line))
        .map(str::to_string)
        .collect();
    if records.is_empty() {
        return Err(format!(
            "{} authoritative {source} contained no deterministic records",
            backend_description(backend).unwrap_or("unknown execution path")
        ));
    }

    if let Some(dir) = &cfg.keep {
        fs::create_dir_all(dir).map_err(|error| {
            format!("cannot create --keep directory {}: {error}", dir.display())
        })?;
        let retained_records = records.join("\n");
        let retained = [
            (
                dir.join(format!("{side}-{backend}.stderr")),
                stderr.as_bytes(),
            ),
            (
                dir.join(format!("{side}-{backend}.log-file")),
                from_file.as_bytes(),
            ),
            (
                dir.join(format!("{side}-{backend}.records")),
                retained_records.as_bytes(),
            ),
        ];
        if let Some((path, _)) = retained.iter().find(|(path, _)| path.exists()) {
            return Err(format!(
                "refusing to overwrite retained evidence {}",
                path.display()
            ));
        }
        for (path, bytes) in retained {
            fs::write(&path, bytes).map_err(|error| {
                format!(
                    "cannot write retained evidence for {backend} to {}: {error}",
                    path.display()
                )
            })?;
        }
    }

    Ok(Capture {
        backend: backend.to_string(),
        source,
        other_stream_bytes: other_bytes,
        exit_code: out.status.code(),
        records,
    })
}

/// Replace each distinct match of `pat` with a stable first-appearance ordinal,
/// so identity and aliasing survive while the host-specific value does not.
fn ordinalize(
    line: &str,
    seen: &mut HashMap<String, usize>,
    prefix: &str,
    is_start: fn(&str) -> Option<usize>,
) -> String {
    let bytes: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest: String = bytes[i..].iter().collect();
        if let Some(len) = is_start(&rest) {
            let token: String = bytes[i..i + len].iter().collect();
            let next = seen.len();
            let id = *seen.entry(token).or_insert(next);
            out.push_str(&format!("{prefix}{id}"));
            i += len;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn canonicalize_host_addresses(line: &str, seen: &mut HashMap<String, usize>) -> String {
    const PREFIX: &str = "<hostaddr ";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find(PREFIX) {
        out.push_str(&rest[..start]);
        let candidate = &rest[start + PREFIX.len()..];
        let Some(hex) = candidate
            .strip_prefix("0x")
            .or_else(|| candidate.strip_prefix("0X"))
        else {
            out.push_str(PREFIX);
            rest = candidate;
            continue;
        };
        let digits = hex.chars().take_while(char::is_ascii_hexdigit).count();
        if digits == 0 || hex.as_bytes().get(digits) != Some(&b'>') {
            out.push_str(PREFIX);
            rest = candidate;
            continue;
        }
        let token = &candidate[..2 + digits];
        let next = seen.len() + 1;
        let ordinal = *seen.entry(token.to_string()).or_insert(next);
        out.push_str(&format!("<addr{ordinal}>"));
        rest = &hex[digits + 1..];
    }
    out.push_str(rest);
    out
}

fn vtime_at(s: &str) -> Option<usize> {
    // 1_767_225_600.007_940_575s
    let b = s.as_bytes();
    if !b[0].is_ascii_digit() {
        return None;
    }
    let n = b
        .iter()
        .take_while(|c| c.is_ascii_digit() || **c == b'_' || **c == b'.')
        .count();
    if n >= 12 && b.get(n) == Some(&b's') && s[..n].contains('.') {
        Some(n + 1)
    } else {
        None
    }
}

/// Thread/process identity appears under several spellings, and they must all
/// map to the SAME ordinal for one identity -- `dtid 3`, `dettid 3` and
/// `DetPid(3)` are one thread, so keying the map on the spelling as well as the
/// number would hand the same thread three different ordinals and manufacture a
/// divergence that is not there.  Key on the NUMBER; keep the spelling intact.
const ID_PREFIXES: &[&str] = &["dtid ", "dettid ", "DetPid(", "DetTid("];

fn normalize_identities(line: &str, seen: &mut HashMap<String, usize>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    'outer: while !rest.is_empty() {
        for kw in ID_PREFIXES {
            if let Some(after) = rest.strip_prefix(*kw) {
                let n = after.chars().take_while(char::is_ascii_digit).count();
                if n > 0 {
                    let digits = &after[..n];
                    let next = seen.len();
                    let id = *seen.entry(digits.to_string()).or_insert(next);
                    out.push_str(kw);
                    out.push_str(&format!("#{id}"));
                    rest = &after[n..];
                    continue 'outer;
                }
            }
        }
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn normalize(records: &[String], enabled: &[String]) -> Vec<String> {
    let on = |name: &str| enabled.iter().any(|e| e == name);
    let mut addrs = HashMap::new();
    let mut tids = HashMap::new();
    records
        .iter()
        .map(|line| {
            let mut s = line.clone();
            if on("wall-clock") {
                // `2026-08-06T13:38:32.654561Z  INFO ...` -> `INFO ...`
                if let Some(idx) = s.find('Z') {
                    let head = &s[..idx];
                    if head.len() >= 19
                        && head.starts_with(|c: char| c.is_ascii_digit())
                        && head.contains('T')
                    {
                        s = s[idx + 1..].trim_start().to_string();
                    }
                }
            }
            if on("host-addresses") {
                s = canonicalize_host_addresses(&s, &mut addrs);
            }
            if on("thread-identity") {
                s = normalize_identities(&s, &mut tids);
            }
            if on("virtual-time") {
                let mut vt = HashMap::new();
                s = ordinalize(&s, &mut vt, "<VTIME>", vtime_at);
                let _ = vt;
            }
            s
        })
        .collect()
}

fn print_manifest(cfg: &Config, a: &Capture, b: &Capture) {
    println!("== provenance ==");
    for c in [a, b] {
        println!(
            "  {:<8} ({}) trace from {:<8} ({} records, {} bytes on the other stream), guest exit {}",
            c.backend,
            backend_description(&c.backend).unwrap_or("unknown execution path"),
            c.source,
            c.records.len(),
            c.other_stream_bytes,
            c.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
        );
    }
    println!(
        "  assurance: diagnostic comparison of one run per execution path; not L2, full parity, or repeat determinism"
    );
    if a.source != b.source {
        println!(
            "  NOTE: the two backends reported through DIFFERENT streams ({} vs {}). That is a \
             known per-backend logging difference, not a guest divergence.",
            a.source, b.source
        );
    }
    println!("== normalizations applied ==");
    if cfg.normalize.is_empty() {
        println!("  (none - records compared verbatim)");
    }
    for name in &cfg.normalize {
        let n = NORMALIZATIONS
            .iter()
            .find(|n| n.name == name.as_str())
            .unwrap();
        println!("  {:<16} {}", n.name, n.what);
        if let Some(m) = n.masks {
            println!("      !! CAUTION: {}", m);
        }
    }
    let off: Vec<&str> = NORMALIZATIONS
        .iter()
        .filter(|n| !cfg.normalize.iter().any(|e| e == n.name))
        .map(|n| n.name)
        .collect();
    if !off.is_empty() {
        println!("  not applied: {}", off.join(", "));
    }
}

fn lossy_normalizations(enabled: &[String]) -> Vec<&'static str> {
    NORMALIZATIONS
        .iter()
        .filter(|normalization| {
            normalization.masks.is_some() && enabled.iter().any(|name| name == normalization.name)
        })
        .map(|normalization| normalization.name)
        .collect()
}

fn matching_records_refusal(enabled: &[String]) -> Option<String> {
    let lossy = lossy_normalizations(enabled);
    (!lossy.is_empty()).then(|| {
        format!(
            "selected records match only after lossy normalization(s) {}; real differences may have been erased",
            lossy.join(", ")
        )
    })
}

fn report_divergence(cfg: &Config, a: &Capture, b: &Capture, na: &[String], nb: &[String]) -> i32 {
    let first = (0..na.len().min(nb.len())).find(|&i| na[i] != nb[i]);
    let ctx = cfg.context;

    match first {
        None if na.len() == nb.len() => {
            if let Some(reason) = matching_records_refusal(&cfg.normalize) {
                eprintln!("\nREFUSAL: {reason}; this diagnostic cannot emit a match.");
                return 2;
            }
            println!(
                "\nDIAGNOSTIC MATCH: {} selected records compared.",
                na.len()
            );
            0
        }
        None => {
            // One is a strict prefix of the other.
            let (short, long, si, li) = if na.len() < nb.len() {
                (a, b, na.len(), nb.len())
            } else {
                (b, a, nb.len(), na.len())
            };
            println!(
                "\nFIRST DIVERGENCE: streams agree for all {si} records, then {} ENDS while {} \
                 continues to {li}.",
                short.backend, long.backend
            );
            let longer = if na.len() < nb.len() { nb } else { na };
            println!("\n  next records only {} produced:", long.backend);
            for line in longer.iter().skip(si).take(ctx) {
                println!("    + {line}");
            }
            1
        }
        Some(i) => {
            println!(
                "\nFIRST DIVERGENCE at record index {i} (1-based record {}).",
                i + 1
            );
            println!(
                "  {} records: {} | {} records: {}",
                a.backend,
                na.len(),
                b.backend,
                nb.len()
            );
            println!("\n  context (identical in both):");
            for (j, line) in na.iter().enumerate().take(i).skip(i.saturating_sub(ctx)) {
                println!("    {j:>6} | {line}");
            }
            println!("\n  >>> divergent record {i}:");
            println!("    - {:<8} {}", a.backend, na[i]);
            println!("    + {:<8} {}", b.backend, nb[i]);
            println!("\n  following context (already downstream of the divergence):");
            for j in (i + 1)..(i + 1 + ctx) {
                let l = na.get(j).map(String::as_str).unwrap_or("<end>");
                let r = nb.get(j).map(String::as_str).unwrap_or("<end>");
                println!("    {j:>6} - {}", l);
                println!("    {j:>6} + {}", r);
            }
            println!(
                "\n  Only the FIRST divergence is meaningful; everything after it is downstream."
            );
            1
        }
    }
}

fn main() {
    rust_script_prelude::init();
    let argv = env::args().collect::<Vec<_>>();
    if argv.get(1).map(String::as_str) == Some("--self-test") {
        if argv.len() != 2 {
            eprintln!("cross-backend-detlog-diff: --self-test must be the only argument");
            std::process::exit(2);
        }
        run_self_test();
        return;
    }
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(message) => {
            // --help / --list-normalizations exit 0; a real parse error exits 2.
            let is_help = message.starts_with("Usage:") || message.contains("\n  default: ");
            if is_help {
                print!("{message}");
                std::process::exit(0);
            }
            eprintln!("cross-backend-detlog-diff: {message}");
            std::process::exit(2);
        }
    };

    if !cfg.hermit.exists() {
        eprintln!(
            "cross-backend-detlog-diff: hermit binary not found at {} (pass --hermit PATH)",
            cfg.hermit.display()
        );
        std::process::exit(2);
    }

    // The retained copies are written by this host-side harness and therefore
    // survive under /tmp. Warn once, outside the per-backend capture loop, so
    // users do not infer that a guest-written --log-file is safe there too.
    if let Some(dir) = &cfg.keep {
        if dir.starts_with("/tmp") {
            eprintln!(
                "cross-backend-detlog-diff: warning: --keep {} is under /tmp, which hermit \
                 overmounts for the guest; these host-written copies survive, but do not use \
                 this location for guest-written evidence.",
                dir.display()
            );
        }
    }

    // NOT env::temp_dir(). Hermit overmounts the guest's /tmp, so a --log-file
    // written under /tmp lands inside the container and silently never appears
    // on the host: the run succeeds, the file is absent, and the harness sees
    // zero records. That failure looks exactly like "this backend emits no
    // trace", which is why it is worth spelling out here.
    let scratch_root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| "neither XDG_CACHE_HOME nor HOME is set".to_string())
        .unwrap_or_else(|error| {
            eprintln!(
                "cross-backend-detlog-diff: {error}; cannot choose a host-visible scratch directory"
            );
            std::process::exit(2);
        })
        .join("hermit-xbackend-detlog");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let tmpdir = scratch_root.join(format!("run-{}-{nonce}", std::process::id()));
    if let Err(e) = fs::create_dir_all(&tmpdir) {
        eprintln!(
            "cross-backend-detlog-diff: cannot create scratch dir {}: {e}",
            tmpdir.display()
        );
        std::process::exit(2);
    }

    let mut captures = Vec::new();
    for (index, backend) in cfg.backends.iter().enumerate() {
        let side = if index == 0 { "left" } else { "right" };
        match capture(&cfg, backend, side, &tmpdir) {
            Ok(c) => captures.push(c),
            Err(e) => {
                if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                    eprintln!(
                        "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                        tmpdir.display()
                    );
                }
                eprintln!("cross-backend-detlog-diff: {e}");
                std::process::exit(2);
            }
        }
    }
    let (a, b) = (&captures[0], &captures[1]);

    println!(
        "cross-backend DETLOG diff: {} vs {}  guest: {}",
        a.backend,
        b.backend,
        cfg.guest.join(" ")
    );
    print_manifest(&cfg, a, b);

    for c in [a, b] {
        if c.records.is_empty() {
            eprintln!(
                "\ncross-backend-detlog-diff: {} produced NO deterministic records on either \
                 stream; there is nothing to compare. Check that the run succeeded and that the \
                 backend is available in this build.",
                c.backend
            );
            if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                eprintln!(
                    "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                    tmpdir.display()
                );
            }
            std::process::exit(2);
        }
    }

    let na = normalize(&a.records, &cfg.normalize);
    let nb = normalize(&b.records, &cfg.normalize);
    let code = report_divergence(&cfg, a, b, &na, &nb);
    if let Err(error) = fs::remove_dir_all(&tmpdir) {
        eprintln!(
            "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {error}",
            tmpdir.display()
        );
    }
    std::process::exit(code);
}

fn run_self_test() {
    let mut failures = Vec::new();
    let mut checks = 0usize;
    let mut check = |condition: bool, message: &str| {
        checks += 1;
        if !condition {
            failures.push(message.to_string());
        }
    };

    for backend in ["ptrace", "dbt", "liteinst", "sabre", "kvm", "e9patch"] {
        check(
            backend_description(backend).is_ok(),
            &format!("known execution path {backend} was rejected"),
        );
    }
    check(
        backend_description("dbi").is_err(),
        "obsolete dbi spelling was accepted",
    );
    check(
        !supports_single_run_log_file("dbt") && supports_single_run_log_file("ptrace"),
        "DBT single-run log sink policy was not represented",
    );
    check(
        backend_description("unknown").is_err(),
        "unknown backend was accepted",
    );
    check(
        backend_description("e9patch")
            .unwrap()
            .contains("preprocessing with the ptrace backend"),
        "e9patch was presented as an independent backend",
    );

    let record = "2026-08-06T13:38:32.654561Z INFO DETLOG dettid 9 brk=0xabcdef01";
    check(is_record(record), "DETLOG line was not selected");
    check(
        !is_record("INFO build chatter"),
        "non-DETLOG line was selected",
    );
    let normalized = normalize(&[record.to_string()], &["wall-clock".into()]);
    check(
        normalized == ["INFO DETLOG dettid 9 brk=0xabcdef01"],
        "default normalization changed more than the wall-clock prefix",
    );

    let complete = "INFO detcore: DETLOG complete";
    let truncated = format!("{complete}\n{TRUNCATION_MARKER}\n");
    check(
        log_was_truncated(&truncated),
        "canonical end-of-file truncation marker was not detected",
    );
    check(
        !log_was_truncated(&format!(
            "INFO detcore: DETLOG guest quoted {TRUNCATION_MARKER}"
        )),
        "guest-controlled marker text on a prefixed line triggered truncation",
    );
    check(
        !log_was_truncated(&format!("{TRUNCATION_MARKER}\ntrailing data")),
        "non-terminal marker triggered truncation",
    );
    check(
        ensure_complete_pair(&truncated, complete).is_err_and(|error| error.starts_with("left ")),
        "one-sided left truncation did not identify and refuse the left side",
    );
    check(
        ensure_complete_pair(complete, &truncated).is_err_and(|error| error.starts_with("right ")),
        "one-sided right truncation did not identify and refuse the right side",
    );
    check(
        ensure_complete_pair(&truncated, &truncated)
            .is_err_and(|error| error.starts_with("left and right ")),
        "two-sided truncation did not identify and refuse both sides",
    );

    let authoritative = "INFO detcore: DETLOG authoritative";
    let forged_longer = "INFO detcore: DETLOG forged-1\nINFO detcore: DETLOG forged-2";
    check(
        matches!(
            select_authoritative_stream("left", "ptrace", authoritative, forged_longer),
            Ok(("log-file", selected, _)) if selected == authoritative
        ),
        "longer forged stderr displaced the authoritative log file",
    );
    check(
        matches!(
            select_authoritative_stream(
                "left",
                "ptrace",
                "INFO detcore: DETLOG file",
                "INFO detcore: DETLOG conflicting-stderr",
            ),
            Ok(("log-file", "INFO detcore: DETLOG file", _))
        ),
        "equal-length conflicting stderr made authoritative selection ambiguous",
    );
    check(
        select_authoritative_stream("left", "ptrace", "", forged_longer).is_err(),
        "guest-controllable stderr was accepted without an authoritative log file",
    );
    check(
        select_authoritative_stream("left", "dbt", "", forged_longer).is_err(),
        "DBT guest-controllable stderr was accepted as evidence",
    );
    check(
        select_authoritative_stream("left", "ptrace", "", "").is_err(),
        "empty authoritative evidence did not refuse",
    );

    let marked_left = normalize(
        &["INFO DETLOG marked=<hostaddr 0x111111> bare=0xabcdef01".into()],
        &["host-addresses".into()],
    );
    let marked_right = normalize(
        &["INFO DETLOG marked=<hostaddr 0xaaaaaa> bare=0xabcdef01".into()],
        &["host-addresses".into()],
    );
    check(
        marked_left == marked_right && marked_left[0].contains("<addr1>"),
        "explicit host-address markers did not canonicalize by ordinal",
    );
    let bare_left = normalize(
        &["INFO DETLOG result=0xabcdef01".into()],
        &["host-addresses".into()],
    );
    let bare_right = normalize(
        &["INFO DETLOG result=0xabcdef02".into()],
        &["host-addresses".into()],
    );
    check(
        bare_left != bare_right,
        "meaningful bare hexadecimal difference was erased",
    );
    let alias_left = normalize(
        &["INFO DETLOG pair=<hostaddr 0x111111>,<hostaddr 0x111111>".into()],
        &["host-addresses".into()],
    );
    let alias_right = normalize(
        &["INFO DETLOG pair=<hostaddr 0xaaaaaa>,<hostaddr 0xbbbbbb>".into()],
        &["host-addresses".into()],
    );
    check(
        alias_left != alias_right,
        "host-address canonicalization erased an aliasing difference",
    );
    check(
        matching_records_refusal(&["wall-clock".into(), "virtual-time".into()]).is_some(),
        "virtual-time-normalized equality could emit a match",
    );
    check(
        matching_records_refusal(&["thread-identity".into()]).is_some(),
        "thread-identity-normalized equality could emit a match",
    );
    check(
        matching_records_refusal(&["wall-clock".into(), "host-addresses".into()]).is_none(),
        "canonical-only normalization was incorrectly classified as lossy",
    );

    if failures.is_empty() {
        println!("cross-backend-detlog-diff self-test: PASS ({checks} checks)");
    } else {
        for failure in &failures {
            eprintln!("cross-backend-detlog-diff self-test: FAIL: {failure}");
        }
        std::process::exit(2);
    }
}
