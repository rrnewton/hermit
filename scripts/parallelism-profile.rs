#!/usr/bin/env -S rust-script --force
//! Copyright (c) Meta Platforms, Inc. and affiliates.
//! All rights reserved.
//!
//! This source code is licensed under the BSD-style license found in the
//! LICENSE file in the root directory of this source tree.
//!
//! ```cargo
//! [package]
//! edition = "2024"
//!
//! [dependencies]
//! serde_json = "1"
//! ```
//!
//! Project how much parallelism a Hermit run could have had, from the profile
//! that `HERMIT_PARALLELISM_PROFILE=<file>` makes Detcore write
//! (`detcore/src/parallelism_probe.rs`). Measurement only: nothing here
//! executes anything in parallel.
//!
//! THE GRAPH. Every committed scheduler turn is a node. Its cost is the CPU the
//! thread used from that commit to its next one (host CPU from
//! `/proc/<tid>/schedstat`, or with `--cost ltime` the thread's logical time).
//! A thread's first commit also gets a birth node holding the CPU it used
//! before that commit. Edges are
//!   * program order: each node follows the previous node of its thread;
//!   * spawn: a child's birth node follows the creator's node that cloned
//!     it, named by the profile's `spawn` line (in a profile without them,
//!     inferred from the creator's `ParentContinue` or `BlockingVfork` turn);
//!   * conflicting checkouts, in commit order: a node that checks a resource
//!     out follows the earlier nodes whose checkout of it conflicts (any pair
//!     in which one side writes);
//!   * the memory domain: every node implicitly holds its address space
//!     (`mm`) read-write for its whole duration, so threads sharing memory
//!     never overlap.
//! Work is the sum of node costs; span is the longest path; work/span is the
//! speedup an unbounded number of CPUs could reach under the model.
//!
//! TWO MODELS. `lock` holds every checked-out resource for the whole node, as
//! a lock held for the time slice would. `effect` treats a turn's checkout as
//! covering only the syscall at the start of the node and the rest as private
//! computation, which is how DetTrace overlapped processes: syscalls ordered,
//! computation in parallel. Under `effect` a syscall costs nothing, so the
//! projection is an upper bound for that model. In both models, the critical
//! path's node costs add up to the span: a node that waited only for another
//! node's start puts none of that node's cost on the path.
//!
//! WHAT IT CANNOT SEE. Syscalls that run in a thread's exclusive slot without
//! taking a turn are not covered by any declared resource; the profile lists
//! them per node (`segment` lines). `--untracked-fs` makes a node whose
//! untracked syscalls touch the file system hold, for the whole node in both
//! models (the syscall may come anywhere in it), the paths they name: read for
//! lookups, write for mutations, including both ends of a rename or link.
//! Every path access also reads one global `FS` resource, which a mutation the
//! probe could not resolve to a path writes, so an unresolved mutation
//! conflicts with every path. Directory listings are not keyed. A profile that
//! did not record paths uses `FS` alone. A node that waited for a child without
//! a turn (WNOHANG) holds its child table, and a node that signalled without
//! one a global `Signals` resource, bracketing those holes too.
//!
//! COARSE RESOURCES. An `InternalIOPolling` retry (a pipe read that would
//! block, a polled wait4) names no object, so as declared every such retry
//! conflicts with every other. `--refine` maps a wait4/waitid retry to the
//! caller's child table and a descriptor retry to the object its descriptor
//! names when the profile recorded it, which is what an identity-carrying
//! resource would allow. It also makes an open that may write, create or
//! truncate hold its `Path` for writing: Detcore declares every open's path
//! read. Compare both to see what the missing identity costs.
//!
//! Usage:
//!   parallelism-profile.rs <profile.jsonl> [--run N] [--cost cpu|ltime]
//!       [--untracked-fs] [--refine] [--ablate K]... [--top N] [--json <out>]
//!       [--path]
//! `--run N` picks the Nth run (0-based) in a file several runs appended to;
//! the default is the last. `--ablate K` drops resources whose kind or key is
//! K before computing (repeatable), for what-if analysis.

#[path = "lib/rust_script_prelude.rs"]
mod rust_script_prelude;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::process::ExitCode;

use serde_json::Value;
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Perm {
    R,
    W,
}

#[derive(Clone, Debug)]
struct Node {
    tid: i64,
    turn: Option<i64>,
    cost: u64,
    /// (kind, key, perm, held for the whole node in both models) after
    /// mapping to analysis resources.
    res: Vec<(String, String, Perm, bool)>,
    mm: Option<String>,
    /// Extra predecessor (spawn edge).
    spawn_pred: Option<usize>,
    label: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Model {
    Lock,
    Effect,
}

struct Options {
    path: String,
    run: Option<usize>,
    cost_ltime: bool,
    untracked_fs: bool,
    refine: bool,
    ablate: Vec<String>,
    top: usize,
    json_out: Option<String>,
    print_path: bool,
    print_threads: bool,
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: parallelism-profile.rs <profile.jsonl> [--run N] [--cost cpu|ltime] \
         [--untracked-fs] [--refine] [--ablate K]... [--top N] [--json <out>] [--path] [--threads]"
    );
    ExitCode::from(2)
}

fn parse_args() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut opts = Options {
        path: String::new(),
        run: None,
        cost_ltime: false,
        untracked_fs: false,
        refine: false,
        ablate: Vec::new(),
        top: 8,
        json_out: None,
        print_path: false,
        print_threads: false,
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--run" => {
                let v = args.next().ok_or("--run needs a value")?;
                opts.run = Some(v.parse().map_err(|e| format!("--run {v}: {e}"))?);
            }
            "--cost" => match args.next().as_deref() {
                Some("cpu") => opts.cost_ltime = false,
                Some("ltime") => opts.cost_ltime = true,
                other => return Err(format!("--cost must be cpu or ltime, got {other:?}")),
            },
            "--untracked-fs" => opts.untracked_fs = true,
            "--refine" => opts.refine = true,
            "--ablate" => opts
                .ablate
                .push(args.next().ok_or("--ablate needs a value")?),
            "--top" => {
                let v = args.next().ok_or("--top needs a value")?;
                opts.top = v.parse().map_err(|e| format!("--top {v}: {e}"))?;
            }
            "--json" => opts.json_out = Some(args.next().ok_or("--json needs a value")?),
            "--path" => opts.print_path = true,
            "--threads" => opts.print_threads = true,
            "-h" | "--help" => return Err(String::new()),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            s => {
                if !opts.path.is_empty() {
                    return Err(format!("unexpected argument {s}"));
                }
                opts.path = s.to_string();
            }
        }
    }
    if opts.path.is_empty() {
        return Err("missing profile path".into());
    }
    Ok(opts)
}

/// The variant name and payload of a serialized `ResourceID`.
fn variant(v: &Value) -> (String, Value) {
    match v {
        Value::String(s) => (s.clone(), Value::Null),
        Value::Object(m) if m.len() == 1 => {
            let (k, v) = m.iter().next().unwrap();
            (k.clone(), v.clone())
        }
        other => (other.to_string(), Value::Null),
    }
}

fn perm_of(s: &str) -> Perm {
    if s == "R" { Perm::R } else { Perm::W }
}

fn mm_key(v: &Value) -> Option<String> {
    let c = v.get("creator")?.as_i64()?;
    let g = v.get("generation")?.as_i64()?;
    Some(format!("{c}.{g}"))
}

/// Untracked syscalls that mutate the file-system namespace or metadata.
const FS_MUTATE: &[&str] = &[
    "unlink",
    "unlinkat",
    "rename",
    "renameat",
    "renameat2",
    "mkdir",
    "mkdirat",
    "rmdir",
    "link",
    "linkat",
    "symlink",
    "symlinkat",
    "chmod",
    "fchmod",
    "fchmodat",
    "chown",
    "fchown",
    "fchownat",
    "lchown",
    "utime",
    "utimes",
    "utimensat",
    "futimesat",
    "truncate",
    "ftruncate",
    "mknod",
    "mknodat",
    "fallocate",
];
/// Untracked syscalls that read the file-system namespace or metadata.
const FS_LOOKUP: &[&str] = &[
    "stat",
    "lstat",
    "newfstatat",
    "statx",
    "access",
    "faccessat",
    "faccessat2",
    "readlink",
    "readlinkat",
    "getdents",
    "getdents64",
    "chdir",
    "getcwd",
    "statfs",
];

/// Linux x86-64 open flags.
mod libc_o {
    pub const WRONLY: u32 = 0o1;
    pub const RDWR: u32 = 0o2;
    pub const CREAT: u32 = 0o100;
    pub const TRUNC: u32 = 0o1000;
    pub const APPEND: u32 = 0o2000;
}

/// Untracked syscalls that observe or reap the caller's children.
const WAITS: &[&str] = &["wait4", "waitid"];
/// Untracked syscalls that send a signal to another task.
const SIGNALS: &[&str] = &[
    "kill",
    "tgkill",
    "tkill",
    "rt_sigqueueinfo",
    "rt_tgsigqueueinfo",
];

struct Commit {
    v: Value,
    untracked: BTreeMap<String, u64>,
    /// What the descriptor of the syscall filing this turn referred to.
    requesting_fd: Option<String>,
    /// The flags of the open filing this turn, if it was an open.
    open_flags: Option<u64>,
    /// The targets of this node's untracked file-system syscalls, when the
    /// profile recorded them.
    fs_paths: Option<Vec<FsTarget>>,
}

/// A path an untracked file-system syscall looked up or changed; `None` when
/// the probe could not resolve it.
#[derive(Clone, Debug)]
struct FsTarget {
    mutates: bool,
    path: Option<String>,
}

/// A `spawn` line: `creator` registered `child`, in process `process`, which
/// `wait` reaps from `wait_parent`. `creator_commit` indexes the creator's
/// latest commit at that point, whose node made the clone.
#[derive(Clone, Debug)]
struct Spawn {
    creator: i64,
    child: i64,
    process: Option<i64>,
    wait_parent: Option<i64>,
    creator_commit: Option<usize>,
}

#[derive(Default)]
struct Run {
    commits: Vec<Commit>,
    spawns: Vec<Spawn>,
}

/// One entry of a segment's `fs` list: `[syscall, "R"|"W", path|null]`, or
/// `[syscall, path]` in a profile written before targets were typed.
fn fs_target(e: &Value) -> Option<FsTarget> {
    let name = e[0].as_str()?;
    match e.as_array()?.len() {
        3 => Some(FsTarget {
            mutates: e[1].as_str()? == "W",
            path: e[2].as_str().map(str::to_string),
        }),
        2 => Some(FsTarget {
            mutates: FS_MUTATE.contains(&name),
            path: Some(e[1].as_str()?.to_string()),
        }),
        _ => None,
    }
}

fn load_runs(path: &str) -> Result<Vec<Run>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    parse_runs(&text, path)
}

/// Parse a profile's text; `path` names it in errors.
fn parse_runs(text: &str, path: &str) -> Result<Vec<Run>, String> {
    let mut runs: Vec<Run> = Vec::new();
    // Segment lines precede the commit of the request they close; they
    // describe that thread's previous node.
    let mut pending: HashMap<i64, BTreeMap<String, u64>> = HashMap::new();
    type FsPaths = Option<Vec<FsTarget>>;
    let mut seg_for_prev: Vec<(usize, usize, BTreeMap<String, u64>, FsPaths)> = Vec::new();
    let mut last_commit_of: HashMap<i64, (usize, usize)> = HashMap::new();
    let mut requesting_fd: HashMap<i64, String> = HashMap::new();
    let mut open_flags: HashMap<i64, u64> = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).map_err(|e| format!("{path}:{}: {e}", n + 1))?;
        match v.get("event").and_then(Value::as_str) {
            Some("start") => {
                runs.push(Run::default());
                pending.clear();
                last_commit_of.clear();
            }
            Some("spawn") => {
                if runs.is_empty() {
                    runs.push(Run::default());
                }
                let r = runs.len() - 1;
                let creator = v["creator"].as_i64().unwrap_or(-1);
                runs[r].spawns.push(Spawn {
                    creator,
                    child: v["child"].as_i64().unwrap_or(-1),
                    process: v["process"].as_i64(),
                    wait_parent: v["wait_parent"].as_i64(),
                    creator_commit: last_commit_of.get(&creator).map(|&(_, i)| i),
                });
            }
            Some("segment") => {
                let tid = v["tid"].as_i64().unwrap_or(-1);
                match v["requesting_fd"].as_str() {
                    Some(t) => requesting_fd.insert(tid, t.to_string()),
                    None => requesting_fd.remove(&tid),
                };
                match v["requesting_open_flags"].as_u64() {
                    Some(f) => open_flags.insert(tid, f),
                    None => open_flags.remove(&tid),
                };
                let counts = pending.entry(tid).or_default();
                if let Some(m) = v["syscalls"].as_object() {
                    for (k, c) in m {
                        *counts.entry(k.clone()).or_default() += c.as_u64().unwrap_or(0);
                    }
                }
                let fs: FsPaths = v["fs"]
                    .as_array()
                    .map(|a| a.iter().filter_map(fs_target).collect());
                // Attribute to the thread's latest commit, if any.
                if let Some(&(r, i)) = last_commit_of.get(&tid) {
                    seg_for_prev.push((r, i, std::mem::take(counts), fs));
                }
            }
            Some("commit") => {
                if runs.is_empty() {
                    runs.push(Run::default());
                }
                let r = runs.len() - 1;
                let tid = v["tid"].as_i64().unwrap_or(-1);
                let untracked = pending.remove(&tid).unwrap_or_default();
                let requesting_fd = requesting_fd.remove(&tid);
                let open_flags = open_flags.remove(&tid);
                runs[r].commits.push(Commit {
                    v,
                    untracked,
                    requesting_fd,
                    open_flags,
                    fs_paths: None,
                });
                last_commit_of.insert(tid, (r, runs[r].commits.len() - 1));
            }
            _ => {}
        }
    }
    for (r, i, counts, fs) in seg_for_prev {
        let commit = &mut runs[r].commits[i];
        for (k, c) in counts {
            *commit.untracked.entry(k).or_default() += c;
        }
        if let Some(fs) = fs {
            commit.fs_paths.get_or_insert_with(Vec::new).extend(fs);
        }
    }
    Ok(runs)
}

struct Graph {
    nodes: Vec<Node>,
    undeclared_turns: u64,
    unknown_cost_turns: u64,
    kinds: BTreeMap<String, u64>,
    untracked_total: BTreeMap<String, u64>,
}

fn build_graph(run: &Run, opts: &Options) -> Graph {
    let commits = &run.commits;
    let ablated = |kind: &str, key: &str| opts.ablate.iter().any(|a| a == kind || a == key);
    let sample = |c: &Commit| -> Option<u64> {
        if opts.cost_ltime {
            c.v["ltime_ns"].as_u64()
        } else {
            c.v["cpu_ns"].as_u64()
        }
    };
    // Pass 1: who spawned each thread, as (parent tid, index of the parent's
    // commit whose node made the clone), and each process's wait parent.
    let mut pid_of: HashMap<i64, i64> = HashMap::new();
    for c in commits {
        if let (Some(t), Some(p)) = (c.v["tid"].as_i64(), c.v["pid"].as_i64()) {
            pid_of.entry(t).or_insert(p);
        }
    }
    let mut spawned_by: HashMap<i64, (i64, usize)> = HashMap::new();
    let mut wait_parent_of: HashMap<i64, i64> = HashMap::new();
    if !run.spawns.is_empty() {
        // The probe's `spawn` lines are authoritative.
        for sp in &run.spawns {
            if sp.creator == sp.child {
                continue;
            }
            if let Some(j) = sp.creator_commit {
                spawned_by.insert(sp.child, (sp.creator, j));
            }
            if sp.process == Some(sp.child)
                && let Some(parent) = sp.wait_parent
            {
                wait_parent_of.insert(sp.child, parent);
            }
        }
    } else {
        // A profile without `spawn` lines: infer creation from two protocols.
        //   * clone/fork: the parent's later ParentContinue{parent, child}
        //     turn; the cloning node is the parent's last commit before it.
        //     This is authoritative and replaces any vfork guess.
        //   * vfork: the parent's BlockingVfork(op) turn, after which the
        //     scheduler admits no other turn until the child registers, so the
        //     next thread seen for the first time is that child, unless a
        //     VforkFailed(op) turn cancelled the operation first.
        // The spawning process is taken to be the wait parent, which
        // CLONE_PARENT makes false; only `spawn` lines say otherwise.
        let mut last_commit_of: HashMap<i64, usize> = HashMap::new();
        // (operation, parent tid, commit index)
        let mut open_vforks: Vec<(String, i64, usize)> = Vec::new();
        for (i, c) in commits.iter().enumerate() {
            let tid = c.v["tid"].as_i64().unwrap_or(-1);
            if !last_commit_of.contains_key(&tid)
                && i > 0
                && let Some((_, parent, j)) = open_vforks.pop()
            {
                spawned_by.entry(tid).or_insert((parent, j));
            }
            for r in c.v["res"].as_array().into_iter().flatten() {
                let (kind, payload) = variant(&r[0]);
                match kind.as_str() {
                    "BlockingVfork" => open_vforks.push((payload.to_string(), tid, i)),
                    "VforkFailed" => {
                        let op = payload.to_string();
                        open_vforks.retain(|(o, parent, _)| *o != op || *parent != tid);
                    }
                    "ParentContinue" => {
                        let parent = payload["parent"].as_i64().unwrap_or(-1);
                        let child = payload["child"].as_i64().unwrap_or(-1);
                        if parent != child
                            && let Some(&j) = last_commit_of.get(&parent)
                        {
                            spawned_by.insert(child, (parent, j));
                        }
                    }
                    _ => {}
                }
            }
            last_commit_of.insert(tid, i);
        }
        for (child, (parent, _)) in &spawned_by {
            wait_parent_of.insert(*child, *pid_of.get(parent).unwrap_or(parent));
        }
    }
    let parent_pid_of_process = wait_parent_of;
    let mut node_of_commit: Vec<usize> = Vec::with_capacity(commits.len());
    let mut pending_birth: HashMap<usize, Vec<usize>> = HashMap::new();

    let mut nodes: Vec<Node> = Vec::new();
    let mut last_node_of: HashMap<i64, usize> = HashMap::new();
    let mut last_sample_of: HashMap<i64, Option<u64>> = HashMap::new();
    let mut undeclared_turns = 0;
    let mut unknown_cost_turns = 0;
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    let mut untracked_total: BTreeMap<String, u64> = BTreeMap::new();

    for c in commits {
        let tid = c.v["tid"].as_i64().unwrap_or(-1);
        let now = sample(c);
        // Close the thread's previous node with the CPU used since it.
        match last_node_of.get(&tid) {
            Some(&prev) => {
                let before = last_sample_of.get(&tid).copied().flatten();
                match (before, now) {
                    (Some(b), Some(a)) if a >= b => nodes[prev].cost = a - b,
                    _ => unknown_cost_turns += 1,
                }
            }
            None => {
                // Birth node: the CPU the thread used before its first commit.
                let birth = nodes.len();
                nodes.push(Node {
                    tid,
                    turn: None,
                    cost: now.unwrap_or(0),
                    res: Vec::new(),
                    mm: c.v["mm"].as_object().and_then(|_| mm_key(&c.v["mm"])),
                    spawn_pred: None,
                    label: format!("birth of tid {tid}"),
                });
                if let Some(&(_, j)) = spawned_by.get(&tid) {
                    match node_of_commit.get(j) {
                        Some(&n) => nodes[birth].spawn_pred = Some(n),
                        // The cloning commit comes later in commit order
                        // only if the parent's ParentContinue turn follows
                        // the child's first commit; resolve it then.
                        None => pending_birth.entry(j).or_default().push(birth),
                    }
                }
                last_node_of.insert(tid, birth);
                if opts.cost_ltime {
                    // A thread's logical time starts at the epoch.
                    nodes[birth].cost = 0;
                }
            }
        }
        last_sample_of.insert(tid, now);

        let mut res = Vec::new();
        let raw = c.v["res"].as_array().cloned().unwrap_or_default();
        if raw.is_empty() {
            undeclared_turns += 1;
        }
        for r in &raw {
            let (kind, payload) = variant(&r[0]);
            *kinds.entry(kind.clone()).or_default() += 1;
            let perm = perm_of(r[1].as_str().unwrap_or("RW"));
            let mapped: Option<(String, Perm)> = match kind.as_str() {
                // An open declares its path read whatever its flags; with
                // `--refine`, an open that may write, create or truncate
                // holds the path for writing.
                "Path" | "PathsTransitive" => {
                    const WRITES: u64 = (libc_o::WRONLY
                        | libc_o::RDWR
                        | libc_o::CREAT
                        | libc_o::TRUNC
                        | libc_o::APPEND) as u64;
                    let writes = opts.refine && c.open_flags.is_some_and(|f| f & WRITES != 0);
                    Some((
                        format!("Path:{}", payload.as_str().unwrap_or("?")),
                        if writes { Perm::W } else { perm },
                    ))
                }
                "FileContents" | "FileMetadata" | "DirectoryContents" => {
                    Some((format!("{kind}:{payload}"), perm))
                }
                "Device" => Some((format!("Device:{}", payload.as_str().unwrap_or("?")), perm)),
                "MemAddrSpace" => Some((format!("MemAddrSpace:{payload}"), Perm::W)),
                "Exit" => {
                    let process = payload["process"].as_i64().unwrap_or(-1);
                    let parent = parent_pid_of_process.get(&process).copied().unwrap_or(-1);
                    Some((format!("Children:{parent}"), Perm::W))
                }
                "WaitChild" => {
                    let parent = payload["parent"].as_i64().unwrap_or(-1);
                    Some((format!("Children:{parent}"), Perm::W))
                }
                "WaitPhysicalChild" => {
                    let child = payload.as_i64().unwrap_or(-1);
                    let parent = parent_pid_of_process.get(&child).copied().unwrap_or(-1);
                    Some((format!("Children:{parent}"), Perm::W))
                }
                // A polling retry carries no identity. As declared it is one
                // global resource. `--refine` gives a wait4/waitid poll the
                // caller's child table, and a descriptor poll the object its
                // descriptor names (for a pipe, its inode).
                "InternalIOPolling" if opts.refine => {
                    let fyi = c.v["fyi"].as_str().unwrap_or("");
                    let pid = c.v["pid"].as_i64().unwrap_or(tid);
                    if fyi.contains("wait4") || fyi.contains("waitid") {
                        Some((format!("Children:{pid}"), Perm::W))
                    } else if let Some(target) = &c.requesting_fd {
                        Some((format!("Fd:{target}"), Perm::W))
                    } else {
                        Some((kind.clone(), Perm::W))
                    }
                }
                // Identity-free tokens: one global resource each.
                "InternalIOPolling" | "FutexWait" => Some((kind.clone(), Perm::W)),
                // A sleep to the epoch is a yield (`yield_request`, the
                // timeslice preemption point); a real sleep waits for global
                // virtual time, which every thread's progress advances.
                "SleepUntil" if payload.as_u64() == Some(0) => None,
                "SleepUntil" => Some(("GlobalTime".to_string(), Perm::W)),
                // Control tokens, per-thread or per-operation: no shared state.
                _ => None,
            };
            if let Some((key, perm)) = mapped {
                let k = key.split(':').next().unwrap_or("").to_string();
                if !ablated(&k, &key) {
                    res.push((k, key, perm, false));
                }
            }
        }
        // Every path a turn declares also reads the global `FS`, which an
        // unresolved untracked mutation writes.
        if opts.untracked_fs && res.iter().any(|(k, _, _, _)| k == "Path") && !ablated("FS", "FS") {
            res.push(("FS".into(), "FS".into(), Perm::R, false));
        }
        if opts.untracked_fs {
            // Inferred from untracked syscalls, which may come anywhere in
            // the node: held for the whole node in both models.
            let mut held = |kind: &str, key: String, perm: Perm| {
                if !ablated(kind, &key) {
                    res.push((kind.to_string(), key, perm, true));
                }
            };
            match &c.fs_paths {
                // Path-keyed: a lookup reads its path and a mutation writes
                // it; an unresolved target writes the global `FS`. A
                // directory listing's directory is not recorded; it reads one
                // shared `Dir:?` key nothing writes, so listings stay
                // unbracketed.
                Some(targets) => {
                    for t in targets {
                        let perm = if t.mutates { Perm::W } else { Perm::R };
                        match &t.path {
                            Some(path) => {
                                held("Path", format!("Path:{path}"), perm);
                                held("FS", "FS".into(), Perm::R);
                            }
                            None => held("FS", "FS".into(), Perm::W),
                        }
                    }
                    if c.untracked.keys().any(|s| s.starts_with("getdents")) {
                        held("Dir", "Dir:?".into(), Perm::R);
                    }
                }
                // An older profile without paths: one global FS resource.
                None => {
                    let mutates = c.untracked.keys().any(|s| FS_MUTATE.contains(&s.as_str()));
                    let looks = c.untracked.keys().any(|s| FS_LOOKUP.contains(&s.as_str()));
                    if mutates || looks {
                        held("FS", "FS".into(), if mutates { Perm::W } else { Perm::R });
                    }
                }
            }
            // A wait that ran without a turn (WNOHANG) still observes and
            // reaps the caller's children; a signal sent without one
            // changes another process.
            let pid = c.v["pid"].as_i64().unwrap_or(tid);
            if c.untracked.keys().any(|s| WAITS.contains(&s.as_str())) {
                held("Children", format!("Children:{pid}"), Perm::W);
            }
            if c.untracked.keys().any(|s| SIGNALS.contains(&s.as_str())) {
                held("Signals", "Signals".into(), Perm::W);
            }
        }
        for (k, n) in &c.untracked {
            *untracked_total.entry(k.clone()).or_default() += n;
        }
        let turn = c.v["turn"].as_i64();
        let fyi = c.v["fyi"].as_str().unwrap_or("");
        let label = format!(
            "turn {} tid {} {}{}",
            turn.unwrap_or(-1),
            tid,
            raw.iter()
                .map(|r| variant(&r[0]).0)
                .collect::<Vec<_>>()
                .join("+"),
            if fyi.is_empty() {
                String::new()
            } else {
                format!(" ({fyi})")
            }
        );
        let idx = nodes.len();
        nodes.push(Node {
            tid,
            turn,
            cost: 0,
            res,
            mm: mm_key(&c.v["mm"]),
            spawn_pred: None,
            label,
        });
        last_node_of.insert(tid, idx);
        if let Some(births) = pending_birth.remove(&node_of_commit.len()) {
            for birth in births {
                nodes[birth].spawn_pred = Some(idx);
            }
        }
        node_of_commit.push(idx);
    }
    Graph {
        nodes,
        undeclared_turns,
        unknown_cost_turns,
        kinds,
        untracked_total,
    }
}

/// Why a node could not start earlier.
#[derive(Clone, Debug)]
enum Because {
    Start,
    Program,
    Spawn,
    Resource(String),
}

/// The node a node waited for, and whether it waited for that node's finish
/// or only its start (an effect-model checkout ends where its node starts).
type Waited = Option<(usize, Endpoint)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Endpoint {
    Start,
    Finish,
}

struct Schedule {
    work: u64,
    span: u64,
    start: Vec<u64>,
    finish: Vec<u64>,
    because: Vec<(Because, Waited)>,
}

/// Earliest-start schedule on unbounded CPUs, nodes taken in commit order.
/// Because every edge points from an earlier node to a later one, this is the
/// longest path through the graph.
fn schedule(g: &Graph, model: Model) -> Schedule {
    let n = g.nodes.len();
    let mut start = vec![0u64; n];
    let mut finish = vec![0u64; n];
    let mut because = vec![(Because::Start, None); n];
    let mut prev_of_tid: HashMap<i64, usize> = HashMap::new();
    // Per resource: (time, node, endpoint) of the last write's release, and of
    // the latest read release since that write.
    let mut last_write: HashMap<String, (u64, usize, Endpoint)> = HashMap::new();
    let mut last_read: HashMap<String, (u64, usize, Endpoint)> = HashMap::new();
    let mut work = 0u64;
    for (i, node) in g.nodes.iter().enumerate() {
        let mut t = 0u64;
        let mut why: (Because, Waited) = (Because::Start, None);
        let consider =
            |time: u64, b: Because, from: Waited, t: &mut u64, why: &mut (Because, Waited)| {
                if time > *t {
                    *t = time;
                    *why = (b, from);
                }
            };
        if let Some(&p) = prev_of_tid.get(&node.tid) {
            consider(
                finish[p],
                Because::Program,
                Some((p, Endpoint::Finish)),
                &mut t,
                &mut why,
            );
        }
        if let Some(p) = node.spawn_pred {
            consider(
                finish[p],
                Because::Spawn,
                Some((p, Endpoint::Finish)),
                &mut t,
                &mut why,
            );
        }
        let mut held: Vec<(String, Perm, bool)> = node
            .res
            .iter()
            .map(|(_, key, perm, whole)| (key.clone(), *perm, *whole || model == Model::Lock))
            .collect();
        if let Some(mm) = &node.mm {
            // The address space is held for the whole node in both models.
            held.push((format!("MM:{mm}"), Perm::W, true));
        }
        for (key, perm, _) in &held {
            if let Some(&(time, from, end)) = last_write.get(key) {
                consider(
                    time,
                    Because::Resource(key.clone()),
                    Some((from, end)),
                    &mut t,
                    &mut why,
                );
            }
            if *perm == Perm::W
                && let Some(&(time, from, end)) = last_read.get(key)
            {
                consider(
                    time,
                    Because::Resource(key.clone()),
                    Some((from, end)),
                    &mut t,
                    &mut why,
                );
            }
        }
        start[i] = t;
        finish[i] = t + node.cost;
        because[i] = why;
        work += node.cost;
        for (key, perm, whole) in held {
            let (release, end) = if whole {
                (finish[i], Endpoint::Finish)
            } else {
                (start[i], Endpoint::Start)
            };
            match perm {
                Perm::W => {
                    last_write.insert(key.clone(), (release, i, end));
                    last_read.remove(&key);
                }
                Perm::R => {
                    let e = last_read.entry(key).or_insert((release, i, end));
                    if release >= e.0 {
                        *e = (release, i, end);
                    }
                }
            }
        }
        prev_of_tid.insert(node.tid, i);
    }
    let span = finish.iter().copied().max().unwrap_or(0);
    Schedule {
        work,
        span,
        start,
        finish,
        because,
    }
}

/// Resources whose edges bind on the critical path, with the path time each
/// accounts for (the cost of the nodes whose waits that resource's edges
/// explain), and the nodes whose whole cost is on the path. A node the path
/// reaches only at its start (an effect-model checkout) contributes no cost
/// and is not listed; the path continues from what that node waited for, so
/// the listed costs add up to the span.
fn critical_blame(g: &Graph, s: &Schedule) -> (Vec<(String, u64, u64)>, Vec<usize>) {
    let mut path = Vec::new();
    let Some(mut i) = (0..g.nodes.len()).max_by_key(|&i| (s.finish[i], std::cmp::Reverse(i)))
    else {
        return (Vec::new(), path);
    };
    let mut end = Endpoint::Finish;
    let mut blame: HashMap<String, (u64, u64)> = HashMap::new();
    loop {
        let (b, from) = &s.because[i];
        if end == Endpoint::Finish {
            path.push(i);
            let label = match b {
                Because::Start => "start".to_string(),
                Because::Program => "program order".to_string(),
                Because::Spawn => "spawn".to_string(),
                Because::Resource(k) => k.clone(),
            };
            let e = blame.entry(label).or_default();
            e.0 += g.nodes[i].cost;
            e.1 += 1;
        }
        match from {
            Some((f, e)) => {
                i = *f;
                end = *e;
            }
            None => break,
        }
    }
    path.reverse();
    let mut v: Vec<(String, u64, u64)> = blame.into_iter().map(|(k, (c, n))| (k, c, n)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    (v, path)
}

fn ms(ns: u64) -> String {
    format!("{:.1} ms", ns as f64 / 1e6)
}

fn ratio(work: u64, span: u64) -> f64 {
    if span == 0 {
        1.0
    } else {
        work as f64 / span as f64
    }
}

fn main() -> ExitCode {
    rust_script_prelude::init();
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("parallelism-profile: {e}");
            }
            return usage();
        }
    };
    let runs = match load_runs(&opts.path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("parallelism-profile: {e}");
            return ExitCode::from(1);
        }
    };
    if runs.is_empty() {
        eprintln!(
            "parallelism-profile: {}: no runs (no start or commit lines)",
            opts.path
        );
        return ExitCode::from(1);
    }
    let r = opts.run.unwrap_or(runs.len() - 1);
    let Some(run) = runs.get(r) else {
        eprintln!(
            "parallelism-profile: --run {r}: the file holds {} run(s)",
            runs.len()
        );
        return ExitCode::from(1);
    };
    let commits = &run.commits;
    let g = build_graph(run, &opts);
    let threads: HashSet<i64> = g.nodes.iter().map(|n| n.tid).collect();
    let unit = if opts.cost_ltime {
        "logical"
    } else {
        "host CPU"
    };
    println!(
        "run {r} of {}: {} commits, {} threads, cost = {unit} time",
        runs.len(),
        commits.len(),
        threads.len()
    );
    if !opts.ablate.is_empty() {
        println!(
            "ablated (treated as non-conflicting): {}",
            opts.ablate.join(", ")
        );
    }
    if opts.untracked_fs {
        println!(
            "untracked file-system, wait and signal syscalls hold resources (FS, Children, Signals)"
        );
    }
    if opts.refine {
        println!("polling retries refined to their child table or descriptor object");
    }
    println!(
        "turns with no declared resource: {}; turns with unknown cost: {}",
        g.undeclared_turns, g.unknown_cost_turns
    );
    println!(
        "resource kinds checked out: {}",
        g.kinds
            .iter()
            .map(|(k, n)| format!("{k}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut untracked: Vec<_> = g.untracked_total.iter().collect();
    untracked.sort_by(|a, b| b.1.cmp(a.1));
    let total_untracked: u64 = g.untracked_total.values().sum();
    println!(
        "untracked syscalls (ran without a turn): {total_untracked}; top: {}",
        untracked
            .iter()
            .take(12)
            .map(|(k, n)| format!("{k}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let mut summary = json!({
        "run": r,
        "commits": commits.len(),
        "threads": threads.len(),
        "cost": if opts.cost_ltime { "ltime" } else { "cpu" },
        "ablate": opts.ablate,
        "untracked_fs": opts.untracked_fs,
        "refine": opts.refine,
        "undeclared_turns": g.undeclared_turns,
        "untracked_syscalls": total_untracked,
    });
    for (name, model) in [("lock", Model::Lock), ("effect", Model::Effect)] {
        let s = schedule(&g, model);
        let (blame, path) = critical_blame(&g, &s);
        println!();
        println!(
            "model {name}: work {} span {} work/span {:.2} (critical path {} nodes)",
            ms(s.work),
            ms(s.span),
            ratio(s.work, s.span),
            path.len()
        );
        println!("  critical path time by the edge that made each node wait:");
        for (k, c, n) in blame.iter().take(opts.top) {
            println!("    {:>10}  {:>5} nodes  {k}", ms(*c), n);
        }
        let mut heaviest: Vec<usize> = path.clone();
        heaviest.sort_by(|a, b| g.nodes[*b].cost.cmp(&g.nodes[*a].cost));
        println!("  heaviest critical-path nodes:");
        for &i in heaviest.iter().take(opts.top.min(5)) {
            println!(
                "    {:>10}  start {:>10}  {}",
                ms(g.nodes[i].cost),
                ms(s.start[i]),
                g.nodes[i].label
            );
        }
        if opts.print_path {
            println!("  critical path (start, cost, why it waited, node):");
            for &i in &path {
                let why = match &s.because[i].0 {
                    Because::Start => "start".to_string(),
                    Because::Program => "program".to_string(),
                    Because::Spawn => "spawn".to_string(),
                    Because::Resource(k) => k.clone(),
                };
                println!(
                    "    {:>10} {:>9}  {:<22} {}",
                    ms(s.start[i]),
                    ms(g.nodes[i].cost),
                    why,
                    g.nodes[i].label
                );
            }
        }
        if opts.print_threads {
            // Per thread: first start, last finish, and the turn of its last
            // commit, in order of last commit.
            let mut spans: BTreeMap<i64, (u64, u64, i64)> = BTreeMap::new();
            for (i, node) in g.nodes.iter().enumerate() {
                let e = spans.entry(node.tid).or_insert((u64::MAX, 0, -1));
                e.0 = e.0.min(s.start[i]);
                e.1 = e.1.max(s.finish[i]);
                if let Some(t) = node.turn {
                    e.2 = e.2.max(t);
                }
            }
            let mut rows: Vec<_> = spans.into_iter().collect();
            rows.sort_by_key(|(_, (_, _, last))| *last);
            println!("  threads (tid, first start, last finish, last turn):");
            for (tid, (first, last, turn)) in rows {
                println!("    {tid:>6} {:>10} {:>10} {turn:>8}", ms(first), ms(last));
            }
        }
        summary[name] = json!({
            "work_ns": s.work,
            "span_ns": s.span,
            "work_over_span": ratio(s.work, s.span),
            "critical_path_nodes": path.len(),
            "blame": blame.iter().take(opts.top).map(|(k, c, n)| json!({"edge": k, "ns": c, "nodes": n})).collect::<Vec<_>>(),
        });
    }
    if let Some(out) = &opts.json_out
        && let Err(e) = std::fs::write(out, serde_json::to_string_pretty(&summary).unwrap() + "\n")
    {
        eprintln!("parallelism-profile: {out}: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Options {
        Options {
            path: String::new(),
            run: None,
            cost_ltime: false,
            untracked_fs: false,
            refine: false,
            ablate: Vec::new(),
            top: 8,
            json_out: None,
            print_path: false,
            print_threads: false,
        }
    }

    /// A commit line for `tid` at `turn`, with host CPU `cpu` ms and
    /// resources `res` (JSON).
    fn commit(turn: u64, tid: i64, cpu_ms: u64, res: &str) -> String {
        format!(
            r#"{{"event":"commit","turn":{turn},"tid":{tid},"pid":{tid},"mm":{{"creator":{tid},"generation":1}},"cpu_ns":{},"ltime_ns":0,"poll":0,"fyi":"","res":{res}}}"#,
            cpu_ms * 1_000_000
        )
    }

    /// Thread 1 forks thread 2; each then computes 100 ms and checks out
    /// `res2` and `res1` respectively before exiting.
    fn fork_pair(res1: &str, res2: &str) -> String {
        [
            r#"{"event":"start","host_pid":1}"#.to_string(),
            commit(0, 1, 0, "[]"),
            commit(
                1,
                1,
                0,
                r#"[[{"ParentContinue":{"parent":1,"child":2}},"W"]]"#,
            ),
            commit(2, 2, 0, "[]"),
            commit(3, 1, 0, res1),
            commit(4, 2, 0, res2),
            commit(5, 1, 100, "[]"),
            commit(6, 2, 100, "[]"),
        ]
        .join("\n")
    }

    fn project_with(text: &str, model: Model, opts: &Options) -> (u64, u64) {
        let runs = parse_runs(text, "test").unwrap();
        let g = build_graph(runs.last().unwrap(), opts);
        let s = schedule(&g, model);
        (s.work, s.span)
    }

    fn project(text: &str, model: Model) -> (u64, u64) {
        project_with(text, model, &opts())
    }

    const START: &str = r#"{"event":"start","host_pid":1}"#;

    /// Thread 1 starts a vfork that fails, computes 100 ms, then clones
    /// thread 2, which computes 100 ms. `spawn` is the probe's spawn line,
    /// if the profile has one.
    fn failed_vfork_then_clone(spawn: Option<&str>) -> String {
        let mut lines = vec![
            START.to_string(),
            commit(0, 1, 0, "[]"),
            commit(1, 1, 0, r#"[[{"BlockingVfork":7},"W"]]"#),
            commit(2, 1, 0, r#"[[{"VforkFailed":7},"W"]]"#),
            // The node of this commit clones thread 2.
            commit(3, 1, 100, "[]"),
        ];
        lines.extend(spawn.map(str::to_string));
        lines.extend([
            commit(4, 2, 0, "[]"),
            commit(
                5,
                1,
                100,
                r#"[[{"ParentContinue":{"parent":1,"child":2}},"W"]]"#,
            ),
            commit(6, 2, 100, "[]"),
        ]);
        lines.join("\n")
    }

    /// A failed vfork leaves no spawn edge for the next new thread to take:
    /// thread 2 was cloned after thread 1's 100 ms, so nothing overlaps.
    #[test]
    fn a_failed_vfork_spawns_nothing() {
        let without_spawn_lines = failed_vfork_then_clone(None);
        assert_eq!(
            project(&without_spawn_lines, Model::Lock),
            (200_000_000, 200_000_000)
        );
        let with_spawn_line = failed_vfork_then_clone(Some(
            r#"{"event":"spawn","creator":1,"child":2,"process":2,"wait_parent":1}"#,
        ));
        assert_eq!(
            project(&with_spawn_line, Model::Lock),
            (200_000_000, 200_000_000)
        );
    }

    /// Thread 1 forks thread 2. Thread 1 computes 100 ms, then a 100 ms node
    /// that checks out X; thread 2 then checks out X and computes 200 ms.
    fn effect_chain() -> String {
        [
            START.to_string(),
            commit(0, 1, 0, "[]"),
            commit(
                1,
                1,
                0,
                r#"[[{"ParentContinue":{"parent":1,"child":2}},"W"]]"#,
            ),
            commit(2, 2, 0, "[]"),
            commit(3, 1, 0, "[]"),
            commit(4, 1, 100, r#"[[{"Path":"x"},"W"]]"#),
            commit(5, 2, 0, r#"[[{"Path":"x"},"W"]]"#),
            commit(6, 1, 200, "[]"),
            commit(7, 2, 200, "[]"),
        ]
        .join("\n")
    }

    /// Under the effect model thread 2 waits only for the start of thread
    /// 1's node that touches X, so that node's computation is not on the
    /// critical path, and the path's costs add up to the span.
    #[test]
    fn critical_path_costs_add_up_to_the_span() {
        let runs = parse_runs(&effect_chain(), "test").unwrap();
        let g = build_graph(runs.last().unwrap(), &opts());
        for model in [Model::Lock, Model::Effect] {
            let s = schedule(&g, model);
            let (blame, path) = critical_blame(&g, &s);
            let path_cost: u64 = path.iter().map(|&i| g.nodes[i].cost).sum();
            let blame_cost: u64 = blame.iter().map(|(_, c, _)| c).sum();
            assert_eq!(path_cost, s.span);
            assert_eq!(blame_cost, s.span);
        }
        let s = schedule(&g, Model::Effect);
        assert_eq!((s.work, s.span), (400_000_000, 300_000_000));
    }

    /// Thread 1's 100 ms node unlinks /x without a turn; thread 2's later
    /// 100 ms node opens /x. `--untracked-fs` holds the unlink's path for
    /// the whole node, since the unlink may come at its end.
    fn untracked_unlink(target: &str) -> String {
        [
            START.to_string(),
            commit(0, 1, 0, "[]"),
            commit(1, 1, 0, r#"[[{"ParentContinue":{"parent":1,"child":2}},"W"]]"#),
            commit(2, 2, 0, "[]"),
            commit(3, 1, 0, "[]"),
            format!(
                r#"{{"event":"segment","tid":1,"syscalls":{{"unlink":1}},"requesting":null,"fs":[{target}]}}"#
            ),
            commit(4, 1, 100, "[]"),
            commit(5, 2, 0, r#"[[{"Path":"/x"},"R"]]"#),
            commit(6, 2, 100, "[]"),
        ]
        .join("\n")
    }

    #[test]
    fn untracked_mutations_are_held_for_the_whole_node() {
        let fs = Options {
            untracked_fs: true,
            ..opts()
        };
        let text = untracked_unlink(r#"["unlink","W","/x"]"#);
        assert_eq!(
            project_with(&text, Model::Effect, &fs),
            (200_000_000, 200_000_000)
        );
        // Without bracketing, the reader overlaps the unlink's node.
        assert_eq!(project(&text, Model::Effect), (200_000_000, 100_000_000));
        // A mutation of another path does not conflict.
        let other = untracked_unlink(r#"["unlink","W","/y"]"#);
        assert_eq!(
            project_with(&other, Model::Effect, &fs),
            (200_000_000, 100_000_000)
        );
        // A mutation the probe could not resolve conflicts with every path.
        let unresolved = untracked_unlink(r#"["unlink","W",null]"#);
        assert_eq!(
            project_with(&unresolved, Model::Effect, &fs),
            (200_000_000, 200_000_000)
        );
    }

    /// Process 1 forks process 2, which creates process 3 with CLONE_PARENT:
    /// process 3's exit belongs to process 1's child table, as the spawn
    /// line's wait parent says, not to its creator's.
    #[test]
    fn clone_parent_exits_belong_to_the_wait_parent() {
        let text = [
            START.to_string(),
            commit(0, 1, 0, "[]"),
            r#"{"event":"spawn","creator":1,"child":2,"process":2,"wait_parent":1}"#.to_string(),
            commit(1, 2, 0, "[]"),
            r#"{"event":"spawn","creator":2,"child":3,"process":3,"wait_parent":1}"#.to_string(),
            commit(
                2,
                3,
                0,
                r#"[[{"Exit":{"group":true,"process":3,"mm":null}},"W"]]"#,
            ),
        ]
        .join("\n");
        let runs = parse_runs(&text, "test").unwrap();
        let g = build_graph(runs.last().unwrap(), &opts());
        let exit = g.nodes.iter().find(|n| n.turn == Some(2)).unwrap();
        assert_eq!(exit.res[0].1, "Children:1");
    }

    #[test]
    fn independent_children_overlap() {
        let text = fork_pair(r#"[[{"Path":"a"},"W"]]"#, r#"[[{"Path":"b"},"W"]]"#);
        for model in [Model::Lock, Model::Effect] {
            let (work, span) = project(&text, model);
            assert_eq!(work, 200_000_000);
            assert_eq!(span, 100_000_000);
        }
    }

    #[test]
    fn a_conflicting_checkout_serializes_under_the_lock_model() {
        let text = fork_pair(r#"[[{"Path":"a"},"W"]]"#, r#"[[{"Path":"a"},"W"]]"#);
        assert_eq!(project(&text, Model::Lock), (200_000_000, 200_000_000));
        // Under the effect model only the syscall at the node's start is
        // ordered; the computation after it overlaps.
        assert_eq!(project(&text, Model::Effect), (200_000_000, 100_000_000));
    }

    #[test]
    fn readers_do_not_conflict() {
        let text = fork_pair(r#"[[{"Path":"a"},"R"]]"#, r#"[[{"Path":"a"},"R"]]"#);
        assert_eq!(project(&text, Model::Lock), (200_000_000, 100_000_000));
    }

    #[test]
    fn one_thread_projects_one() {
        let text = [
            r#"{"event":"start","host_pid":1}"#.to_string(),
            commit(0, 1, 0, "[]"),
            commit(1, 1, 50, r#"[[{"Path":"a"},"R"]]"#),
            commit(2, 1, 120, "[]"),
        ]
        .join("\n");
        let (work, span) = project(&text, Model::Effect);
        assert_eq!(work, span);
        assert_eq!(work, 120_000_000);
    }

    #[test]
    fn the_last_run_of_an_appended_file_is_the_default() {
        let text = format!("{}\n{}", fork_pair("[]", "[]"), fork_pair("[]", "[]"));
        assert_eq!(parse_runs(&text, "test").unwrap().len(), 2);
    }
}
