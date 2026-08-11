// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

//! Runtime admission control and process accounting for the validate driver.
//!
//! Everything here answers a question about the *running world* rather than about
//! the plan: is another validate already driving this checkout, is this process a
//! nested payload of an outer run, how many peers were genuinely burning CPU
//! beside us, was a gate's red caused by the host rather than the tree, and did
//! this run spend its wall clock computing or waiting.
//!
//! # Why these live together
//!
//! Each one is a place where the previous implementation keyed off a *proxy* for
//! the fact it claimed, and each fix here replaces that proxy with something
//! observable:
//!
//! | question | old proxy | observable binding used here |
//! | --- | --- | --- |
//! | is a peer validate running? | `ps \| grep validate.sh` matched a process group | a per-run record whose **flock the kernel releases on death**, plus a measured CPU delta |
//! | am I nested? | the `HERMIT_VALIDATE_ACTIVE` env var alone | that pid must also appear in **this process's `/proc` ancestry** |
//! | is this red environmental? | a regex over the whole gate region | the failing node's own `----- detail -----` region, classified into a **named** class |
//! | did the run do work? | wall clock only | wall **and** `getrusage` CPU (self + reaped children) |
//!
//! # The concurrency primitive is `flock`, never a pidfile and never a scan
//!
//! * `flock` is released by the KERNEL when the holder dies, so a crashed or
//!   `SIGKILL`ed run cannot strand a lock. A pidfile makes the dead-owner case
//!   something you must represent and reclaim; `flock` makes it unrepresentable.
//! * A `ps | grep` scan counts PARKED fixtures. Measured on this box on
//!   2026-08-07: **6 live `validate.sh` process groups, all six orphaned
//!   stop-test fixtures** (`ppid=1`, ages 2h20m-4h30m, parked in `sleep 1` at
//!   CPU/wall ~0.00). A scan-based refusal would have refused EVERY validate on
//!   the box - an outage strictly worse than the bug it set out to fix. The same
//!   fixtures are why the shipped ledger carries `concurrent_validates` values up
//!   to 20 (histogram over the last 200 rows: 11x7, 7x11, 7x12, 5x13, ... , 1x20)
//!   that describe nothing anybody ran.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread::JoinHandle;

// ------------------------------------------------------------------ environmental blocks

/// Retry budget for an environmentally-blocked gate (`VALIDATE_ENV_BLOCK_RETRIES`,
/// validate.sh:903). Two retries means up to three attempts.
pub const ENV_BLOCK_RETRIES_DEFAULT: usize = 2;

/// Resolve the environmental-block retry budget from the environment.
pub fn env_block_max_retries() -> usize {
    match std::env::var("VALIDATE_ENV_BLOCK_RETRIES") {
        Ok(v) if !v.is_empty() => v.parse().unwrap_or(ENV_BLOCK_RETRIES_DEFAULT),
        _ => ENV_BLOCK_RETRIES_DEFAULT,
    }
}

/// Phrases that mean "the kernel/sandbox said no", in lowercase.
const DENIALS: &[&str] = &["operation not permitted", "permission denied", "(os error 1)"];

fn has_denial(line: &str) -> bool {
    DENIALS.iter().any(|d| line.contains(d))
}

fn has_any(line: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| line.contains(n))
}

/// Classify a failed gate's output as an ENVIRONMENTAL block, naming the class.
///
/// Returns `None` for a genuine product/test failure. Misclassifying a real
/// failure as environmental is as harmful as the reverse, so every form-2
/// (banner-less) anchor is pinned to build-tool / VCS phrasing: ordinary GUEST
/// output that legitimately produces `EPERM` - `DETLOG ... madvise ... EPERM
/// (Operation not permitted)`, the `kcmp-eperm` fixture, a `context: Mount`
/// EPERM - must never trip it. The self-test brackets both directions with
/// counts.
///
/// The classes, and what each is evidenced by:
///
/// * `bpfjailer-banner` - the canonical jail banner. The agent sandbox is a
///   BPFJailer jail inherited by every descendant (`validate -> cargo ->
///   rustc/cmake/cc1/ld`), and its FS/EXEC/NET enforcers transiently deny an
///   `open`/`execve` for reasons unrelated to the code under test.
/// * `toolchain-eperm` - a raw `EPERM`/`EACCES` leaked to a build tool with NO
///   banner: `cc1 fatal error: .../stddef.h: Operation not permitted`, a CMake or
///   linker denial on a system path, `rustc error: could not write output to ...`.
///   Those files are world-readable `root:root -rw-r--r--` on this host, so a
///   *compiler* reporting it cannot open a header for a permission reason is
///   never legitimate product behaviour.
/// * `third-party-build` - the vendored DynamoRIO/elfutils build under
///   `reverie-dbi`. At `nproc=316` an unbounded dependency scan drives elfutils
///   into a concurrency-exposed `SIGABRT`; that is a HOST build flake, not a
///   Hermit defect (Hermit source is not what failed to compile).
/// * `proxy-egress` - **NEW.** Egress through `fwdproxy` failed, so a networked
///   gate could not reach GitHub. Verbatim from
///   `/tmp/hermit-validate.WUrHlJ.log` (2026-08-07 09:37): `Lookup error: git
///   ls-remote https://github.com/rrnewton/reverie.git refs/heads/main failed:
///   fatal: unable to access '...': Could not resolve proxy: fwdproxy`, which the
///   old regex did NOT match - that run recorded a PRODUCT red for the Reverie
///   pin gate (the log contains no `ENVIRONMENTAL block` line) when nothing about
///   the tree was wrong. `CONNECT tunnel failed, response 403` is the same class:
///   the proxy is a per-destination allowlist, so a 403 is an egress verdict, not
///   a Hermit result.
/// * `vcs-fs-denial` - **NEW, defence in depth.** A banner-less git FS denial,
///   e.g. a jail denying `git init`/`git config` inside
///   `check-reverie-pin.rs`'s `/tmp` fixture repository. NOTE, honestly: the one
///   occurrence found on disk (`/tmp/hermit-validate.H61gJP.log:1400-1419`,
///   `Enforcer: FS, Reason: FILE_OPEN` while running `git -C
///   /tmp/check-reverie-pin-stale-lock-... config user.name`) DID carry the
///   jail banner and WAS already classified and retried successfully
///   (`:1499 ENVIRONMENTAL block on attempt 1/3`). So this anchor is not fixing a
///   measured miss; it covers the same denial arriving without the banner, which
///   is how the FS enforcer surfaces when it denies a child that captures its own
///   stderr.
pub fn environmental_block_class(output: &str) -> Option<&'static str> {
    let lower = output.to_ascii_lowercase();
    // Form 1: the canonical jail banner, anywhere in the region.
    if lower.contains("blocked on this server based on a security policy")
        || lower.contains("bpfjailer")
        || lower.contains("enforcer: fs, reason:")
        || lower.contains("enforcer: exec, reason:")
        || lower.contains("enforcer: net, reason:")
    {
        return Some("bpfjailer-banner");
    }
    // Form 4 (checked before the per-line scan because it is a whole-region
    // signature): the vendored third-party build script.
    if (lower.contains("failed to run custom build command for") && lower.contains("reverie-dbi"))
        || (lower.contains("panicked at") && lower.contains("reverie-dbi/build.rs"))
    {
        return Some("third-party-build");
    }
    let mut vcs_hit = false;
    for line in lower.lines() {
        // Form 3 (NEW): egress through the forward proxy failed.
        if has_any(
            line,
            &[
                "could not resolve proxy",
                "could not resolve host",
                "connect tunnel failed, response 403",
                "proxy connect aborted",
                "failed to connect to fwdproxy",
            ],
        ) {
            return Some("proxy-egress");
        }
        if !has_denial(line) {
            continue;
        }
        // Form 2: a banner-less denial reported by a build tool. Anchored on
        // compiler / build-system / linker phrasing.
        if (line.contains("fatal error: ") && line.matches(':').count() >= 2)
            || line.contains("cmake error")
            || has_any(
                line,
                &[
                    "cannot open",
                    "error opening",
                    "failed to open",
                    "could not open",
                    "opening dependency file",
                    "could not write output to",
                    "couldn't create the temp file",
                    "can't create",
                    "cannot execute",
                    "could not create temporary file",
                    "failed to build archive at",
                ],
            )
        {
            return Some("toolchain-eperm");
        }
        // Form 5 (NEW): a banner-less git FS denial. Requires BOTH a git-fatal
        // shape and a denial on the same line, so a guest test that merely prints
        // "permission denied" cannot trip it.
        if (line.starts_with("fatal:") || line.contains(" fatal: ") || line.contains(".git/"))
            && has_any(
                line,
                &[
                    "cannot mkdir",
                    "could not create work tree dir",
                    "could not create leading directories",
                    "unable to create",
                    "unable to write",
                    "unable to access",
                    "could not lock",
                    "chmod on",
                    ".git/",
                ],
            )
        {
            vcs_hit = true;
        }
    }
    if vcs_hit {
        return Some("vcs-fs-denial");
    }
    None
}

/// What the retry did to a node's environmental classification.
///
/// `environmental_block_class` binds by COLOCATION — a banner somewhere in the
/// failing node's own detail region. That establishes contemporaneity, not
/// causation. The retry is the differential experiment that settles it: same
/// commit, same code, fresh environment. This type is the experiment's readout,
/// and it deliberately has three variants rather than two, because "we never ran
/// the experiment" is a distinct state from either of its outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvBlockVerdict {
    /// Re-run, and it PASSED. The environment was the difference; the class holds.
    Confirmed,
    /// Re-run, and it FAILED AGAIN. What this refutes is the class's actual
    /// claim — a TRANSIENT host condition that a retry clears. It does not on its
    /// own prove a product bug, because a PERSISTENT denial reproduces too; see
    /// [`RefutedShape`], which splits the case. The excuse is withdrawn either
    /// way, since a persistent condition is not what the retry budget absorbs.
    Refuted,
    /// Never re-run (zero retry budget, unreadable log, empty retry set, or
    /// aborted during the retry). No experiment, so no verdict. This must read as
    /// NEITHER of the other two: an unconfirmed hypothesis keeps no excuse.
    Unconfirmed,
}

impl EnvBlockVerdict {
    /// Settle a classified node against what its retry actually did.
    ///
    /// `retried` must mean "this node executed a second time", not "a retry round
    /// happened" — an aborted retry outcome is NOT a re-run, and treating it as
    /// one is precisely how a never-tested hypothesis would masquerade as a
    /// refuted or confirmed one.
    pub fn settle(retried: bool, passed: bool) -> Self {
        match (retried, passed) {
            (true, true) => Self::Confirmed,
            (true, false) => Self::Refuted,
            (false, _) => Self::Unconfirmed,
        }
    }

    /// True only for the state that earns the node an environmental excuse.
    pub fn is_environmental(self) -> bool {
        matches!(self, Self::Confirmed)
    }

    /// True only for the state where the retry actually settled the question
    /// against the class. Whether that settled failure is a product bug or a
    /// standing host defect is [`RefutedShape`]'s question, not this one.
    ///
    /// `Unconfirmed` is false for BOTH this and `is_environmental` — that pair of
    /// falses IS the third state.
    pub fn is_settled_failure(self) -> bool {
        matches!(self, Self::Refuted)
    }
}

/// What a REFUTED node's NEWEST attempt looked like, compared to the signature
/// that classified it.
///
/// `EnvBlockVerdict::Refuted` is not by itself "product bug". A failing re-run
/// proves only that the failure reproduces at this commit in this host state,
/// and a PERSISTENT host denial reproduces exactly as well as a real defect
/// does. Comparing the newest attempt's class against the original is what
/// separates the two — cheaply, from data the driver already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefutedShape {
    /// The banner is GONE from the newest attempt and the node failed anyway.
    /// The strong case: the classification really was a coincidence, and this is
    /// a product failure.
    BannerGone,
    /// The newest attempt carries the SAME class. A standing host condition, not
    /// the transient flake the retry budget exists to absorb. Retrying again is
    /// wasted; it needs a triager, not another attempt.
    Persistent,
    /// The class CHANGED between attempts, so no single cause is established.
    /// Triage the newest signature on its own.
    SignatureChanged,
}

impl RefutedShape {
    /// Classify a refuted node by its newest attempt. `latest` is `None` when the
    /// newest detail region no longer classifies environmental at all.
    pub fn of(original: &str, latest: Option<&str>) -> Self {
        match latest {
            None => Self::BannerGone,
            Some(l) if l == original => Self::Persistent,
            Some(_) => Self::SignatureChanged,
        }
    }

    /// True only for the shape that positively identifies a product failure.
    /// `Persistent` and `SignatureChanged` are REDs that lost their excuse, which
    /// is not the same claim.
    pub fn is_product_failure(self) -> bool {
        matches!(self, Self::BannerGone)
    }
}

/// Extract one failed DAG node's captured output from the driver's durable log.
///
/// `safe-ci-dag-runner` re-emits a failed step's combined stdout+stderr between
/// `[tag] ----- detail -----` and `[tag] ----- end detail -----`, one line per
/// prefixed line (scheduler.rs:844-849). Reading THAT region - rather than a
/// whole-log tail - is what binds the classification to the node that actually
/// failed, so a jail banner printed by an unrelated concurrent node cannot excuse
/// a genuine product red.
///
/// Returns `None` when the region is absent (log not flushed, or the node's
/// failure predates detail emission).
pub fn extract_node_detail(log: &str, tag: &str) -> Option<String> {
    let open = format!("[{tag}] ----- detail -----");
    let close = format!("[{tag}] ----- end detail -----");
    let prefix = format!("[{tag}] ");
    // The LAST region, so a retried node is classified on its newest attempt.
    let start = log.rfind(&open)? + open.len();
    let rest = &log[start..];
    let end = rest.find(&close).unwrap_or(rest.len());
    let mut out = String::new();
    for line in rest[..end].lines() {
        out.push_str(line.strip_prefix(&prefix).unwrap_or(line));
        out.push('\n');
    }
    Some(out)
}

// ------------------------------------------------------------------ CPU vs wall

/// Clock ticks per second, for converting `/proc/<pid>/stat` utime+stime.
pub fn clk_tck() -> f64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as f64 } else { 100.0 }
}

fn rusage_seconds(who: libc::c_int) -> (f64, f64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(who, &mut ru) } != 0 {
        return (0.0, 0.0);
    }
    let s = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1_000_000.0;
    (s(ru.ru_utime), s(ru.ru_stime))
}

/// CPU seconds (user, sys) for this process PLUS every child it has reaped.
///
/// This is exactly what bash's `times` builtin reports (validate.sh:1614), and it
/// is why the number must be taken in the top-level process: a subshell - or, here,
/// a worker thread's local view - would see only its own accounting. Every gate
/// runs as a child of this process through `safe-ci-dag-runner`, and the runner
/// waits on each one, so `RUSAGE_CHILDREN` accumulates the whole suite.
pub fn process_cpu_seconds() -> (f64, f64) {
    let (su, ss) = rusage_seconds(libc::RUSAGE_SELF);
    let (cu, cs) = rusage_seconds(libc::RUSAGE_CHILDREN);
    (su + cu, ss + cs)
}

/// Aggregate CPU seconds (user+sys) for the process tree rooted at `root`,
/// summed from `/proc/<pid>/stat`.
///
/// Controller-free and host-portable: it does NOT need a delegated cgroup `cpu`
/// controller, which is often absent on the many-core dev hosts. The `comm` field
/// can contain spaces and parentheses, so parsing splits on the LAST `)` and
/// indexes the fixed fields after it - the same rule validate.sh:1404 used.
pub fn tree_cpu_seconds(root: i32) -> f64 {
    let mut ppid: HashMap<i32, i32> = HashMap::new();
    let mut ticks: HashMap<i32, f64> = HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return 0.0;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        let Some(rp) = stat.rfind(')') else { continue };
        let f: Vec<&str> = stat[rp + 2..].split_whitespace().collect();
        if f.len() < 13 {
            continue;
        }
        let Ok(pp) = f[1].parse::<i32>() else { continue };
        let ut: f64 = f[11].parse().unwrap_or(0.0);
        let st: f64 = f[12].parse().unwrap_or(0.0);
        ppid.insert(pid, pp);
        ticks.insert(pid, ut + st);
    }
    // Transitive closure of "is a descendant of root", root included.
    let mut in_tree: std::collections::HashSet<i32> = std::collections::HashSet::new();
    in_tree.insert(root);
    let mut changed = true;
    while changed {
        changed = false;
        for (&p, &pp) in &ppid {
            if !in_tree.contains(&p) && in_tree.contains(&pp) {
                in_tree.insert(p);
                changed = true;
            }
        }
    }
    let total: f64 = ticks.iter().filter(|(p, _)| in_tree.contains(p)).map(|(_, t)| *t).sum();
    total / clk_tck()
}

/// The load-bearing shape hint for a CPU-vs-wall pair.
///
/// CPU (user+sys, whole process tree) against wall is what distinguishes a
/// genuinely busy run from one that is blocked or spinning while merely appearing
/// hung. This is how the 53-minute pre-gate wedge was identified on 2026-08-07:
/// the wall clock alone said "still going", the CPU/wall ratio said "waiting".
///
/// * CPU below 10% of wall over a non-trivial run => waiting/blocked.
/// * ~1.0x on a multi-core host => single-threaded work, or a spin.
///
/// Returns `None` when the run is too short (<30 s) for either shape to mean
/// anything.
pub fn cpu_wall_hint(cpu: f64, wall: f64, host_cpus: usize) -> Option<&'static str> {
    if wall < 30.0 {
        return None;
    }
    if cpu < 0.10 * wall {
        return Some("low CPU vs wall — mostly waiting/blocked, not compute-bound");
    }
    let ratio = if wall > 0.0 { cpu / wall } else { 0.0 };
    if host_cpus > 2 && (0.8..=1.2).contains(&ratio) {
        return Some("~1 core busy — single-threaded or possibly spinning");
    }
    None
}

/// Format the always-printed wall+CPU line (validate.sh's `print_wall_cpu_summary`).
pub fn cpu_wall_line(
    human: fn(f64) -> String,
    wall: f64,
    user: f64,
    sys: f64,
    host_cpus: usize,
) -> String {
    let cpu = user + sys;
    let ratio =
        if wall > 0.0 { format!("{:.1}", cpu / wall) } else { "n/a".to_string() };
    let hint = cpu_wall_hint(cpu, wall, host_cpus)
        .map(|h| format!("  ({h})"))
        .unwrap_or_default();
    format!(
        "wall {} | CPU {} (user {}, sys {}) | CPU/wall {}x across {} cores{}",
        human(wall),
        human(cpu),
        human(user),
        human(sys),
        ratio,
        host_cpus,
        hint
    )
}

// ------------------------------------------------------------------ nesting

/// Environment marker naming the live top-level validate, inherited by every
/// gate this run spawns.
pub const ACTIVE_ENV: &str = "HERMIT_VALIDATE_ACTIVE";

/// What this process is with respect to an outer validate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Nesting {
    /// True only when an outer validate is provably an ANCESTOR of this process.
    pub nested: bool,
    /// The outer pid, when nesting was established.
    pub outer_pid: Option<i32>,
    /// Set when the marker was present but did NOT survive the ancestry check, so
    /// the reason a stale marker was ignored is stated rather than silent.
    pub stale_marker: Option<i32>,
}

/// Walk `/proc/<pid>/status` PPid links from `start` to pid 1, looking for `want`.
fn is_ancestor(want: i32, start: i32) -> bool {
    let mut cur = start;
    // Bounded: the ancestry chain is short, and the bound makes a corrupted
    // /proc unable to hang the driver.
    for _ in 0..256 {
        if cur == want {
            return true;
        }
        if cur <= 1 {
            return false;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{cur}/status")) else {
            return false;
        };
        let Some(line) = status.lines().find(|l| l.starts_with("PPid:")) else {
            return false;
        };
        let Ok(pp) = line[5..].trim().parse::<i32>() else { return false };
        cur = pp;
    }
    false
}

/// Read a process's kernel identity `(state, start_ticks)` from `/proc`.
pub fn process_identity(pid: i32) -> Option<(char, u64)> {
    if pid <= 1 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let fields: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let state = fields.first()?.chars().next()?;
    // `fields[0]` is stat field 3 (state), so field 22 is index 19.
    let start_ticks = fields.get(19)?.parse::<u64>().ok()?;
    Some((state, start_ticks))
}

/// Bind an ancestor PID to its start ticks so PID reuse cannot satisfy proof.
pub fn identity_in_ancestry(want: i32, want_start_ticks: u64) -> bool {
    let mut cur = std::process::id() as i32;
    for _ in 0..256 {
        if cur == want {
            return process_identity(cur).is_some_and(|(state, ticks)| {
                !matches!(state, 'Z' | 'T' | 't') && ticks == want_start_ticks
            });
        }
        if cur <= 1 {
            return false;
        }
        let Ok(status) = std::fs::read_to_string(format!("/proc/{cur}/status")) else {
            return false;
        };
        let Some(line) = status.lines().find(|line| line.starts_with("PPid:")) else {
            return false;
        };
        let Ok(parent) = line[5..].trim().parse::<i32>() else { return false };
        cur = parent;
    }
    false
}

/// Decide whether this invocation is a NESTED payload of an outer validate.
///
/// `ci/dag/portable.json`'s `test.strict_compat` node runs
/// `./scripts/validate.rs --portable-strict-compat-only`, so re-entry is a designed path,
/// not an accident. What must never happen is a full driver inside a full driver:
/// that pays the entire preamble twice, appends a SECOND ledger row, and can
/// publish a SECOND receipt for one logical run.
///
/// **The env var alone is a proxy.** A marker can outlive its writer - exported
/// into an operator's shell, or inherited by a detached unit - and a stale marker
/// would make a legitimate TOP-LEVEL full run exit 2 forever: an outage, not a
/// guard. So nesting is asserted only when the named pid is observably an
/// ancestor of this process in `/proc`. A marker that fails that check is
/// reported as stale and ignored.
pub fn detect_nesting() -> Nesting {
    let raw = std::env::var(ACTIVE_ENV).unwrap_or_default();
    let Ok(outer) = raw.trim().parse::<i32>() else {
        return Nesting { nested: false, outer_pid: None, stale_marker: None };
    };
    if outer <= 0 {
        return Nesting { nested: false, outer_pid: None, stale_marker: None };
    }
    let me = std::process::id() as i32;
    if is_ancestor(outer, me) {
        Nesting { nested: true, outer_pid: Some(outer), stale_marker: None }
    } else {
        Nesting { nested: false, outer_pid: None, stale_marker: Some(outer) }
    }
}

/// Claim the marker for children. Called AFTER [`detect_nesting`], so diagnostics
/// name the run we are nested inside rather than ourselves.
pub fn claim_active_marker() {
    std::env::set_var(ACTIVE_ENV, std::process::id().to_string());
}

// ------------------------------------------------------------------ peer snapshot authority

/// The peer-monitor protocol is versioned because its sequence/final-ack rules
/// are part of the receipt proof, not an implementation detail.
pub const PEER_MONITOR_PROTOCOL: &str = "sequence-final-ack-v1";

const PF_KTHREAD: u64 = 0x0020_0000;

/// A process existed in the snapshot but could not be classified safely.
///
/// This is deliberately distinct from "not a validate".  Unreadable or
/// malformed evidence is UNKNOWN and must make the whole monitor sticky-
/// indeterminate; otherwise a persistent unreadable peer is laundered into
/// proved absence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotUnresolved(pub String);

impl std::fmt::Display for SnapshotUnresolved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SnapshotUnresolved> for String {
    fn from(error: SnapshotUnresolved) -> Self {
        error.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessRecord {
    pid: i32,
    state: char,
    ppid: i32,
    pgid: i32,
    flags: u64,
    start_ticks: u64,
    cgroup: String,
    cgroup_path: String,
    systemd_unit: String,
    systemd_unit_cgroup: String,
    argv: Vec<String>,
}

/// Identity persisted for the lock owner and every candidate validate.
/// Cgroup and unit are both carried because either one alone is a weaker proxy:
/// a nested safe-ci scope remains owned by the enclosing validate service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerProcessIdentity {
    pub pid: i32,
    pub start_ticks: u64,
    pub pgid: i32,
    pub cgroup: String,
    pub cgroup_path: String,
    pub systemd_unit: String,
    pub systemd_unit_cgroup: String,
    pub classification: Option<&'static str>,
}

impl PeerProcessIdentity {
    fn from_record(record: &ProcessRecord) -> Self {
        Self {
            pid: record.pid,
            start_ticks: record.start_ticks,
            pgid: record.pgid,
            cgroup: record.cgroup.clone(),
            cgroup_path: record.cgroup_path.clone(),
            systemd_unit: record.systemd_unit.clone(),
            systemd_unit_cgroup: record.systemd_unit_cgroup.clone(),
            classification: None,
        }
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "pid": self.pid,
            "start_ticks": self.start_ticks,
            "pgid": self.pgid,
            "cgroup": self.cgroup,
            "cgroup_path": self.cgroup_path,
            "systemd_unit": self.systemd_unit,
            "systemd_unit_cgroup": self.systemd_unit_cgroup,
            "classification": self.classification,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PeerSnapshot {
    owner: PeerProcessIdentity,
    same_service: Vec<PeerProcessIdentity>,
    peers: Vec<PeerProcessIdentity>,
}

fn parse_proc_stat(text: &str) -> Result<(char, i32, i32, u64, u64), String> {
    let close = text.rfind(')').ok_or_else(|| "missing stat comm terminator".to_string())?;
    let fields: Vec<&str> = text[close + 1..].split_whitespace().collect();
    if fields.len() < 20 {
        return Err("short stat row".into());
    }
    let mut states = fields[0].chars();
    let state = states.next().ok_or_else(|| "empty stat state".to_string())?;
    if states.next().is_some() {
        return Err("malformed stat state".into());
    }
    let parse = |index: usize, name: &str| {
        fields[index]
            .parse::<i64>()
            .map_err(|e| format!("malformed stat {name}: {e}"))
    };
    let ppid = i32::try_from(parse(1, "ppid")?)
        .map_err(|_| "stat ppid is outside i32".to_string())?;
    let pgid = i32::try_from(parse(2, "pgid")?)
        .map_err(|_| "stat pgid is outside i32".to_string())?;
    let flags = fields[6]
        .parse::<u64>()
        .map_err(|e| format!("malformed stat flags: {e}"))?;
    let start_ticks = fields[19]
        .parse::<u64>()
        .map_err(|e| format!("malformed stat start_ticks: {e}"))?;
    Ok((state, ppid, pgid, flags, start_ticks))
}

fn cgroup_identity(text: &str) -> Result<(String, String, String, String), String> {
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    if lines.is_empty() {
        return Err("empty cgroup identity".into());
    }
    let unified = lines.iter().copied().find(|line| line.starts_with("0::")).unwrap_or(lines[0]);
    let mut pieces = unified.splitn(3, ':');
    let _hierarchy = pieces.next();
    let _controllers = pieces.next();
    let path = pieces.next().ok_or_else(|| "malformed cgroup identity".to_string())?;
    if !path.starts_with('/') {
        return Err("malformed cgroup identity".into());
    }
    let components: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    // `user@*.service` is the systemd user manager, not an application
    // identity.  Prefer the last real application service.  Only when none
    // exists may the innermost scope stand in for the owning unit.
    let service = components.iter().enumerate().rev().find(|(_, component)| {
        component.ends_with(".service") && !component.starts_with("user@")
    });
    let scope = components
        .iter()
        .enumerate()
        .rev()
        .find(|(_, component)| component.ends_with(".scope"));
    let Some((index, unit)) = service.or(scope) else {
        // Root/no-unit cgroups are legitimate for kernel threads.  The caller
        // combines this with stat flags and cmdline before classifying it.
        return Ok((unified.to_string(), path.to_string(), String::new(), String::new()));
    };
    Ok((
        unified.to_string(),
        path.to_string(),
        (*unit).to_string(),
        format!("/{}", components[..=index].join("/")),
    ))
}

fn confirm_exited_after_empty_cmdline(
    proc_root: &Path,
    pid: i32,
    _initial_state: char,
    initial_start_ticks: u64,
) -> std::io::Result<char> {
    // Always re-read stat, even when the first observation was already
    // terminal.  The empty-cmdline read sits between the two observations, so
    // a terminal PID can be reaped and reused during that window just as a live
    // PID can.  Accept only the same kernel identity in a terminal state.
    let stat_text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat"))?;
    let (state, _, _, _, start_ticks) = parse_proc_stat(&stat_text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if start_ticks != initial_start_ticks {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "empty-cmdline PID identity changed start_ticks {initial_start_ticks}->{start_ticks}"
            ),
        ));
    }
    if matches!(state, 'Z' | 'X' | 'x') {
        return Ok(state);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "empty cmdline for live userspace process",
    ))
}

fn read_process_record(proc_root: &Path, pid: i32) -> std::io::Result<ProcessRecord> {
    let process = proc_root.join(pid.to_string());
    let stat_text = std::fs::read_to_string(process.join("stat"))?;
    let (mut state, ppid, pgid, flags, start_ticks) = parse_proc_stat(&stat_text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let cmdline = std::fs::read(process.join("cmdline"))?;
    let argv: Vec<String> = cmdline
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| {
            String::from_utf8(part.to_vec()).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("cmdline is not UTF-8: {e}"),
                )
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    if argv.is_empty() && flags & PF_KTHREAD == 0 {
        // A process may exit between the stat and cmdline reads. Re-read stat
        // and accept only the same kernel identity in an explicit terminal
        // state; an empty cmdline for a still-live or PID-reused process stays
        // unresolved. This is the zombie form of the genuine-exit race.
        state = confirm_exited_after_empty_cmdline(proc_root, pid, state, start_ticks)?;
    }
    let cgroup_text = std::fs::read_to_string(process.join("cgroup"))?;
    let (cgroup, cgroup_path, systemd_unit, systemd_unit_cgroup) =
        cgroup_identity(&cgroup_text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(ProcessRecord {
        pid,
        state,
        ppid,
        pgid,
        flags,
        start_ticks,
        cgroup,
        cgroup_path,
        systemd_unit,
        systemd_unit_cgroup,
        argv,
    })
}

fn read_process_start_ticks(proc_root: &Path, pid: i32) -> std::io::Result<u64> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat"))?;
    parse_proc_stat(&text)
        .map(|(_, _, _, _, start)| start)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Identity component used to bind the controller and the canonical lock owner
/// against PID reuse.  Production is hard-bound to `/proc`.
pub fn process_start_ticks(pid: i32) -> Option<u64> {
    read_process_start_ticks(Path::new("/proc"), pid).ok()
}

fn numeric_pids(proc_root: &Path) -> Result<Vec<i32>, SnapshotUnresolved> {
    let entries = std::fs::read_dir(proc_root)
        .map_err(|e| SnapshotUnresolved(format!("cannot enumerate {}: {e}", proc_root.display())))?;
    let mut pids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            SnapshotUnresolved(format!("cannot enumerate {}: {e}", proc_root.display()))
        })?;
        if let Some(pid) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok()) {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

fn vanished_after_enoent(proc_root: &Path, pid: i32) -> Result<bool, SnapshotUnresolved> {
    Ok(!numeric_pids(proc_root)?.contains(&pid))
}

fn is_direct_validate(argv: &[String]) -> bool {
    if argv.is_empty() {
        return false;
    }
    // rust-script preserves the source path in argv on the interpreted path;
    // direct executable and legacy shell paths are covered too.  Matching a
    // complete basename avoids treating a command that merely mentions the
    // word "validate" as a peer.
    if argv.iter().any(|arg| {
        matches!(
            Path::new(arg).file_name().and_then(|name| name.to_str()),
            Some("validate.rs" | "validate.sh")
        )
    }) {
        return true;
    }
    false
}

fn ancestry(pid: i32, records: &BTreeMap<i32, ProcessRecord>) -> (Vec<i32>, bool) {
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut current = pid;
    while current > 0 {
        if !seen.insert(current) {
            return (chain, false);
        }
        chain.push(current);
        let Some(record) = records.get(&current) else { return (chain, false) };
        if record.ppid == 0 || record.ppid == current {
            return (chain, true);
        }
        current = record.ppid;
    }
    (chain, true)
}

fn collect_peer_snapshot_with<R, S>(
    proc_root: &Path,
    owner_pid: i32,
    mut record_reader: R,
    mut start_reader: S,
) -> Result<PeerSnapshot, SnapshotUnresolved>
where
    R: FnMut(&Path, i32) -> std::io::Result<ProcessRecord>,
    S: FnMut(&Path, i32) -> std::io::Result<u64>,
{
    let mut records = BTreeMap::new();
    for pid in numeric_pids(proc_root)? {
        match record_reader(proc_root, pid) {
            Ok(record) => {
                records.insert(pid, record);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if vanished_after_enoent(proc_root, pid)? {
                    continue;
                }
                return Err(SnapshotUnresolved(format!(
                    "process PID {pid} evidence disappeared but PID remains visible: {error}"
                )));
            }
            Err(error) => {
                return Err(SnapshotUnresolved(format!(
                    "process PID {pid} evidence is unreadable or malformed: {error}"
                )));
            }
        }
    }
    if owner_pid <= 1 || !records.contains_key(&owner_pid) {
        return Err(SnapshotUnresolved(format!(
            "validate-lock owner PID {owner_pid} is not live"
        )));
    }
    let owner_record = records.get(&owner_pid).expect("checked above");
    if owner_record.systemd_unit.is_empty() || owner_record.systemd_unit_cgroup.is_empty() {
        return Err(SnapshotUnresolved(format!(
            "validate-lock owner PID {owner_pid} has no observable systemd unit"
        )));
    }
    let actual: BTreeSet<i32> = records
        .iter()
        .filter_map(|(pid, record)| is_direct_validate(&record.argv).then_some(*pid))
        .collect();
    let roots: Vec<ProcessRecord> = actual
        .iter()
        .filter_map(|pid| {
            let (chain, _) = ancestry(*pid, &records);
            (!chain.iter().skip(1).any(|ancestor| actual.contains(ancestor)))
                .then(|| records.get(pid).expect("actual came from records").clone())
        })
        .collect();
    let owner = PeerProcessIdentity::from_record(owner_record);
    let mut same_service = Vec::new();
    let mut peers = Vec::new();
    for record in roots {
        if record.systemd_unit.is_empty() || record.systemd_unit_cgroup.is_empty() {
            return Err(SnapshotUnresolved(format!(
                "candidate PID {} has no observable systemd unit",
                record.pid
            )));
        }
        let observed = match start_reader(proc_root, record.pid) {
            Ok(start) => start,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if vanished_after_enoent(proc_root, record.pid)? {
                    continue;
                }
                return Err(SnapshotUnresolved(format!(
                    "candidate PID {} identity disappeared but PID remains visible: {error}",
                    record.pid
                )));
            }
            Err(error) => {
                return Err(SnapshotUnresolved(format!(
                    "candidate PID {} identity is unreadable or malformed: {error}",
                    record.pid
                )));
            }
        };
        if observed != record.start_ticks {
            return Err(SnapshotUnresolved(format!(
                "candidate PID {} changed start_ticks {}->{observed}",
                record.pid, record.start_ticks
            )));
        }
        let (chain, _) = ancestry(record.pid, &records);
        let mut identity = PeerProcessIdentity::from_record(&record);
        if chain.contains(&owner_pid) {
            identity.classification = Some("owner-ancestry-self");
            same_service.push(identity);
        } else if record.systemd_unit == owner.systemd_unit
            && record.systemd_unit_cgroup == owner.systemd_unit_cgroup
        {
            identity.classification = Some("reparented-same-service-self");
            same_service.push(identity);
        } else {
            identity.classification = Some("different-systemd-unit-peer");
            peers.push(identity);
        }
    }
    Ok(PeerSnapshot { owner, same_service, peers })
}

fn collect_peer_snapshot(
    proc_root: &Path,
    owner_pid: i32,
) -> Result<PeerSnapshot, SnapshotUnresolved> {
    collect_peer_snapshot_with(proc_root, owner_pid, read_process_record, read_process_start_ticks)
}

/// Complete, sticky state carried from the initial scan through final ack.
#[derive(Clone, Debug)]
pub struct PeerMonitorState {
    pub scan_complete: bool,
    pub indeterminate: bool,
    pub indeterminate_detail: Option<String>,
    pub scan_count: u64,
    pub monitor_ready: bool,
    pub monitor_pid: i32,
    pub monitor_sequence: u64,
    pub final_ack_sequence: Option<u64>,
    pub exclusion_held: bool,
    pub owner: Option<PeerProcessIdentity>,
    pub same_service: Vec<PeerProcessIdentity>,
    pub peers: Vec<PeerProcessIdentity>,
}

fn lock_peer_state(state: &Mutex<PeerMonitorState>) -> std::sync::MutexGuard<'_, PeerMonitorState> {
    // A monitor-thread panic is evidence failure, not permission to skip the
    // ledger append. Recover the state so the controller can mark it sticky-
    // indeterminate and persist the one diagnostic row.
    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl PeerMonitorState {
    fn new(monitor_pid: i32, exclusion_held: bool) -> Self {
        Self {
            scan_complete: false,
            indeterminate: false,
            indeterminate_detail: None,
            scan_count: 0,
            monitor_ready: false,
            monitor_pid,
            monitor_sequence: 0,
            final_ack_sequence: None,
            exclusion_held,
            owner: None,
            same_service: Vec::new(),
            peers: Vec::new(),
        }
    }

    pub fn mark_indeterminate(&mut self, detail: impl Into<String>) {
        self.indeterminate = true;
        if self.indeterminate_detail.is_none() {
            self.indeterminate_detail = Some(detail.into());
        }
    }

    fn merge_snapshot(&mut self, snapshot: PeerSnapshot) {
        if self.owner.as_ref().is_some_and(|owner| owner != &snapshot.owner) {
            self.mark_indeterminate("validate-lock-owner-identity-changed");
        } else if self.owner.is_none() {
            self.owner = Some(snapshot.owner);
        }
        let mut same: BTreeMap<(i32, u64), PeerProcessIdentity> = self
            .same_service
            .drain(..)
            .map(|identity| ((identity.pid, identity.start_ticks), identity))
            .collect();
        for identity in snapshot.same_service {
            same.insert((identity.pid, identity.start_ticks), identity);
        }
        self.same_service = same.into_values().collect();
        let mut peers: BTreeMap<(i32, u64), PeerProcessIdentity> = self
            .peers
            .drain(..)
            .map(|identity| ((identity.pid, identity.start_ticks), identity))
            .collect();
        for identity in snapshot.peers {
            peers.insert((identity.pid, identity.start_ticks), identity);
        }
        self.peers = peers.into_values().collect();
        self.scan_complete = true;
        self.scan_count += 1;
    }

    fn scan(&mut self, proc_root: &Path, owner_pid: i32) {
        self.monitor_sequence += 1;
        match collect_peer_snapshot(proc_root, owner_pid) {
            Ok(snapshot) => self.merge_snapshot(snapshot),
            Err(error) => {
                self.scan_complete = false;
                self.mark_indeterminate(format!("snapshot-unresolved:{error}"));
            }
        }
    }

    pub fn monitor_json(&self) -> serde_json::Value {
        serde_json::json!({
            "scan_complete": self.scan_complete,
            "scan_count": self.scan_count,
            "monitor_protocol": PEER_MONITOR_PROTOCOL,
            "monitor_ready": self.monitor_ready,
            "monitor_pid": self.monitor_pid,
            "monitor_sequence": self.monitor_sequence,
            "final_ack_sequence": self.final_ack_sequence,
            "exclusion_kind": "kernel-flock",
            "exclusion_held": self.exclusion_held,
            "indeterminate": self.indeterminate,
            "indeterminate_detail": self.indeterminate_detail,
        })
    }

    pub fn owner_json(&self) -> serde_json::Value {
        self.owner.as_ref().map(PeerProcessIdentity::json).unwrap_or(serde_json::Value::Null)
    }

    pub fn same_service_json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.same_service.iter().map(PeerProcessIdentity::json).collect())
    }

    pub fn peers_json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.peers.iter().map(PeerProcessIdentity::json).collect())
    }

    /// A zero is authority-bearing only after a synchronous final scan, exact
    /// sequence acknowledgement, held kernel exclusion, and no sticky unknown.
    pub fn qualifies_exclusivity(&self) -> bool {
        self.scan_complete
            && !self.indeterminate
            && self.scan_count >= 2
            && self.monitor_ready
            && self.monitor_sequence > 0
            && self.final_ack_sequence == Some(self.monitor_sequence)
            && self.exclusion_held
            && self.owner.is_some()
    }
}

#[derive(Clone, Debug)]
pub struct PeerMonitorEvidence {
    pub state: PeerMonitorState,
    pub final_acknowledged: bool,
}

impl PeerMonitorEvidence {
    pub fn diagnostic(detail: impl Into<String>) -> Self {
        let mut state = PeerMonitorState::new(std::process::id() as i32, false);
        state.mark_indeterminate(detail);
        Self { state, final_acknowledged: false }
    }
}

fn peer_credentials(fd: i32) -> std::io::Result<(i32, u32, u32)> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut credentials as *mut libc::ucred as *mut libc::c_void,
            &mut length,
        )
    };
    if rc != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(std::io::Error::last_os_error());
    }
    Ok((credentials.pid, credentials.uid, credentials.gid))
}

fn process_identity_matches(pid: i32, start_ticks: u64) -> bool {
    if pid <= 1 {
        return false;
    }
    read_process_start_ticks(Path::new("/proc"), pid).ok() == Some(start_ticks)
}

fn write_socket_json(stream: &mut UnixStream, value: serde_json::Value) {
    let _ = stream.write_all(format!("{}\n", value).as_bytes());
}

fn handle_peer_request(
    stream: &mut UnixStream,
    state: &Arc<Mutex<PeerMonitorState>>,
    proc_root: &Path,
    owner_pid: i32,
    controller_pid: i32,
    controller_start_ticks: u64,
    finalized: &mut bool,
    shutdown: &AtomicBool,
) {
    let peer = peer_credentials(stream.as_raw_fd());
    let authorized = peer.is_ok_and(|(pid, uid, _)| {
        uid == unsafe { libc::getuid() }
            && pid == controller_pid
            && process_identity_matches(pid, controller_start_ticks)
    });
    if !authorized {
        write_socket_json(
            stream,
            serde_json::json!({"ok": false, "error": "unauthorized-controller"}),
        );
        return;
    }
    let mut request = [0u8; 64];
    let amount = stream.read(&mut request).unwrap_or(0);
    let command = std::str::from_utf8(&request[..amount]).unwrap_or("").trim();
    match command {
        "probe" if !*finalized => {
            let state = state.lock().expect("peer monitor state poisoned");
            write_socket_json(
                stream,
                serde_json::json!({
                    "ok": true,
                    "protocol": PEER_MONITOR_PROTOCOL,
                    "monitor_pid": state.monitor_pid,
                    "sequence": state.monitor_sequence,
                    "exclusion_held": state.exclusion_held,
                }),
            );
        }
        "final" if !*finalized => {
            let mut state = state.lock().expect("peer monitor state poisoned");
            state.scan(proc_root, owner_pid);
            state.final_ack_sequence = Some(state.monitor_sequence);
            *finalized = true;
            write_socket_json(
                stream,
                serde_json::json!({
                    "ok": true,
                    "protocol": PEER_MONITOR_PROTOCOL,
                    "monitor_pid": state.monitor_pid,
                    "ack_sequence": state.monitor_sequence,
                    "scan_complete": state.scan_complete,
                    "indeterminate": state.indeterminate,
                    "exclusion_held": state.exclusion_held,
                }),
            );
        }
        "shutdown" => {
            shutdown.store(true, Ordering::Release);
            write_socket_json(stream, serde_json::json!({"ok": true}));
        }
        _ => write_socket_json(
            stream,
            serde_json::json!({"ok": false, "error": "invalid-or-replayed-request"}),
        ),
    }
}

fn monitor_thread(
    listener: UnixListener,
    state: Arc<Mutex<PeerMonitorState>>,
    proc_root: PathBuf,
    owner_pid: i32,
    controller_pid: i32,
    controller_start_ticks: u64,
    shutdown: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<()>,
) {
    {
        let mut state = state.lock().expect("peer monitor state poisoned");
        // Initial failure is sticky, but does not skip monitor setup: the live
        // sequence and final refusal remain observable and auditable.
        state.scan(&proc_root, owner_pid);
        state.monitor_ready = true;
    }
    let _ = ready.send(());
    let mut finalized = false;
    let mut next_scan = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => handle_peer_request(
                &mut stream,
                &state,
                &proc_root,
                owner_pid,
                controller_pid,
                controller_start_ticks,
                &mut finalized,
                &shutdown,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                state
                    .lock()
                    .expect("peer monitor state poisoned")
                    .mark_indeterminate(format!("monitor-accept-failed:{error}"));
                break;
            }
        }
        if !finalized && std::time::Instant::now() >= next_scan {
            state
                .lock()
                .expect("peer monitor state poisoned")
                .scan(&proc_root, owner_pid);
            next_scan = std::time::Instant::now() + std::time::Duration::from_secs(1);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Long-lived peer monitor.  Its kernel flock covers the interval between
/// scans; its final sequence acknowledgement proves progress without guessing
/// from elapsed time.
pub struct PeerSnapshotMonitor {
    socket: PathBuf,
    state: Arc<Mutex<PeerMonitorState>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    expected_monitor_pid: i32,
    // Kept by the controller so exclusion coverage survives monitor-thread
    // failure and remains held until after the ledger append.
    _exclusion: File,
}

impl PeerSnapshotMonitor {
    fn start_at(
        proc_root: PathBuf,
        exclusion_path: PathBuf,
        socket: PathBuf,
        owner_pid: i32,
        controller_pid: i32,
        controller_start_ticks: u64,
    ) -> Result<Self, SnapshotUnresolved> {
        if owner_pid <= 1 || controller_pid <= 1 || controller_start_ticks == 0 {
            return Err(SnapshotUnresolved("invalid peer-monitor owner/controller identity".into()));
        }
        if socket.exists() {
            return Err(SnapshotUnresolved(format!(
                "peer monitor control socket already exists: {}",
                socket.display()
            )));
        }
        if let Some(parent) = socket.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SnapshotUnresolved(format!("cannot create peer monitor socket directory: {e}"))
            })?;
        }
        if let Some(parent) = exclusion_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SnapshotUnresolved(format!("cannot create peer exclusion directory: {e}"))
            })?;
        }
        let exclusion = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&exclusion_path)
            .map_err(|e| SnapshotUnresolved(format!("cannot open peer exclusion lock: {e}")))?;
        let exclusion_held = flock_nb(exclusion.as_raw_fd());
        let listener = UnixListener::bind(&socket)
            .map_err(|e| SnapshotUnresolved(format!("cannot bind peer monitor socket: {e}")))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| SnapshotUnresolved(format!("cannot set peer monitor nonblocking: {e}")))?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| SnapshotUnresolved(format!("cannot protect peer monitor socket: {e}")))?;
        let monitor_pid = std::process::id() as i32;
        let state = Arc::new(Mutex::new(PeerMonitorState::new(monitor_pid, exclusion_held)));
        if !exclusion_held {
            state
                .lock()
                .expect("peer monitor state poisoned")
                .mark_indeterminate("peer-exclusion-lock-contended");
        }
        let shutdown = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let thread_state = state.clone();
        let thread_shutdown = shutdown.clone();
        let thread = std::thread::spawn(move || {
            monitor_thread(
                listener,
                thread_state,
                proc_root,
                owner_pid,
                controller_pid,
                controller_start_ticks,
                thread_shutdown,
                ready_tx,
            )
        });
        if ready_rx.recv_timeout(std::time::Duration::from_secs(600)).is_err() {
            shutdown.store(true, Ordering::Release);
            let _ = UnixStream::connect(&socket);
            let _ = thread.join();
            let _ = std::fs::remove_file(&socket);
            return Err(SnapshotUnresolved(
                "peer monitor died or exceeded the operational ceiling before initial sequence acknowledgement"
                    .into(),
            ));
        }
        Ok(Self {
            socket,
            state,
            shutdown,
            thread: Some(thread),
            expected_monitor_pid: monitor_pid,
            _exclusion: exclusion,
        })
    }

    /// Production hard-binds `/proc` and the uid-derived runtime lock.  There is
    /// intentionally no environment override for either authority input.
    pub fn start_production(
        owner_pid: i32,
        controller_start_ticks: u64,
    ) -> Result<Self, SnapshotUnresolved> {
        let uid = unsafe { libc::getuid() };
        let runtime = PathBuf::from(format!("/run/user/{uid}"));
        if !runtime.is_dir() {
            return Err(SnapshotUnresolved(format!(
                "canonical runtime directory is unavailable: {}",
                runtime.display()
            )));
        }
        Self::start_at(
            PathBuf::from("/proc"),
            runtime.join("hermit-validate-peer-snapshot.lock"),
            runtime.join(format!(
                "hermit-validate-peer-{}-{controller_start_ticks}.sock",
                std::process::id()
            )),
            owner_pid,
            std::process::id() as i32,
            controller_start_ticks,
        )
    }

    /// Explicit stop-test-only constructor.  The production call site never
    /// accepts a proc root or lock path from the caller; this seam is reached
    /// only after [`stop_test_requested`] has diverted execution away from the
    /// product/authority path.
    pub fn start_fixture(
        proc_root: &Path,
        exclusion_path: &Path,
        socket: &Path,
        owner_pid: i32,
        controller_start_ticks: u64,
    ) -> Result<Self, SnapshotUnresolved> {
        Self::start_at(
            proc_root.to_path_buf(),
            exclusion_path.to_path_buf(),
            socket.to_path_buf(),
            owner_pid,
            std::process::id() as i32,
            controller_start_ticks,
        )
    }

    fn request(&self, command: &str) -> Result<serde_json::Value, SnapshotUnresolved> {
        let mut stream = UnixStream::connect(&self.socket).map_err(|e| {
            SnapshotUnresolved(format!("peer monitor {command} connect failed: {e}"))
        })?;
        let ceiling = Some(std::time::Duration::from_secs(600));
        stream.set_read_timeout(ceiling).map_err(|e| {
            SnapshotUnresolved(format!("peer monitor {command} read timeout setup failed: {e}"))
        })?;
        stream.set_write_timeout(ceiling).map_err(|e| {
            SnapshotUnresolved(format!("peer monitor {command} write timeout setup failed: {e}"))
        })?;
        let (server_pid, server_uid, _) = peer_credentials(stream.as_raw_fd())
            .map_err(|e| SnapshotUnresolved(format!("peer monitor identity unavailable: {e}")))?;
        if server_pid != self.expected_monitor_pid || server_uid != unsafe { libc::getuid() } {
            return Err(SnapshotUnresolved("peer monitor kernel identity mismatch".into()));
        }
        stream
            .write_all(format!("{command}\n").as_bytes())
            .map_err(|e| SnapshotUnresolved(format!("peer monitor {command} write failed: {e}")))?;
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .map_err(|e| SnapshotUnresolved(format!("peer monitor {command} read failed: {e}")))?;
        serde_json::from_str(response.trim()).map_err(|e| {
            SnapshotUnresolved(format!("peer monitor {command} response malformed: {e}"))
        })
    }

    pub fn probe(&self) -> Result<u64, SnapshotUnresolved> {
        let value = self.request("probe")?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
            || value.get("protocol").and_then(serde_json::Value::as_str)
                != Some(PEER_MONITOR_PROTOCOL)
            || value.get("monitor_pid").and_then(serde_json::Value::as_i64)
                != Some(self.expected_monitor_pid as i64)
        {
            return Err(SnapshotUnresolved("peer monitor probe response refused".into()));
        }
        value
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .filter(|sequence| *sequence > 0)
            .ok_or_else(|| SnapshotUnresolved("peer monitor probe sequence missing".into()))
    }

    pub fn exclusion_held(&self) -> bool {
        lock_peer_state(&self.state).exclusion_held
    }

    pub fn mark_indeterminate(&self, detail: impl Into<String>) {
        lock_peer_state(&self.state).mark_indeterminate(detail);
    }

    fn request_final(&self) -> Result<u64, SnapshotUnresolved> {
        let value = self.request("final")?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true)
            || value.get("protocol").and_then(serde_json::Value::as_str)
                != Some(PEER_MONITOR_PROTOCOL)
            || value.get("monitor_pid").and_then(serde_json::Value::as_i64)
                != Some(self.expected_monitor_pid as i64)
        {
            return Err(SnapshotUnresolved("peer monitor final response refused".into()));
        }
        value
            .get("ack_sequence")
            .and_then(serde_json::Value::as_u64)
            .filter(|sequence| *sequence > 0)
            .ok_or_else(|| SnapshotUnresolved("peer monitor final ack missing".into()))
    }

    fn stop_thread(&mut self) {
        // Wake the nonblocking loop promptly.  The request is authenticated by
        // the kernel exactly like final/probe; failure is harmless because the
        // loop also observes `shutdown` directly.
        let _ = self.request("shutdown");
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                lock_peer_state(&self.state).mark_indeterminate("monitor-thread-panicked");
            }
        }
        let _ = std::fs::remove_file(&self.socket);
    }

    /// Take the synchronous final snapshot but keep the monitor socket and
    /// exclusion flock alive. The caller retains this guard through the ledger
    /// append, closing the post-ack/pre-receipt peer-start window.
    pub fn final_ack(&mut self) -> PeerMonitorEvidence {
        let ack = self.request_final();
        if let Err(error) = &ack {
            lock_peer_state(&self.state)
                .mark_indeterminate(format!("monitor-final-ack-missing:{error}"));
        }
        let mut state = lock_peer_state(&self.state).clone();
        let acknowledged = ack.is_ok_and(|sequence| {
            state.final_ack_sequence == Some(sequence) && state.monitor_sequence == sequence
        });
        if !self.thread.as_ref().is_some_and(|thread| !thread.is_finished()) {
            state.mark_indeterminate("monitor-died-after-final-ack");
        }
        if !acknowledged {
            state.mark_indeterminate("monitor-final-sequence-mismatch");
        }
        PeerMonitorEvidence { state, final_acknowledged: acknowledged }
    }

}

impl Drop for PeerSnapshotMonitor {
    fn drop(&mut self) {
        self.stop_thread();
    }
}

// ------------------------------------------------------------------ per-checkout invocation lock

/// A held, kernel-backed exclusive lock on this checkout's validate slot.
pub struct InvocationLock {
    _file: File,
    holder: PathBuf,
}

impl Drop for InvocationLock {
    fn drop(&mut self) {
        // Remove the descriptive record; the LOCK itself is released by the
        // kernel when the fd closes, which is the whole point of using flock.
        let _ = std::fs::remove_file(&self.holder);
    }
}

/// Outcome of trying to claim the per-checkout validate slot.
pub enum LockOutcome {
    /// Claimed. Hold the value for the lifetime of the run.
    Acquired(InvocationLock),
    /// Another validate holds it. The lines are the typed refusal message.
    Busy(Vec<String>),
    /// The lock could not be created at all (unwritable `target/`); the caller
    /// proceeds, because refusing every run over a lock-file hiccup would be a
    /// worse outage than the concurrency it guards.
    Unavailable(String),
}

fn flock_nb(fd: i32) -> bool {
    unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Claim the exclusive per-checkout validate slot, or produce a typed refusal.
///
/// SCOPE IS PER-CHECKOUT, deliberately. Box-wide exclusivity belongs to `ci-hub
/// validate-lock`; duplicating it here would give the fleet two independent
/// admission controllers that can disagree. Two validates in ONE checkout are
/// unambiguously wrong: both drive one `target/` tree and one ledger.
///
/// The refusal is IMMEDIATE (never a wait) and names the holder - but only after
/// a LIVENESS CHECK, so a record left by an earlier run can never be presented as
/// a live process. The lock is `flock`, so the "holder died with the lock held"
/// case does not exist to be reclaimed.
pub fn acquire_invocation_lock(root: &Path, profile: &str, commit: &str) -> LockOutcome {
    let dir = root.join("target/validation");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return LockOutcome::Unavailable(format!("cannot create {}: {e}", dir.display()));
    }
    let lock_path = dir.join("validate-invocation.lock");
    let holder = dir.join("validate-invocation.holder");
    let file = match std::fs::OpenOptions::new().create(true).append(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) => {
            return LockOutcome::Unavailable(format!("cannot open {}: {e}", lock_path.display()))
        }
    };
    if !flock_nb(file.as_raw_fd()) {
        let mut msg = vec![
            "another validate is already running in THIS checkout".to_string(),
            format!("checkout: {}", root.display()),
        ];
        let record = std::fs::read_to_string(&holder).unwrap_or_default();
        let holder_pid = record
            .lines()
            .find_map(|l| l.strip_prefix("pid="))
            .and_then(|v| v.trim().parse::<i32>().ok());
        match holder_pid {
            Some(pid) if unsafe { libc::kill(pid, 0) } == 0 => {
                msg.push(format!("holder (pid {pid} is LIVE):"));
                msg.extend(record.lines().map(|l| format!("  {l}")));
            }
            Some(pid) => {
                msg.push(format!(
                    "holder: the lock IS held, but the recorded pid {pid} is NOT alive, so this \
                     record is STALE and does not describe the current holder:"
                ));
                msg.extend(record.lines().map(|l| format!("  {l}")));
            }
            None => msg.push("holder: (lock held, but no holder record was readable)".into()),
        }
        msg.push(
            "this is a refusal, not a wait: two validates in one checkout share target/ and the \
             ledger, and would corrupt each other's results"
                .into(),
        );
        msg.push("wait for the holder to finish, or run in a different checkout".into());
        return LockOutcome::Busy(msg);
    }
    let record = format!(
        "pid={}\nstarted_at={}\ncommit={commit}\nprofile={profile}\ncheckout={}\n",
        std::process::id(),
        crate::utc_now(),
        root.display()
    );
    let _ = std::fs::write(&holder, record);
    LockOutcome::Acquired(InvocationLock { _file: file, holder })
}

// ------------------------------------------------------------------ box-wide live-run registry

/// One live-run record: an flock the kernel drops when this process dies.
pub struct RunRecord {
    _file: File,
    path: PathBuf,
}

impl Drop for RunRecord {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Where live-run records go. The parent workspace when one is resolvable (every
/// slot on this box resolves to the same parent, which is what makes the count
/// box-wide), else a per-uid directory.
pub fn registry_dir(parent: Option<&Path>) -> PathBuf {
    match parent {
        Some(p) => p.join("ignored").join("validate").join("runs"),
        None => {
            let uid = unsafe { libc::getuid() };
            PathBuf::from(format!("/tmp/hermit-validate-runs-{uid}"))
        }
    }
}

/// Publish this run as a live top-level validate.
///
/// Only a TOP-LEVEL, non-stop-test driver registers, which is what keeps parked
/// fixtures and nested payloads out of every peer count by construction rather
/// than by filtering.
pub fn register_run(dir: &Path, profile: &str, checkout: &Path) -> Option<RunRecord> {
    std::fs::create_dir_all(dir).ok()?;
    let pid = std::process::id();
    let path = dir.join(format!("{pid}.run"));
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    if !flock_nb(file.as_raw_fd()) {
        return None;
    }
    let _ = std::fs::write(
        &path,
        format!("pid={pid}\nprofile={profile}\ncheckout={}\n", checkout.display()),
    );
    Some(RunRecord { _file: file, path })
}

/// A peer top-level validate observed by the monitor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerCensus {
    /// Records whose owner is provably alive (their flock is still held).
    pub live: usize,
    /// Of those, the ones whose process tree BURNED CPU between two samples.
    pub cpu_active: usize,
    /// Records whose owner was dead, so the kernel had already released the lock.
    pub stale_reaped: usize,
}

/// Read the pid recorded in a `<pid>.run` file name.
fn record_pid(path: &Path) -> Option<i32> {
    path.file_stem()?.to_str()?.parse().ok()
}

/// Census the registry once. `previous` carries each peer's last CPU sample so a
/// peer can be judged ACTIVE only on an observed CPU delta.
///
/// Liveness is proven by trying the peer's own flock non-blockingly: success
/// means the KERNEL already released it, i.e. the owner is dead, and the record
/// is reaped. Failure (`EWOULDBLOCK`) means a live process still holds it. There
/// is no dead-owner state to represent.
pub fn census_peers(
    dir: &Path,
    self_pid: i32,
    previous: &mut BTreeMap<i32, f64>,
) -> PeerCensus {
    let mut c = PeerCensus::default();
    let Ok(entries) = std::fs::read_dir(dir) else { return c };
    let mut seen: Vec<i32> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("run") {
            continue;
        }
        let Some(pid) = record_pid(&path) else { continue };
        if pid == self_pid {
            continue;
        }
        let Ok(f) = File::open(&path) else { continue };
        if flock_nb(f.as_raw_fd()) {
            // Acquired => nobody holds it => the owner is gone.
            let _ = std::fs::remove_file(&path);
            c.stale_reaped += 1;
            previous.remove(&pid);
            continue;
        }
        c.live += 1;
        seen.push(pid);
        let now = tree_cpu_seconds(pid);
        // A CPU delta of a full tick is the smallest thing /proc can even show;
        // require more than that so scheduler noise is not "busy".
        if let Some(prev) = previous.insert(pid, now) {
            if now - prev > 0.05 {
                c.cpu_active += 1;
            }
        }
    }
    previous.retain(|p, _| seen.contains(p));
    c
}

/// A running peak of CPU-ACTIVE peer validates, sampled for the whole run.
///
/// A point-in-time count at start or finish misses a validate that starts and
/// ends in the middle, which is why this is a monitor and not a probe.
#[derive(Clone)]
pub struct ConcurrencyMonitor {
    peak_active: Arc<AtomicUsize>,
    peak_live: Arc<AtomicUsize>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl ConcurrencyMonitor {
    /// Start sampling `dir` every `period`. The thread is detached and exits when
    /// [`ConcurrencyMonitor::finish`] flips its stop flag.
    pub fn start(dir: PathBuf, period: std::time::Duration) -> ConcurrencyMonitor {
        let m = ConcurrencyMonitor {
            peak_active: Arc::new(AtomicUsize::new(0)),
            peak_live: Arc::new(AtomicUsize::new(0)),
            stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let (pa, pl, stop) = (m.peak_active.clone(), m.peak_live.clone(), m.stop.clone());
        let self_pid = std::process::id() as i32;
        std::thread::spawn(move || {
            let mut prev: BTreeMap<i32, f64> = BTreeMap::new();
            while !stop.load(Ordering::Relaxed) {
                let c = census_peers(&dir, self_pid, &mut prev);
                pa.fetch_max(c.cpu_active, Ordering::Relaxed);
                pl.fetch_max(c.live, Ordering::Relaxed);
                std::thread::sleep(period);
            }
        });
        m
    }

    /// Stop sampling and report `(peak_cpu_active, peak_live)`.
    pub fn finish(&self) -> (usize, usize) {
        self.stop.store(true, Ordering::Relaxed);
        (self.peak_active.load(Ordering::Relaxed), self.peak_live.load(Ordering::Relaxed))
    }
}

// ------------------------------------------------------------------ stop-test seam

/// Environment switch for the stop-path fixture (`scripts/test_validate_stop_paths.py`).
pub const STOP_TEST_ENV: &str = "HERMIT_VALIDATE_STOP_TEST_MODE";

/// Is this invocation the stop-path fixture?
pub fn stop_test_requested() -> bool {
    std::env::var(STOP_TEST_ENV).map(|v| v == "1").unwrap_or(false)
}

fn env_is(name: &str, want: &str) -> bool {
    std::env::var(name).map(|v| v == want).unwrap_or(false)
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// How long an orphaned stop-test fixture may live before self-terminating.
///
/// THE LEAK THIS CLOSES. The fixture's whole job is to park until its parent test
/// signals it, and the test spawns it with `start_new_session=True` - so if the
/// test process dies first (an assertion before the signal, a `wait` timeout, or
/// the agent being recycled), nothing ever signals the fixture and nothing in its
/// new session can. Measured on this box 2026-08-07: **6 orphaned `validate.sh
/// full` process groups, all `ppid=1`, ages 2h20m to 4h30m, each parked in `sleep
/// 1` at CPU/wall ~0.00.** Two independent exits now make that unrepresentable:
/// orphan detection (`getppid() == 1`) fires within a poll, and this deadline is
/// the backstop for the case where the fixture is reparented to something other
/// than init.
pub const STOP_TEST_MAX_SECONDS_DEFAULT: f64 = 300.0;

/// Why the stop-test fixture stopped parking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopTestExit {
    /// Fixture-only clean release after synthetic gates passed.
    SuccessReleased,
    /// `VALIDATE_STOP_TEST_EXIT_EARLY=1`: an ordinary incomplete exit, NOT a stop.
    EarlyExit,
    /// A stop signal arrived (the case the fixture exists to exercise).
    Signalled,
    /// The parent test died without signalling: this fixture is an orphan.
    Orphaned,
    /// The lifetime deadline expired.
    Deadline,
}

/// Park until a stop signal arrives, this process is orphaned, or the deadline
/// expires. `interrupted` reports the signal name once a handler has recorded one.
pub fn stop_test_park(interrupted: fn() -> Option<&'static str>) -> StopTestExit {
    if env_is("VALIDATE_STOP_TEST_EXIT_EARLY", "1") {
        return StopTestExit::EarlyExit;
    }
    let max = env_f64("VALIDATE_STOP_TEST_MAX_SECONDS", STOP_TEST_MAX_SECONDS_DEFAULT);
    let start = std::time::Instant::now();
    loop {
        if env_is("VALIDATE_STOP_TEST_SUCCESS_RELEASE", "1") {
            if let Ok(path) = std::env::var("VALIDATE_STOP_TEST_RELEASE_FILE") {
                if !path.is_empty() && Path::new(&path).is_file() {
                    return StopTestExit::SuccessReleased;
                }
            }
        }
        if interrupted().is_some() {
            return StopTestExit::Signalled;
        }
        if unsafe { libc::getppid() } == 1 {
            return StopTestExit::Orphaned;
        }
        if start.elapsed().as_secs_f64() >= max {
            return StopTestExit::Deadline;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Announce readiness exactly the way the Python test waits for it.
pub fn stop_test_announce() {
    if let Ok(p) = std::env::var("VALIDATE_STOP_TEST_PID_FILE") {
        if !p.is_empty() {
            let _ = std::fs::write(&p, format!("{}\n", std::process::id()));
        }
    }
    println!("VALIDATE_STOP_TEST_READY pid={}", std::process::id());
    let _ = std::io::stdout().flush();
}

/// The cleanup-race hook: signal readiness, then linger inside the critical
/// section while the test hammers the process with `SIGTERM`.
pub fn stop_test_cleanup_hook() {
    let Ok(p) = std::env::var("VALIDATE_STOP_TEST_CLEANUP_READY_FILE") else { return };
    if p.is_empty() {
        return;
    }
    let _ = std::fs::write(&p, format!("{}\n", std::process::id()));
    let delay = env_f64("VALIDATE_STOP_TEST_CLEANUP_DELAY_SECONDS", 0.5);
    std::thread::sleep(std::time::Duration::from_secs_f64(delay));
}

/// Make the evidence-commit window signal-atomic.
///
/// Cleanup is where the single ledger append happens. A second stop signal must
/// not abort it between teardown and that append, or a run would leave no record
/// of having run at all. `SIG_IGN` for the whole window is what
/// `trap '' INT TERM HUP` bought the bash (validate.sh:1817).
pub fn enter_cleanup_critical_section() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
}

// ------------------------------------------------------------------ self-test

#[allow(clippy::too_many_arguments)]
fn write_fake_process(
    root: &Path,
    pid: i32,
    ppid: i32,
    pgid: i32,
    start_ticks: u64,
    argv: &[&str],
    cgroup: &str,
    flags: u64,
) -> Result<(), String> {
    let process = root.join(pid.to_string());
    std::fs::create_dir_all(&process).map_err(|e| format!("fake proc {pid}: {e}"))?;
    let mut fields = vec!["0".to_string(); 20];
    fields[0] = "S".into();
    fields[1] = ppid.to_string();
    fields[2] = pgid.to_string();
    fields[6] = flags.to_string();
    fields[19] = start_ticks.to_string();
    std::fs::write(process.join("stat"), format!("{pid} (fixture) {}\n", fields.join(" ")))
        .map_err(|e| format!("fake proc {pid} stat: {e}"))?;
    let mut cmdline = argv.join("\0").into_bytes();
    if !cmdline.is_empty() {
        cmdline.push(0);
    }
    std::fs::write(process.join("cmdline"), cmdline)
        .map_err(|e| format!("fake proc {pid} cmdline: {e}"))?;
    std::fs::write(process.join("cgroup"), format!("0::{cgroup}\n"))
        .map_err(|e| format!("fake proc {pid} cgroup: {e}"))?;
    Ok(())
}

/// Inert two-sided brackets for everything in this module.
///
/// Nothing here runs a gate, publishes a label, writes the real ledger, or claims
/// a lock any other process could be waiting on: the lock and registry brackets
/// operate inside a private temporary directory, and the concurrency bracket
/// registers a FAKE peer record held by a short-lived child of this process, which
/// cannot authorize anything.
pub fn self_test() -> Result<String, String> {
    // ---- environmental classification: qualifying cases must be ACCEPTED ----
    let accept: &[(&str, &str)] = &[
        (
            "bpfjailer-banner",
            "test tests::x ... An action was blocked on this server based on a security policy!\n\
             Enforcer: FS, Reason: FILE_OPEN\nFAILED",
        ),
        (
            "bpfjailer-banner",
            "[e2e.metadata] Bunnylol `scuba bpfjailer_enforce` for more details",
        ),
        (
            "toolchain-eperm",
            "/usr/include/signal.h:311:11: fatal error: /usr/lib/gcc/x86_64-redhat-linux/11/include/stddef.h: Operation not permitted",
        ),
        (
            "toolchain-eperm",
            "error: could not write output to /w/target/debug/deps/x.rcgu.o: Operation not permitted",
        ),
        (
            "toolchain-eperm",
            "Fatal error: can't create CMakeFiles/ebl_pic.dir/x.c.o: Operation not permitted",
        ),
        (
            "toolchain-eperm",
            "cc: fatal error: cannot execute 'as': execvp: Operation not permitted",
        ),
        (
            "third-party-build",
            "error: failed to run custom build command for `reverie-dbi v0.1.0`",
        ),
        // NEW class 1: the proxy/DNS failure measured in
        // /tmp/hermit-validate.WUrHlJ.log, which the old regex did NOT catch.
        (
            "proxy-egress",
            "Lookup error: git ls-remote https://github.com/rrnewton/reverie.git refs/heads/main \
             failed: fatal: unable to access 'https://github.com/rrnewton/reverie.git/': Could not \
             resolve proxy: fwdproxy",
        ),
        (
            "proxy-egress",
            "fatal: unable to access 'https://github.com/rr-debugger/rr/': CONNECT tunnel failed, \
             response 403",
        ),
        // NEW class 2: a banner-less git FS denial in a /tmp fixture repository.
        (
            "vcs-fs-denial",
            "fatal: could not create leading directories of \
             /tmp/check-reverie-pin-stale-lock-1/.git/config: Operation not permitted",
        ),
        (
            "vcs-fs-denial",
            "error: chmod on /tmp/check-reverie-pin-x/.git/config.lock failed: Permission denied",
        ),
    ];
    let mut accepted = 0usize;
    for (want, text) in accept {
        match environmental_block_class(text) {
            Some(got) if got == *want => accepted += 1,
            other => {
                return Err(format!(
                    "environmental: {text:?} must classify as {want}, got {other:?}"
                ))
            }
        }
    }
    // ---- and violating cases must be REFUSED (else the retry loop would eat
    // every genuine product red and report it as host flake) ----
    let refuse: &[&str] = &[
        // Real guest EPERM output. These are the exact shapes validate.sh's
        // comment names as must-not-match.
        "2026-08-03T14:12:23Z INFO detcore::syscalls::memory: DETLOG [dtid 2800245] madvise advice \
         100 rejected with -1 EPERM (Operation not permitted)",
        "kcmp-eperm: kcmp returned EPERM (Operation not permitted) as expected",
        "context: Mount { .. }: Operation not permitted",
        // Real product failures.
        "test result: FAILED. 9 passed; 1 failed; 0 ignored",
        "thread 'tests::scheduler_is_deterministic' panicked at detcore/src/scheduler.rs:100:5:\n\
         assertion `left == right` failed",
        "error[E0308]: mismatched types",
        // A test that merely PRINTS the words must not be excused.
        "test permission_denied_is_reported ... ok",
        "guest wrote: permission denied",
        "",
    ];
    let mut refused = 0usize;
    for text in refuse {
        if let Some(class) = environmental_block_class(text) {
            return Err(format!(
                "environmental: {text:?} is a PRODUCT failure but classified as {class}"
            ));
        }
        refused += 1;
    }
    // ---- node-detail extraction, both directions ----
    let log = "[build.workspace] ✗ FAIL   Workspace build (12s, exit 101)\n\
               [build.workspace] ----- detail -----\n\
               [build.workspace] error: could not write output to /w/x.o: Operation not permitted\n\
               [build.workspace] ----- end detail -----\n\
               [lint.clippy] ✓ PASS   Clippy (3s)\n";
    let detail = extract_node_detail(log, "build.workspace")
        .ok_or("extract: the failed node's detail region must be found")?;
    if !detail.contains("could not write output to") || detail.contains("[build.workspace]") {
        return Err(format!("extract: prefix must be stripped, got {detail:?}"));
    }
    if environmental_block_class(&detail) != Some("toolchain-eperm") {
        return Err("extract+classify: the extracted region must classify environmental".into());
    }
    if extract_node_detail(log, "lint.clippy").is_some() {
        return Err("extract: a node with no detail region must yield None".into());
    }

    // ---- the retry verdict, all THREE states, end to end ----
    //
    // Classification is only a hypothesis; the retry is the experiment. Bracket
    // it in both directions with the SAME classifier, so neither state can be
    // reached by the reporter alone. A change that only ever confirms is
    // decorative — the refuting direction below is what makes this load-bearing.
    //
    // Direction 1 (CONFIRMED): a genuine jail denial. Classifies, and the re-run
    // at the same commit passes.
    let env_log = "[gate.reverie_pin] ✗ FAIL   Reverie pin (4s, exit 128)\n\
                   [gate.reverie_pin] ----- detail -----\n\
                   [gate.reverie_pin] Enforcer: FS, Reason: FILE_OPEN\n\
                   [gate.reverie_pin] fatal: could not lock config file .git/config\n\
                   [gate.reverie_pin] ----- end detail -----\n";
    let env_detail = extract_node_detail(env_log, "gate.reverie_pin")
        .ok_or("retry-verdict: the blocked node's detail region must be found")?;
    let env_class = environmental_block_class(&env_detail)
        .ok_or("retry-verdict: a jail denial must classify environmental")?;
    if EnvBlockVerdict::settle(true, true) != EnvBlockVerdict::Confirmed
        || !EnvBlockVerdict::settle(true, true).is_environmental()
    {
        return Err(format!(
            "retry-verdict: {env_class} that PASSED on re-run must be CONFIRMED environmental"
        ));
    }

    // Direction 2 (REFUTED): a real product failure that merely COINCIDED with a
    // bad slot. The detail region carries both the jail banner and a genuine test
    // failure, so the classifier — correctly, since it binds by colocation —
    // still says environmental. The failing re-run is what overrules it.
    let coincident = "[test.detcore] ✗ FAIL   Detcore tests (61s, exit 101)\n\
                      [test.detcore] ----- detail -----\n\
                      [test.detcore] Enforcer: FS, Reason: FILE_OPEN\n\
                      [test.detcore] ---- tests_time::clock_monotonic stdout ----\n\
                      [test.detcore] assertion `left == right` failed\n\
                      [test.detcore] test result: FAILED. 412 passed; 1 failed\n\
                      [test.detcore] ----- end detail -----\n";
    let coincident_detail = extract_node_detail(coincident, "test.detcore")
        .ok_or("retry-verdict: the coincident node's detail region must be found")?;
    if environmental_block_class(&coincident_detail).is_none() {
        return Err(
            "retry-verdict: the coincidence fixture must still CLASSIFY — the point is that a \
             classification is a hypothesis the retry can overrule, not that the classifier \
             detects product failures on its own"
                .into(),
        );
    }
    let refuted = EnvBlockVerdict::settle(true, false);
    if refuted != EnvBlockVerdict::Refuted || !refuted.is_settled_failure() {
        return Err(
            "retry-verdict: a classified node that FAILED AGAIN on re-run must be REFUTED — the \
             retry settled the question against the class"
                .into(),
        );
    }
    if refuted.is_environmental() {
        return Err("retry-verdict: a REFUTED node must not keep its environmental excuse".into());
    }

    // REFUTED is not a synonym for "product bug", and saying so would be the same
    // overclaim in the other direction: a PERSISTENT host denial reproduces on
    // re-run exactly as a real defect does. The newest attempt's signature is
    // what separates them, so bracket all three shapes — only the first is a
    // positive product-failure claim.
    let mut shapes = 0usize;
    for (latest, want, product) in [
        (None, RefutedShape::BannerGone, true),
        (Some("bpfjailer-banner"), RefutedShape::Persistent, false),
        (Some("proxy-egress"), RefutedShape::SignatureChanged, false),
    ] {
        let got = RefutedShape::of("bpfjailer-banner", latest);
        if got != want || got.is_product_failure() != product {
            return Err(format!(
                "retry-verdict: newest attempt {latest:?} against bpfjailer-banner must be \
                 {want:?} with is_product_failure={product}, got {got:?}"
            ));
        }
        shapes += 1;
    }
    if shapes != 3 {
        return Err("retry-verdict: all three REFUTED shapes must be bracketed".into());
    }

    // Direction 3 (UNCONFIRMED): never re-run. It must read as NEITHER of the
    // other two — this is the state that would otherwise silently keep its
    // excuse, and it is reachable regardless of what the node's last outcome was.
    for last_outcome_passed in [false, true] {
        let u = EnvBlockVerdict::settle(false, last_outcome_passed);
        if u != EnvBlockVerdict::Unconfirmed || u.is_environmental() || u.is_settled_failure() {
            return Err(format!(
                "retry-verdict: a classified node that was NEVER re-run (passed={last_outcome_passed}) \
                 must be UNCONFIRMED and must read as neither environmental nor product failure"
            ));
        }
    }

    // ---- CPU-vs-wall hints, both directions ----
    if cpu_wall_hint(5.0, 600.0, 316) != Some("low CPU vs wall — mostly waiting/blocked, not compute-bound") {
        return Err("cpu/wall: 5s CPU over 600s wall must read as blocked".into());
    }
    if cpu_wall_hint(600.0, 600.0, 316) != Some("~1 core busy — single-threaded or possibly spinning") {
        return Err("cpu/wall: 1.0x on 316 cores must read as ~1 core busy".into());
    }
    if cpu_wall_hint(4000.0, 600.0, 316).is_some() {
        return Err("cpu/wall: a genuinely parallel run must get NO hint".into());
    }
    if cpu_wall_hint(0.0, 10.0, 316).is_some() {
        return Err("cpu/wall: a <30s run is too short for either shape to mean anything".into());
    }
    let line = cpu_wall_line(|s| format!("{}s", s.round() as i64), 600.0, 30.0, 10.0, 316);
    if !line.contains("CPU/wall 0.1x across 316 cores") || !line.contains("mostly waiting") {
        return Err(format!("cpu/wall: line must carry ratio AND hint, got {line:?}"));
    }

    // ---- process CPU accounting must be live, not a stub ----
    let (u, s) = process_cpu_seconds();
    if u + s <= 0.0 {
        return Err("cpu: getrusage reported zero CPU for a process that has run".into());
    }
    let own = tree_cpu_seconds(std::process::id() as i32);
    if own <= 0.0 {
        return Err("cpu: /proc tree accounting reported zero for our own live tree".into());
    }

    // ---- nesting: ancestry is what binds it, not the env var ----
    let saved = std::env::var(ACTIVE_ENV).ok();
    std::env::set_var(ACTIVE_ENV, "1");
    // pid 1 IS an ancestor of everything, so this is the qualifying positive.
    let positive = detect_nesting();
    if !positive.nested || positive.outer_pid != Some(1) {
        return Err(format!("nesting: pid 1 must be seen as an ancestor, got {positive:?}"));
    }
    // A pid that is NOT in our ancestry is a STALE marker, not nesting. (2^22 is
    // above the default pid_max, so it cannot name a live process.)
    std::env::set_var(ACTIVE_ENV, "4194303");
    let negative = detect_nesting();
    if negative.nested || negative.stale_marker != Some(4194303) {
        return Err(format!("nesting: a non-ancestor marker must be STALE, got {negative:?}"));
    }
    std::env::set_var(ACTIVE_ENV, "not-a-pid");
    if detect_nesting().nested {
        return Err("nesting: a malformed marker must not assert nesting".into());
    }
    std::env::remove_var(ACTIVE_ENV);
    if detect_nesting().nested {
        return Err("nesting: an absent marker must not assert nesting".into());
    }
    match saved {
        Some(v) => std::env::set_var(ACTIVE_ENV, v),
        None => std::env::remove_var(ACTIVE_ENV),
    }

    // ---- identity-safe /proc peer snapshots -------------------------------
    let peer_sandbox = std::env::temp_dir().join(format!(
        "validate-peer-snapshot-selftest-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&peer_sandbox);
    let peer_proc = peer_sandbox.join("proc");
    std::fs::create_dir_all(&peer_proc).map_err(|e| format!("peer fixture: {e}"))?;
    write_fake_process(&peer_proc, 1, 0, 1, 1, &[], "/", PF_KTHREAD)?;
    write_fake_process(
        &peer_proc,
        10,
        1,
        10,
        10,
        &["ci-hub", "validate-lock"],
        "/user.slice/user@1000.service/app.slice/validate-X.service",
        0,
    )?;
    // Realistic same-service case: a reparented validation nested under a
    // safe-ci scope still belongs to validate-X.service and is SELF.
    write_fake_process(
        &peer_proc,
        20,
        1,
        20,
        20,
        &["/slot/scripts/validate.rs", "full"],
        "/user.slice/user@1000.service/app.slice/validate-X.service/safe-ci-A.scope",
        0,
    )?;
    // Different application service is an external peer even under the same
    // user manager.
    write_fake_process(
        &peer_proc,
        40,
        1,
        40,
        40,
        &["/other/scripts/validate.rs", "full"],
        "/user.slice/user@1000.service/app.slice/validate-Z.service/safe-ci-B.scope",
        0,
    )?;
    let peer_snapshot = collect_peer_snapshot(&peer_proc, 10)?;
    if peer_snapshot.owner.systemd_unit != "validate-X.service"
        || peer_snapshot.same_service.len() != 1
        || peer_snapshot.same_service[0].classification != Some("reparented-same-service-self")
        || peer_snapshot.peers.len() != 1
        || peer_snapshot.peers[0].systemd_unit != "validate-Z.service"
    {
        return Err(format!("
            peer identity: expected 1 same-service self + 1 different-unit peer, got \
             {peer_snapshot:?}"
        ));
    }

    // Genuine exit race: the PID disappears from the numeric directory set
    // after enumeration, so absence is proved and must NOT become indeterminate.
    let exit_proc = peer_sandbox.join("exit-race-proc");
    std::fs::create_dir_all(&exit_proc).map_err(|e| format!("exit fixture: {e}"))?;
    for pid in [1, 10, 20, 40] {
        let source = peer_proc.join(pid.to_string());
        let target = exit_proc.join(pid.to_string());
        std::fs::create_dir_all(&target).map_err(|e| format!("exit fixture {pid}: {e}"))?;
        for name in ["stat", "cmdline", "cgroup"] {
            std::fs::copy(source.join(name), target.join(name))
                .map_err(|e| format!("exit fixture {pid}/{name}: {e}"))?;
        }
    }
    let exit_snapshot = collect_peer_snapshot_with(
        &exit_proc,
        10,
        |root, pid| {
            if pid == 40 {
                std::fs::remove_dir_all(root.join("40"))?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "planted genuine exit race",
                ));
            }
            read_process_record(root, pid)
        },
        read_process_start_ticks,
    )?;
    if !exit_snapshot.peers.is_empty() {
        return Err(format!("peer exit race: vanished PID remained a peer: {exit_snapshot:?}"));
    }

    // Genuine exit race, zombie form: stat initially identified a live
    // userspace process, then an empty cmdline coincided with the SAME process
    // reaching Z. That is proved exit, not unknown evidence. A live empty-
    // cmdline process and PID reuse remain refused.
    write_fake_process(
        &exit_proc,
        50,
        1,
        50,
        50,
        &[],
        "/user.slice/user@1000.service/app.slice/validate-Z.service",
        0,
    )?;
    let zombie_stat = std::fs::read_to_string(exit_proc.join("50/stat"))
        .map_err(|e| format!("zombie fixture stat read: {e}"))?
        .replacen(") S ", ") Z ", 1);
    std::fs::write(exit_proc.join("50/stat"), zombie_stat)
        .map_err(|e| format!("zombie fixture stat: {e}"))?;
    if confirm_exited_after_empty_cmdline(&exit_proc, 50, 'S', 50)
        .map_err(|e| format!("zombie exit race was refused: {e}"))?
        != 'Z'
    {
        return Err("zombie exit race did not preserve the terminal state".into());
    }
    if read_process_record(&exit_proc, 50)
        .map_err(|e| format!("terminal zombie evidence was refused: {e}"))?
        .state
        != 'Z'
    {
        return Err("terminal zombie record did not remain terminal".into());
    }
    let live_stat = std::fs::read_to_string(exit_proc.join("50/stat"))
        .map_err(|e| format!("live empty-cmdline fixture stat read: {e}"))?
        .replacen(") Z ", ") S ", 1);
    std::fs::write(exit_proc.join("50/stat"), live_stat)
        .map_err(|e| format!("live empty-cmdline fixture stat: {e}"))?;
    if confirm_exited_after_empty_cmdline(&exit_proc, 50, 'S', 50).is_ok() {
        return Err("live empty-cmdline userspace process was laundered as exited".into());
    }
    write_fake_process(
        &exit_proc,
        50,
        1,
        50,
        51,
        &[],
        "/user.slice/user@1000.service/app.slice/validate-Z.service",
        0,
    )?;
    if confirm_exited_after_empty_cmdline(&exit_proc, 50, 'S', 50).is_ok() {
        return Err("reused PID with an empty cmdline was laundered as exited".into());
    }
    let reused_zombie_stat = std::fs::read_to_string(exit_proc.join("50/stat"))
        .map_err(|e| format!("initial-z reuse fixture stat read: {e}"))?
        .replacen(") S ", ") Z ", 1);
    std::fs::write(exit_proc.join("50/stat"), reused_zombie_stat)
        .map_err(|e| format!("initial-z reuse fixture stat: {e}"))?;
    if confirm_exited_after_empty_cmdline(&exit_proc, 50, 'Z', 50).is_ok() {
        return Err("initial-terminal PID reuse was laundered as the original exit".into());
    }

    // Persistent unreadable numeric PID: UNKNOWN, not absent.  A later clean
    // snapshot cannot recover the sticky state to authority.
    let unresolved = collect_peer_snapshot_with(
        &peer_proc,
        10,
        |root, pid| {
            if pid == 40 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "planted unreadable numeric PID",
                ));
            }
            read_process_record(root, pid)
        },
        read_process_start_ticks,
    )
    .expect_err("persistent unreadable process must be unresolved");
    if !unresolved.0.contains("PID 40") || !unresolved.0.contains("unreadable or malformed") {
        return Err(format!("peer unresolved: wrong refusal detail: {unresolved}"));
    }
    let mut sticky = PeerMonitorState::new(std::process::id() as i32, true);
    sticky.mark_indeterminate(format!("snapshot-unresolved:{unresolved}"));
    sticky.merge_snapshot(peer_snapshot.clone());
    if !sticky.indeterminate || sticky.indeterminate_detail.is_none() {
        return Err("peer unresolved: later readable scan recovered sticky authority".into());
    }

    // ---- liveness protocol: sequence/final ack + SO_PEERCRED ---------------
    let controller_pid = std::process::id() as i32;
    let controller_start = read_process_start_ticks(Path::new("/proc"), controller_pid)
        .map_err(|e| format!("peer monitor controller identity: {e}"))?;
    // Remove the true external peer so the accepted final proves a zero.
    std::fs::remove_dir_all(peer_proc.join("40"))
        .map_err(|e| format!("peer monitor fixture remove external: {e}"))?;
    let mut peer_monitor = PeerSnapshotMonitor::start_at(
        peer_proc.clone(),
        peer_sandbox.join("peer-exclusion.lock"),
        peer_sandbox.join("peer-monitor.sock"),
        10,
        controller_pid,
        controller_start,
    )?;
    if peer_monitor.probe()? == 0 {
        return Err("peer monitor: initial sequence acknowledgement was zero".into());
    }
    // A separate process has the same uid but is not the expected controller.
    // Its final request must be refused without consuming the sequence.
    let attacker = std::process::Command::new("python3")
        .arg("-c")
        .arg(
            "import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); \
             s.sendall(b'final\\n'); print(s.recv(4096).decode()); s.close()",
        )
        .arg(&peer_monitor.socket)
        .output()
        .map_err(|e| format!("peer monitor same-uid negative: {e}"))?;
    let attacker_response = String::from_utf8_lossy(&attacker.stdout);
    if !attacker.status.success() || !attacker_response.contains("unauthorized-controller") {
        return Err(format!(
            "peer monitor: same-uid non-owner was not explicitly refused: status={} out={attacker_response:?} err={:?}",
            attacker.status,
            String::from_utf8_lossy(&attacker.stderr)
        ));
    }
    if peer_monitor
        .state
        .lock()
        .map_err(|_| "peer monitor state poisoned".to_string())?
        .final_ack_sequence
        .is_some()
    {
        return Err("peer monitor: unauthorized caller consumed final sequence".into());
    }
    let ack = peer_monitor.request_final()?;
    let accepted_state = peer_monitor
        .state
        .lock()
        .map_err(|_| "peer monitor state poisoned".to_string())?
        .clone();
    if accepted_state.final_ack_sequence != Some(ack) || !accepted_state.qualifies_exclusivity() {
        return Err(format!(
            "peer monitor: legitimate final did not qualify: {accepted_state:?}"
        ));
    }
    let before_replay = (
        accepted_state.monitor_sequence,
        accepted_state.final_ack_sequence,
    );
    if peer_monitor.request_final().is_ok() {
        return Err("peer monitor: replayed final request was accepted".into());
    }
    let after_replay = peer_monitor
        .state
        .lock()
        .map_err(|_| "peer monitor state poisoned".to_string())?
        .clone();
    if (after_replay.monitor_sequence, after_replay.final_ack_sequence) != before_replay {
        return Err("peer monitor: replay mutated the acknowledged sequence".into());
    }
    peer_monitor.stop_thread();
    drop(peer_monitor);
    let _ = std::fs::remove_dir_all(&peer_sandbox);

    // ---- the invocation lock, BOTH directions, in a private sandbox ----
    //
    // Both directions matter equally: a guard that refuses the sequential case
    // too is a worse outage than the concurrency it prevents.
    let sandbox = std::env::temp_dir().join(format!("validate-lock-selftest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(&sandbox).map_err(|e| format!("lock bracket: {e}"))?;
    let mut lock_accept = 0usize;
    let mut lock_refuse = 0usize;
    {
        let first = match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
            LockOutcome::Acquired(l) => {
                lock_accept += 1;
                l
            }
            LockOutcome::Busy(m) => return Err(format!("lock: a free slot must be granted: {m:?}")),
            LockOutcome::Unavailable(e) => return Err(format!("lock: sandbox unusable: {e}")),
        };
        // NEGATIVE: a concurrent claim, from a real second fd, must be REFUSED and
        // must name the live holder.
        match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
            LockOutcome::Busy(msg) => {
                lock_refuse += 1;
                let joined = msg.join("\n");
                if !joined.contains("is LIVE") || !joined.contains(&std::process::id().to_string()) {
                    return Err(format!("lock: refusal must name the LIVE holder pid: {joined}"));
                }
            }
            _ => return Err("lock: a second concurrent claim MUST be refused".into()),
        }
        drop(first);
    }
    // POSITIVE, and the one that matters most: after the holder releases, the
    // NEXT sequential run must succeed.
    match acquire_invocation_lock(&sandbox, "self-test", "0000000") {
        LockOutcome::Acquired(l) => {
            lock_accept += 1;
            drop(l);
        }
        LockOutcome::Busy(m) => {
            return Err(format!("lock: a SEQUENTIAL re-claim must succeed, got refusal: {m:?}"))
        }
        LockOutcome::Unavailable(e) => return Err(format!("lock: sandbox unusable: {e}")),
    }

    // ---- registry census: live vs stale vs CPU-active ----
    let reg = sandbox.join("runs");
    std::fs::create_dir_all(&reg).map_err(|e| format!("registry bracket: {e}"))?;
    let mut prev: BTreeMap<i32, f64> = BTreeMap::new();
    // A STALE record: a plausible file whose owner never held the lock. The
    // census must reap it rather than counting a peer that does not exist -
    // this is the exact fiction that put `concurrent_validates: 20` in the ledger.
    std::fs::write(reg.join("4194302.run"), "pid=4194302\n").map_err(|e| format!("{e}"))?;
    let c = census_peers(&reg, std::process::id() as i32, &mut prev);
    if c.live != 0 || c.stale_reaped != 1 || reg.join("4194302.run").exists() {
        return Err(format!("registry: a dead owner's record must be reaped, got {c:?}"));
    }
    // A LIVE record: registered by this process, then observed from a census that
    // does NOT exclude us, so the liveness path is exercised for real.
    let held = register_run(&reg, "self-test", &sandbox)
        .ok_or("registry: registering a free slot must succeed")?;
    let c = census_peers(&reg, -1, &mut prev);
    if c.live != 1 || c.stale_reaped != 0 {
        return Err(format!("registry: a live holder must be counted live, got {c:?}"));
    }
    // First sighting has no previous sample, so it can never be "active" yet:
    // activity requires an OBSERVED CPU delta, which is what stops a parked
    // fixture from counting like a 22-core validate.
    if c.cpu_active != 0 {
        return Err(format!("registry: a first sighting cannot be CPU-active, got {c:?}"));
    }
    // Now burn measurable CPU and re-census: the same peer must flip to active.
    let mut spin = 0u64;
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_millis() < 150 {
        spin = spin.wrapping_add(1);
    }
    std::hint::black_box(spin);
    let c2 = census_peers(&reg, -1, &mut prev);
    if c2.live != 1 || c2.cpu_active != 1 {
        return Err(format!("registry: a CPU-burning peer must read active, got {c2:?}"));
    }
    drop(held);
    // And once it is gone the count returns to zero: the guard is not sticky.
    let c3 = census_peers(&reg, -1, &mut prev);
    if c3.live != 0 {
        return Err(format!("registry: a finished peer must stop counting, got {c3:?}"));
    }
    let _ = std::fs::remove_dir_all(&sandbox);

    Ok(format!(
        "runtime: environmental classifier bracketed {accepted} accept / {refused} refuse \
         (incl. the 2 NEW classes), node-detail extraction 1 hit / 1 miss, retry verdict \
         1 CONFIRMED / 1 REFUTED (coincident banner + real test failure) / 2 UNCONFIRMED, \
         refuted shape 1 banner-gone (the only product-failure claim) / 1 persistent / \
         1 signature-changed, \
         CPU-vs-wall hints \
         2 fire / 2 silent, nesting 1 ancestor-accept / 3 refuse, invocation lock \
         {lock_accept} accept (incl. the sequential re-claim) / {lock_refuse} concurrent-refuse, \
         registry census 1 live / 1 stale-reaped / 1 cpu-active, peer identity 1 same-service \
         self / 1 different-unit peer, peer scan 2 genuine-exit accepts (vanish + zombie) / \
         1 live-empty refuse / 2 PID-reuse refuses (live + initial-terminal) / \
         1 persistent-unreadable sticky-refuse, monitor 1 legitimate final-ack / \
         1 same-uid non-owner refuse / 1 replay-refuse"
    ))
}
