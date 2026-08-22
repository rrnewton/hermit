#!/usr/bin/env rust-script
/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */
//! Run one guest under two backends and report the FIRST DETLOG DIVERGENCE.
//!
//! The compatibility scorecard's `parity` column is a hash of piped stdout. That
//! can only ever answer "did these two backends print the same bytes?", which is
//! a weak question: two backends can agree on stdout while disagreeing about
//! every scheduling decision, syscall result, and memory map along the way.
//! Deepening parity past stdout needs a tool that can diff the deterministic
//! trace itself, and that is what this is.
//!
//! **The first-divergence report is the point.** A boolean ("these backends
//! differ") is not actionable. "They agree for 157 records and then ptrace logs
//! `openat(...) = Ok(3)` where dbi logs `openat(...) = Ok(4)`" is a bug report.
//! So this prints the divergent pair with surrounding context from both sides.
//!
//! ## Two things that make this harder than `hermit log-diff`
//!
//! `hermit log-diff` compares two logs. It does not produce them, and producing
//! them across backends is where the sharp edges are:
//!
//! 1. **The backends do not agree on where the log goes.** `ptrace` honours
//!    `--log-file`. `sabre` writes the file *and* duplicates the whole trace to
//!    stderr. `dbi` ignores `--log-file` entirely and writes only to stderr.
//!    A harness that just reads the log file silently compares a full ptrace
//!    trace against an empty dbi one and calls it a difference. This tool reads
//!    whichever stream actually carried the trace and REPORTS WHICH, per backend.
//! 2. **Normalization is where fake parity gets manufactured.** Every
//!    normalization here is opt-in except the wall-clock prefix, and every one
//!    actually applied is listed in the output. If a normalization could hide a
//!    known open bug, enabling it prints a warning naming that bug.
//!
//! ## Usage
//!
//! ```text
//! ./scripts/cross-backend-detlog-diff.rs -- /bin/true
//! ./scripts/cross-backend-detlog-diff.rs --backends ptrace,sabre --detlog-heap -- ./guest arg
//! ./scripts/cross-backend-detlog-diff.rs --normalize host-addresses,virtual-time -- ./guest
//! ./scripts/cross-backend-detlog-diff.rs --list-normalizations
//! ```
//!
//! Exit codes: `0` streams agree · `1` they diverge · `2` the harness could not
//! produce a comparable pair (a run failed, or a backend emitted no trace).
//! Divergence is exit 1, not an error: finding one is a successful measurement.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude; // rust-script cache-key: 088ae17fa4a1 (regen: scripts/lib/prelude-cache-key.sh --write)

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// A normalization the caller can switch on, and the reason it is or is not
/// safe. `default_on` is reserved for rewrites that erase something with no
/// deterministic content at all.
struct Normalization {
    name: &'static str,
    default_on: bool,
    what: &'static str,
    /// Set when enabling this can mask a known open defect. Printed loudly.
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
        what: "hex addresses (0x...) -> first-appearance ordinals, preserving identity and aliasing",
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
            "task dbi-determinize-detlog-thread-id: DBI stamps RAW HOST TIDs into every DETLOG \
             record while ptrace reports small ordinals. Normalizing this hides that open P0",
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

fn usage() -> String {
    let mut s = String::from(
        "Usage: scripts/cross-backend-detlog-diff.rs [OPTIONS] -- <guest> [guest-args...]\n\n\
         Options:\n\
         \x20 --backends A,B          two backends to compare (default: ptrace,dbi)\n\
         \x20 --hermit PATH           hermit binary (default: target/debug/hermit)\n\
         \x20 --detlog-stack          pass --detlog-stack to both runs\n\
         \x20 --detlog-heap           pass --detlog-heap to both runs\n\
         \x20 --context N             records of context around the divergence (default: 5)\n\
         \x20 --normalize LIST        comma-separated; see --list-normalizations\n\
         \x20 --keep DIR              keep the raw captured streams in DIR\n\
         \x20 --list-normalizations   describe every normalization and exit\n\n\
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
        backends: vec!["ptrace".into(), "dbi".into()],
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
                    return Err("--backends takes exactly two, e.g. ptrace,dbi".into());
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
            "--" => {
                cfg.guest = args.collect();
                break;
            }
            other => return Err(format!("unexpected argument {other:?}\n\n{}", usage())),
        }
    }
    if cfg.guest.is_empty() {
        return Err(format!("no guest command given\n\n{}", usage()));
    }
    Ok(cfg)
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

fn capture(cfg: &Config, backend: &str, tmpdir: &Path) -> Result<Capture, String> {
    let log_file = tmpdir.join(format!("{backend}.log-file"));
    let mut cmd = Command::new(&cfg.hermit);
    cmd.arg("--log")
        .arg("info")
        .arg("--log-file")
        .arg(&log_file)
        .arg("run")
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
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let from_file = fs::read_to_string(&log_file).unwrap_or_default();

    // Backends disagree about where the trace goes: ptrace honours --log-file,
    // sabre writes it AND duplicates to stderr, dbi ignores it and uses stderr
    // only. Prefer whichever stream actually carries records; report which.
    let file_records: Vec<String> = from_file
        .lines()
        .filter(|l| is_record(l))
        .map(str::to_string)
        .collect();
    let err_records: Vec<String> = stderr
        .lines()
        .filter(|l| is_record(l))
        .map(str::to_string)
        .collect();
    // Pick the RICHER stream, not merely a non-empty one. sabre writes a
    // handful of records to --log-file and the FULL trace to stderr (4 vs ~90
    // on /bin/true), so "non-empty log-file wins" silently compares against a
    // truncated stream and reports a depth that is an artifact of the pick.
    let (source, records, other_bytes) =
        if !file_records.is_empty() && file_records.len() >= err_records.len() {
            ("log-file", file_records, stderr.len())
        } else {
            ("stderr", err_records, from_file.len())
        };

    if let Some(dir) = &cfg.keep {
        // Same /tmp caveat as the scratch dir: a --keep under /tmp would be
        // fine for these copies (we write them ourselves, after the run), but
        // warn so nobody points --log-file-like paths there by analogy.
        if dir.starts_with("/tmp") {
            eprintln!(
                "cross-backend-detlog-diff: warning: --keep {} is under /tmp, which hermit \
                 overmounts for the guest; the copies here are written by the harness so they \
                 survive, but do not reuse this path for anything the guest writes.",
                dir.display()
            );
        }
        fs::create_dir_all(dir).ok();
        fs::write(dir.join(format!("{backend}.stderr")), &stderr).ok();
        fs::write(dir.join(format!("{backend}.log-file")), &from_file).ok();
        fs::write(dir.join(format!("{backend}.records")), records.join("\n")).ok();
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

fn hex_at(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if b.len() < 8 || b[0] != b'0' || b[1] != b'x' {
        return None;
    }
    let n = b[2..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
    // Only long hex runs are addresses; short ones are flags and sizes.
    if n >= 6 { Some(2 + n) } else { None }
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
                s = ordinalize(&s, &mut addrs, "0xADDR", hex_at);
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
            "  {:<8} trace from {:<8} ({} records, {} bytes on the other stream), guest exit {}",
            c.backend,
            c.source,
            c.records.len(),
            c.other_stream_bytes,
            c.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".into()),
        );
    }
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
        let n = NORMALIZATIONS.iter().find(|n| &n.name == name).unwrap();
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

fn report_divergence(cfg: &Config, a: &Capture, b: &Capture, na: &[String], nb: &[String]) -> i32 {
    let first = (0..na.len().min(nb.len())).find(|&i| na[i] != nb[i]);
    let ctx = cfg.context;

    match first {
        None if na.len() == nb.len() => {
            println!(
                "\nSTREAMS AGREE: {} records compared, no divergence.",
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
            for j in i.saturating_sub(ctx)..i {
                println!("    {j:>6} | {}", na[j]);
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

    // NOT env::temp_dir(). Hermit overmounts the guest's /tmp, so a --log-file
    // written under /tmp lands inside the container and silently never appears
    // on the host: the run succeeds, the file is absent, and the harness sees
    // zero records. That failure looks exactly like "this backend emits no
    // trace", which is why it is worth spelling out here.
    let scratch_root = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".cache/hermit-xbackend-detlog");
    let tmpdir = scratch_root.join(format!("run-{}", std::process::id()));
    if let Err(e) = fs::create_dir_all(&tmpdir) {
        eprintln!(
            "cross-backend-detlog-diff: cannot create scratch dir {}: {e}",
            tmpdir.display()
        );
        std::process::exit(2);
    }

    let mut captures = Vec::new();
    for backend in &cfg.backends {
        match capture(&cfg, backend, &tmpdir) {
            Ok(c) => captures.push(c),
            Err(e) => {
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
            std::process::exit(2);
        }
    }

    let na = normalize(&a.records, &cfg.normalize);
    let nb = normalize(&b.records, &cfg.normalize);
    let code = report_divergence(&cfg, a, b, &na, &nb);
    fs::remove_dir_all(&tmpdir).ok();
    std::process::exit(code);
}
