#!/usr/bin/env -S rust-script --force
//! ```cargo
//! [dependencies]
//! detcore = { package = "hermit-detcore", path = "../detcore" }
//! ```
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Run one guest under two execution paths and report the first canonical INFO
//! divergence.
//!
//! This is a diagnostic comparison of one run from each execution path. It does
//! not establish L2, full parity, or repeat determinism: those require
//! `hermit run --strict --verify --verify-strict --verify-json ...` for each
//! in-scope backend. In particular, matching guest output is not canonical log
//! parity.
//!
//! **The first-divergence report is the point.** A boolean ("these backends
//! differ") is not actionable. "They agree for 157 records and then ptrace logs
//! `openat(...) = Ok(3)` where DBT logs `openat(...) = Ok(4)`" is a bug report.
//! The shared `detcore::logdiff` implementation prints the divergent pair with
//! context from both sides.
//!
//! ## Two things that make this harder than `hermit log-diff`
//!
//! `hermit log-diff` compares two logs. It does not produce them, and producing
//! them across backends is where the sharp edges are:
//!
//! 1. **Evidence must come from an authoritative sink.** `ptrace` honours the
//!    host-opened `--log-file`. DBT publishes a protected ordinary-run log whose
//!    typed decoder authenticates transport initialization separately and
//!    exposes only comparable records. SaBRe forwards its real Detcore records
//!    to raw stderr while the host `--log-file` contains only an incomplete
//!    controller stream. This tool refuses that case rather than accepting
//!    guest-forgeable evidence.
//! 2. **Normalization is where fake parity gets manufactured.** Comparison uses
//!    the fixed `BitwiseInfoV1` policy from `detcore::logdiff`: remove only the
//!    real wall-clock prefix, canonicalize explicitly marked host addresses by
//!    first appearance, and compare every other selected byte exactly. Legacy
//!    lossy normalization flags are recognized but refused before either run.
//!
//! ## Usage
//!
//! ```text
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,ptrace -- /bin/true
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,liteinst --detlog-heap -- ./guest arg
//! ./scripts/cross-backend-detlog-diff.rs --context 3 --backends ptrace,kvm -- ./guest
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

use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

use detcore::detlog::DetLogEvent;
use detcore::detlog::record_suffix;
use detcore::logdiff::BitwiseInfoV1Diagnostics;
use detcore::logdiff::ComparisonSideLabels;
use detcore::logdiff::LogDiffSummary;
use detcore::logdiff::TRUNCATION_MARKER;
use detcore::logdiff::try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics;
use detcore::logdiff::write_bitwise_info_v1_bytes;

/// A historical command-line normalization name and whether the fixed
/// `BitwiseInfoV1` policy applies it. Names that can erase deterministic
/// content remain parse-compatible but are refused before capture.
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
        default_on: true,
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

fn has_authoritative_complete_single_run_log_file(backend: &str) -> bool {
    // DBT now publishes its protected evidence through --log-file. SaBRe still
    // forwards its actual Detcore records to raw stderr while the controller's
    // --log-file remains incomplete.
    backend != "sabre"
}

fn usage() -> String {
    let mut s = String::from(
        "Usage: scripts/cross-backend-detlog-diff.rs [OPTIONS] -- <guest> [guest-args...]\n\n\
         Diagnostic only: this compares one canonical INFO stream per execution path.\n\
         It does not establish L2, full parity, or repeat determinism.\n\n\
         Options:\n\
         \x20 --backends A,B          two execution paths (required)\n\
         \x20 --hermit PATH           hermit binary (default: target/debug/hermit)\n\
         \x20 --detlog-stack          pass --detlog-stack to both runs\n\
         \x20 --detlog-heap           pass --detlog-heap to both runs\n\
         \x20 --context N             completed syscalls before the divergence (default: 5)\n\
         \x20 --normalize LIST        legacy names; lossy requests refuse before capture\n\
         \x20 --keep DIR              keep the raw captured streams in DIR\n\
         \x20 --self-test             run inert comparison and selection checks\n\
         \x20 --list-normalizations   describe every normalization and exit\n\n\
         Backends: ptrace, dbt, liteinst, sabre, kvm. `e9patch` means\n\
         preprocessing followed by the ptrace runtime; it is not a backend.\n\
         DBT publishes comparable records through its protected decoded log.\n\
         SaBRe refuses: its Detcore records use raw guest-controllable stderr,\n\
         while the host --log-file is incomplete.\n\n\
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
                        out.push_str(
                            "  result: REFUSED before capture; BitwiseInfoV1 compares this exactly\n",
                        );
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

fn select_authoritative_stream<'a>(
    side: &str,
    backend: &str,
    from_file: &'a [u8],
    stderr: &'a [u8],
) -> Result<(&'static str, &'a [u8], usize), String> {
    if backend == "sabre" {
        return Err(format!(
            "{side} SaBRe Detcore records are forwarded to raw stderr, which is shared with the guest and can be forged, while the host --log-file contains an incomplete controller stream; no isolated typed/authenticated complete sink is exposed, so this diagnostic refuses SaBRe (use strict canonical verify instead)"
        ));
    }
    if from_file.is_empty() {
        return Err(format!(
            "{side} {} produced no authoritative --log-file evidence; refusing {} guest-controllable stderr byte(s)",
            backend_description(backend).unwrap_or("unknown execution path"),
            stderr.len()
        ));
    }
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
    authoritative: Vec<u8>,
    stderr: Vec<u8>,
}

fn capture(cfg: &Config, backend: &str, side: &str, tmpdir: &Path) -> Result<Capture, String> {
    let log_file = tmpdir.join(format!("{side}-{backend}.log-file"));
    let mut cmd = Command::new(&cfg.hermit);
    cmd.arg("--log").arg("info");
    if has_authoritative_complete_single_run_log_file(backend) {
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
    let stderr = out.stderr;
    let from_file = match fs::read(&log_file) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(format!(
                "cannot read {} --log-file {}: {error}",
                backend_description(backend).unwrap_or("unknown execution path"),
                log_file.display()
            ));
        }
    };

    if !out.status.success() {
        let stderr_text = String::from_utf8_lossy(&stderr);
        let last_stderr = stderr_text.lines().last().unwrap_or("<empty stderr>");
        return Err(format!(
            "{} run failed with {}; refusing to compare its {} captured log bytes as successful evidence (last stderr line: {last_stderr})",
            backend_description(backend).unwrap_or("unknown execution path"),
            out.status,
            stderr.len() + from_file.len()
        ));
    }

    // `--log-file` is opened by Hermit in the host namespace and is the only
    // authoritative single-run sink available here. Stderr is shared with the
    // guest: record-looking lines there are diagnostics, never evidence. The
    // shared comparator validates the complete authoritative stream, including
    // UTF-8, structured records, and the bounded-writer truncation marker.
    let (source, authoritative, other_bytes) =
        select_authoritative_stream(side, backend, &from_file, &stderr)?;

    Ok(Capture {
        backend: backend.to_string(),
        source,
        other_stream_bytes: other_bytes,
        exit_code: out.status.code(),
        authoritative: authoritative.to_vec(),
        stderr,
    })
}

fn print_manifest(cfg: &Config, a: &Capture, b: &Capture) {
    println!("== provenance ==");
    for c in [a, b] {
        println!(
            "  {:<8} ({}) trace from {:<8} ({} authoritative bytes, {} bytes on the other stream), guest exit {}",
            c.backend,
            backend_description(&c.backend).unwrap_or("unknown execution path"),
            c.source,
            c.authoritative.len(),
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

fn validate_fixed_comparison(cfg: &Config) -> Result<(), String> {
    let lossy = lossy_normalizations(&cfg.normalize);
    if !lossy.is_empty() {
        return Err(format!(
            "lossy normalization(s) {} were requested; the fixed BitwiseInfoV1 policy compares virtual time and thread identity exactly, so this diagnostic refuses before running either guest",
            lossy.join(", ")
        ));
    }
    Ok(())
}

struct CanonicalComparison {
    summary: LogDiffSummary,
    total_left: usize,
    total_right: usize,
    diagnostic: Vec<u8>,
}

fn compare_captures(
    cfg: &Config,
    left: &Capture,
    right: &Capture,
) -> Result<CanonicalComparison, String> {
    compare_authoritative_logs(
        cfg.context,
        &left.backend,
        &left.authoritative,
        &right.backend,
        &right.authoritative,
    )
}

fn compare_authoritative_logs(
    context: usize,
    left_label: &str,
    left: &[u8],
    right_label: &str,
    right: &[u8],
) -> Result<CanonicalComparison, String> {
    let mut diagnostic = Vec::new();
    let (summary, total_left, total_right) =
        try_compare_bitwise_info_v1_bytes_with_records_and_diagnostics(
            left,
            right,
            ComparisonSideLabels::new(left_label, right_label),
            BitwiseInfoV1Diagnostics {
                difference_limit: 1,
                syscall_history: u64::try_from(context).unwrap_or(u64::MAX),
                no_color: true,
            },
            &mut diagnostic,
        )
        .map_err(|error| {
            format!(
                "canonical BitwiseInfoV1 comparison of {} and {} failed: {error}",
                left_label, right_label
            )
        })?;
    Ok(CanonicalComparison {
        summary,
        total_left,
        total_right,
        diagnostic,
    })
}

fn comparison_exit_code(summary: &LogDiffSummary) -> i32 {
    if summary.refused || summary.compared_left == 0 || summary.compared_right == 0 {
        2
    } else if summary.diff_found {
        1
    } else if summary.matched_with_evidence() {
        0
    } else {
        2
    }
}

fn report_comparison(left: &Capture, right: &Capture, comparison: &CanonicalComparison) -> i32 {
    let summary = &comparison.summary;
    println!(
        "  selected INFO records: {} {} | {} {} (all parsed records: {} | {})",
        left.backend,
        summary.compared_left,
        right.backend,
        summary.compared_right,
        comparison.total_left,
        comparison.total_right,
    );
    match comparison_exit_code(summary) {
        0 => {
            println!(
                "\nDIAGNOSTIC MATCH: {} selected INFO records compared.",
                summary.compared_left
            );
            0
        }
        1 => {
            match summary.first_divergent_record {
                Some(record) => println!("\nFIRST DIVERGENCE at 1-based log record {record}."),
                None => println!("\nFIRST DIVERGENCE: the selected INFO streams differ."),
            }
            println!(
                "  Only the FIRST divergence is meaningful; everything after it is downstream."
            );
            1
        }
        _ => {
            if summary.refused {
                eprintln!(
                    "\nREFUSAL: the shared canonical comparison reported a truncated input; this is no result, not a difference and not a match."
                );
            } else {
                eprintln!(
                    "\nREFUSAL: the shared canonical comparison selected {} | {} INFO records; both sides must contain evidence.",
                    summary.compared_left, summary.compared_right
                );
            }
            2
        }
    }
}

fn retain_captures(
    cfg: &Config,
    captures: &[Capture],
    comparison: &CanonicalComparison,
) -> Result<(), String> {
    let Some(dir) = &cfg.keep else {
        return Ok(());
    };
    fs::create_dir_all(dir)
        .map_err(|error| format!("cannot create --keep directory {}: {error}", dir.display()))?;

    let expected_counts = [
        comparison.summary.compared_left,
        comparison.summary.compared_right,
    ];
    let mut retained = Vec::<(PathBuf, Vec<u8>)>::new();
    for (index, capture) in captures.iter().enumerate() {
        let side = if index == 0 { "left" } else { "right" };
        let mut canonical_records = Vec::new();
        let count = write_bitwise_info_v1_bytes(
            &capture.authoritative,
            &capture.backend,
            &mut canonical_records,
        )
        .map_err(|error| {
            format!(
                "cannot render canonical INFO records for {}: {error}",
                capture.backend
            )
        })?;
        if count != expected_counts[index] {
            return Err(format!(
                "{} authoritative log changed after comparison: compared {} INFO records but retention found {count}",
                capture.backend, expected_counts[index]
            ));
        }
        retained.extend([
            (
                dir.join(format!("{side}-{}.stderr", capture.backend)),
                capture.stderr.clone(),
            ),
            (
                dir.join(format!("{side}-{}.log-file", capture.backend)),
                capture.authoritative.clone(),
            ),
            (
                dir.join(format!("{side}-{}.records", capture.backend)),
                canonical_records,
            ),
        ]);
    }
    if let Some((path, _)) = retained.iter().find(|(path, _)| path.exists()) {
        return Err(format!(
            "refusing to overwrite retained evidence {}",
            path.display()
        ));
    }
    for (path, bytes) in retained {
        fs::write(&path, bytes).map_err(|error| {
            format!(
                "cannot write retained evidence to {}: {error}",
                path.display()
            )
        })?;
    }
    Ok(())
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

    if let Err(error) = validate_fixed_comparison(&cfg) {
        eprintln!("cross-backend-detlog-diff: {error}");
        std::process::exit(2);
    }

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
        "cross-backend canonical INFO diff: {} vs {}  guest: {}",
        a.backend,
        b.backend,
        cfg.guest.join(" ")
    );
    print_manifest(&cfg, a, b);

    let comparison = match compare_captures(&cfg, a, b) {
        Ok(comparison) => comparison,
        Err(error) => {
            eprintln!("\ncross-backend-detlog-diff: {error}; the comparison produced no result");
            if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                eprintln!(
                    "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                    tmpdir.display()
                );
            }
            std::process::exit(2);
        }
    };
    {
        let mut stdout = std::io::stdout().lock();
        if let Err(error) = stdout.write_all(&comparison.diagnostic) {
            eprintln!("cross-backend-detlog-diff: cannot write comparison report: {error}");
            if let Err(cleanup) = fs::remove_dir_all(&tmpdir) {
                eprintln!(
                    "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {cleanup}",
                    tmpdir.display()
                );
            }
            std::process::exit(2);
        }
    }

    let mut code = report_comparison(a, b, &comparison);
    if code != 2 {
        if let Err(error) = retain_captures(&cfg, &captures, &comparison) {
            eprintln!("cross-backend-detlog-diff: {error}");
            code = 2;
        }
    }
    if let Err(error) = fs::remove_dir_all(&tmpdir) {
        eprintln!(
            "cross-backend-detlog-diff: warning: could not remove scratch dir {}: {error}",
            tmpdir.display()
        );
    }
    std::process::exit(code);
}

fn self_test_record(second: usize, level: &str, target: &str, body: &str) -> String {
    format!("2026-08-06T13:38:{second:02}.654561Z {level} {target}: {body}\n")
}

fn self_test_structured_record(second: usize, body: &str, event: DetLogEvent) -> String {
    self_test_record(
        second,
        "INFO",
        "detcore",
        &format!("{body}{}", record_suffix(event)),
    )
}

fn self_test_config(backends: [&str; 2], normalize: &[&str]) -> Config {
    Config {
        hermit: PathBuf::from("target/debug/hermit"),
        backends: backends.into_iter().map(str::to_string).collect(),
        guest: vec!["/bin/true".to_string()],
        detlog_stack: false,
        detlog_heap: false,
        context: 1,
        normalize: normalize.iter().map(|name| (*name).to_string()).collect(),
        keep: None,
    }
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
        has_authoritative_complete_single_run_log_file("dbt")
            && !has_authoritative_complete_single_run_log_file("sabre")
            && has_authoritative_complete_single_run_log_file("ptrace"),
        "ordinary-run log sink support was not represented",
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

    let authoritative = b"authoritative log bytes";
    let forged_longer = b"forged stderr record one\nforged stderr record two";
    check(
        matches!(
            select_authoritative_stream("left", "ptrace", authoritative, forged_longer),
            Ok(("log-file", selected, _)) if selected == authoritative
        ),
        "longer forged stderr displaced the authoritative log file",
    );
    check(
        select_authoritative_stream("left", "ptrace", b"", forged_longer).is_err(),
        "guest-controllable stderr was accepted without an authoritative log file",
    );
    check(
        matches!(
            select_authoritative_stream("left", "dbt", authoritative, forged_longer),
            Ok(("log-file", selected, _)) if selected == authoritative
        ),
        "DBT authoritative log file was refused or displaced by guest-controllable stderr",
    );
    check(
        select_authoritative_stream("left", "sabre", authoritative, forged_longer).is_err_and(
            |error| {
                error.contains("SaBRe Detcore records are forwarded to raw stderr")
                    && error.contains("host --log-file contains an incomplete controller stream")
                    && error.contains("no isolated typed/authenticated complete sink")
            },
        ),
        "SaBRe guest-controllable stderr or incomplete host log file was accepted as evidence",
    );
    check(
        validate_fixed_comparison(&self_test_config(
            ["ptrace", "ptrace"],
            &["wall-clock", "host-addresses"],
        ))
        .is_ok(),
        "the fixed canonical normalization policy was refused",
    );
    for lossy in ["virtual-time", "thread-identity"] {
        check(
            validate_fixed_comparison(&self_test_config(
                ["ptrace", "ptrace"],
                &["wall-clock", "host-addresses", lossy],
            ))
            .is_err_and(|error| error.contains(lossy)),
            &format!("lossy {lossy} normalization did not refuse before capture"),
        );
    }
    check(
        validate_fixed_comparison(&self_test_config(
            ["ptrace", "dbt"],
            &["wall-clock", "host-addresses"],
        ))
        .is_ok(),
        "DBT cross-backend comparison was refused despite its comparable decoded log",
    );

    let common = format!(
        "{}{}{}",
        self_test_structured_record(
            1,
            "COMMIT turn 17 at time 123ns",
            DetLogEvent::SchedulerCommit {
                scheduler_turn: 17,
                virtual_nanoseconds: 123,
                internal_io_poll: false,
                runtime_maps_read: false,
            },
        ),
        self_test_structured_record(
            2,
            "DETLOG [syscall] finish syscall #36: read = Ok(1)",
            DetLogEvent::SyscallResult {
                finished_syscall_number: 36,
            },
        ),
        self_test_structured_record(
            3,
            "DETLOG [syscall] finish syscall #37: write = Ok(1)",
            DetLogEvent::SyscallResult {
                finished_syscall_number: 37,
            },
        ),
    );
    let left = format!(
        "{common}{}{}",
        self_test_record(4, "INFO", "guest_observer", "payload=A"),
        self_test_record(5, "INFO", "guest_observer", "tail=C"),
    );
    let right = format!(
        "{common}{}{}",
        self_test_record(4, "INFO", "guest_observer", "payload=B"),
        self_test_record(5, "INFO", "guest_observer", "tail=D"),
    );
    for (left_label, first, right_label, second, expected_left, expected_right) in [
        (
            "ptrace",
            left.as_bytes(),
            "dbt",
            right.as_bytes(),
            "INFO guest_observer: payload=A",
            "INFO guest_observer: payload=B",
        ),
        (
            "dbt",
            right.as_bytes(),
            "ptrace",
            left.as_bytes(),
            "INFO guest_observer: payload=B",
            "INFO guest_observer: payload=A",
        ),
    ] {
        match compare_authoritative_logs(1, left_label, first, right_label, second) {
            Ok(comparison) => {
                let summary = &comparison.summary;
                check(
                    comparison_exit_code(summary) == 1,
                    "a nonempty canonical difference did not map to exit 1",
                );
                check(
                    (summary.compared_left, summary.compared_right) == (5, 5),
                    "selected INFO counts did not come from the shared parser",
                );
                check(
                    summary.first_divergent_record == Some(4)
                        && summary.first_divergent_scheduler_turn == Some(17)
                        && summary.first_divergent_virtual_nanoseconds == Some(123)
                        && summary.first_divergent_syscall == Some(37),
                    "first divergence position lost shared turn, time, record, or syscall context",
                );
                check(
                    summary.first_divergent_left_message.as_deref() == Some(expected_left)
                        && summary.first_divergent_right_message.as_deref() == Some(expected_right),
                    "first divergent messages did not preserve left/right direction",
                );
                let diagnostic = String::from_utf8(comparison.diagnostic).unwrap();
                check(
                    diagnostic.contains("Comparing INFO messages")
                        && diagnostic.contains("Divergent syscall context:")
                        && diagnostic.contains(&format!(
                            "Prior completed syscalls for {left_label}:"
                        ))
                        && diagnostic.contains("finish syscall #36: read"),
                    "shared first-divergence report did not include selected scope and context",
                );
                check(
                    diagnostic.contains("More than 1 differences, eliding the rest"),
                    "shared report did not retain first-divergence-only output",
                );
            }
            Err(error) => check(false, &format!("canonical divergence failed: {error}")),
        }
    }

    let mixed = format!(
        "{}{}{}{}",
        self_test_record(1, "WARN", "guest_observer", "not selected"),
        self_test_structured_record(2, "DETLOG stable", DetLogEvent::Other),
        self_test_record(3, "DEBUG", "guest_observer", "not selected"),
        self_test_record(4, "INFO", "guest_observer", "selected"),
    );
    for (left_label, right_label) in [("ptrace", "dbt"), ("dbt", "ptrace")] {
        match compare_authoritative_logs(
            0,
            left_label,
            mixed.as_bytes(),
            right_label,
            mixed.as_bytes(),
        ) {
            Ok(comparison) => {
                check(
                    comparison.summary.matched_with_evidence()
                        && comparison_exit_code(&comparison.summary) == 0,
                    "identical nonempty canonical INFO did not map to exit 0",
                );
                check(
                    (
                        comparison.summary.compared_left,
                        comparison.summary.compared_right,
                    ) == (2, 2)
                        && (comparison.total_left, comparison.total_right) == (4, 4),
                    "selected INFO counts were confused with total parsed records",
                );
                check(
                    comparison.summary.first_divergent_record.is_none()
                        && comparison.summary.first_divergent_left_message.is_none()
                        && comparison.summary.first_divergent_right_message.is_none(),
                    "an identical cross-backend comparison invented a first divergence",
                );
            }
            Err(error) => check(false, &format!("canonical match failed: {error}")),
        }
    }

    let valid = self_test_structured_record(1, "DETLOG stable", DetLogEvent::Other);
    for (first, second, description) in [
        (b"".as_slice(), b"".as_slice(), "empty/empty"),
        (b"".as_slice(), valid.as_bytes(), "empty/nonempty"),
        (valid.as_bytes(), b"".as_slice(), "nonempty/empty"),
    ] {
        match compare_authoritative_logs(0, "left", first, "right", second) {
            Ok(comparison) => check(
                comparison_exit_code(&comparison.summary) == 2,
                &format!("{description} comparison did not map to no-result exit 2"),
            ),
            Err(error) => check(false, &format!("{description} comparison failed: {error}")),
        }
    }

    let truncated = format!("{valid}{TRUNCATION_MARKER}\n");
    for (first, second, description) in [
        (truncated.as_bytes(), valid.as_bytes(), "left-truncated"),
        (valid.as_bytes(), truncated.as_bytes(), "right-truncated"),
        (truncated.as_bytes(), truncated.as_bytes(), "both-truncated"),
    ] {
        match compare_authoritative_logs(0, "left", first, "right", second) {
            Ok(comparison) => check(
                comparison.summary.refused
                    && comparison.summary.compared_left == 0
                    && comparison.summary.compared_right == 0
                    && comparison_exit_code(&comparison.summary) == 2,
                &format!("{description} comparison was not a shared no-result refusal"),
            ),
            Err(error) => check(false, &format!("{description} comparison failed: {error}")),
        }
    }

    let mut invalid_utf8 = valid.as_bytes().to_vec();
    invalid_utf8.insert(invalid_utf8.len() - 1, 0x80);
    for (first, second, expected_side) in [
        (
            invalid_utf8.as_slice(),
            valid.as_bytes(),
            "left is not UTF-8",
        ),
        (
            valid.as_bytes(),
            invalid_utf8.as_slice(),
            "right is not UTF-8",
        ),
    ] {
        let result = compare_authoritative_logs(0, "left", first, "right", second);
        check(
            result.is_err_and(|error| error.contains(expected_side)),
            "invalid UTF-8 was not refused with the correct side label",
        );
    }

    let malformed = self_test_record(
        1,
        "INFO",
        "detcore",
        "DETLOG broken DETLOG_RECORD={not-json}",
    );
    for (first, second) in [
        (malformed.as_bytes(), valid.as_bytes()),
        (valid.as_bytes(), malformed.as_bytes()),
    ] {
        let result = compare_authoritative_logs(0, "left", first, "right", second);
        check(
            result.is_err_and(|error| error.contains("invalid structured DETLOG result")),
            "malformed structured evidence was not refused",
        );
    }

    if failures.is_empty() {
        println!("cross-backend-detlog-diff self-test: PASS ({checks} checks)");
    } else {
        for failure in &failures {
            eprintln!("cross-backend-detlog-diff self-test: FAIL: {failure}");
        }
        std::process::exit(2);
    }
}
