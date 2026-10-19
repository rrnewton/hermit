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
//!   * spawn: a child's birth node follows the parent node that cloned it
//!     (the parent node committed just before its `ParentContinue` turn);
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
//! projection is an upper bound for that model.
//!
//! WHAT IT CANNOT SEE. Syscalls that run in a thread's exclusive slot without
//! taking a turn are not covered by any declared resource; the profile lists
//! them per node (`segment` lines). `--untracked-fs` makes a node whose
//! untracked syscalls touch the file system hold the paths they name (read
//! for lookups, write for mutations; directory listings are not keyed), or one
//! global `FS` resource for a profile that did not record paths; a node that waited for a child
//! without a turn (WNOHANG) hold its child table, and a node that signalled
//! without one hold a global `Signals` resource, bracketing that hole.
//!
//! POLL TARGETS. A profile made with `HERMIT_PARALLEL_POLL_IDENTITY=1`
//! records each polling request's declared target (`poll_target`); every
//! interpretation then keys the retry by it (`Children:<pid>`, one `OFD:` key
//! per open file description, `Futex:`).
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
    /// (kind, key, perm) after mapping to analysis resources.
    res: Vec<(String, String, Perm)>,
    mm: Option<String>,
    /// Extra predecessor (spawn edge).
    spawn_pred: Option<usize>,
    untracked: BTreeMap<String, u64>,
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
            "--ablate" => opts.ablate.push(args.next().ok_or("--ablate needs a value")?),
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
    "unlink", "unlinkat", "rename", "renameat", "renameat2", "mkdir", "mkdirat", "rmdir",
    "link", "linkat", "symlink", "symlinkat", "chmod", "fchmod", "fchmodat", "chown",
    "fchown", "fchownat", "lchown", "utime", "utimes", "utimensat", "futimesat", "truncate",
    "ftruncate", "mknod", "mknodat", "fallocate",
];
/// Untracked syscalls that read the file-system namespace or metadata.
const FS_LOOKUP: &[&str] = &[
    "stat", "lstat", "newfstatat", "statx", "access", "faccessat", "faccessat2", "readlink",
    "readlinkat", "getdents", "getdents64", "chdir", "getcwd", "statfs",
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
const SIGNALS: &[&str] = &["kill", "tgkill", "tkill", "rt_sigqueueinfo", "rt_tgsigqueueinfo"];

struct Commit {
    v: Value,
    untracked: BTreeMap<String, u64>,
    /// What the descriptor of the syscall filing this turn referred to.
    requesting_fd: Option<String>,
    /// The flags of the open filing this turn, if it was an open.
    open_flags: Option<u64>,
    /// The (syscall, path) pairs of this node's untracked file-system
    /// syscalls, when the profile recorded them.
    fs_paths: Option<Vec<(String, String)>>,
}

fn load_runs(path: &str) -> Result<Vec<Vec<Commit>>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut runs: Vec<Vec<Commit>> = Vec::new();
    // Segment lines precede the commit of the request they close; they
    // describe that thread's previous node.
    let mut pending: HashMap<i64, BTreeMap<String, u64>> = HashMap::new();
    type FsPaths = Option<Vec<(String, String)>>;
    let mut seg_for_prev: Vec<(usize, usize, BTreeMap<String, u64>, FsPaths)> = Vec::new();
    let mut last_commit_of: HashMap<i64, (usize, usize)> = HashMap::new();
    let mut requesting_fd: HashMap<i64, String> = HashMap::new();
    let mut open_flags: HashMap<i64, u64> = HashMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value =
            serde_json::from_str(line).map_err(|e| format!("{path}:{}: {e}", n + 1))?;
        match v.get("event").and_then(Value::as_str) {
            Some("start") => {
                runs.push(Vec::new());
                pending.clear();
                last_commit_of.clear();
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
                let fs: FsPaths = v["fs"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|e| Some((e[0].as_str()?.to_string(), e[1].as_str()?.to_string())))
                        .collect()
                });
                // Attribute to the thread's latest commit, if any.
                if let Some(&(r, i)) = last_commit_of.get(&tid) {
                    seg_for_prev.push((r, i, std::mem::take(counts), fs));
                }
            }
            Some("commit") => {
                if runs.is_empty() {
                    runs.push(Vec::new());
                }
                let r = runs.len() - 1;
                let tid = v["tid"].as_i64().unwrap_or(-1);
                let untracked = pending.remove(&tid).unwrap_or_default();
                let requesting_fd = requesting_fd.remove(&tid);
                let open_flags = open_flags.remove(&tid);
                runs[r].push(Commit {
                    v,
                    untracked,
                    requesting_fd,
                    open_flags,
                    fs_paths: None,
                });
                last_commit_of.insert(tid, (r, runs[r].len() - 1));
            }
            _ => {}
        }
    }
    for (r, i, counts, fs) in seg_for_prev {
        let commit = &mut runs[r][i];
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

fn build_graph(commits: &[Commit], opts: &Options) -> Graph {
    let ablated = |kind: &str, key: &str| opts.ablate.iter().any(|a| a == kind || a == key);
    let sample = |c: &Commit| -> Option<u64> {
        if opts.cost_ltime {
            c.v["ltime_ns"].as_u64()
        } else {
            c.v["cpu_ns"].as_u64()
        }
    };
    // Pass 1: who spawned each thread, as (parent tid, index of the parent's
    // commit whose node made the clone). Two protocols reveal it:
    //   * clone/fork: the parent's later ParentContinue{parent, child} turn;
    //     the cloning node is the parent's last commit before that turn;
    //   * vfork: the parent's BlockingVfork turn, after which the scheduler
    //     admits no other turn until the child registers, so the next thread
    //     seen for the first time is that child.
    let mut pid_of: HashMap<i64, i64> = HashMap::new();
    for c in commits {
        if let (Some(t), Some(p)) = (c.v["tid"].as_i64(), c.v["pid"].as_i64()) {
            pid_of.entry(t).or_insert(p);
        }
    }
    let mut spawned_by: HashMap<i64, (i64, usize)> = HashMap::new();
    {
        let mut last_commit_of: HashMap<i64, usize> = HashMap::new();
        let mut open_vforks: Vec<(i64, usize)> = Vec::new();
        for (i, c) in commits.iter().enumerate() {
            let tid = c.v["tid"].as_i64().unwrap_or(-1);
            if !last_commit_of.contains_key(&tid) && i > 0 {
                if let Some(v) = open_vforks.pop() {
                    spawned_by.entry(tid).or_insert(v);
                }
            }
            for r in c.v["res"].as_array().into_iter().flatten() {
                let (kind, payload) = variant(&r[0]);
                match kind.as_str() {
                    "BlockingVfork" => open_vforks.push((tid, i)),
                    "ParentContinue" => {
                        let parent = payload["parent"].as_i64().unwrap_or(-1);
                        let child = payload["child"].as_i64().unwrap_or(-1);
                        if parent != child
                            && let Some(&j) = last_commit_of.get(&parent)
                        {
                            spawned_by.entry(child).or_insert((parent, j));
                        }
                    }
                    _ => {}
                }
            }
            last_commit_of.insert(tid, i);
        }
    }
    let parent_pid_of_process: HashMap<i64, i64> = spawned_by
        .iter()
        .map(|(child, (parent, _))| (*child, *pid_of.get(parent).unwrap_or(parent)))
        .collect();
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
                    untracked: BTreeMap::new(),
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
                    const WRITES: u64 = (libc_o::WRONLY | libc_o::RDWR | libc_o::CREAT
                        | libc_o::TRUNC | libc_o::APPEND) as u64;
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
                // A request that names its target (the poll identity
                // experiment) declares it: hold each object it waits on.
                "InternalIOPolling" if !c.v["poll_target"].is_null() => {
                    let (variant_name, target) = variant(&c.v["poll_target"]);
                    match variant_name.as_str() {
                        "Children" => Some((format!("Children:{}", target), Perm::W)),
                        "OpenFiles" => {
                            for file in target.as_array().into_iter().flatten() {
                                let key = format!(
                                    "OFD:{}.{}",
                                    file["creator"].as_i64().unwrap_or(-1),
                                    file["sequence"].as_u64().unwrap_or(0)
                                );
                                if !ablated("OFD", &key) {
                                    res.push(("OFD".into(), key, Perm::W));
                                }
                            }
                            None
                        }
                        "Futex" => Some((format!("Futex:{}", target), Perm::W)),
                        _ => Some((kind.clone(), Perm::W)),
                    }
                }
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
                    res.push((k, key, perm));
                }
            }
        }
        if opts.untracked_fs {
            match &c.fs_paths {
                // Path-keyed: a lookup reads its path and a mutation writes
                // it. A directory listing's directory is not recorded; it
                // reads one shared `Dir:?` key nothing writes, so listings
                // stay unbracketed.
                Some(paths) => {
                    for (name, path) in paths {
                        let mutates = FS_MUTATE.contains(&name.as_str());
                        let key = format!("Path:{path}");
                        if !ablated("Path", &key) {
                            res.push(("Path".into(), key, if mutates { Perm::W } else { Perm::R }));
                        }
                    }
                    if c.untracked.keys().any(|s| s.starts_with("getdents")) && !ablated("Dir", "Dir:?") {
                        res.push(("Dir".into(), "Dir:?".into(), Perm::R));
                    }
                }
                // An older profile without paths: one global FS resource.
                None => {
                    let mutates = c.untracked.keys().any(|s| FS_MUTATE.contains(&s.as_str()));
                    let looks = c.untracked.keys().any(|s| FS_LOOKUP.contains(&s.as_str()));
                    if (mutates || looks) && !ablated("FS", "FS") {
                        res.push((
                            "FS".into(),
                            "FS".into(),
                            if mutates { Perm::W } else { Perm::R },
                        ));
                    }
                }
            }
            // A wait that ran without a turn (WNOHANG) still observes and
            // reaps the caller's children; a signal sent without one
            // changes another process.
            let pid = c.v["pid"].as_i64().unwrap_or(tid);
            let key = format!("Children:{pid}");
            if c.untracked.keys().any(|s| WAITS.contains(&s.as_str())) && !ablated("Children", &key) {
                res.push(("Children".into(), key, Perm::W));
            }
            if c.untracked.keys().any(|s| SIGNALS.contains(&s.as_str())) && !ablated("Signals", "Signals") {
                res.push(("Signals".into(), "Signals".into(), Perm::W));
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
            if fyi.is_empty() { String::new() } else { format!(" ({fyi})") }
        );
        let idx = nodes.len();
        nodes.push(Node {
            tid,
            turn,
            cost: 0,
            res,
            mm: mm_key(&c.v["mm"]),
            spawn_pred: None,
            untracked: c.untracked.clone(),
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

struct Schedule {
    work: u64,
    span: u64,
    start: Vec<u64>,
    finish: Vec<u64>,
    because: Vec<(Because, Option<usize>)>,
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
    // Per resource: (time, node) of the last write's release, and of the
    // latest read release since that write.
    let mut last_write: HashMap<String, (u64, usize)> = HashMap::new();
    let mut last_read: HashMap<String, (u64, usize)> = HashMap::new();
    let mut work = 0u64;
    for (i, node) in g.nodes.iter().enumerate() {
        let mut t = 0u64;
        let mut why = (Because::Start, None);
        let consider = |time: u64, b: Because, from: Option<usize>, t: &mut u64, why: &mut (Because, Option<usize>)| {
            if time > *t {
                *t = time;
                *why = (b, from);
            }
        };
        if let Some(&p) = prev_of_tid.get(&node.tid) {
            consider(finish[p], Because::Program, Some(p), &mut t, &mut why);
        }
        if let Some(p) = node.spawn_pred {
            consider(finish[p], Because::Spawn, Some(p), &mut t, &mut why);
        }
        let mut held: Vec<(String, Perm, bool)> = node
            .res
            .iter()
            .map(|(_, key, perm)| (key.clone(), *perm, model == Model::Lock))
            .collect();
        if let Some(mm) = &node.mm {
            // The address space is held for the whole node in both models.
            held.push((format!("MM:{mm}"), Perm::W, true));
        }
        for (key, perm, _) in &held {
            if let Some(&(time, from)) = last_write.get(key) {
                consider(time, Because::Resource(key.clone()), Some(from), &mut t, &mut why);
            }
            if *perm == Perm::W
                && let Some(&(time, from)) = last_read.get(key)
            {
                consider(time, Because::Resource(key.clone()), Some(from), &mut t, &mut why);
            }
        }
        start[i] = t;
        finish[i] = t + node.cost;
        because[i] = why;
        work += node.cost;
        for (key, perm, whole) in held {
            let release = if whole { finish[i] } else { start[i] };
            match perm {
                Perm::W => {
                    last_write.insert(key.clone(), (release, i));
                    last_read.remove(&key);
                }
                Perm::R => {
                    let e = last_read.entry(key).or_insert((release, i));
                    if release >= e.0 {
                        *e = (release, i);
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
/// accounts for (the cost of nodes reached through that resource's edges).
fn critical_blame(g: &Graph, s: &Schedule) -> (Vec<(String, u64, u64)>, Vec<usize>) {
    let mut path = Vec::new();
    let Some(mut i) = (0..g.nodes.len()).max_by_key(|&i| (s.finish[i], std::cmp::Reverse(i)))
    else {
        return (Vec::new(), path);
    };
    let mut blame: HashMap<String, (u64, u64)> = HashMap::new();
    loop {
        path.push(i);
        let (b, from) = &s.because[i];
        let label = match b {
            Because::Start => "start".to_string(),
            Because::Program => "program order".to_string(),
            Because::Spawn => "spawn".to_string(),
            Because::Resource(k) => k.clone(),
        };
        let e = blame.entry(label).or_default();
        e.0 += g.nodes[i].cost;
        e.1 += 1;
        match from {
            Some(f) => i = *f,
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
    if span == 0 { 1.0 } else { work as f64 / span as f64 }
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
        eprintln!("parallelism-profile: {}: no runs (no start or commit lines)", opts.path);
        return ExitCode::from(1);
    }
    let r = opts.run.unwrap_or(runs.len() - 1);
    let Some(commits) = runs.get(r) else {
        eprintln!("parallelism-profile: --run {r}: the file holds {} run(s)", runs.len());
        return ExitCode::from(1);
    };
    let g = build_graph(commits, &opts);
    let threads: HashSet<i64> = g.nodes.iter().map(|n| n.tid).collect();
    let unit = if opts.cost_ltime { "logical" } else { "host CPU" };
    println!(
        "run {r} of {}: {} commits, {} threads, cost = {unit} time",
        runs.len(),
        commits.len(),
        threads.len()
    );
    if !opts.ablate.is_empty() {
        println!("ablated (treated as non-conflicting): {}", opts.ablate.join(", "));
    }
    if opts.untracked_fs {
        println!("untracked file-system, wait and signal syscalls hold resources (FS, Children, Signals)");
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
