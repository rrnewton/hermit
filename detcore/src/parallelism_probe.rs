/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Measurement-only record of the parallelism a run could have had.
//!
//! Setting `HERMIT_PARALLELISM_PROFILE=<path>` makes the scheduler append one
//! JSON line per committed turn to `<path>`: the turn number, the thread, its
//! process and address space, the resources the turn checked out with their
//! permissions, and two work samples taken while the thread is stopped at the
//! commit: the thread's host CPU time (`/proc/<tid>/schedstat`) and its
//! logical time as the global clock last accounted it. The work a thread does
//! between two of its commits belongs to the earlier commit's turn, so a
//! reader recovers each turn's cost from consecutive samples of one thread.
//! Before each request a `segment` line also lists, by name, the syscalls the
//! thread ran since its previous request without taking a turn: those ran in
//! the thread's exclusive slot and are not covered by any declared resource.
//! `scripts/parallelism-profile.rs` builds the happens-before graph from
//! these lines and reports work, span and the resources on the critical path.
//!
//! Nothing here changes scheduling, time or guest-visible state: the probe only
//! reads, and every entry point returns at once when the variable is unset.
//! The host CPU samples are host timing; they are measurements, never inputs.
//! The local hooks write from whichever process runs the local tool, so the
//! profile is meaningful only for the ptrace backend, where that is Hermit.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::LineWriter;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::json;

use crate::resources::Permission;
use crate::resources::Resources;
use crate::types::DetPid;
use crate::types::DetTid;
use crate::types::LogicalTime;
use crate::types::MmId;

/// The environment variable naming the profile's output file.
pub const PARALLELISM_PROFILE_ENV: &str = "HERMIT_PARALLELISM_PROFILE";

/// The profile's output, line buffered because a run may end without
/// dropping the scheduler. `None` when the profile is disabled.
fn writer() -> Option<&'static Mutex<LineWriter<File>>> {
    static WRITER: OnceLock<Option<Mutex<LineWriter<File>>>> = OnceLock::new();
    WRITER
        .get_or_init(|| {
            let path = PathBuf::from(std::env::var_os(PARALLELISM_PROFILE_ENV)?);
            // An enabled profile that cannot be written is fatal: a
            // measurement run that silently records nothing would be misread.
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap_or_else(|error| {
                    panic!(
                        "{PARALLELISM_PROFILE_ENV}={}: cannot open for append: {error}",
                        path.display()
                    )
                });
            Some(Mutex::new(LineWriter::new(file)))
        })
        .as_ref()
}

/// The environment variable that turns on the eager vfork-rejoin experiment.
pub const EAGER_VFORK_REJOIN_ENV: &str = "HERMIT_PARALLELISM_EAGER_VFORK_REJOIN";

/// Whether the eager vfork-rejoin experiment is on: the scheduler readmits a
/// vfork parent as soon as its clone returns instead of after all runnable
/// work. This CHANGES THE SCHEDULE and makes it host-timed; it exists only to
/// measure the parallelism a build would have under Linux's vfork resumption,
/// and is never on by default.
pub fn eager_vfork_rejoin() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(EAGER_VFORK_REJOIN_ENV).is_some_and(|v| v == "1"))
}

/// Whether the profile is enabled for this process.
pub fn enabled() -> bool {
    writer().is_some()
}

/// Append an arbitrary event to the profile; a no-op when it is disabled.
pub fn note(value: serde_json::Value) {
    line(value);
}

fn line(value: serde_json::Value) {
    let Some(writer) = writer() else {
        return;
    };
    let mut out = writer.lock().unwrap();
    // A lost line would corrupt the graph, so a write failure is fatal.
    serde_json::to_writer(&mut *out, &value)
        .and_then(|()| out.write_all(b"\n").map_err(serde_json::Error::io))
        .unwrap_or_else(|error| panic!("{PARALLELISM_PROFILE_ENV}: write failed: {error}"));
}

/// Syscalls each thread ran since its last turn request, by name, plus the
/// syscall it is running now (the one that may file the next request), with
/// that syscall's first three arguments and the thread's ID in `/proc`.
#[derive(Default)]
struct Segment {
    counts: BTreeMap<&'static str, u32>,
    current: Option<(&'static str, [u64; 3], i32)>,
    /// The paths that untracked file-system syscalls named, by syscall.
    fs_paths: BTreeSet<(&'static str, String)>,
    /// The path the current syscall names, if it is a file-system syscall.
    current_path: Option<String>,
}

/// File-system syscalls whose path the probe records when they run without
/// a turn: they read or change the namespace another process may use.
const FS_PATH_SYSCALLS: &[&str] = &[
    "stat",
    "lstat",
    "newfstatat",
    "statx",
    "access",
    "faccessat",
    "faccessat2",
    "readlink",
    "readlinkat",
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
    "fchmodat",
    "chown",
    "fchownat",
    "lchown",
    "utime",
    "utimes",
    "utimensat",
    "truncate",
    "mknod",
    "mknodat",
    "chdir",
    "statfs",
];

/// Whether the probe wants the rendered form of syscall `name` (for its path).
pub fn wants_rendering(name: &str) -> bool {
    enabled() && FS_PATH_SYSCALLS.contains(&name)
}

/// The first quoted string in a rendered syscall, which for the syscalls in
/// `FS_PATH_SYSCALLS` is the path they name.
fn first_quoted(shown: &str) -> Option<String> {
    let start = shown.find('"')? + 1;
    let len = shown[start..].find('"')?;
    Some(shown[start..start + len].to_string())
}

fn segments() -> &'static Mutex<HashMap<i32, Segment>> {
    static SEGMENTS: OnceLock<Mutex<HashMap<i32, Segment>>> = OnceLock::new();
    SEGMENTS.get_or_init(Default::default)
}

/// Syscalls whose first argument is the descriptor a polling retry waits on.
const FD_FIRST: &[&str] = &[
    "read", "write", "readv", "writev", "pread64", "pwrite64", "preadv", "pwritev", "preadv2",
    "pwritev2", "recvfrom", "sendto", "recvmsg", "sendmsg", "accept", "accept4", "connect",
    "splice", "tee",
];

/// Note that thread `tid` entered syscall `name` with arguments `args` (the
/// first three); `proc_tid` names the thread under `/proc`. Called from the
/// local syscall path; a no-op unless the profile is enabled.
pub fn note_syscall(
    tid: DetTid,
    name: &'static str,
    args: [u64; 3],
    proc_tid: i32,
    shown: Option<&str>,
) {
    if !enabled() && !crate::poll_identity::enabled() {
        return;
    }
    let mut segments = segments().lock().unwrap();
    let segment = segments.entry(tid.as_raw()).or_default();
    let path = shown.and_then(first_quoted);
    if let Some((previous, _, _)) = segment.current.replace((name, args, proc_tid)) {
        *segment.counts.entry(previous).or_default() += 1;
        if let Some(previous_path) = segment.current_path.take() {
            segment.fs_paths.insert((previous, previous_path));
        }
    }
    segment.current_path = path;
}

/// The syscall `tid` is in, with its first three arguments, as
/// `note_syscall` last recorded it before the thread's next request.
pub fn current_syscall(tid: DetTid) -> Option<(&'static str, [u64; 3])> {
    let segments = segments().lock().unwrap();
    let (name, args, _) = segments.get(&tid.as_raw())?.current?;
    Some((name, args))
}

/// Count a trapped instruction `name` (such as `rdtsc`) that thread `tid`
/// executed without a turn, alongside its untracked syscalls.
pub fn note_event(tid: DetTid, name: &'static str) {
    if !enabled() {
        return;
    }
    let mut segments = segments().lock().unwrap();
    *segments
        .entry(tid.as_raw())
        .or_default()
        .counts
        .entry(name)
        .or_default() += 1;
}

/// Write the syscalls thread `tid` ran since its previous request, other than
/// the syscall that filed it, and name the syscall filing this one. For a
/// descriptor syscall, also name what the descriptor refers to (for a pipe,
/// `pipe:[inode]`), which is the identity an `InternalIOPolling` request does
/// not carry. Called as the thread files a turn request, before the scheduler
/// can commit it.
pub fn flush_segment(tid: DetTid) {
    if !enabled() {
        return;
    }
    let (counts, requesting, fs_paths) = {
        let mut segments = segments().lock().unwrap();
        let segment = segments.entry(tid.as_raw()).or_default();
        // The syscall filing this request is the next turn's own, covered by
        // its resources, so it is not counted as untracked.
        segment.current_path = None;
        let fs_paths = std::mem::take(&mut segment.fs_paths);
        (
            std::mem::take(&mut segment.counts),
            segment.current.take(),
            fs_paths,
        )
    };
    let target = requesting.and_then(|(name, args, proc_tid)| {
        if !FD_FIRST.contains(&name) {
            return None;
        }
        std::fs::read_link(format!("/proc/{proc_tid}/fd/{}", args[0] as i32))
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
    });
    // An open's flags: whether it may create, truncate or write what its
    // `Path` turn (always declared read) names.
    let open_flags = requesting.and_then(|(name, args, _)| match name {
        "openat" => Some(args[2]),
        "open" => Some(args[1]),
        "creat" => Some((libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC) as u64),
        _ => None,
    });
    line(json!({
        "event": "segment",
        "tid": tid.as_raw(),
        "syscalls": counts,
        "requesting": requesting.map(|(name, _, _)| name),
        "requesting_fd": target,
        "requesting_open_flags": open_flags,
        "fs": fs_paths.into_iter().map(|(name, path)| json!([name, path])).collect::<Vec<_>>(),
    }));
}

/// What the probe last learned about a thread from its request.
#[derive(Clone, Copy)]
struct RequestNote {
    pid: DetPid,
    logical_time: LogicalTime,
}

/// The scheduler's part of the probe. One exists only while the profile is
/// enabled.
pub struct ParallelismProbe {
    notes: HashMap<DetTid, RequestNote>,
}

impl std::fmt::Debug for ParallelismProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelismProbe").finish_non_exhaustive()
    }
}

fn perm_name(perm: &Permission) -> &'static str {
    match perm {
        Permission::R => "R",
        Permission::W => "W",
        Permission::RW => "RW",
    }
}

/// The host CPU time of thread `tid`, in nanoseconds, from the first field of
/// `/proc/<tid>/schedstat`.
fn host_cpu_ns(tid: i32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{tid}/schedstat")).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

impl ParallelismProbe {
    /// The scheduler's recorder, or `None` when the profile is disabled.
    pub fn from_env() -> Option<Self> {
        if !enabled() {
            return None;
        }
        line(json!({"event": "start", "host_pid": std::process::id()}));
        Some(ParallelismProbe {
            notes: HashMap::new(),
        })
    }

    /// Note the process and logical time a thread's request carried.
    pub fn note_request(&mut self, tid: DetTid, pid: DetPid, logical_time: LogicalTime) {
        self.notes.insert(tid, RequestNote { pid, logical_time });
    }

    /// Record a committed turn. `physical` is the host (process, thread) the
    /// scheduler registered for `tid`, if any.
    pub fn note_commit(
        &mut self,
        turn: u64,
        tid: DetTid,
        resources: &Resources,
        mm: Option<MmId>,
        physical: Option<(i32, i32)>,
    ) {
        // Without a registered host thread, a ptrace guest's scheduler ID is
        // its host thread ID.
        let (cpu_source, host_tid) = match physical {
            Some((_, host_tid)) => ("registered", host_tid),
            None => ("dettid", tid.as_raw()),
        };
        let cpu_ns = host_cpu_ns(host_tid);
        let note = self.notes.get(&tid).copied();
        let mut res: Vec<(String, &'static str, serde_json::Value)> = resources
            .resources
            .iter()
            .map(|(id, perm)| {
                let value = serde_json::to_value(id).unwrap_or(serde_json::Value::Null);
                (value.to_string(), perm_name(perm), value)
            })
            .collect();
        // HashMap order is arbitrary; sort so the file is stable.
        res.sort_by(|a, b| a.0.cmp(&b.0));
        let res: Vec<serde_json::Value> = res
            .into_iter()
            .map(|(_, perm, value)| json!([value, perm]))
            .collect();
        line(json!({
            "event": "commit",
            "turn": turn,
            "tid": tid.as_raw(),
            "pid": note.map(|n| n.pid.as_raw()),
            "mm": mm.map(|mm| serde_json::to_value(mm).unwrap_or(serde_json::Value::Null)),
            "host_tid": host_tid,
            "cpu_source": cpu_source,
            "cpu_ns": cpu_ns,
            "ltime_ns": note.map(|n| n.logical_time.as_nanos()),
            "poll": resources.poll_attempt,
            "poll_target": resources.poll_target.as_ref().map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null)),
            "fyi": resources.fyi,
            "res": res,
        }));
    }
}
