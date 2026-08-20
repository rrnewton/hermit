/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Everything to do with post-processing hermit/detcore logs.

use core::fmt::Display;
use core::fmt::Formatter;
use core::fmt::Result;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::str::FromStr;
use std::sync::LazyLock;

use clap;
use clap::Parser;
use regex::Regex;
use tempfile::NamedTempFile;

/// Selects the set of log messages compared for determinism.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LogComparisonMode {
    /// Compare deterministic Detcore and scheduler messages.
    #[default]
    Deterministic,
    /// Compare every INFO message exactly, while leaving any captured DEBUG or
    /// TRACE messages available for diagnostics. This is the observation
    /// envelope used by the `BitwiseInfoV1` verification policy.
    Info,
    /// Compare every captured log message without filtering.
    FullTrace,
}

/// Options for calling `log_diff`.
#[derive(Debug, Parser, Clone)]
pub struct LogDiffOpts {
    /// UNSAFE: strips numbers and temporary paths before comparison.
    ///
    /// This erases timestamps and syscall values that bitwise parity exists to
    /// compare. Never use this option to make a failing parity diff pass; doing
    /// so is cheating. It is only for non-parity diagnostic localization.
    #[clap(long = "unsafe-strip-lines")]
    pub strip_lines: bool,

    /// Canonicalize host memory addresses before comparison WITHOUT erasing them.
    ///
    /// Only addresses a producer has explicitly marked with the
    /// `<hostaddr 0x...>` wrapper (see [`host_addr`]) are canonicalized; each
    /// distinct marked address is rewritten to an ordinal placeholder
    /// `<addr{N}>` assigned by order of first appearance within a single run
    /// (see `canonicalize_addresses_in_line`). Unlike [`Self::strip_lines`],
    /// this discards ONLY the host-specific raw pointer value: it preserves
    /// identity (same address -> same ordinal), ordering (introduction
    /// sequence), and aliasing (two names for one address collapse to one
    /// ordinal), and it leaves every other byte -- virtual-time timestamps,
    /// syscall argument/result values, counts, sizes, flags -- untouched for an
    /// exact comparison. In canonical parity, this preserves the ability to DETECT a
    /// difference (allocation-order or aliasing changes still diverge), which
    /// wholesale stripping throws away.
    ///
    /// The marker is REQUIRED (a bare `0x...` literal is left exact) because
    /// nothing in the compared DETLOG stream can otherwise distinguish a varying
    /// host pointer from a reproducible hex value -- syscall arguments printed
    /// `{:#x}` (e.g. `flock` `operation=0x2` vs `0x6`), guest memory ranges,
    /// content digests, cpuid leaves. A blanket `0x` canonicalization would
    /// collapse those too, silently erasing real syscall-argument divergence:
    /// a "softer strip" and exactly the fake-green this policy exists to prevent.
    #[clap(skip)]
    pub canonicalize_addresses: bool,

    /// The internal message set to compare.
    #[clap(skip)]
    pub comparison: LogComparisonMode,
    /// Limit the number of differences printed. Set to 0 for no limit.
    #[clap(long, default_value = "20")]
    pub limit: u64,

    /// Before comparison, filter out lines which contain this substring.
    #[clap(long)]
    pub ignore_lines: Vec<String>,

    /// Show this many completed syscalls before each run-specific divergence point.
    /// Set to 0 to omit history.
    #[clap(long, default_value = "0")]
    pub syscall_history: u64,
    /// Disable colored console output for line diffs.
    #[clap(long)]
    pub no_color: bool,

    /// Do not consider "COMMIT" messages for deterministic checks.
    #[clap(long)]
    pub skip_commit: bool,

    /// Do not consider "DETLOG" messages for deterministic checks.
    #[clap(long)]
    pub skip_detlog: bool,

    /// Use git diff instead of the internal, basic log comparison.
    #[clap(long)]
    pub git_diff: bool,

    /// In case --skip-detlog=false this parameter further filters which
    /// "DETLOG" messages will be included for deterministic checks
    #[clap(long, default_values = &["syscall", "syscallresult", "other"])]
    pub include_detlogs: Vec<DetLogFilter>,
}

impl LogDiffOpts {
    fn is_skip(&self, filter: DetLogFilter) -> bool {
        !self.include_detlogs.contains(&filter)
    }

    fn skip_detlog(&self, entry: &str) -> bool {
        if self.skip_detlog {
            return true;
        }

        if is_detlog_syscall(entry) && self.is_skip(DetLogFilter::Syscall) {
            return true;
        }
        if is_detlog_syscall_result(entry) && self.is_skip(DetLogFilter::SyscallResult) {
            return true;
        }

        if !is_detlog_syscall(entry)
            && !is_detlog_syscall_result(entry)
            && self.is_skip(DetLogFilter::Other)
        {
            return true;
        }

        false
    }

    fn filter_deterministic<'a>(&self, v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
        v.iter()
            .filter_map(|(i, s)| {
                if (is_detlog(s) && !self.skip_detlog(s) && !is_scheduler_committed_time(s))
                    || (is_commit(s) && !self.skip_commit && !is_internal_io_poll_commit(s))
                {
                    Some((*i, *s))
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Indicates which DETLOG entries to be used for log-diff comparison
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetLogFilter {
    ///the start of syscall will be used for logdiff
    Syscall,
    ///the syscall result  will be used for logdiff
    SyscallResult,
    ///all other unspecified DETLOG entries will be used for logdiff
    Other,
}

impl FromStr for DetLogFilter {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "syscall" => Ok(DetLogFilter::Syscall),
            "syscallresult" => Ok(DetLogFilter::SyscallResult),
            "other" => Ok(DetLogFilter::Other),
            _ => Err(anyhow::Error::msg(format!(
                "unknown value {} for DetLogFilter",
                s
            ))),
        }
    }
}

/// N.B. we don't want to specify two different notions of "default", so we use the
/// `Clap` instance above.
impl Default for LogDiffOpts {
    fn default() -> Self {
        let v: Vec<String> = vec![];
        LogDiffOpts::parse_from(v.iter())
    }
}

/// In fully-deterministic modes, many log lines should be fully determinstic across runs.
/// But as that is a work-in-progress, this utility strips known-nondeterministic
/// information from logs.
///
/// This erasure is deliberately lossy and is NOT a parity claim: it backs the
/// `Stripped` comparator only (`bitwise_parity: false`). `BitwiseInfoV1`
/// canonicalizes rather than erases -- see `canonicalize_addresses_in_line`.
/// Lossy as it is, each pattern must still erase only what it names: erasing a
/// neighbouring field turns a real divergence into a reported match.
///
/// Example input/output:
///   `Input:  COMMIT turn 3, dettid 231635 using resources Resources { tid: DetPid { inner: 231635 }, resources: {Path("/proc/231635/fd/1"): W} }`
///   `Output: COMMIT turn <NUM>, dettid <NUM> using resources Resources { tid: DetPid { inner: <NUM> }, resources: {Path("/proc/<pid>/fd/<num>"): W} }`
///
/// As you can see this is overkill and smarter strategies would be possible. For example,
/// ones that remember and post-facto-determinize certain identifiers.
pub fn strip_log_entry(log: &str) -> String {
    // Memory addresses, like 0x7fcfb7e7d450
    //
    // TODO: use a debug allocator that increases only, never reusing. Also, consider
    // post-facto processing all of these into new virtual addresses based on the order they're seen.
    static RE0: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b0[xX][A-Fa-f0-9]+\b").unwrap());

    // Every number, plus common duration suffixes so fractional timing jitter is
    // not left behind. This one is terrible overkill: for `hermit run` itself and
    // for all command tests the full contents of a COMMIT line should already be
    // deterministic, so nothing here should need erasing. It is retained for
    // `spawn_fn_*` variants, which fork from another process and so exercise only
    // a *partial* detcore setup without a true process tree of their own.
    //
    // N.B. RE4 must run BEFORE this pattern, or `800.709_180s` is consumed here as
    // a bare number and never reaches the duration rule.
    static RE1: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b[\d][\d_]*(?:\.[\d][\d_]*)?(?:ns|us|µs|ms)?\b").unwrap());

    // A quoted /tmp path. `[^"]*` stops at the path's OWN closing quote: a greedy
    // `.*` here ran to the last quote on the line and erased every field after the
    // path, so two entries differing only downstream of a /tmp path compared equal.
    //
    // TODO: only strip this information if the config specified to the host /tmp through.
    // Otherwise we can determinize /tmp access fully.
    static RE2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"/tmp/[^"]*""#).unwrap());

    // TODO: only strip this one if we're allowing through the host /proc or failing to determinize tids/pids:
    static RE3: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/proc/[\d]+/").unwrap());

    // TODO: only strip this if we're running a library-based test where we can't
    // guarantee the starting state of the allocator/etc.
    static RE4: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[\d][\d_.]*s\b").unwrap());

    let log = RE4.replace_all(log, "<NANOSECONDS>");
    let log = RE3.replace_all(&log, "/proc/<PID>/");
    let log = RE0.replace_all(&log, "<ADDR>");
    let log = RE1.replace_all(&log, "<NUM>");
    let log = RE2.replace_all(&log, "/tmp/<somewhere>\"");
    String::from(log)
}

/// Wrap a host memory address so `canonicalize_addresses_in_line` will
/// canonicalize it. Producers that print a genuinely host-specific pointer
/// (one that varies run-to-run, e.g. a supervisor-side allocation) should emit
/// it via this helper -- `<hostaddr 0x7fcfb7e7d450>` -- instead of a bare
/// `0x...` literal. Only marked addresses are canonicalized, so reproducible hex
/// (syscall arguments, guest memory ranges, digests) is compared exactly.
///
/// Note: nothing in detcore's current DETLOG output prints a varying host
/// pointer -- guest addresses are determinized and every logged `0x...` value is
/// reproducible -- so this marker is presently unused in production and exists
/// so a future host-pointer print opts into canonicalization deliberately rather
/// than being swept up by a blanket regex.
pub fn host_addr(addr: usize) -> String {
    format!("<hostaddr {addr:#x}>")
}

/// Rewrite each MARKED host memory address (`<hostaddr 0x...>`, see
/// [`host_addr`]) in `line` to an ordinal placeholder `<addr{N}>`, numbered by
/// order of first appearance within a single run. `map`/`next` carry the per-run
/// assignment state threaded across all of that run's lines, so `next` should
/// start at 1 and the same `map`/`next` must be reused for every line of one run
/// (and a FRESH pair used for the other run).
///
/// Canonical parity strips the wall-clock prefix, canonicalizes these marked
/// addresses, and compares everything else exactly. This step differs
/// from [`strip_log_entry`]'s `<ADDR>` erasure in one decisive way: erasure maps
/// every address to a single token, so two runs that allocate in a DIFFERENT
/// ORDER, or that ALIAS differently (one address printed twice vs. two distinct
/// addresses), compare EQUAL -- the exact defect parity exists to catch. An
/// ordinal assigned by first appearance keeps identity, order, and aliasing, so
/// those cases still diverge while a pure ASLR-shift (same structure, different
/// raw values) compares equal.
///
/// Only the explicit `<hostaddr ...>` marker is canonicalized. A bare `0x...`
/// literal is left byte-for-byte for exact comparison: a blanket regex cannot
/// tell a varying host pointer from a reproducible syscall argument printed
/// `{:#x}` (`flock` `operation=0x2` vs `0x6`, `membarrier` bitmasks), so
/// canonicalizing every hex token would erase real syscall-argument divergence.
/// Decimal values (virtual-time timestamps, counts, sizes) are likewise exact.
fn canonicalize_addresses_in_line(
    line: &str,
    map: &mut HashMap<String, usize>,
    next: &mut usize,
) -> String {
    // Match ONLY the explicit host-address marker emitted by `host_addr`; the
    // captured group is the raw `0x...` value used as the ordinal key.
    static RE_HOSTADDR: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"<hostaddr (0[xX][A-Fa-f0-9]+)>").unwrap());

    RE_HOSTADDR
        .replace_all(line, |caps: &regex::Captures| {
            let addr = &caps[1];
            let ord = match map.get(addr) {
                Some(existing) => *existing,
                None => {
                    let assigned = *next;
                    *next += 1;
                    map.insert(addr.to_string(), assigned);
                    assigned
                }
            };
            format!("<addr{ord}>")
        })
        .into_owned()
}

fn messages_for_comparison(messages: &[(usize, &str)], opts: &LogDiffOpts) -> Vec<String> {
    if opts.strip_lines {
        messages
            .iter()
            .map(|(_, message)| strip_log_entry(message))
            .collect()
    } else if opts.canonicalize_addresses {
        let mut addresses = HashMap::new();
        let mut next_address = 1usize;
        messages
            .iter()
            .map(|(_, message)| {
                canonicalize_addresses_in_line(message, &mut addresses, &mut next_address)
            })
            .collect()
    } else {
        messages
            .iter()
            .map(|(_, message)| (*message).to_owned())
            .collect()
    }
}

fn canonical_info_from_str(contents: &str) -> Vec<String> {
    let info = filter_infos(&extract_log_messages(contents));
    let opts = LogDiffOpts {
        canonicalize_addresses: true,
        comparison: LogComparisonMode::Info,
        ..Default::default()
    };
    messages_for_comparison(&info, &opts)
}

/// Print the canonical INFO messages that strict verification would compare for
/// one captured log.
///
/// This removes the real wall-clock prefix and rewrites only explicitly marked
/// host addresses to first-appearance ordinals. It does not run the lossy
/// `--unsafe-strip-lines` transformation: scheduler turns, virtual time, syscall
/// values, counts, flags, and every other substantive byte are preserved.
pub fn write_canonical_info(file: &Path, writer: &mut impl Write) -> std::io::Result<usize> {
    let bytes = std::fs::read(file)?;
    let contents = String::from_utf8_lossy(&bytes);
    let messages = canonical_info_from_str(&contents);
    for message in &messages {
        writeln!(writer, "{message}")?;
    }
    Ok(messages.len())
}

/// Separate a full, continuous log into discrete (possibly-multiline) log messages,
/// stripping off the timestamps in the process.  Return lines tagged with their
/// index number.
/// The timestamp that begins every log record. A record runs from one match to
/// the next, so records may span multiple lines -- which is why comparison is
/// done on records and never on lines or bytes.
static RECORD_START: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"((Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) \d\d \d\d:\d\d:\d\d\.\d+|\d+-\d\d-\d\dT\d\d:\d\d:\d\d.\d+Z) +")
        .unwrap()
});

/// Byte offsets at which each record starts.
fn record_starts(contents: &str) -> Vec<usize> {
    RECORD_START
        .find_iter(contents)
        .map(|m| m.start())
        .collect()
}

/// How many records in `contents` are known COMPLETE.
///
/// A record is complete only once the *next* record has begun, because nothing
/// else marks its end. In a log still being written the final record may be
/// half-flushed, so it is never counted and never compared: a partial write must
/// not read as a difference. A buffer with one record start has zero complete
/// records, and that is a real answer, not a failure.
pub fn complete_record_count(contents: &str) -> usize {
    record_starts(contents).len().saturating_sub(1)
}

/// The prefix of `contents` holding exactly its first `n` complete records.
///
/// Returns `None` when fewer than `n` complete records are present, so a caller
/// can tell "the runs agree over n records" apart from "n records have not been
/// written yet". Those two must never collapse into one answer.
pub fn take_complete_records(contents: &str, n: usize) -> Option<&str> {
    let starts = record_starts(contents);
    if n == 0 {
        return Some(&contents[..0]);
    }
    // Record n-1 ends where record n begins, so n complete records require n+1 starts.
    starts.get(n).map(|end| &contents[..*end])
}

fn extract_log_messages(contents: &str) -> Vec<(usize, &str)> {
    let ts = &*RECORD_START;
    let tag = Regex::new("^(ERROR|WARN|INFO|DEBUG|TRACE) ").unwrap();
    let iter = ts
        .split(contents) // Not aware of a streaming version of this RE split operation.
        .enumerate()
        .map(|(i, s)| (i, s.trim()))
        .filter(|(_, s)| !s.is_empty())
        .map(|(i, s)| {
            // Only let through lines that start with one of the expected tags:
            if !tag.is_match(s) {
                panic!("Log line without expected tag: {}", s);
            } else {
                (i, s)
            }
        });
    iter.collect()
}

fn is_info(line: &str) -> bool {
    line.starts_with("INFO ")
}

fn is_commit(line: &str) -> bool {
    line.contains(" COMMIT ")
}

fn is_detlog(line: &str) -> bool {
    line.contains(" DETLOG ")
}

/// A scheduler COMMIT turn that only grants the `InternalIOPolling` resource, i.e. a
/// granted retry of a nonblocking poll (poll/epoll_wait/wait4/futex/recv/send...). These
/// grants are internal bookkeeping of Hermit's blocking-via-polling mechanism: how many
/// times a thread is re-granted permission to re-attempt a nonblocking syscall before it
/// stops returning would-block depends on when a concurrent external-IO action (e.g. a
/// child linker process writing to a pipe) becomes ready on the host, which is wall-clock
/// dependent and not tied to the (RCB-deterministic) logical schedule. The corresponding
/// `NONCOMMIT ... polling resource` skips are already excluded from comparison (they are
/// not tagged `COMMIT`); excluding the matching grant-COMMITs keeps the deterministic
/// comparison consistent and focused on guest-observable events (the actual syscall
/// results, still compared via their DETLOG entries).
///
/// SaBRe's inherited stdio pipes emit an outer device-resource turn before the inner polling
/// turn. The scheduler tags that outer turn with `[sabre-internal-pipe-io]`; it is the same
/// host-timing-sensitive operation and is normalized here as well. A SaBRe task with a loopback
/// peer similarly tags the strong yield before each zero-timeout poll with
/// `[sabre-loopback-poll-zero-timeout]`. The scheduler additionally suppresses the per-retry
/// "advance global time for scheduler turn" DETLOG line for these turns at the source (see
/// `Scheduler::bump_global_time`), so the two mechanisms together make the deterministic
/// comparison insensitive to host-timing-dependent polling-loop counts.
fn is_internal_io_poll_commit(line: &str) -> bool {
    is_commit(line)
        && (line.contains("{InternalIOPolling: ")
            || line.contains(" [sabre-internal-pipe-io]")
            || line.contains(" [sabre-loopback-poll-zero-timeout]"))
}

/// The scheduler's per-turn `committed_time` advance bookkeeping. `committed_time` tracks
/// the global logical clock, which still moves forward when an `InternalIOPolling` retry
/// (see `is_internal_io_poll_commit`) advances time -- and the number of those retries is
/// host-timing nondeterministic. That makes the *presence* of this line on a given turn
/// retry-count sensitive, so we exclude it from the deterministic comparison. No
/// guest-observable signal is lost: the value is redundant with the (retained,
/// retry-count-insensitive) "advance global time for scheduler turn" DETLOG line and with
/// the per-turn committed time echoed on each COMMIT line.
fn is_scheduler_committed_time(line: &str) -> bool {
    line.contains("advancing committed_time from ")
}

fn is_detcore(line: &str) -> bool {
    static PREFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new("^(ERROR|WARN|INFO|DEBUG|TRACE).* detcore:").unwrap());

    PREFIX.is_match(line)
}

fn is_detlog_syscall(line: &str) -> bool {
    is_detlog(line) && line.contains("[syscall]")
}

fn is_detlog_syscall_result(line: &str) -> bool {
    is_detlog_syscall(line) && line.contains("finish syscall")
}

// TODO:
// Append together a sequence of messages while truncating if there are too many.
fn _truncate_messages(_v: &[&str]) -> String {
    unimplemented!()
}

fn filter_infos<'a>(v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
    v.iter().filter(|(_i, s)| is_info(s)).copied().collect()
}

fn filter_detcore<'a>(v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
    v.iter().filter(|(_, s)| is_detcore(s)).copied().collect()
}

fn filter_ignored<'a>(lines: Vec<(usize, &'a str)>, omits: &Vec<String>) -> Vec<(usize, &'a str)> {
    lines
        .into_iter()
        .filter(|(_ix, ln)| {
            let mut keep = true;
            for omit in omits {
                if ln.contains(omit) {
                    keep = false
                }
            }
            keep
        })
        .collect()
}

fn collect_syscalls<'a>(v: &[(usize, &'a str)]) -> Vec<(usize, &'a str)> {
    v.iter()
        .filter(|(_, entry)| is_detlog_syscall(entry))
        .copied()
        .collect()
}

fn first_different_message_indices(
    left: &[(usize, &str)],
    right: &[(usize, &str)],
    opts: &LogDiffOpts,
) -> Option<(Option<usize>, Option<usize>)> {
    let compared_left = messages_for_comparison(left, opts);
    let compared_right = messages_for_comparison(right, opts);
    let common = compared_left.len().min(compared_right.len());

    if let Some(position) =
        (0..common).find(|&position| compared_left[position] != compared_right[position])
    {
        return Some((Some(left[position].0), Some(right[position].0)));
    }

    match compared_left.len().cmp(&compared_right.len()) {
        Ordering::Less => Some((None, Some(right[common].0))),
        Ordering::Greater => Some((Some(left[common].0), None)),
        Ordering::Equal => None,
    }
}

fn parse_underscored_u64(value: &str) -> Option<u64> {
    value.replace('_', "").parse().ok()
}

fn parse_virtual_nanoseconds(value: &str, unit: Option<&str>) -> Option<u64> {
    match unit {
        None | Some("ns") if !value.contains('.') => parse_underscored_u64(value),
        Some("s") => {
            let value = value.replace('_', "");
            let (seconds, fraction) = value.split_once('.').unwrap_or((&value, ""));
            if fraction.len() > 9 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let seconds = seconds.parse::<u64>().ok()?;
            let fraction = if fraction.is_empty() {
                0
            } else {
                let digits = fraction.parse::<u64>().ok()?;
                digits.checked_mul(10_u64.pow((9 - fraction.len()) as u32))?
            };
            seconds.checked_mul(1_000_000_000)?.checked_add(fraction)
        }
        _ => None,
    }
}

fn commit_position(message: &str) -> Option<(u64, Option<u64>)> {
    static TURN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\bCOMMIT turn ([0-9][0-9_]*)\b").unwrap());
    static TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b(?:at time|on previously committed) ([0-9][0-9_]*(?:\.[0-9_]+)?)(ns|s)?\b")
            .unwrap()
    });

    let turn = parse_underscored_u64(TURN.captures(message)?.get(1)?.as_str())?;
    let virtual_nanoseconds = TIME.captures(message).and_then(|captures| {
        parse_virtual_nanoseconds(
            captures.get(1)?.as_str(),
            captures.get(2).map(|unit| unit.as_str()),
        )
    });
    Some((turn, virtual_nanoseconds))
}

fn commit_position_at_or_before(
    messages: &[(usize, &str)],
    message_index: usize,
) -> Option<(u64, Option<u64>)> {
    messages
        .iter()
        .rev()
        .filter(|(index, _)| *index <= message_index)
        .find_map(|(_, message)| commit_position(message))
}

fn syscall_at_or_before<'a>(
    syscalls: &[(usize, &'a str)],
    index: usize,
) -> Option<(usize, &'a str)> {
    syscalls
        .iter()
        .rev()
        .find(|(syscall_index, _)| *syscall_index <= index)
        .copied()
}

fn write_syscall_context(
    w: &mut impl std::io::Write,
    left_index: usize,
    right_index: usize,
    left_syscalls: &[(usize, &str)],
    right_syscalls: &[(usize, &str)],
    history_count: u64,
) -> std::io::Result<()> {
    if history_count == 0 {
        return Ok(());
    }

    let left_current = syscall_at_or_before(left_syscalls, left_index);
    let right_current = syscall_at_or_before(right_syscalls, right_index);
    if left_current.is_none() && right_current.is_none() {
        return Ok(());
    }

    writeln!(w, "Divergent syscall context:")?;
    for (label, current) in [("run 1", left_current), ("run 2", right_current)] {
        if let Some((index, syscall)) = current {
            writeln!(w, "  {label}, log message {index}: {syscall}")?;
        } else {
            writeln!(w, "  {label}: <no syscall observed>")?;
        }
    }

    let history_limit = usize::try_from(history_count).unwrap_or(usize::MAX);
    for (label, index, syscalls) in [
        ("run 1", left_index, left_syscalls),
        ("run 2", right_index, right_syscalls),
    ] {
        let history_boundary =
            syscall_at_or_before(syscalls, index).map_or(index, |(current_index, _)| current_index);
        let mut history = syscalls
            .iter()
            .rev()
            .filter(|(syscall_index, entry)| {
                *syscall_index < history_boundary && is_detlog_syscall_result(entry)
            })
            .take(history_limit)
            .copied()
            .collect::<Vec<_>>();
        history.reverse();
        if !history.is_empty() {
            writeln!(w, "  Prior completed syscalls for {label}:")?;
            for (_, syscall) in history {
                writeln!(w, "    {syscall}")?;
            }
        }
    }
    writeln!(w)?;

    Ok(())
}

/// A comparison of two strings.
///
/// Displays comparison result without any formatting
pub struct Comparison<'a> {
    left: &'a str,
    right: &'a str,
    no_color: bool,
}
impl<'a> Comparison<'a> {
    /// Store two values to be compared in future.
    ///
    /// Expensive diffing is deferred until calling `Debug::fmt`.
    pub fn new(no_color: bool, left: &'a str, right: &'a str) -> Comparison<'a> {
        Comparison {
            left,
            right,
            no_color,
        }
    }
}
impl<'a> Display for Comparison<'a> {
    fn fmt(&self, f: &mut Formatter) -> Result {
        if self.no_color {
            writeln!(f, "Diff < left / right > :")?;
            writeln!(f, "<\"{}\"", self.left)?;
            writeln!(f, ">\"{}\"", self.right)
        } else {
            pretty_assertions::Comparison::new(&self.left, &self.right).fmt(f)
        }
    }
}

/// Returns `true` if a difference is found.
///
/// We could use an existing diff library on the entire log, but this provides us more
/// control over how to present the (stripped/unstripped) differences, and to focus on the
/// per-line divergence(s), and potentially focus on the first point of divergence.
//
// Future TODO:
//  - report bulk differences, e.g. leftover lines, but with truncation
//  - report only the first K per-line differences
//  - detect reorderings and/or switch to larger differences for consecutive multi-line mismatches
fn diff_vecs(
    which: &str,
    v1: &[(usize, &str)],
    v2: &[(usize, &str)],
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    left_syscalls: &[(usize, &str)],
    right_syscalls: &[(usize, &str)],
) -> std::io::Result<bool> {
    writeln!(w, "  Comparing {which} messages...\n")?;
    if v1.is_empty() && v2.is_empty() {
        return Ok(false);
    }

    // Prepare both complete streams before the compare loop: address ordinals
    // are assigned by first appearance across the full selected stream, not by
    // however many differences the caller asks us to print.
    let compared_left = messages_for_comparison(v1, opts);
    let compared_right = messages_for_comparison(v2, opts);

    let mut diff_count = 0;
    for (position, ((left_index, left), (right_index, right))) in
        v1.iter().zip(v2.iter()).enumerate()
    {
        let left_compared = &compared_left[position];
        let right_compared = &compared_right[position];
        if left_compared == right_compared {
            continue;
        }

        if diff_count >= opts.limit && opts.limit != 0 {
            writeln!(
                w,
                "More than {} differences, eliding the rest...",
                opts.limit
            )?;
            break;
        }

        write!(
            w,
            "({which}) Mismatch at log messages {left_index} (run 1) and {right_index} (run 2): {}",
            Comparison::new(opts.no_color, left_compared, right_compared)
        )?;
        if opts.strip_lines || opts.canonicalize_addresses {
            write!(
                w,
                "({which}) Original entries before normalization: {}",
                Comparison::new(opts.no_color, left, right)
            )?;
        }
        write_syscall_context(
            w,
            *left_index,
            *right_index,
            left_syscalls,
            right_syscalls,
            opts.syscall_history,
        )?;

        diff_count += 1;
    }

    match v1.len().cmp(&v2.len()) {
        Ordering::Less => {
            writeln!(
                w,
                "Run 2 contains {} extra messages not matched in run 1. Displaying up to 10:",
                v2.len() - v1.len()
            )?;
            diff_count += 1;
            let start = v2.len() - std::cmp::min(10, v2.len() - v1.len());
            for (_, message) in &v2[start..] {
                writeln!(w, "{message}")?;
            }
        }
        Ordering::Greater => {
            writeln!(
                w,
                "Run 1 contains {} extra messages not matched in run 2. Displaying up to 10:",
                v1.len() - v2.len()
            )?;
            diff_count += 1;
            let start = v1.len() - std::cmp::min(10, v1.len() - v2.len());
            for (_, message) in &v1[start..] {
                writeln!(w, "{message}")?;
            }
        }
        Ordering::Equal => {}
    }

    Ok(diff_count > 0)
}

fn git_diff(
    which: &str,
    v1: &[(usize, &str)],
    v2: &[(usize, &str)],
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
    left_syscalls: &[(usize, &str)],
    right_syscalls: &[(usize, &str)],
) -> std::io::Result<bool> {
    writeln!(w, "  Comparing {which} messages...\n")?;

    let mut file1 = NamedTempFile::new()?;
    let mut file2 = NamedTempFile::new()?;

    for (_, line) in v1 {
        writeln!(file1, "{line}")?;
    }
    for (_, line) in v2 {
        writeln!(file2, "{line}")?;
    }

    match Command::new("git")
        .args(["diff", "--color", "--color-words", "-w"])
        .arg(file1.path())
        .arg(file2.path())
        .status()
    {
        Ok(code) => Ok(!code.success()),
        Err(error) => {
            eprintln!("Error launching git, falling back to basic diff: {error}");
            diff_vecs(which, v1, v2, opts, w, left_syscalls, right_syscalls)
        }
    }
}

/// What a log comparison actually compared, alongside whether it differed.
///
/// A bare "no difference found" boolean cannot distinguish *"the two message
/// streams were compared and matched"* from *"there were no messages to
/// compare"*. Both selected lists being empty is a NO-RESULT, not a match, so
/// the counts travel with the verdict and a parity consumer can require nonzero
/// execution before believing a green.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogDiffSummary {
    /// True if a substantive difference was found between the two runs.
    pub diff_found: bool,
    /// Number of messages actually selected for comparison from the first run.
    pub compared_left: usize,
    /// Number of messages actually selected for comparison from the second run.
    pub compared_right: usize,
    /// Scheduler turn at the first different selected message, when the first
    /// run has a preceding scheduler COMMIT message that identifies it.
    pub first_divergent_scheduler_turn: Option<u64>,
    /// Virtual nanoseconds at that same scheduler COMMIT, when its time is
    /// present and parseable.
    pub first_divergent_virtual_nanoseconds: Option<u64>,
    /// 1-based index of the first record that differs. "Diverged somewhere in
    /// the first N records" is a bound, not a location, and on a long run the
    /// two are far apart; this is the location.
    pub first_divergent_record: Option<usize>,
}

impl LogDiffSummary {
    /// True only when the comparison both ran on a nonempty selection *and*
    /// found no difference. An empty-vs-empty comparison is never a match.
    pub fn matched_with_evidence(&self) -> bool {
        !self.diff_found && self.compared_left > 0 && self.compared_right > 0
    }
}

/// Process log messages from two files.  Log messages look like this:
///     "Apr 09 06:08:03.100  INFO detcore: [detcore, dtid 2]  finish syscall: close(2) = Ok(0)"
///
/// With some complexities:
///  * Some entries are multi-line (contain newlines).
///  * Some stripping of nondeterministic information is needed for direct comparability.
///  * Certain lines are intended to be deterministic/comparable, in their contents,
///    and others in their *presence* but not their details.
///
/// Reports only whether the two files differ. See [`log_diff_detailed`] when the
/// caller must also know how many messages were actually compared; a bare
/// `false` here cannot distinguish a match from an empty comparison.
//
// TODO: we should replace this with a diff algorithm that can handle insertions while maintaining
// alignment. There's also no reason we can't output the stripped relevant lines and use a separate
// diff tool.
pub fn log_diff(file_a: &Path, file_b: &Path, opts: &LogDiffOpts) -> bool {
    log_diff_detailed(file_a, file_b, opts).diff_found
}

/// Like [`log_diff`], but returns the counted comparison evidence rather than a
/// bare boolean.
pub fn log_diff_detailed(file_a: &Path, file_b: &Path, opts: &LogDiffOpts) -> LogDiffSummary {
    try_log_diff_detailed(file_a, file_b, opts).expect("could not read or compare log inputs")
}

/// Fallible form of [`log_diff_detailed`] for user-facing callers. Missing or
/// unreadable inputs are ordinary command errors, not process panics.
pub fn try_log_diff_detailed(
    file_a: &Path,
    file_b: &Path,
    opts: &LogDiffOpts,
) -> std::io::Result<LogDiffSummary> {
    // For now the log-diff mode reads both logs fully into memory. This could be
    // modified in the future for a streaming solution, at least for scrolling through
    // the identical prefixes of very large logs.
    let vec_a = std::fs::read(file_a)?;
    let vec_b = std::fs::read(file_b)?;
    let str_a = String::from_utf8_lossy(&vec_a);
    let str_b = String::from_utf8_lossy(&vec_b);
    log_diff_summary_from_strs(str_a, str_b, opts, &mut std::io::stderr())
}

/// Total records in a log, including a final record that may still be being
/// written. Use [`complete_record_count`] when the log is still growing.
pub fn record_count(contents: &str) -> usize {
    record_starts(contents).len()
}

/// Like [`try_log_diff_detailed`], but also reports how many records each log
/// contained. Every log comparison should be able to say what it read, so a
/// caller is never left to infer coverage from a bare verdict.
pub fn try_log_diff_with_records(
    file_a: &Path,
    file_b: &Path,
    opts: &LogDiffOpts,
) -> std::io::Result<(LogDiffSummary, usize, usize)> {
    let vec_a = std::fs::read(file_a)?;
    let vec_b = std::fs::read(file_b)?;
    let str_a = String::from_utf8_lossy(&vec_a);
    let str_b = String::from_utf8_lossy(&vec_b);
    let records_a = record_count(&str_a);
    let records_b = record_count(&str_b);
    let summary = log_diff_summary_from_strs(&str_a, &str_b, opts, &mut std::io::stderr())?;
    Ok((summary, records_a, records_b))
}

/// A comparison of two logs that may still be growing.
///
/// The verdict alone is not reportable. "The runs agree" and "the runs agree so
/// far as anyone has looked" are different claims, and a caller who cannot tell
/// them apart will stop early and conclude the wrong thing. The record counts
/// are therefore part of the result, not optional detail alongside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrefixComparison {
    /// The verdict over the compared prefix only.
    pub summary: LogDiffSummary,
    /// Complete records present in the first log when it was read.
    pub records_available_left: usize,
    /// Complete records present in the second log when it was read.
    pub records_available_right: usize,
    /// Records actually compared: the shorter of the two available counts.
    pub records_compared: usize,
}

impl PrefixComparison {
    /// True when one log has complete records the other has not reached yet, so
    /// the comparison is bounded by reading rather than by the runs agreeing.
    pub fn one_side_is_ahead(&self) -> bool {
        self.records_available_left != self.records_available_right
    }
}

/// Compare only the records both logs have finished writing.
///
/// The tail of a log being written may be half-flushed, and a half-flushed
/// record is not a difference -- it is an absence. Truncating both inputs to
/// their common *complete* prefix is what makes this safe to run against a live
/// run: bytes that have not been written yet can never be mistaken for bytes
/// that disagree.
pub fn compare_complete_prefix(
    contents_a: &str,
    contents_b: &str,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<PrefixComparison> {
    let records_available_left = complete_record_count(contents_a);
    let records_available_right = complete_record_count(contents_b);
    let records_compared = records_available_left.min(records_available_right);
    // Both are Some: `records_compared` is at most each side's complete count.
    let prefix_a = take_complete_records(contents_a, records_compared)
        .expect("common prefix never exceeds either side's complete record count");
    let prefix_b = take_complete_records(contents_b, records_compared)
        .expect("common prefix never exceeds either side's complete record count");
    let summary = log_diff_summary_from_strs(prefix_a, prefix_b, opts, w)?;
    Ok(PrefixComparison {
        summary,
        records_available_left,
        records_available_right,
        records_compared,
    })
}

/// Boolean-only wrapper retained for tests that only ask "did it differ?".
/// Prefer [`log_diff_summary_from_strs`] where the counts matter.
#[cfg(test)]
fn log_diff_from_strs(
    file_a_str: impl AsRef<str>,
    file_b_str: impl AsRef<str>,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<bool> {
    Ok(log_diff_summary_from_strs(file_a_str, file_b_str, opts, w)?.diff_found)
}

fn log_diff_summary_from_strs(
    file_a_str: impl AsRef<str>,
    file_b_str: impl AsRef<str>,
    opts: &LogDiffOpts,
    w: &mut impl std::io::Write,
) -> std::io::Result<LogDiffSummary> {
    let all_a = filter_ignored(
        extract_log_messages(file_a_str.as_ref()),
        &opts.ignore_lines,
    );
    let all_b = filter_ignored(
        extract_log_messages(file_b_str.as_ref()),
        &opts.ignore_lines,
    );

    writeln!(
        w,
        "Logs contain {} | {} messages total",
        all_a.len(),
        all_b.len(),
    )?;

    let detcore_a = filter_detcore(&all_a);
    let detcore_b = filter_detcore(&all_b);
    let infos_a = filter_infos(&all_a);
    let infos_b = filter_infos(&all_b);
    let detlogs_a = opts.filter_deterministic(&detcore_a);
    let detlogs_b = opts.filter_deterministic(&detcore_b);
    let left_syscalls = collect_syscalls(&all_a);
    let right_syscalls = collect_syscalls(&all_b);
    writeln!(
        w,
        "Logs contain {} | {} detcore-specific messages",
        detcore_a.len(),
        detcore_b.len(),
    )?;
    writeln!(
        w,
        "Logs contain {} | {} INFO messages",
        infos_a.len(),
        infos_b.len(),
    )?;
    writeln!(
        w,
        "Logs contain {} | {} DETLOG & scheduler COMMIT messages",
        detlogs_a.len(),
        detlogs_b.len(),
    )?;

    if opts.strip_lines {
        writeln!(
            w,
            "Normalizing known nondeterministic numerical data before comparison..."
        )?;
    } else if opts.canonicalize_addresses {
        writeln!(
            w,
            "Canonicalizing host addresses (ordinal by first appearance); comparing everything else exactly..."
        )?;
    }

    let (which, compared_a, compared_b) = match opts.comparison {
        LogComparisonMode::Deterministic => ("DETLOG", &detlogs_a, &detlogs_b),
        LogComparisonMode::Info => ("INFO", &infos_a, &infos_b),
        LogComparisonMode::FullTrace => ("full trace", &all_a, &all_b),
    };

    let first_different = first_different_message_indices(compared_a, compared_b, opts);
    let first_position_candidate = first_different.and_then(|(left_index, right_index)| {
        left_index
            .and_then(|index| commit_position_at_or_before(&all_a, index))
            .or_else(|| right_index.and_then(|index| commit_position_at_or_before(&all_b, index)))
    });

    let diff_found = if opts.git_diff {
        git_diff(
            which,
            compared_a,
            compared_b,
            opts,
            w,
            &left_syscalls,
            &right_syscalls,
        )?
    } else {
        diff_vecs(
            which,
            compared_a,
            compared_b,
            opts,
            w,
            &left_syscalls,
            &right_syscalls,
        )?
    };

    let summary = LogDiffSummary {
        diff_found,
        compared_left: compared_a.len(),
        compared_right: compared_b.len(),
        first_divergent_scheduler_turn: diff_found
            .then_some(first_position_candidate)
            .flatten()
            .map(|(turn, _)| turn),
        first_divergent_virtual_nanoseconds: diff_found
            .then_some(first_position_candidate)
            .flatten()
            .and_then(|(_, time)| time),
        first_divergent_record: diff_found
            .then_some(first_different)
            .flatten()
            .and_then(|(left_index, right_index)| left_index.or(right_index)),
    };

    if diff_found {
        writeln!(w, "Done processing logs, differences found.")?;
    } else if summary.compared_left == 0 && summary.compared_right == 0 {
        // Say this plainly rather than letting "no differences" stand in for
        // "nothing was compared": a caller that treats the two alike turns a
        // no-result into a green.
        writeln!(
            w,
            "Done processing logs, but ZERO {which} messages were selected on either side: \
             nothing was compared (no-result, not a match)."
        )?;
    } else {
        writeln!(
            w,
            "Done processing logs, no substantive differences found ({} | {} {which} messages compared).",
            summary.compared_left, summary.compared_right,
        )?;
    }
    Ok(summary)
}

#[cfg(test)]
mod test {
    use clap::CommandFactory;
    use clap::Parser;
    use pretty_assertions::assert_eq;

    use crate::logdiff::DetLogFilter;

    /// One well-formed log record. Records are delimited by their leading
    /// timestamp, so `body` may contain newlines and still be one record.
    fn record(second: usize, body: &str) -> String {
        format!("Apr 09 06:08:{second:02}.100  INFO detcore: {body}\n")
    }

    fn info_opts() -> super::LogDiffOpts {
        super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            ..Default::default()
        }
    }

    fn compare(left: &str, right: &str) -> super::PrefixComparison {
        super::compare_complete_prefix(left, right, &info_opts(), &mut Vec::new())
            .expect("comparing in-memory strings cannot fail on I/O")
    }

    #[test]
    fn a_record_is_complete_only_once_the_next_one_starts() {
        assert_eq!(super::complete_record_count(""), 0);

        // One record start means one record that is still being written.
        let one = record(1, "first");
        assert_eq!(super::complete_record_count(&one), 0);

        // The second start is what proves the first record finished.
        let two = format!("{}{}", record(1, "first"), record(2, "second"));
        assert_eq!(super::complete_record_count(&two), 1);

        let three = format!("{two}{}", record(3, "third"));
        assert_eq!(super::complete_record_count(&three), 2);
    }

    #[test]
    fn a_multiline_record_counts_once_and_is_not_split_at_its_newlines() {
        let multiline = format!(
            "{}{}",
            record(1, "first\n    continued detail\n    more detail"),
            record(2, "second")
        );
        // Two starts, so one complete record -- the embedded newlines are part
        // of record one, not boundaries of their own.
        assert_eq!(super::complete_record_count(&multiline), 1);

        let prefix = super::take_complete_records(&multiline, 1).unwrap();
        assert!(prefix.contains("continued detail"));
        assert!(prefix.contains("more detail"));
        assert!(!prefix.contains("second"));
    }

    #[test]
    fn asking_past_the_written_end_is_none_not_a_short_answer() {
        let two = format!("{}{}", record(1, "first"), record(2, "second"));
        assert_eq!(super::take_complete_records(&two, 0), Some(""));
        assert!(super::take_complete_records(&two, 1).is_some());
        // Only one record is complete, so two is not yet readable. Returning a
        // truncated prefix here would let "not written yet" pass as "agrees".
        assert_eq!(super::take_complete_records(&two, 2), None);
        assert_eq!(super::take_complete_records(&two, 99), None);
    }

    #[test]
    fn a_half_written_final_record_is_never_a_difference() {
        // Identical complete records; the runs differ only in the tail that
        // neither has finished flushing.
        let left = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "TAIL-LEFT")
        );
        let right = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "TAIL-RIGHT-AND-LONGER")
        );

        let comparison = compare(&left, &right);
        assert!(
            !comparison.summary.diff_found,
            "an unfinished record must not read as a divergence"
        );
        assert_eq!(comparison.records_compared, 2);
        assert!(!comparison.one_side_is_ahead());
    }

    #[test]
    fn a_difference_inside_the_completed_prefix_is_found() {
        let left = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "LEFT"),
            record(3, "tail")
        );
        let right = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "RIGHT"),
            record(3, "tail")
        );

        let comparison = compare(&left, &right);
        assert!(comparison.summary.diff_found);
        assert_eq!(comparison.records_compared, 2);
    }

    #[test]
    fn comparison_is_bounded_by_the_shorter_log_and_says_so() {
        let ahead = format!(
            "{}{}{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "same"),
            record(4, "same"),
            record(5, "same")
        );
        let behind = format!(
            "{}{}{}",
            record(1, "same"),
            record(2, "same"),
            record(3, "same")
        );

        let comparison = compare(&ahead, &behind);
        assert!(!comparison.summary.diff_found);
        assert_eq!(comparison.records_available_left, 4);
        assert_eq!(comparison.records_available_right, 2);
        // Only what both sides have finished writing.
        assert_eq!(comparison.records_compared, 2);
        assert!(
            comparison.one_side_is_ahead(),
            "the caller must be able to see the comparison was reading-bound"
        );
    }

    #[test]
    fn the_first_differing_record_is_located_not_just_bounded() {
        // 40 identical records, one difference at record 13, then 40 more.
        let build = |marker: &str| {
            (1..=81)
                .map(|index| {
                    let body = if index == 13 { marker } else { "same" };
                    record(index % 60, &format!("record {index} {body}"))
                })
                .collect::<String>()
        };
        let left = build("LEFT");
        let right = build("RIGHT");

        let found = compare(&left, &right).summary.first_divergent_record;
        assert_eq!(
            found,
            Some(13),
            "bisection must name the record, not merely the prefix that contains it"
        );
    }

    #[test]
    fn identical_logs_have_no_first_divergent_record() {
        let same = format!("{}{}{}", record(1, "a"), record(2, "b"), record(3, "c"));
        assert_eq!(compare(&same, &same).summary.first_divergent_record, None);
        // And an empty comparison reports no location rather than record zero.
        assert_eq!(compare("", "").summary.first_divergent_record, None);
    }

    #[test]
    fn nothing_written_yet_is_a_no_result_not_a_match() {
        // A single unfinished record on each side: zero complete records.
        let comparison = compare(&record(1, "first"), &record(1, "first"));
        assert_eq!(comparison.records_compared, 0);
        assert!(!comparison.summary.diff_found);
        assert!(
            !comparison.summary.matched_with_evidence(),
            "comparing zero records must never report a match"
        );
    }

    #[test]
    fn unsafe_strip_lines_cli_name_and_warning_are_explicit() {
        let options = super::LogDiffOpts::try_parse_from(["log-diff", "--unsafe-strip-lines"])
            .expect("the explicitly unsafe spelling should parse");
        assert!(options.strip_lines);

        assert!(super::LogDiffOpts::try_parse_from(["log-diff", "--strip-lines"]).is_err());

        let mut help = Vec::new();
        super::LogDiffOpts::command()
            .write_long_help(&mut help)
            .expect("write clap help");
        let help = String::from_utf8(help).expect("help is UTF-8");
        assert!(help.contains("--unsafe-strip-lines"));
        assert!(help.contains("erases timestamps and syscall values"));
        assert!(help.contains("make a failing parity diff pass"));
        assert!(help.contains("doing so is cheating"));
        assert!(!help.contains("--strip-lines"));
    }

    #[test]
    fn test_compare_with_no_color() {
        let str1 = "test1";
        let str2 = "test2";

        assert_eq!(
            format!("{}", super::Comparison::new(true, str1, str2))
                .split('\n')
                .collect::<Vec<&str>>(),
            ["Diff < left / right > :", "<\"test1\"", ">\"test2\"", "",]
        );
    }

    #[test]
    fn test_compare_with_color() {
        let str1 = "test1";
        let str2 = "test2";

        assert_eq!(
            format!("{}", super::Comparison::new(false, str1, str2))
                .split('\n')
                .collect::<Vec<&str>>(),
            [
                "\u{1b}[1mDiff\u{1b}[0m \u{1b}[31m< left\u{1b}[0m / \u{1b}[32mright >\u{1b}[0m :",
                "\u{1b}[31m<\"test\u{1b}[0m\u{1b}[1;48;5;52;31m1\u{1b}[0m\u{1b}[31m\"\u{1b}[0m",
                "\u{1b}[32m>\"test\u{1b}[0m\u{1b}[1;48;5;22;32m2\u{1b}[0m\u{1b}[32m\"\u{1b}[0m",
                "",
            ]
        );
    }

    #[test]
    fn test_log_diff_with_color() -> std::io::Result<()> {
        let str1 = "INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #11: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)";
        let str2 = "INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #15: mmap(NULL, 3954880, PROT_READ | PROT_EXEC, MAP_PRIVATE | MAP_DENYWRITE, 3, 0) = Ok(140737347883008)";
        let mut result = Vec::<u8>::new();

        super::log_diff_from_strs(
            str1,
            str2,
            &super::LogDiffOpts {
                limit: 1,
                strip_lines: false,
                canonicalize_addresses: false,
                comparison: super::LogComparisonMode::Deterministic,
                syscall_history: 5,
                no_color: false,
                skip_commit: false,
                skip_detlog: false,
                git_diff: false,
                ignore_lines: Vec::new(),
                include_detlogs: vec![
                    DetLogFilter::Syscall,
                    DetLogFilter::SyscallResult,
                    DetLogFilter::Other,
                ],
            },
            &mut result,
        )?;

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("  Comparing DETLOG messages..."));
        assert!(output.contains("Mismatch at log messages 0 (run 1) and 0 (run 2)"));
        assert!(output.contains("run 1, log message 0: INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #11"));
        assert!(output.contains("run 2, log message 0: INFO detcore: DETLOG [syscall][detcore, dtid 3]  finish syscall #15"));
        assert!(!output.contains("eliding the rest"));

        Ok(())
    }

    #[test]
    fn test_log_diff_reports_each_runs_syscall_context() -> std::io::Result<()> {
        let log_a = r#"2022-09-06T14:15:47.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: read(3, 0x1000, 1) = Ok(1)
2022-09-06T14:15:48.000000Z INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x2000, 1) = Ok(1)"#;
        let log_b = r#"2022-09-06T14:15:47.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: read(3, 0x1000, 1) = Ok(1)
2022-09-06T14:15:48.000000Z INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x3000, 1) = Ok(1)"#;
        let mut result = Vec::new();
        let options = super::LogDiffOpts {
            no_color: true,
            syscall_history: 1,
            ..Default::default()
        };

        assert!(super::log_diff_from_strs(
            log_a,
            log_b,
            &options,
            &mut result
        )?);

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("Mismatch at log messages 2 (run 1) and 2 (run 2)"));
        assert!(output.contains("run 1, log message 2: INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x2000, 1) = Ok(1)"));
        assert!(output.contains("run 2, log message 2: INFO detcore: DETLOG [syscall] finish syscall #2: write(1, 0x3000, 1) = Ok(1)"));
        assert!(output.contains("Prior completed syscalls for run 1:"));
        assert!(output.contains("Prior completed syscalls for run 2:"));
        assert_eq!(output.matches("finish syscall #1: read").count(), 2);
        Ok(())
    }

    #[test]
    fn test_full_trace_detects_unnormalized_timing_difference() -> std::io::Result<()> {
        let log_a = "INFO detcore: DETLOG [syscall] finish syscall #1: clock_gettime(CLOCK_MONOTONIC, 100) = Ok(0)";
        let log_b = "INFO detcore: DETLOG [syscall] finish syscall #1: clock_gettime(CLOCK_MONOTONIC, 101) = Ok(0)";
        let normalized = super::LogDiffOpts {
            strip_lines: true,
            no_color: true,
            ..Default::default()
        };

        assert!(!super::log_diff_from_strs(
            log_a,
            log_b,
            &normalized,
            &mut Vec::new()
        )?);

        let verbose = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            strip_lines: false,
            syscall_history: 1,
            no_color: true,
            ..Default::default()
        };
        let mut result = Vec::new();
        assert!(super::log_diff_from_strs(
            log_a,
            log_b,
            &verbose,
            &mut result
        )?);

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("Comparing full trace messages"));
        assert!(output.contains("clock_gettime(CLOCK_MONOTONIC, 100)"));
        assert!(output.contains("clock_gettime(CLOCK_MONOTONIC, 101)"));
        assert!(output.contains("run 1"));
        assert!(output.contains("run 2"));
        Ok(())
    }

    #[test]
    fn info_scope_compares_info_exactly_without_promoting_debug_diagnostics() -> std::io::Result<()>
    {
        let stable_info = "2026-08-06T01:00:00.000000Z INFO detcore: DETLOG [syscall] finish syscall #1: write(1, 0x2, 1) = Ok(1)";
        let left = format!(
            "{stable_info}\n2026-08-06T01:00:00.000001Z DEBUG detcore: diagnostic host timing=100"
        );
        let right = format!(
            "{stable_info}\n2026-08-06T01:00:00.000002Z DEBUG detcore: diagnostic host timing=200"
        );
        let info = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            no_color: true,
            ..Default::default()
        };

        // Positive bracket: DEBUG remains present in both captures, but the
        // BitwiseInfoV1 envelope selects exactly the one INFO event per side.
        let matched = super::log_diff_summary_from_strs(&left, &right, &info, &mut Vec::new())?;
        assert!(matched.matched_with_evidence());
        assert_eq!(matched.compared_left, 1);
        assert_eq!(matched.compared_right, 1);

        // Negative bracket: a real INFO payload difference must still fail.
        let divergent_info = right.replace("write(1, 0x2, 1)", "write(1, 0x6, 1)");
        let diverged =
            super::log_diff_summary_from_strs(&left, divergent_info, &info, &mut Vec::new())?;
        assert!(diverged.diff_found);

        // DEBUG comparison remains an explicit diagnostic mode rather than an
        // implicit part of INFO parity.
        let full_trace = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            no_color: true,
            ..Default::default()
        };
        let debug_diverged =
            super::log_diff_summary_from_strs(left, right, &full_trace, &mut Vec::new())?;
        assert!(debug_diverged.diff_found);
        Ok(())
    }

    #[test]
    fn test_log_diff_compares_detlog() -> std::io::Result<()> {
        let log_file_a = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:48.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 7984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:48.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: write(1, 0x6022a0, 70) = ?
2022-09-06T14:15:48.904049Z  INFO detcore: COMMIT 2
2022-09-06T14:15:48.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;

        let log_file_b = r#"2022-09-06T14:15:47.891501Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x602000-0x623000 rw-p 0 0:0 0 [heap] -> 74b43faf7b78ace9443772ef63a30f66feaf9bd320256c82b8bd880634d19d46
2022-09-06T14:15:47.903997Z  INFO detcore: DETLOG [memory][detcore, dtid 3] 0x7ffffffdd000-0x7ffffffff000 rw-p 0 0:0 0 [stack] -> 1984d1aaf386fce67eaa926624ecc1d5a4105828e4f286ee59cc69c0491cd5fe
2022-09-06T14:15:47.904049Z  INFO detcore: DETLOG [syscall][detcore, dtid 3] inbound syscall: write(1, 0x6022a0, 70) = ?
2022-09-06T14:15:47.904049Z  INFO detcore: COMMIT 1
2022-09-06T14:15:47.904782Z  INFO detcore::scheduler: [sched-step5] >>>>>>>"#;
        let mut result = Vec::<u8>::new();

        let log_options = super::LogDiffOpts {
            no_color: true,
            git_diff: false,
            ..Default::default()
        };
        super::log_diff_from_strs(log_file_a, log_file_b, &log_options, &mut result)?;

        let output = String::from_utf8(result).unwrap();
        assert!(output.contains("Mismatch at log messages 2 (run 1) and 2 (run 2)"));
        assert!(output.contains("Mismatch at log messages 4 (run 1) and 4 (run 2)"));
        assert!(output.contains("INFO detcore: COMMIT 2"));
        assert!(output.contains("INFO detcore: COMMIT 1"));

        Ok(())
    }

    #[test]
    fn test_filter_deterministic() {
        let opts = super::LogDiffOpts {
            include_detlogs: vec![
                DetLogFilter::Syscall,
                DetLogFilter::SyscallResult,
                DetLogFilter::Other,
            ],
            ..Default::default()
        };

        let v = opts.filter_deterministic(
            &[
                (
                    1,
                    "INFO detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                (
                    2,
                    "INFO DETLOG detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ],
        );

        assert_eq!(
            v,
            vec![
                (
                    2,
                    "INFO DETLOG detcore: registers [dtid 3]. user_regs_struct { r15..."
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ]
        );
    }

    #[test]
    fn test_filter_deterministic_with_filter() {
        let opts = super::LogDiffOpts {
            include_detlogs: vec![DetLogFilter::Syscall],
            skip_commit: true,
            ..Default::default()
        };

        let v = opts.filter_deterministic(
            &[
                (
                    1,
                    "INFO detcore: registers [dtid 3]. user_regs_struct { r15...",
                ),
                (
                    2,
                    "INFO DETLOG detcore:[syscall] syscall 1",
                ),
                (
                    3,
                    "INFO COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000",
                ),
            ],
        );
        assert_eq!(v, vec![(2, "INFO DETLOG detcore:[syscall] syscall 1")]);
    }

    /// Regression: the deterministic comparison must ignore the scheduler bookkeeping emitted
    /// by nonblocking-IO poll retries, whose count is host-timing nondeterministic (e.g. how
    /// many times a thread re-polls a pipe before a child process makes it ready). Only the
    /// `{InternalIOPolling: ...}` COMMIT turn and the `advancing committed_time` clock line
    /// should be dropped; ordinary COMMIT turns and DETLOG entries must be retained.
    #[test]
    fn test_filter_deterministic_drops_io_polling_bookkeeping() {
        let opts = super::LogDiffOpts::default();
        let v = opts.filter_deterministic(&[
            (
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {InternalIOPolling: W}, on previously committed 1s",
            ),
            (
                1,
                "DEBUG detcore::scheduler: DETLOG [sched-step1] advancing committed_time from 1 to 2",
            ),
            (
                2,
                "INFO detcore: DETLOG [syscall][detcore, dtid 5] finish syscall #9: read(3, 0x1000, 1) = Ok(1)",
            ),
            (
                3,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Path(\"/proc/5/fd/3\"): R}, on previously committed 2s",
            ),
        ]);
        // The InternalIOPolling COMMIT (0) and the committed_time line (1) are dropped; the
        // guest-observable syscall (2) and the ordinary COMMIT turn (3) survive.
        assert_eq!(
            v,
            vec![
                (
                    2,
                    "INFO detcore: DETLOG [syscall][detcore, dtid 5] finish syscall #9: read(3, 0x1000, 1) = Ok(1)"
                ),
                (
                    3,
                    "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Path(\"/proc/5/fd/3\"): R}, on previously committed 2s"
                ),
            ]
        );
    }

    #[test]
    fn test_filter_deterministic_drops_sabre_internal_pipe_resource_turn() {
        let opts = super::LogDiffOpts::default();
        let v = opts.filter_deterministic(&[
            (
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {Device(ContainerStdout): W}, on previously committed 1s [sabre-internal-pipe-io]",
            ),
            (
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Device(ContainerStdout): W}, on previously committed 2s",
            ),
        ]);

        assert_eq!(
            v,
            vec![(
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {Device(ContainerStdout): W}, on previously committed 2s"
            )]
        );
    }

    #[test]
    fn test_filter_deterministic_drops_sabre_loopback_poll_yield() {
        let opts = super::LogDiffOpts::default();
        let v = opts.filter_deterministic(&[
            (
                0,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {SchedYield: W}, on previously committed 1s [sabre-loopback-poll-zero-timeout]",
            ),
            (
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {SchedYield: W}, on previously committed 2s",
            ),
        ]);

        assert_eq!(
            v,
            vec![(
                1,
                "INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 18, dettid 5 using resources {SchedYield: W}, on previously committed 2s"
            )]
        );
    }

    /// Regression: two runs that differ only in how many nonblocking-IO poll retries occurred
    /// must compare as deterministic, while a genuine guest-observable divergence must still be
    /// reported. `run_b` below performs one extra poll retry (an extra InternalIOPolling COMMIT
    /// plus its committed_time advance) but the guest syscalls are identical.
    #[test]
    fn test_log_diff_ignores_extra_io_poll_retries() -> std::io::Result<()> {
        let common_head = "2022-09-06T14:15:47.000000Z  INFO detcore: DETLOG [syscall][detcore, dtid 5] inbound syscall: poll(0x1000, 1, -1) = ?";
        let common_tail = "2022-09-06T14:15:47.100000Z  INFO detcore: DETLOG [syscall][detcore, dtid 5] finish syscall #9: poll(0x1000, 1, -1) = Ok(1)";
        let poll_retry = "2022-09-06T14:15:47.050000Z  INFO detcore::scheduler: [sched-step5] >>> COMMIT turn 17, dettid 5 using resources {InternalIOPolling: W}, on previously committed 1s\n2022-09-06T14:15:47.050000Z DEBUG detcore::scheduler: DETLOG [sched-step1] advancing committed_time from 1 to 2";

        let run_a = format!("{common_head}\n{poll_retry}\n{common_tail}");
        // run_b polls one extra time before the fd is ready:
        let run_b = format!("{common_head}\n{poll_retry}\n{poll_retry}\n{common_tail}");

        let opts = super::LogDiffOpts {
            no_color: true,
            strip_lines: true,
            ..Default::default()
        };
        // Differ only in retry count -> deterministic (no diff reported):
        assert!(!super::log_diff_from_strs(
            &run_a,
            &run_b,
            &opts,
            &mut Vec::new()
        )?);

        // But a real divergence in the guest-observable syscall result is still caught. (Use a
        // non-numeric change: numeric-only differences are erased by `strip_lines` normalization.)
        let run_c = run_a.replace("= Ok(1)", "= Err(Errno(EBADF))");
        assert!(super::log_diff_from_strs(
            &run_a,
            &run_c,
            &opts,
            &mut Vec::new()
        )?);
        Ok(())
    }

    /// Canonical parity positive control: two runs that differ ONLY in their raw
    /// host addresses -- same structure, same introduction order, same aliasing
    /// (a pure ASLR shift) -- compare EQUAL after canonicalization. The same
    /// inputs compared RAW (neither stripped nor canonicalized) diverge, proving
    /// canonicalization is doing real work and is not just the identity.
    #[test]
    fn canonical_address_only_difference_compares_equal() -> std::io::Result<()> {
        let run_a =
            "2022-09-06T14:15:47.000000Z  INFO detcore: [t] p=<hostaddr 0x1111> q=<hostaddr 0x2222>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] use <hostaddr 0x1111> then <hostaddr 0x2222>";
        let run_b =
            "2022-09-06T14:15:47.000000Z  INFO detcore: [t] p=<hostaddr 0xaaaa> q=<hostaddr 0xbbbb>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] use <hostaddr 0xaaaa> then <hostaddr 0xbbbb>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            !super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "address-only (ASLR-shift) difference must compare EQUAL under canonicalization"
        );

        // Positive control: raw comparison (no strip, no canonicalize) diverges on
        // the very same inputs, so the equality above is not vacuous.
        let raw = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: false,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &raw, &mut Vec::new())?,
            "raw comparison must still see the differing addresses"
        );
        Ok(())
    }

    /// Canonical parity negative control (allocation order): two runs whose only
    /// difference is the ORDER in which two addresses are introduced must compare
    /// UNEQUAL. This is exactly the divergence wholesale stripping hides -- it is
    /// the positive control that canonicalization preserves distinguishability.
    #[test]
    fn canonical_allocation_order_difference_compares_unequal() -> std::io::Result<()> {
        // Both runs: alloc, alloc, then a line pairing the two addresses. In run_b
        // the two addresses are introduced in the opposite order, so the ordinals
        // on the shared "pair" line are swapped.
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] alloc <hostaddr 0x1111>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] alloc <hostaddr 0x2222>
2022-09-06T14:15:49.000000Z  INFO detcore: [t] pair <hostaddr 0x1111> <hostaddr 0x2222>";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] alloc <hostaddr 0xbbbb>
2022-09-06T14:15:48.000000Z  INFO detcore: [t] alloc <hostaddr 0xaaaa>
2022-09-06T14:15:49.000000Z  INFO detcore: [t] pair <hostaddr 0xaaaa> <hostaddr 0xbbbb>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "an allocation-order difference must compare UNEQUAL under canonicalization"
        );

        // And wholesale stripping DOES hide it: both addresses collapse to a
        // single <ADDR> token.
        let stripped = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            strip_lines: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            !super::log_diff_from_strs(run_a, run_b, &stripped, &mut Vec::new())?,
            "wholesale stripping erases the allocation-order difference (the defect)"
        );
        Ok(())
    }

    /// Canonical parity negative control (aliasing): a run that prints ONE address
    /// twice (aliased) must not match a run that prints TWO distinct addresses in
    /// the same positions. `<addr1>,<addr1>` vs `<addr1>,<addr2>` diverges.
    #[test]
    fn canonical_aliasing_difference_compares_unequal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] two <hostaddr 0x1111> <hostaddr 0x1111>";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] two <hostaddr 0xaaaa> <hostaddr 0xbbbb>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "an aliasing difference (1,1 vs 1,2) must compare UNEQUAL"
        );
        Ok(())
    }

    /// Canonical parity exact-value control: a reproducible
    /// hex value printed as a syscall argument (`flock` `operation={:#x}`) is
    /// NOT a host address and must be compared EXACTLY. Two runs differing only
    /// in `operation=0x2` vs `0x6` must compare UNEQUAL under canonicalization --
    /// proving the canonicalizer touches only marked `<hostaddr ...>` pointers
    /// and does not become a "softer strip" that swallows syscall-argument
    /// divergence. A marked host address on the SAME line, differing only by an
    /// ASLR shift, must not by itself make the runs diverge.
    #[test]
    fn canonical_syscall_arg_hex_difference_compares_unequal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0x1111>";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x6) at <hostaddr 0xaaaa>";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "a bare syscall-argument hex difference (0x2 vs 0x6) must compare UNEQUAL: \
             it is reproducible and NOT a host address"
        );

        // Positive control: with ONLY the marked host address differing (same
        // syscall argument), the ASLR shift is canonicalized away and the runs
        // compare EQUAL -- so the divergence above is due to the syscall arg, not
        // the address.
        let addr_only_a = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0x1111>";
        let addr_only_b = "2022-09-06T14:15:47.000000Z  INFO detcore: flock(fd=3, operation=0x2) at <hostaddr 0xaaaa>";
        assert!(
            !super::log_diff_from_strs(addr_only_a, addr_only_b, &canonical, &mut Vec::new())?,
            "an address-only difference alongside an identical syscall arg must compare EQUAL"
        );
        Ok(())
    }

    /// Canonical parity exact-value control: a single virtual-time timestamp difference (a
    /// decimal value, NOT a `0x` address) must compare UNEQUAL -- canonicalization
    /// touches only host addresses and leaves every other byte for exact
    /// comparison. This is the sharp edge: virtual time is compared exactly even
    /// though the wall-clock PREFIX is stripped.
    #[test]
    fn canonical_virtual_time_difference_compares_unequal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: COMMIT turn 5 at time 100";
        let run_b = "2022-09-06T14:15:47.000000Z  INFO detcore: COMMIT turn 5 at time 200";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "a virtual-time (decimal) difference must compare UNEQUAL under canonicalization"
        );
        Ok(())
    }

    /// Canonical parity wall-clock control: runs that differ ONLY in the real wall-clock
    /// timestamp PREFIX compare EQUAL -- the prefix is stripped by
    /// `extract_log_messages` before any comparison, so nothing else needs to see
    /// it. This is the one genuinely-irreproducible datum the policy discards.
    #[test]
    fn canonical_wall_clock_prefix_difference_compares_equal() -> std::io::Result<()> {
        let run_a = "2022-09-06T14:15:47.000000Z  INFO detcore: [t] use 0x1111
2022-09-06T14:15:48.000000Z  INFO detcore: [t] use 0x1111";
        // Same message bodies, entirely different wall-clock prefixes (and format).
        let run_b = "Apr 09 06:08:03.100  INFO detcore: [t] use 0x1111
Jun 09 06:49:17.742  INFO detcore: [t] use 0x1111";

        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        assert!(
            !super::log_diff_from_strs(run_a, run_b, &canonical, &mut Vec::new())?,
            "a wall-clock-prefix-only difference must compare EQUAL"
        );
        Ok(())
    }

    #[test]
    fn one_log_canonical_info_preserves_values_and_only_rewrites_marked_addresses() {
        let log = "2026-08-13T01:02:03.000000Z INFO detcore: COMMIT turn 17 at time 123456 bare=0x2 marked=<hostaddr 0xaaaa>\n\
2026-08-13T01:02:03.000001Z DEBUG detcore: diagnostic=999\n\
2026-08-13T01:02:03.000002Z INFO detcore: DETLOG count=42 bare=0x6 marked=<hostaddr 0xaaaa> other=<hostaddr 0xbbbb>";

        assert_eq!(
            super::canonical_info_from_str(log),
            vec![
                "INFO detcore: COMMIT turn 17 at time 123456 bare=0x2 marked=<addr1>",
                "INFO detcore: DETLOG count=42 bare=0x6 marked=<addr1> other=<addr2>",
            ]
        );
    }

    #[test]
    fn first_log_divergence_reports_preceding_commit_turn_and_virtual_time() -> std::io::Result<()>
    {
        let left = "2026-08-13T01:02:03.000000Z INFO detcore::scheduler: COMMIT turn 17, dettid 2, on previously committed 12.345_678_901s\n\
2026-08-13T01:02:03.000001Z INFO detcore: DETLOG count=42";
        let right = "2026-08-13T01:02:04.000000Z INFO detcore::scheduler: COMMIT turn 17, dettid 2, on previously committed 12.345_678_901s\n\
2026-08-13T01:02:04.000001Z INFO detcore: DETLOG count=43";
        let opts = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };

        let diverged = super::log_diff_summary_from_strs(left, right, &opts, &mut Vec::new())?;
        assert!(diverged.diff_found);
        assert_eq!(diverged.first_divergent_scheduler_turn, Some(17));
        assert_eq!(
            diverged.first_divergent_virtual_nanoseconds,
            Some(12_345_678_901)
        );

        let matched = super::log_diff_summary_from_strs(left, left, &opts, &mut Vec::new())?;
        assert!(matched.matched_with_evidence());
        assert_eq!(matched.first_divergent_scheduler_turn, None);
        assert_eq!(matched.first_divergent_virtual_nanoseconds, None);
        Ok(())
    }

    #[test]
    fn log_divergence_without_commit_metadata_reports_no_position() -> std::io::Result<()> {
        let opts = super::LogDiffOpts {
            comparison: super::LogComparisonMode::Info,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        let summary = super::log_diff_summary_from_strs(
            "INFO detcore: DETLOG count=42",
            "INFO detcore: DETLOG count=43",
            &opts,
            &mut Vec::new(),
        )?;
        assert!(summary.diff_found);
        assert_eq!(summary.first_divergent_scheduler_turn, None);
        assert_eq!(summary.first_divergent_virtual_nanoseconds, None);
        Ok(())
    }

    /// NEGATIVE: two logs with nothing to compare must NOT be reported as a
    /// match with evidence. `diff_found` is legitimately false (there is no
    /// difference between two empty selections), but the counts are zero, so
    /// `matched_with_evidence()` must refuse. This is the "green with zero
    /// executed work" no-result, and it is the reason the counts exist.
    #[test]
    fn empty_selection_is_a_no_result_not_a_match() -> std::io::Result<()> {
        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        let summary = super::log_diff_summary_from_strs("", "", &canonical, &mut Vec::new())?;
        assert!(!summary.diff_found, "two empty logs do not differ");
        assert_eq!(summary.compared_left, 0);
        assert_eq!(summary.compared_right, 0);
        assert!(
            !summary.matched_with_evidence(),
            "zero compared messages must never count as a verified match"
        );
        Ok(())
    }

    /// POSITIVE: the same predicate must still FIRE on a real comparison, so the
    /// guard is not merely refusing everything.
    #[test]
    fn nonempty_identical_selection_is_a_match_with_evidence() -> std::io::Result<()> {
        let run = "Apr 09 06:08:03.100  INFO detcore: [t] finish syscall: close(2) = Ok(0)
Apr 09 06:08:03.200  INFO detcore: [t] finish syscall: exit_group(0)";
        let canonical = super::LogDiffOpts {
            comparison: super::LogComparisonMode::FullTrace,
            canonicalize_addresses: true,
            no_color: true,
            ..Default::default()
        };
        let summary = super::log_diff_summary_from_strs(run, run, &canonical, &mut Vec::new())?;
        assert!(!summary.diff_found);
        assert_eq!(summary.compared_left, 2);
        assert_eq!(summary.compared_right, 2);
        assert!(
            summary.matched_with_evidence(),
            "a real, nonempty, identical comparison must count as a match"
        );
        Ok(())
    }

    #[test]
    fn test_filter_infos() {
        let v = super::filter_infos(&[
            (
                0,
                "DEBUG detcore::scheduler: [sched-step3] advancing committed_time from 946684799165300000 to 946684799205300000",
            ),
            (
                1,
                "INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, ...",
            ),
        ]);
        assert_eq!(
            v,
            vec![(
                1,
                "INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, ..."
            )]
        );
    }

    #[test]
    fn test_extract_log_messages() {
        let s = "
Jan 09 06:08:03.100  INFO detcore: [detcore, dtid 2]  finish syscall: close(2) = Ok(0)
Feb 09 06:49:17.742 DEBUG detcore::scheduler: [sched-step3] advancing committed_time from 946684799165300000 to 946684799205300000
Apr 09 06:49:17.742  INFO detcore::scheduler: [scheduler] >>>>>>>

 COMMIT turn 5, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946684799205300000
Jan 09 06:49:03.100  INFO detcore: registers [dtid 3]. user_regs_struct { r15: 140737354129904, r14: 0, r13: 1, r12: 946684799000118840, rbp: 140737488344736, rbx: 0, r11: 518, r10: 140737488342434, r9: 0, r8: 1, rax: 0, rcx: 0, rdx: 2, rsi: 0, rdi: 140737354052880, orig_rax: 18446744073709551615, rip: 140737351875567, cs: 51, eflags: 66118, rsp: 140737488344064, ss: 43, fs_base: 0, gs_base: 0, ds: 0, es: 0, fs: 0, gs: 0 }
Jun 09 06:49:17.742 TRACE detcore::scheduler: [scheduler] Guest unblocked (<ivar Go>); clear ivars for the next turn on dettid 2
";

        let v = super::extract_log_messages(s);
        eprintln!("Split into {} log messages", v.len());
        for x in &v {
            eprintln!("{:?}", x);
        }
        assert_eq!(v.len(), 5);
    }

    #[test]
    fn test_canonicalize_addresses_in_line() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        let mut next = 1usize;
        // First appearance numbers marked addresses by order; a repeated address
        // reuses its ordinal (identity + aliasing). A BARE `0x...` literal and
        // decimal values are left untouched and compared exactly.
        assert_eq!(
            super::canonicalize_addresses_in_line(
                "a=<hostaddr 0x1111> b=<hostaddr 0x2222> c=<hostaddr 0x1111> raw=0x4444 n=42",
                &mut map,
                &mut next
            ),
            "a=<addr1> b=<addr2> c=<addr1> raw=0x4444 n=42"
        );
        // State threads across lines within one run: a known address keeps its
        // ordinal, a new one continues the count.
        assert_eq!(
            super::canonicalize_addresses_in_line(
                "use <hostaddr 0x2222> then <hostaddr 0x3333>",
                &mut map,
                &mut next
            ),
            "use <addr2> then <addr3>"
        );
        // A bare hex literal is never canonicalized, even one identical to a
        // marked address seen earlier: reproducible hex is compared exactly.
        assert_eq!(
            super::canonicalize_addresses_in_line("bare 0x1111", &mut map, &mut next),
            "bare 0x1111"
        );
    }

    #[test]
    fn test_strip_log() {
        assert_eq!(super::strip_log_entry("800.709_180s"), "<NANOSECONDS>");
        assert_eq!(super::strip_log_entry("98.91618ms"), "<NUM>");
        assert_eq!(super::strip_log_entry("98.91619ms"), "<NUM>");
        assert_eq!(super::strip_log_entry("x86_64"), "x86_64");
        assert_eq!(
            super::strip_log_entry(
                "COMMIT turn 66, dettid 2 using resources {Path(\"/proc/2/fd/1\"): W} at time 946_684_800.709_180_000s"
            ),
            "COMMIT turn <NUM>, dettid <NUM> using resources {Path(\"/proc/<PID>/fd/<NUM>\"): W} at time <NANOSECONDS>"
        );
    }

    /// Erasing a `/tmp` path must consume the path and nothing else.
    ///
    /// The pattern was previously `/tmp/.*"`, whose greedy `.*` ran to the LAST
    /// quote on the line rather than the path's own closing quote. Every field
    /// after the path was therefore erased too, so two entries differing only
    /// downstream of a `/tmp` path compared EQUAL under the stripped
    /// comparator -- a divergence silently reported as a match.
    #[test]
    fn strip_tmp_path_does_not_swallow_rest_of_line() {
        let read = super::strip_log_entry(r#"open path="/tmp/scratch" flags="O_RDONLY""#);
        let write = super::strip_log_entry(r#"open path="/tmp/scratch" flags="O_WRONLY""#);

        assert_eq!(read, r#"open path="/tmp/<somewhere>" flags="O_RDONLY""#);
        assert_eq!(write, r#"open path="/tmp/<somewhere>" flags="O_WRONLY""#);
        assert_ne!(
            read, write,
            "entries differing after a /tmp path must not collapse to equal"
        );
    }

    /// The narrowed pattern must still do its job: two entries whose only
    /// difference is the host-chosen `/tmp` path still compare equal, which is
    /// the whole reason this erasure exists.
    #[test]
    fn strip_tmp_path_still_erases_a_differing_tmp_path() {
        assert_eq!(
            super::strip_log_entry(r#"open path="/tmp/hermit-aaaa/f" flags="O_RDONLY""#),
            super::strip_log_entry(r#"open path="/tmp/hermit-bbbb/f" flags="O_RDONLY""#),
        );
    }

    /// Two distinct `/tmp` paths on one line are each erased individually,
    /// rather than the first one swallowing the second along with everything
    /// between them.
    #[test]
    fn strip_tmp_path_erases_each_path_separately() {
        assert_eq!(
            super::strip_log_entry(r#"rename from="/tmp/a" to="/tmp/b" ok="1""#),
            r#"rename from="/tmp/<somewhere>" to="/tmp/<somewhere>" ok="<NUM>""#
        );
    }
}
